//! Step execution: fail-fast, with rollback.
//!
//! Steps that mutate the target carry an undo command. As they succeed we push
//! those onto a stack; if any later step fails, the stack is unwound in reverse
//! so a failed deploy doesn't leave a half-changed target. Later services never
//! start after a failure.
//!
//! Ctrl-C is a failure too. While `execute` runs, `interrupt` counts the
//! keypress instead of dying; the step in flight finishes or is killed with it,
//! and the stack unwinds as if that step had failed. A Ctrl-C during the unwind
//! abandons it and says so, rather than leaving the operator guessing.

use crate::config::{Span, Target};
use crate::deployers::{PlannedStep, StepKind};
use crate::interrupt;
use crate::plan::{ServicePlan, Soak};
use crate::secrets::redact::scrub;
use anyhow::Result;
use std::collections::BTreeMap;
use std::process::Command;

pub struct Outcome {
    pub ok: bool,
    pub rolled_back: usize,
    /// Undo commands that ran and did not succeed. Counted apart from
    /// `rolled_back` so a failed unwind is never summarised as "nothing
    /// needed rolling back".
    pub rollback_failed: usize,
    pub failed_step: Option<String>,
    /// The run stopped because the operator pressed Ctrl-C.
    pub interrupted: bool,
    /// A Ctrl-C during the unwind stopped it part-way.
    pub abandoned: bool,
}

impl Outcome {
    fn success() -> Self {
        Outcome {
            ok: true,
            rolled_back: 0,
            rollback_failed: 0,
            failed_step: None,
            interrupted: false,
            abandoned: false,
        }
    }

    fn failed(failed_step: String, unwound: Unwound, interrupted: bool) -> Self {
        Outcome {
            ok: false,
            rolled_back: unwound.done,
            rollback_failed: unwound.failed,
            failed_step: Some(failed_step),
            interrupted,
            abandoned: unwound.abandoned,
        }
    }
}

type Undo = (String, String, Target, String);

fn run_local(command: &str, cwd: Option<&String>) -> Result<bool> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    Ok(cmd.status()?.success())
}

/// Run a remote step. With `method: local` the "remote" is this machine, so the
/// command runs in a local shell instead of over ssh.
pub fn run_ssh(target: &Target, host: &str, command: &str) -> Result<bool> {
    if target.is_local() {
        return run_local(command, None);
    }
    let status = Command::new("ssh")
        .args(target.ssh_args())
        .arg(format!("{}@{host}", target.ssh.user))
        .arg(command)
        .status()?;
    Ok(status.success())
}

struct StepResult {
    ok: bool,
    failure: Option<String>,
}

impl From<bool> for StepResult {
    fn from(ok: bool) -> Self {
        Self { ok, failure: None }
    }
}

const HTTP_DETAIL_LIMIT: usize = 512;

fn bounded_safe(text: &str) -> String {
    scrub(text).chars().take(HTTP_DETAIL_LIMIT).collect()
}

/// Only display an unambiguous ASCII URL subset. Never decode escapes or echo
/// unknown syntax. Query, fragment and userinfo have no diagnostic value here.
fn http_display_url(raw: &str) -> String {
    let hidden = || "[URL hidden]".to_string();
    if !raw.is_ascii() || raw.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return hidden();
    }
    let base = raw.split(['?', '#']).next().unwrap_or("");
    let (prefix, rest) = if let Some(rest) = base.strip_prefix("https://") {
        ("https://", rest)
    } else if let Some(rest) = base.strip_prefix("http://") {
        ("http://", rest)
    } else if let Some(rest) = base.strip_prefix("//") {
        ("//", rest)
    } else {
        if base.is_empty() || base.starts_with("//") {
            return hidden();
        }
        return if safe_http_path(base) {
            bounded_safe(base)
        } else {
            hidden()
        };
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, ""), |(a, _)| (a, &rest[a.len()..]));
    if authority.matches('@').count() > 1 {
        return hidden();
    }
    let host = authority.rsplit('@').next().unwrap_or("");
    // Hostnames, IPv4 and bracketed IPv6, optionally with a numeric port.
    let valid_host = if let Some(ipv6) = host.strip_prefix('[') {
        ipv6.split_once(']').is_some_and(|(ip, port)| {
            ip.parse::<std::net::Ipv6Addr>().is_ok() && (port.is_empty() || valid_http_port(port))
        })
    } else {
        let (name, port) = host
            .split_once(':')
            .map_or((host, ""), |(n, _)| (n, &host[n.len()..]));
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && (port.is_empty() || valid_http_port(port))
    };
    if !valid_host || !safe_http_path(path) {
        return hidden();
    }
    bounded_safe(&format!("{prefix}{host}{path}"))
}

