//! Deployers are **compilers**: a service's config becomes an ordered list of
//! `PlannedStep`s. Nothing executes here — the executor runs the steps.

pub mod commands;
pub mod docker_compose;
pub mod files;
pub mod hugo;
pub mod macos_app;
pub mod nginx_vhost;

use crate::config::{Config, Service, Target};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_yaml::Value;

/// One concrete step. `Command` runs locally; `Ssh` runs on the target;
/// `Http` is a request (used by verify).
#[derive(Debug, Clone, Serialize)]
pub enum StepKind {
    Command {
        command: String,
        cwd: Option<String>,
    },
    Ssh {
        command: String,
    },
    Http {
        url: String,
        expect_status: u16,
        retries: u32,
        interval: u64,
    },
    /// Write a local file with an explicit mode. Used for rendered `.env`
    /// files: putting secret values in a shell command would leak them into
    /// plan output, `ps`, and shell history.
    WriteFile {
        path: String,
        mode: u32,
        #[serde(skip_serializing)]
        content: String,
    },
}

/// A long-lived config file on the target that a step overwrites, carried
/// alongside the step so `deliver` can diff it against what is running there
/// before the swap (see [`crate::configdiff`]).
///
/// Attached at compile time, by the deployer that already computed the content,
/// so there is no second code path deriving the same bytes a second way.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Absolute path of the file as it exists on the target.
    pub remote_path: String,
    /// Exactly what this deploy will put there.
    pub content: String,
}

/// Where a service keeps its deploy state on the target, carried alongside the
/// step that writes it so `deliver status` / `deliver history` can read back
/// what is live (see [`crate::readback`]).
///
/// Declared at compile time by the deployer that already computed these paths,
/// for the same reason [`LiveConfig`] is: the read-back must look exactly where
/// the deploy wrote, and a second derivation of the same paths is a second
/// thing to drift.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseState {
    /// The append-only TSV of past deploys (`.deliver/history.tsv`).
    pub history_path: String,
    /// The symlink pointing at the live release, for deployers that use the
    /// atomic release layout. `None` for a deployer that has no such layout
    /// (Compose replaces containers in place).
    pub live_path: Option<String>,
    /// The directory holding retained releases, when there is one.
    pub releases_dir: Option<String>,
    /// The file the activate step writes the outgoing release path to, so a
    /// one-step `deliver rollback` can find it. `Some` wherever
    /// `releases_dir` is, because a targeted rollback has to keep it honest:
    /// after swapping the live symlink somewhere else, the marker must name
    /// what *was* live, not what was live two rollbacks ago.
    pub previous_marker: Option<String>,
}

/// Where a service's runtime logs can be read on the target, carried alongside
/// a step so `deliver logs` can tail them (see [`crate::logs`]).
///
/// Declared by the deployer that already knows the answer, for the same reason
/// [`ReleaseState`] is: the Compose invocation that tails the logs must be the
/// one that started the containers, down to the `-f` files and the `-p`
/// project, and a second derivation of it is a second thing to drift.
///
/// Deployers that genuinely cannot know are silent rather than guessing — a
/// `files` release is served by somebody else's web server, and where *that*
/// writes its logs is a fact about the host, not about the deploy. Those
/// services say so themselves, with a `logs:` block on the service.
#[derive(Debug, Clone, PartialEq)]
pub enum LogSource {
    /// A Compose project: its own `docker compose … logs`, run from `dir`.
    Compose { compose: String, dir: String },
    /// A systemd unit, read with `journalctl -u`.
    Unit { name: String },
    /// Log files on the target, read with `tail`.
    Files { paths: Vec<String> },
    /// An exact command from the config's `logs.command`.
    Command { command: String },
}

