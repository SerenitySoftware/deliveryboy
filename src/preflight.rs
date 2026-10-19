//! Preflight — cheap checks that run before anything is built or mutated.
//!
//! The point is ordering: a missing `hugo`, an unreadable conf, or an SSH key
//! that isn't loaded should fail in seconds, *before* we touch the target. See
//! docs/deploy-lifecycle.md. Every problem is reported in one pass rather than
//! one-at-a-time.

use crate::config::{Config, DnsCheck, Target};
use crate::deployers::{ComposeProject, StepKind};
use crate::plan::ServicePlan;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub struct Report {
    pub problems: Vec<String>,
    pub checked: Vec<String>,
    /// Printed, but not problems: the run goes on.
    pub warnings: Vec<String>,
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

/// Prints `missing` when there is no `sudo` and `password` when it would ask
/// for one, and nothing otherwise. Only those two: a host whose sudoers allows
/// just the commands the deploy runs refuses `true` and still deploys fine.
fn sudo_probe_script(sudo: &str) -> String {
    let q = crate::remote::shell_quote(sudo);
    format!(
        "PATH=\"$PATH:/usr/local/sbin:/usr/sbin:/sbin\"; \
         command -v {q} >/dev/null 2>&1 || {{ echo missing; exit 0; }}; \
         {q} -n true 2>&1 >/dev/null | grep -qi 'password is required' && echo password; true"
    )
}

/// Whether the `sudo` every step runs under will work, for a target that uses
/// it. Without it the first mutating step fails — the deploy lock, before the
/// lock learned to say why — and nothing in the output names `sudo`.
fn sudo_problem(target: &Target, host: &str) -> Option<String> {
    let script = sudo_probe_script("sudo");
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
    let answer = String::from_utf8_lossy(&output.ok()?.stdout)
        .trim()
        .to_string();
    let why = match answer.as_str() {
        "missing" => "sudo is not installed",
        "password" => "sudo needs a password",
        _ => return None,
    };
    Some(format!(
        "{why} on {}, and every step on this target runs under it — set `sudo: false` \
         on the target for a root login, or allow sudo without a password",
        where_on(target, host)
    ))
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
        // The Compose files are the one input that deployer hangs on, and
        // without this a misspelled one is found only at `scp`, after the
        // image build.
        if service.deployer == "docker-compose" {
            for rel in crate::deployers::docker_compose::input_files(&service.config) {
                let exists = repo_root.join(&rel).exists();
                checks.push((rel, exists));
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

/// `docker compose` is a plugin, so `docker` on the path proves nothing.
fn have_compose() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_docker(dir: &Path, args: &[String]) -> std::io::Result<Output> {
    Command::new("docker")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to).map(|_| ())
    }
}

fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

/// Have `docker compose config --quiet` parse, merge and interpolate the
/// project exactly as the target's root will hold it — the Compose files and
/// includes where the ship steps put them, and the rendered `.env`, which a
/// service's `env_file:` needs and a clean checkout does not have. It needs no
/// daemon and touches nothing remote. The scratch copy, `.env` included, is
/// removed before this returns.
fn compose_config(
    project: &ComposeProject,
    repo_root: &Path,
    scratch: &Path,
    run: &dyn Fn(&Path, &[String]) -> std::io::Result<Output>,
) -> ComposeVerdict {
    let _ = std::fs::remove_dir_all(scratch);
    // Compose names files by absolute path; the scratch copy's means nothing
    // to the operator, the root-relative one does. Both spellings, longest
    // first: on macOS the temp dir is a symlink into `/private`.
    let mut prefixes = vec![format!("{}/", scratch.display())];
    let result = (|| {
        std::fs::create_dir_all(scratch)
            .map_err(|e| format!("cannot create {}: {e}", scratch.display()))?;
        if let Ok(real) = scratch.canonicalize() {
            prefixes.push(format!("{}/", real.display()));
        }
        prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));
        for (src, dest) in &project.layout {
            copy_tree(&repo_root.join(src), &scratch.join(dest))
                .map_err(|e| format!("cannot copy {src} to validate it: {e}"))?;
        }
        if let Some((path, contents)) = &project.env {
            write_secret_file(&scratch.join(path), contents)
                .map_err(|e| format!("cannot render {path} to validate it: {e}"))?;
        }
        let mut args = vec!["compose".to_string()];
        for file in &project.files {
            args.extend(["-f".to_string(), file.clone()]);
        }
        args.extend(
            ["-p", &project.project, "config", "--quiet"]
                .iter()
                .map(|a| a.to_string()),
        );
        run(scratch, &args).map_err(|e| format!("cannot run docker compose: {e}"))
    })();
    let _ = std::fs::remove_dir_all(scratch);
    let output = match result {
        Ok(output) => output,
        Err(why) => return ComposeVerdict::Unchecked(why),
    };

    // Values are scrubbed in case a parse error quotes a line of `.env`.
    let tidy = |line: &str| {
        let mut line = line.to_string();
        for prefix in &prefixes {
            line = line.replace(prefix.as_str(), "");
        }
        crate::secrets::redact::scrub(line.trim())
    };
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.contains("level=warning") {
            let msg = line
                .split_once("msg=\"")
                .map(|(_, rest)| rest.strip_suffix('"').unwrap_or(rest).replace("\\\"", "\""))
                .unwrap_or_else(|| line.to_string());
            warnings.push(tidy(&msg));
        } else {
            errors.push(tidy(line));
        }
    }
    if output.status.success() {
        return ComposeVerdict::Valid(warnings);
    }
    let why = if errors.is_empty() {
        format!("exited {}", output.status)
    } else {
        errors.join("; ")
    };
    // An `env_file:` this deploy does not render may be kept on the target by
    // hand; it is missing here, not necessarily there, so say so and go on.
    if project.env.is_none() && why.starts_with("env file ") && why.contains(" not found") {
        return ComposeVerdict::Unchecked(format!(
            "{why} — this deploy does not render it, so it must already be on the target"
        ));
    }
    ComposeVerdict::Invalid(why)
}