fn valid_http_port(port: &str) -> bool {
    port.strip_prefix(':').is_some_and(|p| {
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()
    })
}

fn safe_http_path(path: &str) -> bool {
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/-._~".contains(&b))
}

fn step_label(step: &PlannedStep) -> String {
    if let StepKind::Http { url, .. } = &step.kind {
        let label = step.label.replace(url, &http_display_url(url));
        if label.chars().any(char::is_control) {
            return "HTTP check".into();
        }
        bounded_safe(&label)
    } else {
        scrub(&step.label)
    }
}

fn step_detail(step: &PlannedStep) -> String {
    match &step.kind {
        StepKind::Http { url, .. } => http_display_url(url),
        _ => scrub(&step.detail()),
    }
}

fn step_failure(step: &PlannedStep, result: &StepResult) -> String {
    match &result.failure {
        Some(reason) => scrub(&format!("{}: {reason}", step_label(step))),
        None => step_label(step),
    }
}

fn http_command(url: &str, follow_redirects: bool) -> Command {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}\n%{redirect_url}",
        "--max-time",
        "20",
    ]);
    if follow_redirects {
        cmd.arg("-L");
    }
    cmd.arg(url);
    cmd
}

fn http_metadata(stdout: &[u8]) -> (String, Option<String>) {
    let mut fields = stdout.splitn(2, |byte| *byte == b'\n');
    let status = std::str::from_utf8(fields.next().unwrap_or_default())
        .unwrap_or_default()
        .trim();
    let code = if status.len() == 3 && status.bytes().all(|b| b.is_ascii_digit()) {
        status.to_string()
    } else {
        "unknown".into()
    };
    let redirect = fields.next().unwrap_or_default();
    let destination = (!redirect.is_empty()).then(|| {
        std::str::from_utf8(redirect)
            .map(http_display_url)
            .unwrap_or_else(|_| "[URL hidden]".into())
    });
    (code, destination)
}

/// stderr can contain arbitrary headers, URLs and server text. Use only curl's
/// numeric exit code, never guess which portions of that text are safe.
fn http_failure(code: &str, expect: u16, redirect: Option<&str>, exit: Option<i32>) -> String {
    let mut text = format!("http {code} (want {expect})");
    if exit != Some(0) {
        let reason = match exit {
            Some(5 | 6) => "could not resolve host",
            Some(7) => "Failed to connect (connection failed)",
            Some(28) => "operation timed out",
            Some(35 | 60) => "TLS connection or certificate failed",
            Some(47) => "too many redirects",
            _ => "request failed",
        };
        text.push_str(&format!(
            ", curl {}: {reason} [diagnostic detail hidden]",
            exit.map_or_else(|| "unknown".into(), |n| n.to_string())
        ));
    }
    if let Some(destination) = redirect {
        text.push_str(&format!(", redirect to {destination}"));
    }
    bounded_safe(&text)
}

fn run_http(
    url: &str,
    expect: u16,
    retries: u32,
    interval: u64,
    follow_redirects: bool,
) -> Result<StepResult> {
    let mut last = None;
    for attempt in 1..=retries.max(1) {
        // curl keeps the binary dependency-free.
        let out = http_command(url, follow_redirects).output()?;
        let (code, redirect) = http_metadata(&out.stdout);
        if code == expect.to_string() {
            return Ok(true.into());
        }
        let reason = http_failure(&code, expect, redirect.as_deref(), out.status.code());
        eprintln!(
            "     {}, attempt {attempt}/{}",
            scrub(&reason),
            retries.max(1)
        );
        last = Some(reason);
        if attempt < retries.max(1)
            && !interrupt::nap(std::time::Duration::from_secs(interval), || {
                interrupt::count() > 0
            })
        {
            break;
        }
    }
    Ok(StepResult {
        ok: false,
        failure: last,
    })
}