impl LogSource {
    /// The shell command that reads this log on the target.
    ///
    /// `tail` is the number of existing lines to show first and `follow` keeps
    /// the stream open. `sudo` is the target's own prefix: the deploy writes
    /// these logs under sudo, so reading them usually needs the same.
    pub fn command(&self, sudo: &str, follow: bool, tail: usize) -> String {
        match self {
            LogSource::Compose { compose, dir } => {
                let follow = if follow { " --follow" } else { "" };
                format!(
                    "cd {} && {sudo}{compose} logs --no-color --tail {tail}{follow}",
                    crate::remote::shell_quote(dir)
                )
            }
            LogSource::Unit { name } => {
                let follow = if follow { " -f" } else { "" };
                format!(
                    "{sudo}journalctl --no-pager -u {} -n {tail}{follow}",
                    crate::remote::shell_quote(name)
                )
            }
            LogSource::Files { paths } => {
                // `-F` rather than `-f`: a log file that rotates mid-tail is the
                // normal case, and `-f` would sit on the renamed inode forever.
                let follow = if follow { " -F" } else { "" };
                let quoted: Vec<String> = paths
                    .iter()
                    .map(|p| crate::remote::shell_quote(p))
                    .collect();
                format!("{sudo}tail -n {tail}{follow} -- {}", quoted.join(" "))
            }
            // Verbatim, with the two things the command cannot know filled in.
            // No sudo: an operator who wrote the command wrote the whole of it.
            LogSource::Command { command } => command
                .replace("{follow}", if follow { "-f" } else { "" })
                .replace("{tail}", &tail.to_string())
                .trim()
                .to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedStep {
    pub label: String,
    pub kind: StepKind,
    /// How to undo this step, if it can be undone. Set on mutating steps (e.g.
    /// activating a release); the executor unwinds these in reverse when a later
    /// step fails, so a failed deploy doesn't leave a half-changed target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<String>,
    /// True when the step handles secret values, so plan/dry-run print a
    /// placeholder instead of its contents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
    /// Cleanup steps run after the service's real steps, are never rolled back,
    /// and never fail a deploy. On failure they're skipped so the intermediate
    /// artifacts survive for debugging.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cleanup: bool,
    /// The live file this step replaces, when it replaces one. Never
    /// serialized: a rendered vhost carries resolved secrets, exactly like
    /// `WriteFile::content`.
    #[serde(skip_serializing)]
    pub live_config: Option<LiveConfig>,
    /// Where this step records what was deployed, when it records anything.
    /// Not serialized: it is compile-time metadata for a local read-back, not
    /// part of the step the executor runs.
    #[serde(skip_serializing)]
    pub release_state: Option<ReleaseState>,
    /// How this step's service can be tailed on the target, when the deployer
    /// knows. Not serialized, for the same reason `release_state` is not: it is
    /// compile-time metadata for a local read, not part of the step.
    #[serde(skip_serializing)]
    pub log_source: Option<LogSource>,
}

impl PlannedStep {
    pub fn command(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: StepKind::Command {
                command: command.into(),
                cwd: None,
            },
            rollback: None,
            cleanup: false,
            secret: false,
            live_config: None,
            release_state: None,
            log_source: None,
        }
    }
    pub fn command_in(
        label: impl Into<String>,
        command: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            kind: StepKind::Command {
                command: command.into(),
                cwd: Some(cwd.into()),
            },
            rollback: None,
            cleanup: false,
            secret: false,
            live_config: None,
            release_state: None,
            log_source: None,
        }
    }
    pub fn ssh(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            kind: StepKind::Ssh {
                command: command.into(),
            },
            rollback: None,
            cleanup: false,
            secret: false,
            live_config: None,
            release_state: None,
            log_source: None,
        }
    }

