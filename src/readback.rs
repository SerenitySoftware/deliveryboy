//! Read back what is actually deployed — `deliver status` and `deliver history`.
//!
//! Every release-based deploy already *writes* a durable record on the target:
//! the live path is a symlink named after the deploy id, retained releases sit
//! beside it, and `.deliver/history.tsv` gets one row per deploy (number,
//! deploy id, release, sha, UTC timestamp). Until now nothing read any of it
//! back, so "what version is on prod right now?" meant an ssh session and a
//! `readlink` by hand — for each of ~10 apps.
//!
//! The design mirrors [`crate::configdiff`], for the same reason: the deployer
//! that writes the record declares where it wrote it
//! ([`crate::deployers::ReleaseState`], attached to the step that does the
//! writing), and this module only reads. There is no second copy of the path
//! arithmetic to drift away from the first.
//!
//! Like the config diff, the read is **read-only and batched** — one connection
//! per (target, host), nothing on the target is touched — and everything on the
//! way out goes through [`crate::secrets::redact`].

use crate::config::Target;
use crate::deployers::ReleaseState;
use crate::plan::ServicePlan;
use crate::remote::{capture, nonce, shell_quote};
use serde::Serialize;
use std::collections::BTreeMap;

/// One service on one host whose deploy state can be read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub service: String,
    pub target: String,
    pub host: String,
    pub state: ReleaseState,
}

/// What the live path points at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Live {
    /// The symlink resolves to this release directory.
    Release { path: String },
    /// Something is there, but it is a plain directory — a deploy has not
    /// migrated it into the release layout yet.
    Unmanaged,
    /// Nothing is at the live path.
    Missing,
    /// This deployer has no live symlink (Compose replaces containers in place).
    NotApplicable,
}

/// One row of `.deliver/history.tsv`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deploy {
    pub number: String,
    pub deploy_id: String,
    pub release: String,
    pub sha: String,
    pub at: String,
}

/// Everything read back for one service on one host.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub service: String,
    pub target: String,
    pub host: String,
    /// False when the target could not be read at all — an unreachable host, a
    /// refused login, a shell that never answered.
    pub reachable: bool,
    pub live: Live,
    /// Retained release directories, newest first.
    pub releases: Vec<String>,
    /// Recorded deploys, newest first.
    pub history: Vec<Deploy>,
    pub history_path: String,
    pub live_path: Option<String>,
    pub releases_dir: Option<String>,
    /// Where the activate step records the outgoing release, carried through
    /// so `deliver rollback --to` can keep it honest after a targeted swap.
    pub previous_marker: Option<String>,
}

impl Status {
    /// The deploy id that is live, when one can be established.
    ///
    /// A symlink is the authority: it is what the web server actually follows.
    /// Without one — Compose, which has no symlink — the newest history row is
    /// the best available answer, and [`Status::live_is_recorded`] says which
    /// of the two the caller is looking at.
    pub fn live_deploy_id(&self) -> Option<String> {
        match &self.live {
            Live::Release { path } => Some(basename(path).to_string()),
            Live::NotApplicable => self.history.first().map(|d| d.deploy_id.clone()),
            _ => None,
        }
    }

    /// True when the live deploy id came from the symlink rather than history.
    pub fn live_is_recorded(&self) -> bool {
        matches!(self.live, Live::Release { .. })
    }