fn run_step(step: &PlannedStep, target: &Target, host: &str, dry_run: bool) -> Result<StepResult> {
    if dry_run {
        println!("        (dry-run, not executed)");
        if step.rollback.is_some() {
            println!("        (undo available if a later step fails)");
        }
        return Ok(true.into());
    }
    match &step.kind {
        StepKind::Command { command, cwd } => run_local(command, cwd.as_ref()).map(Into::into),
        StepKind::Ssh { command } => run_ssh(target, host, command).map(Into::into),
        StepKind::Http {
            url,
            expect_status,
            retries,
            interval,
            follow_redirects,
        } => run_http(url, *expect_status, *retries, *interval, *follow_redirects),
        StepKind::WriteFile {
            path,
            mode,
            content,
        } => write_file(path, *mode, content).map(Into::into),
    }
}

/// Write a file with an exact mode, creating parents. The mode is set *before*
/// the contents land, so a secret is never briefly world-readable.
fn write_file(path: &str, _mode: u32, content: &str) -> Result<bool> {
    use std::io::Write;
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(_mode);
    }
    let mut file = opts.open(p)?;
    file.write_all(content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(_mode))?;
    }
    Ok(true)
}

#[derive(Default)]
struct Unwound {
    done: usize,
    /// Undos that ran and failed (or could not be run at all).
    failed: usize,
    abandoned: bool,
}

/// Undo, in reverse order, the mutating steps that already succeeded. A Ctrl-C
/// while this runs stops it before the next undo: the operator asked twice.
fn unwind(pending: Vec<Undo>) -> Unwound {
    let mut unwound = Unwound::default();
    if pending.is_empty() {
        println!("\nnothing to roll back (no reversible step had run yet)");
        return unwound;
    }
    let before = interrupt::count();
    println!("\n↩ rolling back {} step(s)…", pending.len());
    for (label, undo, target, host) in pending.into_iter().rev() {
        let label = scrub(&label);
        println!("  undo: {label}");
        let result = run_ssh(&target, &host, &undo);
        if interrupt::count() > before {
            eprintln!(
                "\n✗ rollback abandoned during: {label} — `deliver status` says where each service stands."
            );
            unwound.abandoned = true;
            return unwound;
        }
        match result {
            Ok(true) => unwound.done += 1,
            Ok(false) => {
                unwound.failed += 1;
                eprintln!("  ✗ rollback failed for: {label} — resolve by hand");
            }
            Err(e) => {
                unwound.failed += 1;
                eprintln!("  ✗ rollback error for {label}: {}", scrub(&e.to_string()));
            }
        }
    }
    unwound
}

/// Stop for a Ctrl-C that landed during (or just before) `label`.
fn interrupted_at(label: &str, undoable: Vec<Undo>) -> Outcome {
    let label = scrub(label);
    eprintln!("\n✗ interrupted at: {label}");
    eprintln!("  stopping — later steps and services will not run.");
    Outcome::failed(format!("interrupted at {label}"), unwind(undoable), true)
}