    /// Write a file locally with a fixed mode; contents stay out of logs.
    pub fn write_file(
        label: impl Into<String>,
        path: impl Into<String>,
        mode: u32,
        content: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            kind: StepKind::WriteFile {
                path: path.into(),
                mode,
                content: content.into(),
            },
            rollback: None,
            cleanup: false,
            secret: true,
            live_config: None,
            release_state: None,
            log_source: None,
        }
    }

    /// Mark this as a cleanup step (runs last, non-fatal, skipped on failure).
    pub fn into_cleanup(mut self) -> Self {
        self.cleanup = true;
        self
    }

    /// Attach an undo command (run over ssh) to a mutating step.
    pub fn http(
        label: impl Into<String>,
        url: impl Into<String>,
        expect_status: u16,
        retries: u32,
        interval: u64,
    ) -> Self {
        Self {
            label: label.into(),
            kind: StepKind::Http {
                url: url.into(),
                expect_status,
                retries,
                interval,
            },
            rollback: None,
            cleanup: false,
            secret: false,
            live_config: None,
            release_state: None,
            log_source: None,
        }
    }

    pub fn with_rollback(mut self, rollback: impl Into<String>) -> Self {
        self.rollback = Some(rollback.into());
        self
    }

    /// Declare the live file this step overwrites, so a deploy can show what it
    /// changes on the target before changing it.
    pub fn with_live_config(
        mut self,
        remote_path: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        self.live_config = Some(LiveConfig {
            remote_path: remote_path.into(),
            content: content.into(),
        });
        self
    }

    /// Declare where this step records the deploy, so `status`, `history` and
    /// `rollback --to` can read it back from the target later.
    pub fn with_release_state(mut self, state: ReleaseState) -> Self {
        self.release_state = Some(state);
        self
    }

    /// Declare how this step's service writes its runtime logs, so
    /// `deliver logs` can tail exactly what this deploy started.
    pub fn with_log_source(mut self, source: LogSource) -> Self {
        self.log_source = Some(source);
        self
    }

    pub fn type_name(&self) -> &'static str {
        match self.kind {
            StepKind::Command { .. } => "command",
            StepKind::Ssh { .. } => "ssh",
            StepKind::Http { .. } => "http",
            StepKind::WriteFile { .. } => "write",
        }
    }

    pub fn detail(&self) -> String {
        match &self.kind {
            StepKind::Command { command, .. } | StepKind::Ssh { command } => command.clone(),
            StepKind::Http { url, .. } => url.clone(),
            // Never echo the contents — that's the point of this step kind.
            StepKind::WriteFile { path, mode, .. } => {
                format!("{path} (mode {mode:o}, contents hidden)")
            }
        }
    }
}

/// Everything a deployer needs to compile, without touching the network.
pub struct PlanContext {
    pub app: String,
    pub target: Target,
    /// The specific host this plan is for — a target may have several.
    pub host: String,
    pub sudo: bool,
    /// Repo root, so deployers can read files they need at compile time
    /// (e.g. nginx_vhost parsing the vhost for domains + cert paths).
    pub repo_root: std::path::PathBuf,
    /// This deploy's identity (deploy id + the app's release number).
    pub version: crate::version::DeployVersion,
    /// Per-run scratch directory (see `work_dir`).
    pub work_dir: String,
    /// Resolves secret values from the configured provider chain.
    pub secrets: std::rc::Rc<crate::secrets::Resolver>,
}

