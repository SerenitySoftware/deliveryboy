//! Step execution: fail-fast, with rollback.
//!
//! Steps that mutate the target carry an undo command. As they succeed we push
//! those onto a stack; if any later step fails, the stack is unwound in reverse
//! so a failed deploy doesn't leave a half-changed target. Later services never
//! start after a failure.

use crate::config::Target;
use crate::deployers::{PlannedStep, StepKind};
use crate::plan::ServicePlan;
use anyhow::Result;
use std::collections::BTreeMap;
use std::process::Command;

pub struct Outcome {
    pub ok: bool,
    pub rolled_back: usize,
    pub failed_step: Option<String>,
}

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
        if attempt < retries.max(1) {
            std::thread::sleep(std::time::Duration::from_secs(interval));
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
fn write_file(path: &str, mode: u32, content: &str) -> Result<bool> {
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
        opts.mode(mode);
    }
    let mut file = opts.open(p)?;
    file.write_all(content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(true)
}

/// Undo, in reverse order, the mutating steps that already succeeded.
fn unwind(pending: Vec<(String, String, Target, String)>) -> usize {
    if pending.is_empty() {
        println!("\nnothing to roll back (no reversible step had run yet)");
        return 0;
    }
    println!("\n↩ rolling back {} step(s)…", pending.len());
    let mut done = 0;
    for (label, undo, target, host) in pending.into_iter().rev() {
        println!("  undo: {label}");
        match run_ssh(&target, &host, &undo) {
            Ok(true) => done += 1,
            Ok(false) => eprintln!("  ✗ rollback failed for: {label} — resolve by hand"),
            Err(e) => eprintln!("  ✗ rollback error for {label}: {e}"),
        }
    }
    done
}

pub fn execute(
    plan: &[ServicePlan],
    targets: &BTreeMap<String, Target>,
    dry_run: bool,
) -> Result<Outcome> {
    // (label, undo command, target) for successful, reversible steps.
    let mut undoable: Vec<(String, String, Target, String)> = Vec::new();

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
            println!(
                "    {:>2}/{}. {} [{}]",
                i + 1,
                work.len(),
                step.label,
                step.type_name()
            );
            let ok = run_step(step, target, &sp.host, dry_run)?;
            if !ok {
                // Leave intermediates on disk — they're the evidence.
                if !cleanup.is_empty() {
                    println!(
                        "     (keeping build artifacts for debugging — `deliver clean` removes them)"
                    );
                }
                eprintln!("\n✗ failed: {} ({})", step.label, step.detail());
                eprintln!("  stopping — later steps and services will not run.");
                let rolled_back = if dry_run { 0 } else { unwind(undoable) };
                return Ok(Outcome {
                    ok: false,
                    rolled_back,
                    failed_step: Some(step.label.clone()),
                });
            }
            if let Some(undo) = &step.rollback {
                undoable.push((
                    step.label.clone(),
                    undo.clone(),
                    target.clone(),
                    sp.host.clone(),
                ));
            }
        }

        for step in &cleanup {
            println!("    cleanup: {}", step.label);
            // Never fail a good deploy over cleanup; just say so.
            match run_step(step, target, &sp.host, dry_run) {
                Ok(true) => {}
                _ => eprintln!("    (cleanup did not complete: {})", step.label),
            }
        }
        println!();
    }
    Ok(Outcome {
        ok: true,
        rolled_back: 0,
        failed_step: None,
    })
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
            sp.service, sp.target, sp.host, step.label
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
