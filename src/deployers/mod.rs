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
        }
    }

    pub fn with_rollback(mut self, rollback: impl Into<String>) -> Self {
        self.rollback = Some(rollback.into());
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