impl PlanContext {
    pub fn sudo_prefix(&self) -> &'static str {
        if self.sudo {
            "sudo "
        } else {
            ""
        }
    }

    /// Copy files to the target over scp, honoring the target's login method
    /// (a plain `cp` when `method: local`).
    ///
    /// scp rather than rsync, deliberately. Everything shipped here is freshly
    /// built — an image tarball, a release archive, a rendered `.env` — so
    /// there is no delta for rsync to exploit, and requiring rsync on the far
    /// end buys nothing over the ssh that is already a hard dependency.
    ///
    /// `-p` preserves mode, which is what keeps a `0600` `.env` at `0600` in
    /// flight rather than relying on a follow-up chmod.
    pub fn copy(&self, src: &str, dest_dir: &str) -> String {
        self.copy_many(&[src.to_string()], dest_dir, false)
    }

    /// Copy a directory and its contents, creating `{dest_dir}/{basename}`.
    pub fn copy_dir(&self, src: &str, dest_dir: &str) -> String {
        self.copy_many(&[src.to_string()], dest_dir, true)
    }

    pub fn copy_many(&self, srcs: &[String], dest_dir: &str, recursive: bool) -> String {
        let sources = srcs.join(" ");
        if self.target.is_local() {
            let flags = if recursive { "-Rp" } else { "-p" };
            return format!("cp {flags} {sources} {dest_dir}/");
        }
        let recurse = if recursive { "-r " } else { "" };
        format!(
            "scp -C -p {recurse}{} {sources} {}{dest_dir}/",
            self.target.scp_args().join(" "),
            self.target.dest_prefix(&self.host)
        )
    }

    /// Scratch space for build intermediates (tarballs, staging dirs).
    ///
    /// Lives under the system temp dir, not the repo: a tarball is a build
    /// artifact, not project state, so it has no business in a working tree
    /// (or a .gitignore). Each run gets its own directory, so concurrent runs
    /// can't collide and nothing stale is ever picked up.
    pub fn work_dir(&self) -> String {
        self.work_dir.clone()
    }

    pub fn resolver(&self) -> &crate::secrets::Resolver {
        &self.secrets
    }

    /// Human label for where a copy is going.
    pub fn dest_label(&self) -> String {
        if self.target.is_local() {
            "local".to_string()
        } else {
            self.host.clone()
        }
    }
}

/// Read a string field from a service's `config:` block.
pub fn cfg_str(cfg: &Value, key: &str) -> Option<String> {
    cfg.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn cfg_bool(cfg: &Value, key: &str, default: bool) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

pub fn compile_service(
    config: &Config,
    service: &Service,
    ctx: &PlanContext,
) -> Result<Vec<PlannedStep>> {
    let mut steps = Vec::new();
    for raw in &service.pre {
        steps.push(compile_raw_step(raw, ctx)?);
    }
    steps.extend(match service.deployer.as_str() {
        "commands" => commands::compile(&service.config, ctx)?,
        "docker-compose" | "docker_compose" | "compose" => {
            docker_compose::compile(&service.config, ctx)?
        }
        "files" => files::compile(&service.config, ctx)?,
        "hugo" => hugo::compile(&service.config, ctx)?,
        // Hyphens are canonical; the underscore spellings stay accepted so
        // existing configs keep working.
        "macos-app" | "macos_app" => macos_app::compile(&service.config, ctx)?,
        "nginx-vhost" | "nginx_vhost" => nginx_vhost::compile(&service.config, ctx)?,
        other => bail!(
            "unknown deployer '{other}' (have: {})",
            known_deployers().join(", ")
        ),
    });
    for raw in &service.post {
        steps.push(compile_raw_step(raw, ctx)?);
    }
    for check in &service.verify {
        steps.push(crate::verify::compile(check, ctx)?);
    }
    let _ = config;
    Ok(steps)
}

/// `before`/`after` escape hatch: `{command: ...}` or `{ssh: ...}`.
pub fn compile_raw_step(raw: &Value, ctx: &PlanContext) -> Result<PlannedStep> {
    let expand = |value: &str| {
        value
            .replace("{version}", &ctx.version.marketing_version())
            .replace("{release}", &ctx.version.release_display())
            .replace("{deploy}", &ctx.version.id)
            .replace("{sha}", &ctx.version.git.short_sha)
            .replace("{work}", &ctx.work_dir())
    };
    if let Some(cmd) = raw.get("command").and_then(|v| v.as_str()) {
        let cmd = expand(cmd);
        return Ok(PlannedStep::command(format!("$ {cmd}"), cmd));
    }
    if let Some(cmd) = raw.get("ssh").and_then(|v| v.as_str()) {
        let cmd = expand(cmd);
        return Ok(PlannedStep::ssh(format!("ssh: {cmd}"), cmd));
    }
    bail!("unrecognized raw step (expected 'command' or 'ssh'): {raw:?}")
}

pub fn known_deployers() -> &'static [&'static str] {
    &[
        "commands",
        "docker-compose",
        "files",
        "hugo",
        "macos-app",
        "nginx-vhost",
    ]
}