enum ComposeVerdict {
    /// Parsed and merged; Compose's warnings (an unset `${VAR}` is one).
    Valid(Vec<String>),
    /// Could not be checked here, and that is not the project's fault.
    Unchecked(String),
    /// Compose refused it, and the target's `docker compose` would too.
    Invalid(String),
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

/// Every name a certificate step will ask a CA for, by (target, host).
fn cert_names(plan: &[ServicePlan]) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut names: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for sp in plan {
        for step in &sp.steps {
            if step.cert_domains.is_empty() {
                continue;
            }
            names
                .entry((sp.target.clone(), sp.host.clone()))
                .or_default()
                .extend(step.cert_domains.iter().cloned());
        }
    }
    names
}

/// The addresses a name resolves to here, or `None` when it does not.
fn resolve(name: &str) -> Option<BTreeSet<IpAddr>> {
    let addrs: BTreeSet<IpAddr> = (name, 443)
        .to_socket_addrs()
        .ok()?
        .map(|a| a.ip())
        .collect();
    (!addrs.is_empty()).then_some(addrs)
}

/// The name ssh would actually dial: a `Host` alias in `~/.ssh/config` is not
/// something DNS can answer for.
fn dial_name(host: &str) -> String {
    Command::new("ssh")
        .args(["-G", host])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("hostname ").map(str::to_string))
        })
        .unwrap_or_else(|| host.to_string())
}

