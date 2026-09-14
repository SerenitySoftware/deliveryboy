//! Show what a release changes in the infrastructure config already running on
//! the target, before the swap rather than after it.
//!
//! `plan` shows the *steps* and `verify` proves the *result*. Neither answers
//! the question that matters most on a shared host with a dozen apps on it:
//! what does this release do to the nginx vhost and the compose file that are
//! live right now? The content is already in hand at compile time — the vhost is
//! rendered locally before it ships, and the compose file is a file in the repo
//! — so one read of the target is enough to turn that into a unified diff.
//!
//! Three properties this module is built around:
//!
//! * **Read-only, and never fatal.** Nothing here mutates the target, and a
//!   failed read downgrades to a printed line rather than a failed deploy. A
//!   feature whose whole job is to *inform* must not become a new way for a
//!   release to break.
//! * **One round trip per host.** Every path for a given (target, host) is read
//!   by a single `cat` loop, not one connection per file.
//! * **Scrubbed.** A rendered vhost holds resolved secrets, so every line
//!   leaving here goes through [`crate::secrets::redact`] — both sides of the
//!   diff, since the value that is *already live* on the target is the same
//!   secret as the one about to replace it.

use crate::config::Target;
use crate::plan::ServicePlan;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

/// Lines of unchanged context printed around each hunk.
const CONTEXT: usize = 3;

/// Above this many lines on either side, the O(n·m) table is not worth building
/// for a file nobody is going to read line-by-line anyway.
const MAX_DIFF_LINES: usize = 2_000;

/// One file this release will install, and where it lives on the target.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub service: String,
    pub target: String,
    pub host: String,
    pub remote_path: String,
    /// Exactly what this deploy will put at `remote_path`.
    pub content: String,
}

/// What the read of the live file found.
#[derive(Debug, Clone, PartialEq)]
pub enum Live {
    /// The file exists and this is what is in it.
    Present(String),
    /// Nothing is there yet — the release creates it.
    Absent,
    /// The path could not be read. Informational; the deploy continues.
    Unreadable(String),
}

/// A pending file paired with what is live, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub pending: Pending,
    pub live: Live,
}

impl Change {
    /// True when the live file already matches what we are about to install.
    pub fn unchanged(&self) -> bool {
        matches!(&self.live, Live::Present(text) if normalize(text) == normalize(&self.pending.content))
    }
}

/// Trailing-whitespace noise is not a config change worth showing: a file that
/// round-tripped through `scp` and back differs only by a final newline.
fn normalize(text: &str) -> &str {
    text.trim_end_matches(['\n', '\r'])
}

/// Every long-lived config file the compiled plan will overwrite.
///
/// Pure: walks the plan the deployers already produced, so the content shown is
/// by construction the same string the step installs — there is no second code
/// path to drift out of sync with the first.
pub fn collect(plan: &[ServicePlan]) -> Vec<Pending> {
    let mut pending = Vec::new();
    for sp in plan {
        for step in &sp.steps {
            let Some(live) = &step.live_config else {
                continue;
            };
            pending.push(Pending {
                service: sp.service.clone(),
                target: sp.target.clone(),
                host: sp.host.clone(),
                remote_path: live.remote_path.clone(),
                content: live.content.clone(),
            });
        }
    }
    pending
}

/// A marker no config file will contain, used to frame each file in the output
/// of the one batched read.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("__DELIVER_CFG_{:016x}_{}__", nanos, std::process::id())
}

/// Single-quote a path for `sh`.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Build the one script that reads every path for a host.
///
/// `cat` writes no trailing newline of its own, so the script always adds
/// exactly one before the end marker and the parser removes exactly one — which
/// makes a file with no final newline round-trip unchanged.
fn read_script(paths: &[String], sudo: &str, nonce: &str) -> String {
    let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
    format!(
        "for p in {}; do \
           printf '%s BEGIN %s\\n' {n} \"$p\"; \
           if [ -f \"$p\" ]; then \
             if {sudo}cat -- \"$p\" 2>/dev/null; then printf '\\n%s END ok\\n' {n}; \
             else printf '\\n%s END unreadable\\n' {n}; fi; \
           elif [ -e \"$p\" ]; then printf '\\n%s END notfile\\n' {n}; \
           else printf '\\n%s END absent\\n' {n}; fi; \
         done",
        quoted.join(" "),
        n = shell_quote(nonce),
    )
}