/// `lock` is the deploy lock this run holds, refreshed before every step and
/// soak round so a long release is not taken over as abandoned while it runs;
/// `None` when nothing is held (a dry run, `verify`).
pub fn execute(
    plan: &[ServicePlan],
    targets: &BTreeMap<String, Target>,
    dry_run: bool,
    lock: Option<&crate::lock::Guard>,
) -> Result<Outcome> {
    // A dry run runs nothing, so Ctrl-C keeps its default meaning there.
    let _armed = (!dry_run).then(interrupt::arm);
    let interrupted = || !dry_run && interrupt::count() > 0;
    // (label, undo command, target) for successful, reversible steps.
    let mut undoable: Vec<Undo> = Vec::new();

    for sp in plan {
        let target = &targets[&sp.target];
        // Cleanup steps run after the real work, and never gate it.
        let (work, cleanup): (Vec<&PlannedStep>, Vec<&PlannedStep>) =
            sp.steps.iter().partition(|s| !s.cleanup);
        println!(
            "  • {} → {} [{}] ({} steps)",
            sp.service,
            sp.target,
            sp.host,
            work.len()
        );
        for (i, step) in work.iter().enumerate() {
            if interrupted() {
                return Ok(interrupted_at(&step_label(step), undoable));
            }
            println!(
                "    {:>2}/{}. {} [{}]",
                i + 1,
                work.len(),
                step_label(step),
                step.type_name()
            );
            if let Some(lock) = lock {
                lock.refresh();
            }
            let result = run_step(step, target, &sp.host, dry_run)?;
            if result.ok {
                if let Some(undo) = &step.rollback {
                    undoable.push((
                        step_label(step),
                        undo.clone(),
                        target.clone(),
                        sp.host.clone(),
                    ));
                }
            }
            // Whether the step finished or died with the keypress, the
            // operator asked to stop: everything that did change is undone.
            if interrupted() {
                return Ok(interrupted_at(&step_label(step), undoable));
            }
            if !result.ok {
                // Leave intermediates on disk — they're the evidence.
                if !cleanup.is_empty() {
                    println!(
                        "     (keeping build artifacts for debugging — `deliver clean` removes them)"
                    );
                }
                eprintln!(
                    "\n✗ failed: {} ({})",
                    step_failure(step, &result),
                    step_detail(step)
                );
                eprintln!("  stopping — later steps and services will not run.");
                let unwound = if dry_run {
                    Unwound::default()
                } else {
                    unwind(undoable)
                };
                // Travels on into the notification payload, so it leaves here
                // already scrubbed.
                return Ok(Outcome::failed(step_failure(step, &result), unwound, false));
            }
        }

        for step in &cleanup {
            if interrupted() {
                return Ok(interrupted_at(&step_label(step), undoable));
            }
            println!("    cleanup: {}", step_label(step));
            // Never fail a good deploy over cleanup; just say so.
            match run_step(step, target, &sp.host, dry_run) {
                Ok(result) if result.ok => {}
                _ => eprintln!("    (cleanup did not complete: {})", step_label(step)),
            }
        }
        println!();
    }

    // Every service is live; the undo stack is still whole, so a check that
    // stops passing inside the window unwinds exactly as one that never passed.
    if let Some(failed) = soak(plan, targets, dry_run, lock)? {
        if interrupted() {
            return Ok(interrupted_at(&failed, undoable));
        }
        let unwound = if dry_run {
            Unwound::default()
        } else {
            unwind(undoable)
        };
        return Ok(Outcome::failed(failed, unwound, false));
    }
    if interrupted() {
        return Ok(interrupted_at("the end of the release", undoable));
    }
    Ok(Outcome::success())
}

/// When each soaking service's checks re-run, as seconds after the window
/// opens: `every`, `2 × every`, … up to `for`. Services interleave by time.
fn soak_schedule(soaking: &[(&ServicePlan, &Soak)]) -> Vec<(u64, usize, u64, u64)> {
    // (at, which service, round, of rounds)
    let mut events = Vec::new();
    for (i, (_, soak)) in soaking.iter().enumerate() {
        let rounds = soak.duration.0 / soak.every.0;
        for round in 1..=rounds {
            events.push((round * soak.every.0, i, round, rounds));
        }
    }
    events.sort();
    events
}

/// Re-run the verify checks of every service that has a soak window, on each
/// one's own schedule, once the whole release has shipped. Returns the check
/// that failed — scrubbed, since it travels on into the failure notice.
fn soak(
    plan: &[ServicePlan],
    targets: &BTreeMap<String, Target>,
    dry_run: bool,
    lock: Option<&crate::lock::Guard>,
) -> Result<Option<String>> {
    let soaking: Vec<(&ServicePlan, &Soak)> = plan
        .iter()
        .filter_map(|sp| sp.soak.as_ref().map(|soak| (sp, soak)))
        .collect();
    let Some(window) = soaking.iter().map(|(_, soak)| soak.duration.0).max() else {
        return Ok(None);
    };
    println!(
        "  • soak: re-running verify checks for {} before the release counts as done",
        Span(window)
    );
    if dry_run {
        for (sp, soak) in &soaking {
            println!(
                "    {} [{}]: {} check(s) every {} for {}",
                sp.service,
                sp.host,
                soak.checks.len(),
                soak.every,
                soak.duration
            );
        }
        println!("        (dry-run, not executed)\n");
        return Ok(None);
    }

    let start = std::time::Instant::now();
    for (at, i, round, rounds) in soak_schedule(&soaking) {
        let due = std::time::Duration::from_secs(at);
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            if !interrupt::nap(wait, || interrupt::count() > 0) {
                return Ok(Some("soak".to_string()));
            }
        }
        let (sp, soak) = soaking[i];
        let target = &targets[&sp.target];
        // A soak window is exactly the long, quiet stretch a stale lock is
        // measured over: keep the lock fresh for every round.
        if let Some(lock) = lock {
            lock.refresh();
        }
        for check in &soak.checks {
            let passed = run_step(check, target, &sp.host, false)?;
            if interrupt::count() > 0 {
                return Ok(Some(scrub(&format!("soak: {}", step_label(check)))));
            }
            if !passed.ok {
                // A `remote_command` check's label already is its command.
                let detail = step_detail(check);
                let already_in_label = match &check.kind {
                    StepKind::Http { .. } => step_label(check).ends_with(&detail),
                    _ => check.label.ends_with(&check.detail()),
                };
                let detail = if already_in_label {
                    String::new()
                } else {
                    format!(" ({})", scrub(&detail))
                };
                eprintln!(
                    "\n✗ soak failed at {} of {}, round {round}/{rounds}: {}{detail}",
                    Span(start.elapsed().as_secs()),
                    Span(window),
                    step_failure(check, &passed),
                );
                eprintln!(
                    "  {} passed its checks when it went live, then stopped passing.",
                    sp.service
                );
                return Ok(Some(scrub(&format!(
                    "soak: {}",
                    step_failure(check, &passed)
                ))));
            }
        }
        println!(
            "    {:>6} {} [{}] round {round}/{rounds}: ✓ {} check(s)",
            Span(at).to_string(),
            sp.service,
            sp.host,
            soak.checks.len()
        );
    }
    println!("  ✓ soak: every check held for {}\n", Span(window));
    Ok(None)
}