fn show(addrs: &BTreeSet<IpAddr>) -> String {
    addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One line per certificate name that does not resolve to the target, and
/// nothing when they agree. A wildcard cannot be looked up, and a name still
/// holding a placeholder is refused at compile time, so neither is asked.
fn dns_mismatches(
    names: &BTreeSet<String>,
    host: &str,
    lookup: impl Fn(&str) -> Option<BTreeSet<IpAddr>>,
) -> Vec<String> {
    let checkable: Vec<&String> = names
        .iter()
        .filter(|n| !n.starts_with("*.") && !n.contains("__") && !n.contains(['{', '$']))
        .collect();
    if checkable.is_empty() {
        return Vec::new();
    }
    let Some(target) = lookup(host) else {
        return vec![format!(
            "target {host} does not resolve here — cannot check that {} point at it",
            checkable
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )];
    };
    checkable
        .into_iter()
        .filter_map(|name| match lookup(name) {
            None => Some(format!(
                "{name} does not resolve — target {host} is {}; certbot cannot issue it until it does",
                show(&target)
            )),
            Some(addrs) if addrs.is_disjoint(&target) => Some(format!(
                "{name} resolves to {} — target {host} is {}",
                show(&addrs),
                show(&target)
            )),
            Some(_) => None,
        })
        .collect()
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
    let mut warnings = Vec::new();

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

    // 2b. each Compose project parses and merges. A file that exists but does
    // not would otherwise get as far as `start services`, after the live
    // config is overwritten — and that step's undo runs the same broken file.
    let mut seen = BTreeSet::new();
    let projects: Vec<(&str, &ComposeProject)> = plan
        .iter()
        .flat_map(|sp| {
            sp.steps
                .iter()
                .filter_map(move |s| s.compose_project.as_ref().map(|c| (sp.service.as_str(), c)))
        })
        .filter(|(service, _)| seen.insert(*service))
        .collect();
    if !projects.is_empty() {
        if have_compose() {
            for (service, project) in projects {
                // A missing input is already a problem above; validating
                // without it would only say so again, less clearly.
                if project
                    .layout
                    .iter()
                    .any(|(src, _)| !repo_root.join(src).exists())
                {
                    continue;
                }
                let scratch = std::env::temp_dir()
                    .join(format!("deliver-compose-{}-{service}", std::process::id()));
                match compose_config(project, repo_root, &scratch, &run_docker) {
                    ComposeVerdict::Valid(lines) => {
                        checked.push(format!("{service}: Compose project validates"));
                        warnings.extend(lines.into_iter().map(|l| format!("{service}: {l}")));
                    }
                    ComposeVerdict::Unchecked(why) => warnings.push(format!(
                        "{service}: Compose project not validated locally: {why}"
                    )),
                    ComposeVerdict::Invalid(why) => problems.push(format!(
                        "{service}: docker compose config rejects the project: {why}"
                    )),
                }
            }
        } else {
            warnings.push(
                "Compose project not validated: `docker compose` is not installed locally"
                    .to_string(),
            );
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
                if target.uses_sudo() {
                    if let Some(problem) = sudo_problem(target, &host) {
                        problems.push(problem);
                    }
                }
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

        // 5. certificate names point at the target — certbot's HTTP-01
        // challenge cannot pass otherwise, and failed authorizations are
        // rate-limited. Read off this machine's resolver, not the target's.
        for ((target_name, host), names) in cert_names(plan) {
            let Some(target) = config.targets.get(&target_name) else {
                continue;
            };
            let mode = target.preflight.as_ref().map(|p| p.dns).unwrap_or_default();
            if target.is_local() || mode == DnsCheck::Skip {
                continue;
            }
            let lines = dns_mismatches(&names, &dial_name(&host), resolve);
            match mode {
                DnsCheck::Fail => problems.extend(lines),
                _ => warnings.extend(lines),
            }
        }
    }

    Report {
        problems,
        checked,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deliver-pf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deploy")).unwrap();
        std::fs::create_dir_all(dir.join("conf/rules")).unwrap();
        std::fs::write(dir.join("deploy/compose.yml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("conf/rules/a.yml"), "groups: []\n").unwrap();
        dir
    }

    fn project(env: Option<(&str, &str)>) -> ComposeProject {
        ComposeProject {
            project: "demo".into(),
            files: vec!["deploy/compose.yml".into()],
            layout: vec![
                ("deploy/compose.yml".into(), "compose.yml".into()),
                ("conf/rules".into(), "conf/rules".into()),
            ],
            env: env.map(|(p, c)| (p.to_string(), c.to_string())),
        }
    }

    fn exited(code: i32, stderr: &str) -> Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code as u32)
        };
        Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn compose_is_validated_in_the_layout_the_target_root_will_hold() {
        let repo = compose_repo("layout");
        let scratch = repo.with_extension("scratch");
        let seen = std::cell::RefCell::new(None);
        let run = |dir: &Path, args: &[String]| {
            // Inspect the copy while it exists: it is gone once this returns.
            let env = std::fs::read_to_string(dir.join(".env")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(dir.join(".env"))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            *seen.borrow_mut() = Some((
                args.to_vec(),
                dir.join("compose.yml").is_file(),
                dir.join("conf/rules/a.yml").is_file(),
                env,
            ));
            Ok(exited(0, ""))
        };
        let verdict = compose_config(&project(Some((".env", "A='1'\n"))), &repo, &scratch, &run);
        assert!(matches!(verdict, ComposeVerdict::Valid(ref w) if w.is_empty()));
        let (args, compose, include, env) = seen.into_inner().expect("compose ran");
        assert_eq!(
            args.join(" "),
            "compose -f deploy/compose.yml -p demo config --quiet"
        );
        assert!(
            compose,
            "a Compose file lands at the root under its own name"
        );
        assert!(include, "an include keeps its repo-relative path");
        assert_eq!(env, "A='1'\n");
        assert!(
            !scratch.exists(),
            "the scratch copy and its .env are removed"
        );
    }

    #[test]
    fn compose_warnings_and_errors_name_files_relative_to_the_root() {
        let repo = compose_repo("messages");
        let scratch = repo.with_extension("scratch");
        let s = scratch.display().to_string();
        let warn = r#"time="x" level=warning msg="The \"TOKEN\" variable is not set. Defaulting to a blank string."
"#;
        let run = |_: &Path, _: &[String]| Ok(exited(0, warn));
        match compose_config(&project(None), &repo, &scratch, &run) {
            ComposeVerdict::Valid(w) => assert_eq!(
                w,
                vec!["The \"TOKEN\" variable is not set. Defaulting to a blank string."]
            ),
            _ => panic!("a warning is not a refusal"),
        }
        let bad = format!(
            "{warn}validating {s}/compose.yml: services.app additional properties 'imag' not allowed\n"
        );
        let run = |_: &Path, _: &[String]| Ok(exited(1, &bad));
        match compose_config(&project(None), &repo, &scratch, &run) {
            ComposeVerdict::Invalid(why) => assert_eq!(
                why,
                "validating compose.yml: services.app additional properties 'imag' not allowed"
            ),
            _ => panic!("a non-zero exit is a refusal"),
        }
    }

    #[test]
    fn an_env_file_the_deploy_does_not_render_is_unchecked_not_refused() {
        // Kept on the target by hand: missing here proves nothing about there.
        let repo = compose_repo("handkept");
        let scratch = repo.with_extension("scratch");
        let msg = format!(
            "env file {}/.env not found: stat {}/.env: no such file or directory\n",
            scratch.display(),
            scratch.display()
        );
        let run = |_: &Path, _: &[String]| Ok(exited(1, &msg));
        assert!(matches!(
            compose_config(&project(None), &repo, &scratch, &run),
            ComposeVerdict::Unchecked(ref why) if why.starts_with("env file .env not found")
        ));
        // When this deploy renders it, the same message is a real refusal.
        assert!(matches!(
            compose_config(&project(Some((".other", "A=1\n"))), &repo, &scratch, &run),
            ComposeVerdict::Invalid(_)
        ));
    }

    #[test]
    fn real_compose_refuses_a_misspelled_key_and_reads_the_rendered_env() {
        if !have_compose() {
            return; // CI's macOS runners have no Docker; Linux runs this.
        }
        let repo = compose_repo("real");
        std::fs::write(
            repo.join("compose.yml"),
            "services:\n  app:\n    image: demo:latest\n    env_file: .env\n",
        )
        .unwrap();
        let scratch = repo.with_extension("scratch");
        let with_env = ComposeProject {
            project: "demo".into(),
            files: vec!["compose.yml".into()],
            layout: vec![("compose.yml".into(), "compose.yml".into())],
            env: Some((".env".into(), "A='1'\n".into())),
        };
        assert!(matches!(
            compose_config(&with_env, &repo, &scratch, &run_docker),
            ComposeVerdict::Valid(_)
        ));
        std::fs::write(
            repo.join("compose.yml"),
            "services:\n  app:\n    imag: demo:latest\n",
        )
        .unwrap();
        match compose_config(&with_env, &repo, &scratch, &run_docker) {
            ComposeVerdict::Invalid(why) => assert!(why.contains("imag"), "{why}"),
            _ => panic!("a misspelled key must be refused"),
        }
    }

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

    fn addrs(list: &[&str]) -> Option<BTreeSet<IpAddr>> {
        Some(list.iter().map(|a| a.parse().unwrap()).collect())
    }

    fn lookup(name: &str) -> Option<BTreeSet<IpAddr>> {
        match name {
            "box.example.com" => addrs(&["209.38.61.110"]),
            "example.com" | "www.example.com" => addrs(&["209.38.61.110", "2001:db8::1"]),
            "elsewhere.example.com" => addrs(&["203.0.113.9"]),
            _ => None,
        }
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn names_that_point_at_the_target_say_nothing() {
        let lines = dns_mismatches(
            &names(&["example.com", "www.example.com"]),
            "box.example.com",
            lookup,
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_name_pointed_elsewhere_names_both_addresses() {
        let lines = dns_mismatches(
            &names(&["example.com", "elsewhere.example.com"]),
            "box.example.com",
            lookup,
        );
        assert_eq!(
            lines,
            vec!["elsewhere.example.com resolves to 203.0.113.9 — target box.example.com is 209.38.61.110"]
        );
    }

    #[test]
    fn a_name_that_does_not_resolve_yet_says_so() {
        let lines = dns_mismatches(&names(&["new.example.com"]), "box.example.com", lookup);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("new.example.com does not resolve"),
            "{lines:?}"
        );
    }

    #[test]
    fn wildcards_and_placeholders_are_not_looked_up() {
        let lines = dns_mismatches(
            &names(&["*.example.com", "__SITE_DOMAIN__"]),
            "unresolvable.example.com",
            lookup,
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn the_probe_looks_where_sudo_would() {
        // nginx is in /usr/sbin on Debian, which a deploy user's PATH omits.
        let script = probe_script(&tools(&["nginx"]));
        assert!(script.starts_with("PATH=\"$PATH:/usr/local/sbin:/usr/sbin:/sbin\""));
        assert!(script.contains("command -v 'nginx'"), "{script}");
    }

    #[cfg(unix)]
    #[test]
    fn the_sudo_probe_names_a_missing_sudo_and_nothing_else() {
        let run = |sudo: &str| {
            let out = Command::new("sh")
                .arg("-c")
                .arg(sudo_probe_script(sudo))
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(run("deliver-no-such-sudo"), "missing");
        // A sudo that refuses `true` for any other reason — sudoers allowing
        // only the deploy's own commands — is not flagged (`env -n` errors).
        assert_eq!(run("env"), "");
    }
}
