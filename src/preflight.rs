//! Preflight — cheap checks that run before anything is built or mutated.
//!
//! The point is ordering: a missing `hugo`, an unreadable conf, or an SSH key
//! that isn't loaded should fail in seconds, *before* we touch the target. See
//! docs/deploy-lifecycle.md. Every problem is reported in one pass rather than
//! one-at-a-time.

use crate::config::{Config, Target};
use crate::deployers::StepKind;
use crate::plan::ServicePlan;
use std::collections::{BTreeMap, BTreeSet};
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

/// Binaries the plan's steps run on each (target, host), as the deployers
/// declared them. Declared rather than parsed: an ssh step is a compound shell
/// script, and some of them install their own tool when it is absent.
fn remote_tools_needed(plan: &[ServicePlan]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut needed: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for sp in plan {
        for step in &sp.steps {
            if step.remote_tools.is_empty() {
                continue;
            }
            needed
                .entry((sp.target.clone(), sp.host.clone()))
                .or_default()
                .extend(step.remote_tools.iter().cloned());
        }
    }
    needed
}

/// One shell script that prints the name of every tool the target lacks, one
/// per line, and nothing else. A name with a space is a subcommand
/// (`docker compose` is a plugin, so `docker` alone proves nothing about it).
///
/// The sbin directories are appended because a deploy user's non-interactive
/// `PATH` usually omits them while the `sudo` the steps run under does not —
/// `nginx` lives in `/usr/sbin` on Debian. Probing through sudo instead would
/// fail on a host whose sudoers allows only the commands the deploy runs.
fn probe_script(tools: &BTreeSet<String>) -> String {
    let mut script = String::from("PATH=\"$PATH:/usr/local/sbin:/usr/sbin:/sbin\"");
    for tool in tools {
        let quoted = crate::remote::shell_quote(tool);
        let probe = if tool.contains(' ') {
            format!("{tool} version")
        } else {
            format!("command -v {quoted}")
        };
        script.push_str(&format!(
            "; {probe} >/dev/null 2>&1 || printf '%s\\n' {quoted}"
        ));
    }
    script.push_str("; true");
    script
}

/// The tools in `tools` that the target does not have, in one round trip.
fn missing_remote_tools(
    target: &Target,
    host: &str,
    tools: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let script = probe_script(tools);
    let output = if target.is_local() {
        Command::new("sh").arg("-c").arg(&script).output()
    } else {
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(&script)
            .stderr(Stdio::null())
            .output()
    };
    match output {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| tools.contains(*line))
            .map(str::to_string)
            .collect()),
        _ => Err(format!(
            "could not check the tools on {}",
            where_on(target, host)
        )),
    }
}

/// How a report line names the machine a remote check ran on.
fn where_on(target: &Target, host: &str) -> String {
    if target.is_local() {
        "the local target".to_string()
    } else {
        format!("{}@{host}", target.ssh.user)
    }
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
                // Nor does a `files` build's output on a clean checkout: the
                // `build:` step that creates `src` runs after preflight.
                if key == "src" && service.config.get("build").is_some() {
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

/// `also_hosts` are (target, host) pairs to probe on top of the plan's own —
/// how `deliver preflight` still checks reachability for a service whose plan
/// would not compile. Empty for every other caller.
pub fn run(
    config: &Config,
    plan: &[ServicePlan],
    repo_root: &Path,
    check_remote: bool,
    also_hosts: &[(String, String)],
) -> Report {
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
        let remote_tools = remote_tools_needed(plan);
        let mut seen = BTreeSet::new();
        let pairs = plan
            .iter()
            .map(|sp| (sp.target.clone(), sp.host.clone()))
            .chain(also_hosts.iter().cloned());
        for (target_name, host) in pairs {
            // One check per (target, host) pair.
            if !seen.insert((target_name.clone(), host.clone())) {
                continue;
            }
            if let Some(target) = config.targets.get(&target_name) {
                match ssh_reachable(target, &host) {
                    // Only claim a check passed when it actually did.
                    Ok(()) => checked.push(if target.is_local() {
                        "target is local (no ssh needed)".to_string()
                    } else {
                        format!("{} reachable", target.describe(&host))
                    }),
                    Err(problem) => {
                        problems.push(problem);
                        continue;
                    }
                }
                // 4. the target's own tooling — only once we know we can ask.
                let Some(tools) = remote_tools.get(&(target_name.clone(), host.clone())) else {
                    continue;
                };
                match missing_remote_tools(target, &host, tools) {
                    Ok(missing) => {
                        let present: Vec<&str> = tools
                            .iter()
                            .filter(|tool| !missing.contains(tool))
                            .map(|tool| tool.as_str())
                            .collect();
                        if !present.is_empty() {
                            checked.push(format!(
                                "{} tool(s) present on {}: {}",
                                present.len(),
                                where_on(target, &host),
                                present.join(", ")
                            ));
                        }
                        for tool in missing {
                            problems.push(format!(
                                "required tool not installed on {}: {tool}",
                                where_on(target, &host)
                            ));
                        }
                    }
                    Err(problem) => problems.push(problem),
                }
            }
        }
    }

    Report { problems, checked }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Target {
        serde_yaml::from_str("{method: local, dir: .}").unwrap()
    }

    fn tools(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_probe_names_exactly_the_tools_the_target_lacks() {
        let missing = missing_remote_tools(
            &local(),
            "localhost",
            &tools(&["sh", "deliver-no-such-tool-a", "deliver-no-such-tool-b"]),
        )
        .unwrap();
        assert_eq!(
            missing,
            vec!["deliver-no-such-tool-a", "deliver-no-such-tool-b"]
        );
    }

    #[test]
    fn a_subcommand_is_probed_as_itself_not_as_its_binary() {
        // `sh` exists, but `sh version` does not run a script called
        // "version" successfully — the probe must see through to that.
        let missing =
            missing_remote_tools(&local(), "localhost", &tools(&["sh deliver-no-such"])).unwrap();
        assert_eq!(missing, vec!["sh deliver-no-such"]);
        let script = probe_script(&tools(&["docker compose"]));
        assert!(script.contains("docker compose version"), "{script}");
        assert!(!script.contains("command -v 'docker compose'"), "{script}");
    }

    #[test]
    fn the_probe_looks_where_sudo_would() {
        // nginx is in /usr/sbin on Debian, which a deploy user's PATH omits.
        let script = probe_script(&tools(&["nginx"]));
        assert!(script.starts_with("PATH=\"$PATH:/usr/local/sbin:/usr/sbin:/sbin\""));
        assert!(script.contains("command -v 'nginx'"), "{script}");
    }
}
