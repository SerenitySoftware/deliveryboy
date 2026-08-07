//! `.deliver.yml` — schema, loading, and validation.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SUPPORTED_VERSION: u32 = 1;

/// The canonical per-repo config file, and what `deliver init` writes.
pub const CONFIG_FILENAME: &str = ".deliver.yml";

/// Config locations searched (in order) at each directory level, when `--config`
/// isn't given. Both a single file and a `.deliveryboy/` directory are supported.
pub const CANDIDATES: &[&str] = &[
    ".deliver.yml",
    ".deliver.yaml",
    ".deliveryboy/config.yml",
    ".deliveryboy/config.yaml",
    ".deliveryboy.yml",
];

/// Find a config by walking up from `start` (so it works from a subdirectory,
/// like `git`). Stops at a repo root (`.git`) or the filesystem root.
pub fn discover(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    let mut dir = start.as_path();
    loop {
        for candidate in CANDIDATES {
            let path = dir.join(candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
        // Don't climb past a repo boundary.
        if dir.join(".git").exists() {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    bail!(
        "no config file found in {} or any parent (looked for: {}).\n\
         Run `deliver init` to create one, or pass --config <path>.",
        start.display(),
        CANDIDATES.join(", ")
    )
}

/// Resolve the config path: an explicit `--config` wins, else discovery.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(path) => {
            if !path.is_file() {
                bail!("no config file at {}", path.display());
            }
            Ok(path.to_path_buf())
        }
        None => discover(&std::env::current_dir()?),
    }
}

/// How to log in to a target: the ssh-specific knobs, kept together rather than
/// scattered across the target (user and port are ssh concerns, not target ones).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshAuth {
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Identity file, e.g. ~/.ssh/deploy.pem (`-i`).
    #[serde(default)]
    pub key: Option<String>,
    /// Let the agent supply keys too. Default true.
    #[serde(default = "default_true")]
    pub agent: bool,
    /// `StrictHostKeyChecking` value (yes | no | accept-new).
    #[serde(default)]
    pub strict_host_key_checking: Option<String>,
    /// ProxyJump host (`-J`), for a bastion.
    #[serde(default)]
    pub jump: Option<String>,
    /// Escape hatch: extra raw ssh arguments.
    #[serde(default)]
    pub options: Vec<String>,
}