/// Run each step once, in order, with nothing to unwind: the checks
/// `deliver rollback` runs against the release it just restored. Returns the
/// first step that failed — scrubbed, since it travels on into the notice.
pub fn check(plan: &[ServicePlan], targets: &BTreeMap<String, Target>) -> Result<Option<String>> {
    for sp in plan {
        let target = &targets[&sp.target];
        println!("  • {} → {} [{}]", sp.service, sp.target, sp.host);
        for step in &sp.steps {
            println!("    {} [{}]", step_label(step), step.type_name());
            let result = run_step(step, target, &sp.host, false)?;
            if !result.ok {
                eprintln!(
                    "\n✗ failed: {} ({})",
                    step_failure(step, &result),
                    step_detail(step)
                );
                return Ok(Some(step_failure(step, &result)));
            }
        }
    }
    Ok(None)
}

/// `deliver rollback` — repoint each service's live symlink at its previous
/// release, newest service first.
pub fn rollback(plan: &[ServicePlan], targets: &BTreeMap<String, Target>) -> Result<bool> {
    let reversible: Vec<(&ServicePlan, &PlannedStep)> = plan
        .iter()
        .flat_map(|sp| {
            sp.steps
                .iter()
                .filter(|s| s.rollback.is_some())
                .map(move |s| (sp, s))
        })
        .collect();

    if reversible.is_empty() {
        println!("nothing in this config is rollback-able (no release-based service).");
        return Ok(false);
    }

    let mut all_ok = true;
    for (sp, step) in reversible.into_iter().rev() {
        let target = &targets[&sp.target];
        let undo = step.rollback.as_ref().unwrap();
        println!(
            "  • {} → {} [{}]: {}",
            sp.service,
            sp.target,
            sp.host,
            scrub(&step.label)
        );
        match run_ssh(target, &sp.host, undo) {
            Ok(true) => {}
            _ => {
                all_ok = false;
                eprintln!("  ✗ rollback failed for {}", sp.service);
            }
        }
    }
    Ok(all_ok)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deliver-exec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn local_plan(steps: Vec<PlannedStep>) -> (Vec<ServicePlan>, BTreeMap<String, Target>) {
        let target: Target = serde_yaml::from_str("{method: local, dir: .}").unwrap();
        let plan = vec![ServicePlan {
            service: "api".into(),
            target: "local".into(),
            host: "localhost".into(),
            steps,
            after_tag: false,
            soak: None,
        }];
        (plan, BTreeMap::from([("local".to_string(), target)]))
    }

    /// Press Ctrl-C once for each of `marks`, as each file appears — the
    /// operator watching a step start.
    fn press_when(marks: Vec<PathBuf>) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let handle = std::thread::spawn(move || {
            let mut marks = marks.into_iter();
            let mut next = marks.next();
            while let Some(mark) = &next {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if mark.exists() {
                    interrupt::raise();
                    next = marks.next();
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        (done, handle)
    }

    fn at(dir: &Path, name: &str) -> String {
        dir.join(name).display().to_string()
    }

    #[test]
    fn ctrl_c_unwinds_what_already_changed_and_runs_nothing_after() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("interrupt");
        let (plan, targets) = local_plan(vec![
            PlannedStep::ssh("swap", format!("touch {}", at(&dir, "swapped")))
                .with_rollback(format!("touch {}", at(&dir, "unswapped"))),
            PlannedStep::command(
                "slow build",
                format!("touch {} && sleep 1", at(&dir, "started")),
            ),
            PlannedStep::ssh("later", format!("touch {}", at(&dir, "later"))),
        ]);
        let (done, watcher) = press_when(vec![dir.join("started")]);
        let outcome = execute(&plan, &targets, false, None).unwrap();
        done.store(true, Ordering::SeqCst);
        watcher.join().unwrap();

        assert!(!outcome.ok);
        assert!(outcome.interrupted);
        assert!(!outcome.abandoned);
        assert_eq!(
            outcome.failed_step.as_deref(),
            Some("interrupted at slow build")
        );
        assert_eq!(outcome.rolled_back, 1);
        assert!(dir.join("unswapped").exists(), "the swap was undone");
        assert!(!dir.join("later").exists(), "nothing ran after Ctrl-C");
    }

    #[test]
    fn ctrl_c_during_the_unwind_abandons_it() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("abandon");
        let (plan, targets) = local_plan(vec![
            PlannedStep::ssh("first", "true")
                .with_rollback(format!("touch {}", at(&dir, "undo-1"))),
            PlannedStep::ssh("second", "true")
                .with_rollback(format!("touch {} && sleep 1", at(&dir, "undo-2"))),
            PlannedStep::command(
                "slow build",
                format!("touch {} && sleep 1", at(&dir, "started")),
            ),
        ]);
        let (done, watcher) = press_when(vec![dir.join("started"), dir.join("undo-2")]);
        let outcome = execute(&plan, &targets, false, None).unwrap();
        done.store(true, Ordering::SeqCst);
        watcher.join().unwrap();

        assert!(outcome.interrupted);
        assert!(outcome.abandoned);
        assert_eq!(outcome.rolled_back, 0);
        assert!(
            !dir.join("undo-1").exists(),
            "the unwind stopped at the second Ctrl-C"
        );
    }

    #[test]
    fn a_failing_undo_is_counted_as_failed_not_as_nothing_to_undo() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = scratch("undo-fails");
        let (plan, targets) = local_plan(vec![
            PlannedStep::ssh("swap", "true").with_rollback("false"),
            PlannedStep::ssh("also", format!("touch {}", at(&dir, "changed")))
                .with_rollback(format!("rm {}", at(&dir, "changed"))),
            PlannedStep::ssh("check", "false"),
        ]);
        let outcome = execute(&plan, &targets, false, None).unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.failed_step.as_deref(), Some("check"));
        assert_eq!(outcome.rolled_back, 1, "the undo that worked is counted");
        assert_eq!(
            outcome.rollback_failed, 1,
            "the undo that failed is counted apart"
        );
        assert!(!dir.join("changed").exists(), "the working undo still ran");
    }

    #[test]
    fn a_dry_run_is_not_armed() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (plan, targets) = local_plan(vec![PlannedStep::ssh("x", "true")]);
        interrupt::raise();
        // A count left over from elsewhere means nothing to a dry run…
        assert!(execute(&plan, &targets, true, None).unwrap().ok);
        // …while a real run starts from zero.
        assert!(execute(&plan, &targets, false, None).unwrap().ok);
    }
}