    /// The history row for whatever is live, when the two agree.
    pub fn live_deploy(&self) -> Option<&Deploy> {
        let id = self.live_deploy_id()?;
        self.history.iter().find(|d| d.deploy_id == id)
    }
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Every service in the plan that records deploy state on the target.
///
/// Pure: it walks the plan the deployers already produced, so the paths read
/// are by construction the paths the deploy writes.
pub fn collect(plan: &[ServicePlan]) -> Vec<Request> {
    let mut seen: Vec<Request> = Vec::new();
    for sp in plan {
        for step in &sp.steps {
            let Some(state) = &step.release_state else {
                continue;
            };
            let request = Request {
                service: sp.service.clone(),
                target: sp.target.clone(),
                host: sp.host.clone(),
                state: state.clone(),
            };
            // A service compiles one record step; belt and braces against a
            // deployer that ever grows a second one.
            if !seen
                .iter()
                .any(|r| r.service == request.service && r.host == request.host)
            {
                seen.push(request);
            }
        }
    }
    seen
}

/// One framed probe: a key to find the answer by, and the snippet that answers.
struct Probe {
    key: String,
    snippet: String,
}

/// Build the one script that answers every probe for a host.
///
/// Each answer is framed by a nonce so several of them travel back over a
/// single connection. The script adds exactly one newline before the end
/// marker and the parser removes exactly one, so an answer with no trailing
/// newline round-trips unchanged.
fn probe_script(probes: &[Probe], nonce: &str) -> String {
    let mut script = String::new();
    for probe in probes {
        script.push_str(&format!(
            "printf '%s BEGIN %s\\n' {n} {k}; {{ {s}; }} 2>/dev/null; printf '\\n%s END\\n' {n}; ",
            n = shell_quote(nonce),
            k = shell_quote(&probe.key),
            s = probe.snippet,
        ));
    }
    script
}

/// Parse what [`probe_script`] printed back into one answer per key.
///
/// A key the script never reported on is simply absent from the map, which is
/// how the caller tells "nothing there" from "the target never answered".
fn parse_probes(output: &str, nonce: &str) -> BTreeMap<String, String> {
    let begin = format!("{nonce} BEGIN ");
    let end = format!("{nonce} END");
    let mut found = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.split('\n') {
        if let Some(key) = line.strip_prefix(begin.as_str()) {
            current = Some((key.trim().to_string(), Vec::new()));
            continue;
        }
        if line.trim_end() == end {
            if let Some((key, mut body)) = current.take() {
                // The script's own trailing newline is the last empty element.
                if body.last() == Some(&"") {
                    body.pop();
                }
                found.insert(key, body.join("\n"));
            }
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    found
}

/// The three snippets that read one service's state.
fn probes_for(index: usize, state: &ReleaseState, sudo: &str) -> Vec<Probe> {
    let mut probes = vec![Probe {
        key: format!("{index}:history"),
        snippet: format!(
            "if [ -f {h} ]; then {sudo}cat -- {h}; fi",
            h = shell_quote(&state.history_path)
        ),
    }];
    if let Some(live) = &state.live_path {
        probes.push(Probe {
            key: format!("{index}:live"),
            snippet: format!(
                "if [ -L {p} ]; then printf 'link %s\\n' \"$({sudo}readlink {p})\"; \
                 elif [ -d {p} ]; then printf 'dir\\n'; fi",
                p = shell_quote(live)
            ),
        });
    }
    if let Some(releases) = &state.releases_dir {
        probes.push(Probe {
            key: format!("{index}:releases"),
            // The same `ls -1d */` the prune step uses, so what is listed here
            // is what pruning counts.
            snippet: format!(
                "if [ -d {r} ]; then cd {r} && {sudo}ls -1d */; fi",
                r = shell_quote(releases)
            ),
        });
    }
    probes
}

/// Parse `.deliver/history.tsv`, newest first.
///
/// Rows that are not five tab-separated fields are skipped rather than guessed
/// at: the file is appended to by a shell `printf` on the target and a
/// half-written line should not turn into a confident wrong answer.
pub fn parse_history(text: &str) -> Vec<Deploy> {
    let mut rows: Vec<Deploy> = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 5 || fields[1].is_empty() {
                return None;
            }
            Some(Deploy {
                number: fields[0].trim().to_string(),
                deploy_id: fields[1].trim().to_string(),
                release: fields[2].trim().to_string(),
                sha: fields[3].trim().to_string(),
                at: fields[4].trim().to_string(),
            })
        })
        .collect();
    rows.reverse();
    rows
}

/// Read every request off its target, one connection per (target, host).
pub fn read(requests: Vec<Request>, targets: &BTreeMap<String, Target>) -> Vec<Status> {
    // Group by where the read happens, keeping each request's index so the
    // answers can be matched back up.
    let mut by_host: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, request) in requests.iter().enumerate() {
        by_host
            .entry((request.target.clone(), request.host.clone()))
            .or_default()
            .push(i);
    }

    let mut answers: BTreeMap<usize, Option<BTreeMap<String, String>>> = BTreeMap::new();
    for ((target_name, host), indexes) in by_host {
        let Some(target) = targets.get(&target_name) else {
            for i in indexes {
                answers.insert(i, None);
            }
            continue;
        };
        // The deploy writes these paths under sudo, so a 0600 history or a
        // root-owned releases directory is only readable the same way.
        let sudo = if target.uses_sudo() { "sudo " } else { "" };
        let probes: Vec<Probe> = indexes
            .iter()
            .flat_map(|i| probes_for(*i, &requests[*i].state, sudo))
            .collect();
        let marker = nonce();
        let script = probe_script(&probes, &marker);
        let found = match capture(target, &host, &script) {
            Ok(output) => parse_probes(&output, &marker),
            Err(_) => BTreeMap::new(),
        };
        for i in indexes {
            // No answer at all for a service means the host never ran the
            // script — unreachable, not "nothing is deployed".
            let mine: BTreeMap<String, String> = found
                .iter()
                .filter(|(k, _)| k.starts_with(&format!("{i}:")))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            answers.insert(i, if mine.is_empty() { None } else { Some(mine) });
        }
    }