impl Default for SshAuth {
    fn default() -> Self {
        Self {
            user: default_user(),
            port: default_port(),
            key: None,
            agent: true,
            strict_host_key_checking: None,
            jump: None,
            options: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// A single host — shorthand for `hosts: [x]`.
    #[serde(default)]
    pub host: Option<String>,
    /// Several hosts. Every service is planned and run once per host.
    #[serde(default)]
    pub hosts: Vec<String>,
    pub dir: String,
    /// `ssh` (default) or `local` (this machine — no remote hop).
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub ssh: SshAuth,
    /// Run privileged steps with sudo. Defaults to true for remote targets
    /// (server paths need it) and false for `method: local`, where the deploy
    /// directory is usually yours and a sudo prompt would just block.
    #[serde(default)]
    pub sudo: Option<bool>,
    /// Deprecated: `user`/`port` now live under `ssh:`. Still accepted so older
    /// configs keep working; the values are folded into `ssh` on load.
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

fn default_method() -> String {
    "ssh".to_string()
}

fn default_user() -> String {
    "root".to_string()
}

fn default_port() -> u16 {
    22
}

/// `~` in a config path refers to the operator's home, not a literal directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

impl Target {
    /// Fold the deprecated top-level user/port into `ssh`, and validate.
    fn normalize(&mut self, name: &str) -> Result<()> {
        if let Some(user) = self.user.take() {
            if self.ssh.user == default_user() {
                self.ssh.user = user;
            }
        }
        if let Some(port) = self.port.take() {
            if self.ssh.port == default_port() {
                self.ssh.port = port;
            }
        }
        if self.host.is_some() && !self.hosts.is_empty() {
            bail!("target '{name}': set either `host:` or `hosts:`, not both");
        }
        if self.hosts.is_empty() {
            match self.host.clone() {
                Some(h) => self.hosts.push(h),
                None if self.is_local() => self.hosts.push("localhost".to_string()),
                None => bail!("target '{name}': `host:` or `hosts:` is required"),
            }
        }
        if self.method != "ssh" && self.method != "local" {
            bail!(
                "target '{name}': method must be 'ssh' or 'local' (got '{}')",
                self.method
            );
        }
        Ok(())
    }

    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    pub fn is_local(&self) -> bool {
        self.method == "local"
    }

    pub fn uses_sudo(&self) -> bool {
        self.sudo.unwrap_or(!self.is_local())
    }

    /// Arguments for invoking ssh.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = vec!["-p".to_string(), self.ssh.port.to_string()];
        if let Some(key) = &self.ssh.key {
            args.push("-i".into());
            args.push(expand_tilde(key));
            if !self.ssh.agent {
                args.push("-o".into());
                args.push("IdentitiesOnly=yes".into());
            }
        }
        if let Some(shkc) = &self.ssh.strict_host_key_checking {
            args.push("-o".into());
            args.push(format!("StrictHostKeyChecking={shkc}"));
        }
        if let Some(jump) = &self.ssh.jump {
            args.push("-J".into());
            args.push(jump.clone());
        }
        args.extend(self.ssh.options.iter().cloned());
        args
    }

    /// Arguments for invoking scp.
    ///
    /// Identical to `ssh_args`, except for the port: scp spells it `-o Port=`,
    /// because it reserves `-p` for preserving mode and timestamps.
    pub fn scp_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        let mut rest = self.ssh_args().into_iter();
        while let Some(arg) = rest.next() {
            if arg == "-p" {
                if let Some(port) = rest.next() {
                    args.push("-o".into());
                    args.push(format!("Port={port}"));
                }
            } else {
                args.push(arg);
            }
        }
        args
    }

    /// `user@host:` for remote targets, empty for local ones.
    pub fn dest_prefix(&self, host: &str) -> String {
        if self.is_local() {
            String::new()
        } else {
            format!("{}@{host}:", self.ssh.user)
        }
    }

    pub fn describe(&self, host: &str) -> String {
        if self.is_local() {
            "local".to_string()
        } else {
            let key = self
                .ssh
                .key
                .as_deref()
                .map(|k| format!(" (key {k})"))
                .unwrap_or_else(|| " (agent)".to_string());
            format!("ssh {}@{host}:{}{key}", self.ssh.user, self.ssh.port)
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// Ordered fallback chain — the first provider that has a name wins.
    #[serde(default)]
    pub providers: Option<serde_yaml::Value>,
    /// The secrets this app needs. A list of names, or a mapping when a secret
    /// comes from somewhere other than the default chain.
    #[serde(default)]
    pub define: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub deployer: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Raw steps run before the deployer's own steps.
    #[serde(default, alias = "before")]
    pub pre: Vec<serde_yaml::Value>,
    /// Raw steps run after them.
    #[serde(default, alias = "after")]
    pub post: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub config: serde_yaml::Value,
    #[serde(default)]
    pub verify: Vec<serde_yaml::Value>,
}

fn default_true() -> bool {
    true
}

/// Tag the commit that shipped, so "what's live?" is answerable from git alone.
/// Off by default — tagging is a side effect on your history, so it's opt-in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Tag name template. `{version}` `{deploy}` `{sha}` are substituted.
    #[serde(default)]
    pub name: Option<String>,
    /// Push the tag after creating it.
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub remote: Option<String>,
    /// Annotated (default) vs lightweight.
    #[serde(default = "default_true")]
    pub annotate: bool,
}

/// Governs both identities: how the app's release number is derived, and
/// whether a successful deploy tags the commit that shipped. Delivery Boy's own
/// deploy id is always computed independently (see version.rs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersioningConfig {
    /// tag (default) | commit | commit-count
    #[serde(default, alias = "from")]
    pub version_from: Option<String>,
    /// Refuse a release from a dirty working tree.
    #[serde(default)]
    pub require_clean: bool,
    /// Refuse a release unless HEAD exactly matches its configured upstream.
    #[serde(default)]
    pub require_pushed: bool,
    /// Optional branch name required for a release (for example `main`).
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub tag: Option<TagConfig>,
}

fn default_notification_events() -> Vec<String> {
    vec!["succeeded".to_string(), "failed".to_string()]
}

/// A release-level notice. Notices run outside the deploy plan: a failed notice
/// cannot roll back a good release, and failures can still be announced after
/// the plan has unwound its reversible steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationConfig {
    /// `slack` is currently supported through an incoming webhook.
    pub channel: String,
    /// Name of a declared secret containing the webhook URL.
    pub webhook_secret: String,
    /// `started`, `succeeded`, and/or `failed`.
    #[serde(default = "default_notification_events")]
    pub events: Vec<String>,
    /// Optional command that prints a complete JSON payload for a successful
    /// release. It runs in the repository root.
    #[serde(default)]
    pub success_payload_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub app: String,
    #[serde(default)]
    pub defaults: Defaults,
    pub targets: BTreeMap<String, Target>,
    #[serde(default)]
    pub secrets: Option<SecretsConfig>,
    /// `release:` remains accepted as the older spelling.
    #[serde(default, alias = "release")]
    pub versioning: Option<VersioningConfig>,
    #[serde(default)]
    pub notifications: Vec<NotificationConfig>,
    pub services: BTreeMap<String, Service>,
}

impl Config {
    /// Resolve a service's target name, falling back to `defaults.target`.
    pub fn target_name_for(&self, service: &Service) -> Result<String> {
        match service
            .target
            .clone()
            .or_else(|| self.defaults.target.clone())
        {
            Some(name) => Ok(name),
            None => bail!("service has no target and defaults.target is unset"),
        }
    }

    pub fn target_for(&self, service: &Service) -> Result<(String, Target)> {
        let name = self.target_name_for(service)?;
        let target = self
            .targets
            .get(&name)
            .with_context(|| format!("unknown target '{name}'"))?;
        Ok((name, target.clone()))
    }
}

pub fn load(path: &Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut config: Config =
        serde_yaml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;

    if config.version != SUPPORTED_VERSION {
        bail!(
            "unsupported config version {} (this build supports {})",
            config.version,
            SUPPORTED_VERSION
        );
    }
    if config.targets.is_empty() {
        bail!("no targets defined");
    }
    if config.services.is_empty() {
        bail!("no services defined");
    }
    for (name, target) in config.targets.iter_mut() {
        target.normalize(name)?;
    }
    for (name, service) in &config.services {
        config
            .target_for(service)
            .with_context(|| format!("service '{name}'"))?;
        for dep in &service.needs {
            if !config.services.contains_key(dep) {
                bail!("service '{name}': needs unknown service '{dep}'");
            }
        }
    }
    for notice in &config.notifications {
        if notice.channel != "slack" {
            bail!(
                "notification channel '{}' is not supported (have: slack)",
                notice.channel
            );
        }
        for event in &notice.events {
            if !matches!(event.as_str(), "started" | "succeeded" | "failed") {
                bail!(
                    "notification event '{event}' is not supported (have: started, succeeded, failed)"
                );
            }
        }
    }
    Ok(config)
}
