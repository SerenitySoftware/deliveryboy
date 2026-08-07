//! Preflight — cheap checks that run before anything is built or mutated.
//!
//! The point is ordering: a missing `hugo`, an unreadable conf, or an SSH key
//! that isn't loaded should fail in seconds, *before* we touch the target. See
//! docs/deploy-lifecycle.md. Every problem is reported in one pass rather than
//! one-at-a-time.

use crate::config::{Config, Target};
use crate::deployers::StepKind;
use crate::plan::ServicePlan;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Report {
    pub problems: Vec<String>,
    pub checked: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Tools the compiled plan will invoke locally.
fn tools_needed(plan: &[ServicePlan]) -> BTreeSet<String> {
    let mut tools = BTreeSet::new();
    for sp in plan {
        for step in &sp.steps {
            match &step.kind {
                StepKind::Command { command, .. } => {
                    // First word of the command line is the binary.
                    if let Some(first) = command.split_whitespace().next() {
                        if !first.contains('=') && !first.starts_with('(') {
                            tools.insert(first.to_string());
                        }
                    }
                }
                StepKind::Ssh { .. } => {
                    tools.insert("ssh".into());
                }
                StepKind::Http { .. } => {
                    tools.insert("curl".into());
                }
                // Written by the CLI itself — no external tool needed.
                StepKind::WriteFile { .. } => {}
            }
        }
    }
    // Shell builtins / control words are not binaries.
    for skip in [
        "set", "if", "cd", "test", "[", "printf", "echo", "for", "while",
    ] {
        tools.remove(skip);
    }
    tools
}

/// Local files the config points at must exist before we build or ship.
fn input_files(config: &Config, repo_root: &Path) -> Vec<(String, bool)> {
    let mut checks = Vec::new();
    for service in config.services.values() {
        if !service.enabled {
            continue;
        }
        for key in ["conf", "source", "src", "script"] {
            if let Some(rel) = service.config.get(key).and_then(|v| v.as_str()) {
                // `script` may carry arguments ("nginx/x.sh activate").
                let path = rel.split_whitespace().next().unwrap_or(rel);
                // Artifacts the deploy itself produces (e.g. the hugo tarball) don't exist yet.
                if path.ends_with(".tar.gz") {
                    continue;
                }
                let full = repo_root.join(path);
                checks.push((path.to_string(), full.exists()));
            }
        }
        if let Some(vhosts) = service.config.get("vhosts").and_then(|v| v.as_sequence()) {
            for vh in vhosts {
                if let Some(rel) = vh.get("conf").and_then(|v| v.as_str()) {
                    checks.push((rel.to_string(), repo_root.join(rel).exists()));
                }
            }
        }
    }
    checks
}

/// SSH must work without a prompt, or the deploy stalls mid-flight. Uses the
/// target's configured login method (identity file, jump host, options).
fn ssh_reachable(target: &Target, host: &str) -> Result<(), String> {
    if target.is_local() {
        return Ok(()); // nothing to reach
    }
    let status = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .args(target.ssh_args())
        .arg(format!("{}@{host}", target.ssh.user))
        .arg("true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            let hint = match target.ssh.key.as_deref() {
                Some(key) => format!("check the identity file ({key}) and access"),
                None => "load your key (ssh-add), or set targets.<name>.ssh.key".to_string(),
            };
            Err(format!(
                "cannot ssh non-interactively to {}@{host}:{} — {hint}",
                target.ssh.user, target.ssh.port
            ))
        }
    }
}

pub fn run(config: &Config, plan: &[ServicePlan], repo_root: &Path, check_remote: bool) -> Report {
    let mut problems = Vec::new();
    let mut checked = Vec::new();

    // 1. local tooling
    let tools = tools_needed(plan);
    let (present, missing): (Vec<&String>, Vec<&String>) = tools.iter().partition(|t| have(t));
    if !present.is_empty() {
        checked.push(format!(
            "{} tool(s) present: {}",
            present.len(),
            present
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for tool in missing {
        problems.push(format!("required tool not installed locally: {tool}"));
    }

    // 2. input files
    let files = input_files(config, repo_root);
    let found = files.iter().filter(|(_, exists)| *exists).count();
    if found > 0 {
        checked.push(format!("{found} input file(s) present"));
    }
    for (path, exists) in &files {
        if !exists {
            problems.push(format!("missing file referenced by config: {path}"));
        }
    }

    // 3. remote reachability (skipped for --dry-run / plan)
    if check_remote {
        let mut seen = BTreeSet::new();
        for sp in plan {
            // One check per (target, host) pair.
            if !seen.insert((sp.target.clone(), sp.host.clone())) {
                continue;
            }
            if let Some(target) = config.targets.get(&sp.target) {
                match ssh_reachable(target, &sp.host) {
                    // Only claim a check passed when it actually did.
                    Ok(()) => checked.push(if target.is_local() {
                        "target is local (no ssh needed)".to_string()
                    } else {
                        format!("{} reachable", target.describe(&sp.host))
                    }),
                    Err(problem) => problems.push(problem),
                }
            }
        }
    }

    Report { problems, checked }
}
