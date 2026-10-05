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
            failed_step: None,
            interrupted: false,
            abandoned: false,
        }
    }

    fn failed(failed_step: String, unwound: Unwound, interrupted: bool) -> Self {
        Outcome {
            ok: false,
            rolled_back: unwound.done,
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

fn run_http(url: &str, expect: u16, retries: u32, interval: u64) -> Result<bool> {
    for attempt in 1..=retries.max(1) {
        // curl keeps the binary dependency-free.
        let out = Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "20",
                url,
            ])
            .output()?;
        let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if code == expect.to_string() {
            return Ok(true);
        }
        eprintln!(
            "     http {code} (want {expect}), attempt {attempt}/{}",
            retries.max(1)
        );
        if attempt < retries.max(1)
            && !interrupt::nap(std::time::Duration::from_secs(interval), || {
                interrupt::count() > 0
            })
        {
            break;
        }
    }
    Ok(false)
}

fn run_step(step: &PlannedStep, target: &Target, host: &str, dry_run: bool) -> Result<bool> {
    if dry_run {
        println!("        (dry-run, not executed)");
        if step.rollback.is_some() {
            println!("        (undo available if a later step fails)");
        }
        return Ok(true);
    }
    match &step.kind {
        StepKind::Command { command, cwd } => run_local(command, cwd.as_ref()),
        StepKind::Ssh { command } => run_ssh(target, host, command),
        StepKind::Http {
            url,
            expect_status,
            retries,
            interval,
        } => run_http(url, *expect_status, *retries, *interval),
        StepKind::WriteFile {
            path,
            mode,
            content,
        } => write_file(path, *mode, content),
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
            Ok(false) => eprintln!("  ✗ rollback failed for: {label} — resolve by hand"),
            Err(e) => eprintln!("  ✗ rollback error for {label}: {}", scrub(&e.to_string())),
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

pub fn execute(
    plan: &[ServicePlan],
    targets: &BTreeMap<String, Target>,
    dry_run: bool,
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
                return Ok(interrupted_at(&step.label, undoable));
            }
            println!(
                "    {:>2}/{}. {} [{}]",
                i + 1,
                work.len(),
                scrub(&step.label),
                step.type_name()
            );
            let ok = run_step(step, target, &sp.host, dry_run)?;
            if ok {
                if let Some(undo) = &step.rollback {
                    undoable.push((
                        step.label.clone(),
                        undo.clone(),
                        target.clone(),
                        sp.host.clone(),
                    ));
                }
            }
            // Whether the step finished or died with the keypress, the
            // operator asked to stop: everything that did change is undone.
            if interrupted() {
                return Ok(interrupted_at(&step.label, undoable));
            }
            if !ok {
                // Leave intermediates on disk — they're the evidence.
                if !cleanup.is_empty() {
                    println!(
                        "     (keeping build artifacts for debugging — `deliver clean` removes them)"
                    );
                }
                eprintln!(
                    "\n✗ failed: {} ({})",
                    scrub(&step.label),
                    scrub(&step.detail())
                );
                eprintln!("  stopping — later steps and services will not run.");
                let unwound = if dry_run {
                    Unwound::default()
                } else {
                    unwind(undoable)
                };
                // Travels on into the notification payload, so it leaves here
                // already scrubbed.
                return Ok(Outcome::failed(scrub(&step.label), unwound, false));
            }
        }

        for step in &cleanup {
            if interrupted() {
                return Ok(interrupted_at(&step.label, undoable));
            }
            println!("    cleanup: {}", scrub(&step.label));
            // Never fail a good deploy over cleanup; just say so.
            match run_step(step, target, &sp.host, dry_run) {
                Ok(true) => {}
                _ => eprintln!("    (cleanup did not complete: {})", scrub(&step.label)),
            }
        }
        println!();
    }

    // Every service is live; the undo stack is still whole, so a check that
    // stops passing inside the window unwinds exactly as one that never passed.
    if let Some(failed) = soak(plan, targets, dry_run)? {
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
        for check in &soak.checks {
            let passed = run_step(check, target, &sp.host, false)?;
            if interrupt::count() > 0 {
                return Ok(Some(scrub(&format!("soak: {}", check.label))));
            }
            if !passed {
                // A `remote_command` check's label already is its command.
                let detail = check.detail();
                let detail = if check.label.ends_with(&detail) {
                    String::new()
                } else {
                    format!(" ({})", scrub(&detail))
                };
                eprintln!(
                    "\n✗ soak failed at {} of {}, round {round}/{rounds}: {}{detail}",
                    Span(start.elapsed().as_secs()),
                    Span(window),
                    scrub(&check.label),
                );
                eprintln!(
                    "  {} passed its checks when it went live, then stopped passing.",
                    sp.service
                );
                return Ok(Some(scrub(&format!("soak: {}", check.label))));
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
            println!("    {} [{}]", scrub(&step.label), step.type_name());
            if !run_step(step, target, &sp.host, false)? {
                eprintln!(
                    "\n✗ failed: {} ({})",
                    scrub(&step.label),
                    scrub(&step.detail())
                );
                return Ok(Some(scrub(&step.label)));
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
    use std::sync::{Arc, Mutex};

    // The interrupt count is process-wide; these tests take turns with it.
    static SERIAL: Mutex<()> = Mutex::new(());

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
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        let outcome = execute(&plan, &targets, false).unwrap();
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
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        let outcome = execute(&plan, &targets, false).unwrap();
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
    fn a_dry_run_is_not_armed() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (plan, targets) = local_plan(vec![PlannedStep::ssh("x", "true")]);
        interrupt::raise();
        // A count left over from elsewhere means nothing to a dry run…
        assert!(execute(&plan, &targets, true).unwrap().ok);
        // …while a real run starts from zero.
        assert!(execute(&plan, &targets, false).unwrap().ok);
    }
}