    requests
        .into_iter()
        .enumerate()
        .map(|(i, request)| {
            let answer = answers.remove(&i).flatten();
            let reachable = answer.is_some();
            let answer = answer.unwrap_or_default();
            let live = match answer.get(&format!("{i}:live")) {
                None if request.state.live_path.is_none() => Live::NotApplicable,
                None => Live::Missing,
                Some(text) => match text.trim() {
                    "" => Live::Missing,
                    "dir" => Live::Unmanaged,
                    other => match other.strip_prefix("link ") {
                        Some(path) if !path.trim().is_empty() => Live::Release {
                            path: path.trim().to_string(),
                        },
                        _ => Live::Missing,
                    },
                },
            };
            let mut releases: Vec<String> = answer
                .get(&format!("{i}:releases"))
                .map(|text| {
                    text.lines()
                        .map(|l| l.trim().trim_end_matches('/').to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            releases.reverse();
            let history = answer
                .get(&format!("{i}:history"))
                .map(|text| parse_history(text))
                .unwrap_or_default();
            Status {
                service: request.service,
                target: request.target,
                host: request.host,
                reachable,
                live,
                releases,
                history,
                history_path: request.state.history_path,
                live_path: request.state.live_path,
                releases_dir: request.state.releases_dir,
                previous_marker: request.state.previous_marker,
            }
        })
        .collect()
}

fn heading(status: &Status) -> String {
    if status.host.is_empty() {
        format!("▸ {}  → {}", status.service, status.target)
    } else {
        format!(
            "▸ {}  → {} ({})",
            status.service, status.target, status.host
        )
    }
}

/// `deliver status` — one block per service, answering "what is live?".
pub fn render_status(statuses: &[Status]) -> String {
    let mut out = String::new();
    for status in statuses {
        out.push_str(&heading(status));
        out.push('\n');
        if !status.reachable {
            out.push_str("    could not read the target\n\n");
            continue;
        }
        match (&status.live, status.live_deploy_id()) {
            (Live::Unmanaged, _) => {
                out.push_str("    live          a plain directory, not a release symlink\n");
                out.push_str("                  the next deploy migrates it into releases/\n");
            }
            (_, None) => {
                out.push_str("    live          nothing deployed yet\n");
            }
            (_, Some(id)) => {
                let recorded = status.live_deploy();
                let release = recorded
                    .map(|d| d.release.clone())
                    .unwrap_or_else(|| "unknown release".into());
                let source = if status.live_is_recorded() {
                    ""
                } else {
                    "  (newest recorded deploy — this deployer has no live symlink)"
                };
                out.push_str(&format!("    live          {release} · {id}{source}\n"));
                match recorded {
                    Some(d) => out.push_str(&format!(
                        "    deployed      {} · sha {}\n",
                        d.at,
                        short_sha(&d.sha)
                    )),
                    None => out.push_str(&format!(
                        "    deployed      not recorded in {}\n",
                        status.history_path
                    )),
                }
            }
        }
        if let (Some(live_path), Live::Release { path }) = (&status.live_path, &status.live) {
            out.push_str(&format!("    path          {live_path} → {path}\n"));
        }
        if status.releases_dir.is_some() {
            out.push_str(&format!(
                "    retained      {} release(s)\n",
                status.releases.len()
            ));
        }
        out.push_str(&format!(
            "    history       {} deploy(s) recorded\n\n",
            status.history.len()
        ));
    }
    crate::secrets::redact::scrub(out.trim_end())
}

/// Shorten a full sha for a column, leaving anything else alone.
fn short_sha(sha: &str) -> String {
    if sha.len() > 10 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        sha[..10].to_string()
    } else {
        sha.to_string()
    }
}

/// `deliver history` — the recorded deploys, newest first.
///
/// `limit` of 0 means every row.
pub fn render_history(statuses: &[Status], limit: usize) -> String {
    let mut out = String::new();
    for status in statuses {
        out.push_str(&heading(status));
        out.push('\n');
        if !status.reachable {
            out.push_str("    could not read the target\n\n");
            continue;
        }
        if status.history.is_empty() {
            out.push_str(&format!(
                "    no deploys recorded in {}\n\n",
                status.history_path
            ));
            continue;
        }
        let live = status.live_deploy_id();
        out.push_str("       #  deploy                    release       sha         when\n");
        let shown = if limit == 0 {
            status.history.len()
        } else {
            limit.min(status.history.len())
        };
        for deploy in status.history.iter().take(shown) {
            let marker = if Some(&deploy.deploy_id) == live.as_ref() {
                "  ← live"
            } else {
                ""
            };
            out.push_str(&format!(
                "    {:>4}  {:<24}  {:<12}  {:<10}  {}{marker}\n",
                deploy.number,
                deploy.deploy_id,
                deploy.release,
                short_sha(&deploy.sha),
                deploy.at,
            ));
        }
        if shown < status.history.len() {
            out.push_str(&format!(
                "          … {} older deploy(s) — pass --limit 0 for all\n",
                status.history.len() - shown
            ));
        }
        out.push('\n');
    }
    crate::secrets::redact::scrub(out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ReleaseState {
        ReleaseState {
            history_path: "/var/app/.deliver/history.tsv".into(),
            live_path: Some("/var/app/web".into()),
            releases_dir: Some("/var/app/releases".into()),
            previous_marker: Some("/var/app/releases/.deliver-previous".into()),
        }
    }

    #[test]
    fn history_rows_come_back_newest_first() {
        let text = "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n\
                    2\t20260202-b\tv0.2.0\tdef\t2026-02-02T00:00:00Z\n";
        let rows = parse_history(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].deploy_id, "20260202-b");
        assert_eq!(rows[0].release, "v0.2.0");
        assert_eq!(rows[1].deploy_id, "20260101-a");
    }

    #[test]
    fn a_half_written_row_is_skipped_rather_than_guessed_at() {
        let text = "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n2\t2026020";
        let rows = parse_history(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].deploy_id, "20260101-a");
    }

    #[test]
    fn framed_answers_round_trip_through_one_batched_read() {
        let marker = "__NONCE__";
        let probes = vec![
            Probe {
                key: "0:history".into(),
                snippet: "true".into(),
            },
            Probe {
                key: "0:live".into(),
                snippet: "true".into(),
            },
        ];
        let script = probe_script(&probes, marker);
        assert!(script.contains("'0:history'"), "{script}");
        // What the target would print for that script.
        let output = "__NONCE__ BEGIN 0:history\na\tb\n\n__NONCE__ END\n\
                      __NONCE__ BEGIN 0:live\nlink /var/app/releases/x\n__NONCE__ END\n";
        let found = parse_probes(output, marker);
        assert_eq!(found.get("0:history").map(String::as_str), Some("a\tb"));
        assert_eq!(
            found.get("0:live").map(String::as_str),
            Some("link /var/app/releases/x")
        );
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_its_last_line() {
        let found = parse_probes("N BEGIN 0:history\nonly line\nN END\n", "N");
        assert_eq!(
            found.get("0:history").map(String::as_str),
            Some("only line")
        );
    }

    #[test]
    fn the_live_symlink_names_the_deploy_that_is_live() {
        let status = Status {
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::Release {
                path: "/var/app/releases/20260202-b".into(),
            },
            releases: vec!["20260202-b".into(), "20260101-a".into()],
            history: parse_history(
                "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n\
                 2\t20260202-b\tv0.2.0\tdef0123456789\t2026-02-02T00:00:00Z\n",
            ),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
        };
        assert_eq!(status.live_deploy_id().as_deref(), Some("20260202-b"));
        assert_eq!(status.live_deploy().unwrap().release, "v0.2.0");
        let text = render_status(std::slice::from_ref(&status));
        assert!(text.contains("v0.2.0 · 20260202-b"), "{text}");
        assert!(text.contains("2 release(s)"), "{text}");
        assert!(text.contains("sha def0123456"), "{text}");
        let history = render_history(&[status], 1);
        assert!(history.contains("← live"), "{history}");
        assert!(history.contains("1 older deploy(s)"), "{history}");
    }

    #[test]
    fn a_live_release_missing_from_history_says_so_rather_than_inventing_one() {
        let status = Status {
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::Release {
                path: "/var/app/releases/20260303-c".into(),
            },
            releases: vec!["20260303-c".into()],
            history: Vec::new(),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
        };
        let text = render_status(&[status]);
        assert!(text.contains("unknown release · 20260303-c"), "{text}");
        assert!(text.contains("not recorded in"), "{text}");
    }

    #[test]
    fn an_unreachable_target_is_reported_not_read_as_empty() {
        let status = Status {
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: false,
            live: Live::Missing,
            releases: Vec::new(),
            history: Vec::new(),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
        };
        let text = render_status(&[status]);
        assert!(text.contains("could not read the target"), "{text}");
        assert!(!text.contains("nothing deployed yet"), "{text}");
    }

    #[test]
    fn a_deployer_with_no_symlink_falls_back_to_the_newest_recorded_deploy() {
        let status = Status {
            service: "stack".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::NotApplicable,
            releases: Vec::new(),
            history: parse_history("7\t20260404-d\tv1.0.0\tfeed\t2026-04-04T00:00:00Z\n"),
            history_path: state().history_path,
            live_path: None,
            releases_dir: None,
            previous_marker: None,
        };
        assert!(!status.live_is_recorded());
        let text = render_status(&[status]);
        assert!(text.contains("v1.0.0 · 20260404-d"), "{text}");
        assert!(text.contains("no live symlink"), "{text}");
        // No release directory means no retained count to claim.
        assert!(!text.contains("retained"), "{text}");
    }
}