#[cfg(test)]
mod http_verification_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    // Every listener has a deadline, bounded reads and unwind-safe shutdown.
    // Redirects are relative, so curl can only return to this numeric loopback.
    struct Fixture {
        url: String,
        stop: Arc<AtomicBool>,
        paths: Arc<Mutex<Vec<String>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn new(statuses: Vec<u16>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let paths = Arc::new(Mutex::new(Vec::new()));
            let stopping = stop.clone();
            let requests = paths.clone();
            let thread = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut count = 0;
                while !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    // Windows (and BSD) sockets accepted from a non-blocking
                    // listener stay non-blocking: reads would return
                    // WouldBlock before curl's request arrived, the reply
                    // would go out with the request unread and closing then
                    // resets the connection (curl 56/23).
                    if stream.set_nonblocking(false).is_err()
                        || stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .is_err()
                        || stream
                            .set_write_timeout(Some(Duration::from_millis(200)))
                            .is_err()
                    {
                        continue;
                    }
                    let mut request = Vec::new();
                    let mut buf = [0; 512];
                    let read_deadline = Instant::now() + Duration::from_secs(1);
                    while request.len() < 4096 && Instant::now() < read_deadline {
                        let remaining = (4096 - request.len()).min(buf.len());
                        match stream.read(&mut buf[..remaining]) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let path = String::from_utf8_lossy(&request)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    requests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(path);
                    let status = statuses.get(count).copied().unwrap_or(200);
                    count += 1;
                    let location = if status == 301 {
                        "Location: /docs\r\n"
                    } else {
                        ""
                    };
                    let _ = write!(stream, "HTTP/1.1 {status} Test\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n");
                }
            });
            Self {
                url,
                stop,
                paths,
                thread: Some(thread),
            }
        }

        fn count(&self) -> usize {
            self.paths.lock().unwrap_or_else(|e| e.into_inner()).len()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "deliver-http-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn plan(step: PlannedStep) -> (Vec<ServicePlan>, BTreeMap<String, Target>) {
        let target = serde_yaml::from_str("{method: local, dir: .}").unwrap();
        (
            vec![ServicePlan {
                service: "fixture".into(),
                target: "local".into(),
                host: "127.0.0.1".into(),
                steps: vec![step],
                after_tag: false,
                soak: None,
            }],
            BTreeMap::from([("local".into(), target)]),
        )
    }

    #[test]
    fn http_verification_filters_urls_and_metadata() {
        let sensitive = [
            "user-marker",
            "pass-marker",
            "query-marker",
            "fragment-marker",
        ];
        let raw = "https://user-marker:pass-marker@example.com/docs?query-marker#fragment-marker";
        let out = http_display_url(raw);
        assert!(out == "https://example.com/docs");
        assert!(sensitive.iter().all(|s| !out.contains(s)));
        for (raw, expected) in [
            ("//example.com/docs?token", "//example.com/docs"),
            ("/docs#token", "/docs"),
            ("docs?token", "docs"),
        ] {
            assert!(http_display_url(raw) == expected);
        }
        let (_, redirect) = http_metadata(b"301\n/docs\r\nAuthorization: unregistered-marker");
        assert!(redirect.as_deref() == Some("[URL hidden]"));
        for raw in [
            "/docs%0aAuthorization:token",
            "https://a\\b/docs",
            "https://a@b@c/docs",
            "/docs\r\nCookie:token",
            "/docs\u{1b}[31m",
            "javascript:token",
            "",
            "https://[bad]/",
        ] {
            assert!(http_display_url(raw) == "[URL hidden]");
        }
        assert!(http_display_url("http://[::1]:8080/docs") == "http://[::1]:8080/docs");
        let (code, redirect) = http_metadata(b"301\n/docs?query-marker#fragment-marker");
        assert!(code == "301" && redirect.as_deref() == Some("/docs"));
        for raw in [&b""[..], &b"junk\n\xff"[..], &b"200 Cookie: marker"[..]] {
            assert!(http_metadata(raw).0 == "unknown");
        }
        let (code, redirect) = http_metadata(b"200\n\xff");
        assert!(code == "200" && redirect.as_deref() == Some("[URL hidden]"));
        let long = format!("/{}", "x".repeat(2000));
        assert!(http_display_url(&long).chars().count() <= HTTP_DETAIL_LIMIT);
    }

    #[test]
    fn http_verification_fixed_reasons_and_scrubbing() {
        crate::secrets::redact::record("HTTP_PATH", "http-path-secret-8741");
        let url = "https://user-marker:pass-marker@example.com/http-path-secret-8741?query-marker#fragment-marker";
        let step = PlannedStep::http(format!("verify {url}"), url, 200, 1, 0);
        let reason = http_failure("000", 200, None, Some(7));
        assert!(reason.contains("Failed to connect") && reason.contains("curl 7"));
        let result = StepResult {
            ok: false,
            failure: Some(reason),
        };
        let final_text = step_failure(&step, &result);
        assert!(final_text.contains("[redacted:HTTP_PATH]") && final_text.contains("want 200"));
        for marker in [
            "user-marker",
            "pass-marker",
            "query-marker",
            "fragment-marker",
            "http-path-secret-8741",
        ] {
            assert!(!final_text.contains(marker));
            assert!(!step_detail(&step).contains(marker));
        }
        // No API accepts stderr: unregistered header/body values cannot enter it.
        for exit in [None, Some(1), Some(28), Some(47), Some(60)] {
            let text = http_failure("000", 200, None, exit);
            assert!(text.contains("diagnostic detail hidden"));
        }
        assert!(bounded_safe(&"é".repeat(2000)).chars().count() == HTTP_DETAIL_LIMIT);
    }

    #[test]
    fn http_verification_redirect_arguments_are_opt_in() {
        for follow in [false, true] {
            let cmd = http_command("http://127.0.0.1/", follow);
            let args: Vec<_> = cmd.get_args().collect();
            assert!(args.iter().filter(|arg| **arg == "-L").count() == usize::from(follow));
            assert!(!args.iter().any(|arg| *arg == "--location-trusted"));
        }
        let step = PlannedStep::http("check", "http://127.0.0.1/", 200, 1, 0);
        assert!(matches!(
            step.kind,
            StepKind::Http {
                follow_redirects: false,
                ..
            }
        ));
        assert!(matches!(
            step.with_follow_redirects(true).kind,
            StepKind::Http {
                follow_redirects: true,
                ..
            }
        ));
    }

    #[test]
    fn http_verification_redirect_status_and_retry_semantics() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _armed = interrupt::arm();
        for (follow, expected, ok, count) in [
            (false, 200, false, 1),
            (false, 301, true, 1),
            (true, 200, true, 2),
        ] {
            let server = Fixture::new(vec![301, 200]);
            let result = run_http(&server.url, expected, 1, 0, follow).unwrap();
            assert!(result.ok == ok && server.count() == count);
            if !ok {
                let text = result.failure.unwrap();
                assert!(text.contains("http 301 (want 200)") && text.contains("/docs"));
            }
            if follow {
                assert!(server
                    .paths
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last()
                    .is_some_and(|p| p == "/docs"));
            }
        }
        let server = Fixture::new(vec![503, 200]);
        assert!(run_http(&server.url, 200, 2, 0, false).unwrap().ok);
        assert!(server.count() == 2);
        let server = Fixture::new(vec![503, 502]);
        let result = run_http(&server.url, 200, 2, 0, false).unwrap();
        assert!(!result.ok && result.failure.unwrap().contains("http 502"));
        // A successful final response leaves no stale failure in Outcome.
        let server = Fixture::new(vec![301, 200]);
        let step = PlannedStep::http("probe", &server.url, 200, 1, 0).with_follow_redirects(true);
        let (plan, targets) = plan(step);
        // execute arms the interrupt handler itself; release this test's guard.
        drop(_armed);
        let outcome = execute(&plan, &targets, false, None).unwrap();
        assert!(outcome.ok && outcome.failed_step.is_none());
    }

    #[test]
    fn http_verification_execute_check_soak_and_dry_run() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let server = Fixture::new(vec![301]);
        let url = format!("{}?query-marker#fragment-marker", server.url);
        let step = PlannedStep::http(format!("verify {url}"), &url, 200, 1, 0);
        let (mut plan, targets) = plan(step.clone());
        assert!(execute(&plan, &targets, true, None).unwrap().ok);
        assert!(server.count() == 0);
        let scratch = Scratch::new();
        let marker = scratch.0.join("later");
        plan[0].steps.push(PlannedStep::write_file(
            "later",
            marker.to_str().unwrap(),
            0o600,
            "later",
        ));
        let outcome = execute(&plan, &targets, false, None).unwrap();
        let text = outcome.failed_step.unwrap();
        assert!(!outcome.ok && !marker.exists());
        assert!(text.contains("http 301") && text.contains("/docs"));
        for marker in ["query-marker", "fragment-marker"] {
            assert!(
                !text.contains(marker),
                "HTTP diagnostic contains a sensitive fixture value"
            );
        }

        let server = Fixture::new(vec![503, 502]);
        plan[0].steps = vec![PlannedStep::http("probe", &server.url, 200, 1, 0)];
        let text = check(&plan, &targets).unwrap().unwrap();
        assert!(text.contains("probe: http 503 (want 200)"));
        let checks = plan[0].steps.clone();
        plan[0].steps.clear();
        plan[0].soak = Some(Soak {
            duration: Span(1),
            every: Span(1),
            checks,
        });
        let outcome = execute(&plan, &targets, false, None).unwrap();
        assert!(outcome
            .failed_step
            .unwrap()
            .contains("soak: probe: http 502"));
    }

    #[test]
    fn http_verification_closed_port_reason() {
        let _turn = interrupt::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _armed = interrupt::arm();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let result = run_http(&url, 200, 1, 0, false).unwrap();
        assert!(!result.ok, "ephemeral port may have been reused");
        let text = result.failure.unwrap();
        assert!(
            text.contains("http 000") && text.contains("curl 7"),
            "closed-port fixture did not refuse the connection"
        );
    }
}