/// Parse what [`read_script`] printed back into one [`Live`] per path.
///
/// Anything the script did not report on — a connection that died halfway, a
/// shell that refused the loop — is left out, and the caller reports those paths
/// as unreadable rather than silently claiming they are absent.
fn parse_read(output: &str, nonce: &str) -> BTreeMap<String, Live> {
    let begin = format!("{nonce} BEGIN ");
    let end = format!("{nonce} END ");
    let mut found = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.split('\n') {
        if let Some(path) = line.strip_prefix(begin.as_str()) {
            current = Some((path.to_string(), Vec::new()));
            continue;
        }
        if let Some(status) = line.strip_prefix(end.as_str()) {
            if let Some((path, body)) = current.take() {
                // The script's own trailing newline is the last empty element.
                let mut body = body;
                if body.last() == Some(&"") {
                    body.pop();
                }
                found.insert(
                    path,
                    match status.trim() {
                        "ok" => Live::Present(body.join("\n")),
                        "absent" => Live::Absent,
                        "notfile" => Live::Unreadable("not a regular file".into()),
                        _ => Live::Unreadable("permission denied".into()),
                    },
                );
            }
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    found
}

/// Run the read on the target. `method: local` reads this machine, matching how
/// [`crate::exec::run_ssh`] treats a local target.
fn capture(target: &Target, host: &str, script: &str) -> std::io::Result<String> {
    let out = if target.is_local() {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stderr(Stdio::null())
            .output()?
    } else {
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(script)
            .stderr(Stdio::null())
            .output()?
    };
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Read the live side of every pending file, one connection per (target, host).
pub fn read_live(pending: Vec<Pending>, targets: &BTreeMap<String, Target>) -> Vec<Change> {
    let mut by_host: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for p in &pending {
        let paths = by_host
            .entry((p.target.clone(), p.host.clone()))
            .or_default();
        if !paths.contains(&p.remote_path) {
            paths.push(p.remote_path.clone());
        }
    }

    let mut live: BTreeMap<(String, String, String), Live> = BTreeMap::new();
    for ((target_name, host), paths) in by_host {
        let Some(target) = targets.get(&target_name) else {
            continue;
        };
        let marker = nonce();
        // The install steps already read and write these paths under sudo, so
        // reading them any other way would just fail on a 0600 conf.
        let sudo = if target.uses_sudo() { "sudo " } else { "" };
        let script = read_script(&paths, sudo, &marker);
        let found = match capture(target, &host, &script) {
            Ok(output) => parse_read(&output, &marker),
            Err(e) => {
                let reason = e.to_string();
                for path in &paths {
                    live.insert(
                        (target_name.clone(), host.clone(), path.clone()),
                        Live::Unreadable(reason.clone()),
                    );
                }
                continue;
            }
        };
        for path in paths {
            let state = found
                .get(&path)
                .cloned()
                .unwrap_or_else(|| Live::Unreadable("no answer from the target".into()));
            live.insert((target_name.clone(), host.clone(), path), state);
        }
    }

    pending
        .into_iter()
        .map(|p| {
            let live = live
                .get(&(p.target.clone(), p.host.clone(), p.remote_path.clone()))
                .cloned()
                .unwrap_or_else(|| Live::Unreadable("target is not configured".into()));
            Change { pending: p, live }
        })
        .collect()
}

/// Longest-common-subsequence lengths for two line slices.
fn lcs_table(a: &[&str], b: &[&str]) -> Vec<Vec<u32>> {
    let mut table = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

#[derive(Debug, PartialEq)]
enum Op<'a> {
    Keep(&'a str),
    Del(&'a str),
    Add(&'a str),
}

fn ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Op<'a>> {
    let table = lcs_table(a, b);
    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(Op::Keep(a[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(Op::Del(a[i]));
            i += 1;
        } else {
            out.push(Op::Add(b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Op::Del(l)));
    out.extend(b[j..].iter().map(|l| Op::Add(l)));
    out
}

/// A unified diff of `old` → `new`, or `None` when they are the same.
///
/// Hand-rolled rather than pulled in as a dependency: the CLI ships as one
/// binary people install from crates.io, and a screenful of LCS is a smaller
/// cost than another crate in `Cargo.lock` for every user.
pub fn unified(old: &str, new: &str) -> Option<String> {
    if normalize(old) == normalize(new) {
        return None;
    }
    let a: Vec<&str> = normalize(old).split('\n').collect();
    let b: Vec<&str> = normalize(new).split('\n').collect();
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return Some(format!(
            "(file too large to diff line-by-line: {} live line(s) → {} new line(s))",
            a.len(),
            b.len()
        ));
    }

    let ops = ops(&a, &b);
    // Which ops to print: every change, plus CONTEXT unchanged lines around it.
    let changed: Vec<bool> = ops.iter().map(|o| !matches!(o, Op::Keep(_))).collect();
    let mut show = vec![false; ops.len()];
    for (idx, is_change) in changed.iter().enumerate() {
        if !is_change {
            continue;
        }
        let lo = idx.saturating_sub(CONTEXT);
        let hi = (idx + CONTEXT).min(ops.len() - 1);
        for slot in show.iter_mut().take(hi + 1).skip(lo) {
            *slot = true;
        }
    }

    let mut out = String::new();
    // Line numbers are the live file's, so a reported line matches what an
    // operator sees when they open the file on the box.
    let mut live_line = 0usize;
    let mut skipping = false;
    for (idx, op) in ops.iter().enumerate() {
        if matches!(op, Op::Keep(_) | Op::Del(_)) {
            live_line += 1;
        }
        if !show[idx] {
            if !skipping {
                out.push_str("   ...\n");
                skipping = true;
            }
            continue;
        }
        skipping = false;
        match op {
            Op::Keep(line) => out.push_str(&format!("{live_line:>4}   {line}\n")),
            Op::Del(line) => out.push_str(&format!("{live_line:>4} - {line}\n")),
            Op::Add(line) => out.push_str(&format!("     + {line}\n")),
        }
    }
    Some(out.trim_end().to_string())
}

/// Render every change as the block printed before a deploy.
///
/// Returns `None` when there is nothing to say — no service in this plan owns a
/// long-lived config file — so the caller can stay silent rather than print an
/// empty heading.
pub fn render(changes: &[Change]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let mut out = String::new();
    for change in changes {
        let Pending {
            service,
            remote_path,
            host,
            content,
            ..
        } = &change.pending;
        let where_ = if host.is_empty() {
            remote_path.clone()
        } else {
            format!("{host}:{remote_path}")
        };
        match &change.live {
            Live::Absent => {
                out.push_str(&format!("{service} → {where_}\n"));
                out.push_str(&format!(
                    "   new file ({} line(s))\n\n",
                    normalize(content).split('\n').count()
                ));
            }
            Live::Unreadable(reason) => {
                out.push_str(&format!("{service} → {where_}\n"));
                out.push_str(&format!("   could not read the live file: {reason}\n\n"));
            }
            Live::Present(text) => match unified(text, content) {
                None => {
                    out.push_str(&format!("{service} → {where_}\n"));
                    out.push_str("   no changes to live config\n\n");
                }
                Some(diff) => {
                    out.push_str(&format!("{service} → {where_}\n"));
                    out.push_str(&format!("{diff}\n\n"));
                }
            },
        }
    }
    // Both sides of a rendered vhost hold resolved secrets — the live one just
    // as much as the one about to replace it.
    Some(crate::secrets::redact::scrub(out.trim_end()))
}

/// True when at least one file on the target demonstrably changes.
///
/// A file we could not read is deliberately *not* counted. It is reported on
/// its own line, but stopping the run to ask "apply these changes?" about a
/// diff we were unable to compute would add friction and no information — the
/// operator already confirmed the release, and nothing here is a gate.
pub fn any_changes(changes: &[Change]) -> bool {
    changes.iter().any(|c| match &c.live {
        Live::Absent => true,
        Live::Present(_) => !c.unchanged(),
        Live::Unreadable(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_produce_no_diff() {
        assert_eq!(unified("a\nb\nc\n", "a\nb\nc\n"), None);
    }

    #[test]
    fn a_trailing_newline_is_not_a_change() {
        assert_eq!(unified("a\nb", "a\nb\n"), None);
    }

    #[test]
    fn a_replaced_line_shows_both_sides() {
        let diff = unified("a\nb\nc", "a\nB\nc").unwrap();
        assert!(diff.contains("- b"), "{diff}");
        assert!(diff.contains("+ B"), "{diff}");
        assert!(diff.contains("   a"), "{diff}");
    }

    #[test]
    fn distant_unchanged_lines_are_elided() {
        let live: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let mut new = live.clone();
        new.push_str("line 41\n");
        let diff = unified(&live, &new).unwrap();
        assert!(diff.contains("..."), "{diff}");
        assert!(diff.contains("+ line 41"), "{diff}");
        assert!(!diff.contains("line 5"), "{diff}");
    }

    #[test]
    fn line_numbers_follow_the_live_file() {
        let diff = unified("a\nb\nc\nd", "a\nb\nc\nD").unwrap();
        assert!(diff.contains("   4 - d"), "{diff}");
    }

    #[test]
    fn a_huge_file_falls_back_to_a_summary() {
        let live: String = (0..MAX_DIFF_LINES + 5).map(|i| format!("l{i}\n")).collect();
        let diff = unified(&live, "one line").unwrap();
        assert!(diff.contains("too large to diff"), "{diff}");
    }

    #[test]
    fn read_output_is_parsed_back_into_file_contents() {
        let n = "__NONCE__";
        let output = format!(
            "{n} BEGIN /etc/nginx/sites-available/a\nserver {{\n  listen 80;\n}}\n{n} END ok\n\
             {n} BEGIN /srv/b.yml\n\n{n} END absent\n\
             {n} BEGIN /srv/c.yml\n\n{n} END unreadable\n"
        );
        let found = parse_read(&output, n);
        assert_eq!(
            found.get("/etc/nginx/sites-available/a"),
            Some(&Live::Present("server {\n  listen 80;\n}".into()))
        );
        assert_eq!(found.get("/srv/b.yml"), Some(&Live::Absent));
        assert!(matches!(found.get("/srv/c.yml"), Some(Live::Unreadable(_))));
    }

    #[test]
    fn a_file_with_no_final_newline_round_trips() {
        let n = "__NONCE__";
        // The script adds exactly one newline before the marker.
        let output = format!("{n} BEGIN /srv/x\nno trailing newline\n{n} END ok\n");
        let found = parse_read(&output, n);
        assert_eq!(
            found.get("/srv/x"),
            Some(&Live::Present("no trailing newline".into()))
        );
    }

    #[test]
    fn a_path_the_target_never_answered_for_is_unreadable_not_absent() {
        let found = parse_read("", "__NONCE__");
        assert!(found.is_empty());
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        let script = read_script(&["/a b/c'd".to_string()], "sudo ", "__N__");
        assert!(script.contains(r"'/a b/c'\''d'"), "{script}");
        assert!(script.contains("sudo cat --"), "{script}");
    }

    #[test]
    fn unchanged_ignores_a_trailing_newline_difference() {
        let change = Change {
            pending: Pending {
                service: "web".into(),
                target: "production".into(),
                host: "host".into(),
                remote_path: "/srv/x".into(),
                content: "a\nb\n".into(),
            },
            live: Live::Present("a\nb".into()),
        };
        assert!(change.unchanged());
        assert!(!any_changes(std::slice::from_ref(&change)));
    }

    #[test]
    fn an_absent_live_file_counts_as_a_change() {
        let change = Change {
            pending: Pending {
                service: "web".into(),
                target: "production".into(),
                host: "host".into(),
                remote_path: "/srv/x".into(),
                content: "a\n".into(),
            },
            live: Live::Absent,
        };
        assert!(any_changes(std::slice::from_ref(&change)));
        let text = render(std::slice::from_ref(&change)).unwrap();
        assert!(text.contains("new file (1 line(s))"), "{text}");
    }

    #[test]
    fn a_matching_live_file_says_so_explicitly() {
        let change = Change {
            pending: Pending {
                service: "nginx".into(),
                target: "production".into(),
                host: "host".into(),
                remote_path: "/etc/nginx/sites-available/x".into(),
                content: "server {}\n".into(),
            },
            live: Live::Present("server {}".into()),
        };
        let text = render(std::slice::from_ref(&change)).unwrap();
        assert!(text.contains("no changes to live config"), "{text}");
    }

    #[test]
    fn rendered_output_elides_resolved_secrets_on_both_sides() {
        crate::secrets::redact::record("VHOST_TOKEN", "zzz-live-config-secret-77");
        let change = Change {
            pending: Pending {
                service: "nginx".into(),
                target: "production".into(),
                host: "host".into(),
                remote_path: "/etc/nginx/sites-available/x".into(),
                content: "token zzz-live-config-secret-77;\nnew line;\n".into(),
            },
            live: Live::Present("token zzz-live-config-secret-77;".into()),
        };
        let text = render(std::slice::from_ref(&change)).unwrap();
        assert!(!text.contains("zzz-live-config-secret-77"), "{text}");
        assert!(text.contains("[redacted:VHOST_TOKEN]"), "{text}");
        assert!(text.contains("+ new line;"), "{text}");
    }

    #[test]
    fn an_unreadable_live_file_is_reported_but_does_not_ask_for_confirmation() {
        let change = Change {
            pending: Pending {
                service: "nginx".into(),
                target: "production".into(),
                host: "host".into(),
                remote_path: "/etc/nginx/sites-available/x".into(),
                content: "server {}\n".into(),
            },
            live: Live::Unreadable("permission denied".into()),
        };
        let text = render(std::slice::from_ref(&change)).unwrap();
        assert!(text.contains("could not read the live file"), "{text}");
        assert!(!any_changes(std::slice::from_ref(&change)));
    }

    #[test]
    fn nothing_pending_renders_nothing() {
        assert_eq!(render(&[]), None);
    }
}
