//! `.deliver.yml` — schema, loading, and validation.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SUPPORTED_VERSION: u32 = 1;

/// The canonical per-repo config file, and what `deliver init` writes.
pub const CONFIG_FILENAME: &str = ".deliver.yml";

/// JSON Schema for `.deliver.yml`, for editors: completion, inline docs and
/// validation while the file is being written, before `deliver validate` can
/// run. Strict exactly where [`load`] is (the `deny_unknown_fields` structs),
/// descriptive where a deployer reads its `config:` block by hand.
pub const SCHEMA: &str = include_str!("config.schema.json");

/// Where [`SCHEMA`] is published — its `$id`, and what `deliver init` points
/// the editor at. Versioned with `version:`, so a v2 config gets its own URL.
pub const SCHEMA_URL: &str = "https://deliveryboy.app/schema/v1.json";

/// The first line of a scaffolded config: the yaml-language-server modeline
/// that VS Code, Zed, Helix and Neovim's yamlls read.
pub fn schema_modeline() -> String {
    format!("# yaml-language-server: $schema={SCHEMA_URL}")
}

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
    /// Advisory deploy-lock knobs. Locking itself is not optional — see
    /// [`crate::lock`] — but how long an abandoned lock blocks the next run is.
    #[serde(default)]
    pub lock: Option<LockConfig>,
    /// Deprecated: `user`/`port` now live under `ssh:`. Still accepted so older
    /// configs keep working; the values are folded into `ssh` on load.
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

/// How long a deploy lock may sit before a later run is allowed to take it
/// over. The window exists because a Ctrl-C kills `deliver` outright, and a
/// lock nobody can release is worse than one that is briefly too generous.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockConfig {
    /// Seconds. `0` never takes a lock over, so a crashed run has to be cleared
    /// by hand or with `--force`.
    #[serde(default)]
    pub stale_after: Option<u64>,
}

/// An hour is longer than any release this tool has been observed to run and
/// far shorter than a working day, so an abandoned lock clears itself before
/// the next person needs the box.
pub const DEFAULT_LOCK_STALE_AFTER: u64 = 3600;

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
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            return std::path::Path::new(&home)
                .join(rest)
                .to_string_lossy()
                .to_string();
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

    /// Where the advisory deploy lock lives: *beside* the target directory, not
    /// inside it. When a `files` service serves the release root, `dir` is the
    /// live symlink the deploy is about to swap, and a lock underneath it would
    /// be carried away mid-release. `<root>.releases` is the same convention.
    pub fn lock_dir(&self) -> String {
        format!("{}.deliver-lock", self.dir.trim_end_matches('/'))
    }

    pub fn lock_stale_after(&self) -> u64 {
        self.lock
            .as_ref()
            .and_then(|l| l.stale_after)
            .unwrap_or(DEFAULT_LOCK_STALE_AFTER)
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

/// Where a service's runtime logs live on the target, for `deliver logs`.
///
/// Only the `docker-compose` deployer can answer this from the deploy itself;
/// a `files` or `hugo` release is served by a web server the deploy never
/// configured, and a `commands` service could be anything. Rather than guess a
/// path that would be wrong on half of the hosts this tool targets, those
/// services say where to look.
///
/// Exactly one of `unit`, `files` or `command` is set — see [`LogsConfig::validate`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogsConfig {
    /// A systemd unit, read with `journalctl -u`.
    #[serde(default)]
    pub unit: Option<String>,
    /// Log files on the target, read with `tail`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Escape hatch: run this exact command on the target. `{tail}` becomes the
    /// line count and `{follow}` becomes `-f` when `--follow` is given.
    #[serde(default)]
    pub command: Option<String>,
}

impl LogsConfig {
    /// One source, not none and not several: two answers to "where are the
    /// logs?" would mean the command silently picks one of them.
    fn validate(&self, service: &str) -> Result<()> {
        let set: Vec<&str> = [
            self.unit.as_ref().map(|_| "unit"),
            (!self.files.is_empty()).then_some("files"),
            self.command.as_ref().map(|_| "command"),
        ]
        .into_iter()
        .flatten()
        .collect();
        match set.len() {
            1 => Ok(()),
            0 => bail!(
                "service '{service}': `logs:` needs one of `unit:`, `files:` or `command:`"
            ),
            _ => bail!(
                "service '{service}': `logs:` sets {} — use exactly one of `unit:`, `files:` or `command:`",
                set.join(" and ")
            ),
        }
    }
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
    /// Where this service's runtime logs are, when the deployer cannot say.
    #[serde(default)]
    pub logs: Option<LogsConfig>,
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
    /// Local steps that run only after the release tag exists and, when
    /// configured, has been pushed. Use this for work that needs the final tag,
    /// such as creating a GitHub release from artifacts built earlier.
    #[serde(default)]
    pub after_tag: Vec<serde_yaml::Value>,
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
        if let Some(logs) = &service.logs {
            logs.validate(name)?;
        }
    }
    if let Some(versioning) = &config.versioning {
        if !versioning.after_tag.is_empty()
            && !versioning
                .tag
                .as_ref()
                .map(|tag| tag.enabled)
                .unwrap_or(false)
        {
            bail!("versioning.after_tag requires versioning.tag.enabled: true");
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

#[cfg(test)]
mod schema_tests {
    //! The schema is a second description of what [`load`] accepts, so these
    //! tests hold the two together: every config the CLI accepts must satisfy
    //! the schema, every field `config.rs` gains must reach it, and a typo the
    //! loader refuses must be one an editor underlines.

    use super::*;
    use serde_json::Value as Json;
    use std::collections::BTreeSet;

    fn validator() -> jsonschema::Validator {
        let schema: Json = serde_json::from_str(SCHEMA).expect("SCHEMA is JSON");
        jsonschema::draft7::new(&schema).expect("SCHEMA is a valid draft-07 schema")
    }

    fn yaml_to_json(text: &str) -> Json {
        let yaml: serde_yaml::Value = serde_yaml::from_str(text).expect("YAML");
        serde_json::to_value(yaml).expect("YAML with string keys")
    }

    fn schema_errors(instance: &Json) -> Vec<String> {
        let validator = validator();
        validator
            .iter_errors(instance)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect()
    }

    fn load_text(name: &str, text: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!("deliver-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.yml"));
        std::fs::write(&path, text).unwrap();
        load(&path)
    }

    fn manifest(rel: &str) -> Option<String> {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).ok()
    }

    /// Every config this repository writes down anywhere: the raw strings the
    /// integration tests feed the CLI, the YAML in the README and the docs, and
    /// the repo's own release config. Files missing from a packaged crate are
    /// skipped, not failed.
    fn corpus() -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(src) = manifest("tests/plan.rs") {
            for (i, chunk) in src.split("r#\"").skip(1).enumerate() {
                let body = chunk.split("\"#").next().unwrap_or_default();
                if body.contains("version: 1") && body.contains("services:") {
                    out.push((format!("tests/plan.rs raw string #{i}"), body.to_string()));
                }
            }
        }
        let mut docs = vec!["README.md".to_string()];
        if let Ok(entries) =
            std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("site/content/docs"))
        {
            for entry in entries.flatten() {
                docs.push(format!(
                    "site/content/docs/{}",
                    entry.file_name().to_string_lossy()
                ));
            }
        }
        for doc in docs {
            let Some(text) = manifest(&doc) else { continue };
            for (i, chunk) in text.split("```yaml\n").skip(1).enumerate() {
                let body = chunk.split("```").next().unwrap_or_default();
                if body.contains("version: 1") && body.contains("services:") {
                    out.push((format!("{doc} yaml block #{i}"), body.to_string()));
                }
            }
        }
        if let Some(own) = manifest(".deliver.yml") {
            out.push((".deliver.yml".into(), own));
        }
        out
    }

    #[test]
    fn the_schema_is_valid_and_published_where_init_points() {
        let schema: Json = serde_json::from_str(SCHEMA).unwrap();
        assert!(jsonschema::meta::is_valid(&schema));
        assert_eq!(schema["$id"], SCHEMA_URL);
        assert!(schema_modeline().ends_with(SCHEMA_URL));
    }

    /// The site serves a copy at [`SCHEMA_URL`]. A copy that drifts from the
    /// one the binary prints is a schema an editor enforces and the CLI does not.
    #[test]
    fn the_site_copy_is_the_same_schema() {
        let Some(site) = manifest("site/static/schema/v1.json") else {
            assert!(
                manifest("site/hugo.toml").is_none(),
                "the site exists but has no site/static/schema/v1.json"
            );
            return;
        };
        assert_eq!(
            site, SCHEMA,
            "run: deliver schema > site/static/schema/v1.json"
        );
    }

    #[test]
    fn every_config_the_loader_accepts_satisfies_the_schema() {
        let corpus = corpus();
        assert!(corpus.len() >= 20, "corpus shrank to {}", corpus.len());
        let mut checked = 0;
        let mut failures = Vec::new();
        for (name, text) in &corpus {
            if load_text("corpus", text).is_err() {
                continue; // a deliberately bad config, or a template
            }
            // Loads, but exists to prove `plan` refuses it: a `commands`
            // service with no steps. The schema refusing it earlier is the point.
            if text.contains("deployer: commands\n    config: {}") {
                assert!(!schema_errors(&yaml_to_json(text)).is_empty(), "{name}");
                continue;
            }
            checked += 1;
            let errors = schema_errors(&yaml_to_json(text));
            if !errors.is_empty() {
                failures.push(format!("{name}:\n  {}\n{text}", errors.join("\n  ")));
            }
        }
        assert!(checked >= 20, "only {checked} corpus configs load");
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    /// Uses every field `config.rs` defines, so the drift check below has a
    /// value for each of them.
    const KITCHEN_SINK: &str = r#"
version: 1
app: sink
defaults: {target: production}
targets:
  production:
    host: app.example.com
    dir: /srv/sink
    method: ssh
    sudo: true
    lock: {stale_after: 600}
    ssh:
      user: deploy
      port: 2222
      key: ~/.ssh/deploy.pem
      agent: false
      strict_host_key_checking: accept-new
      jump: bastion.example.com
      options: [-o, ServerAliveInterval=30]
  edge:
    hosts: [a.example.com, b.example.com]
    dir: /srv/sink
secrets:
  providers: [env]
  define: [WEBHOOK]
versioning:
  version_from: commit-count
  require_clean: true
  require_pushed: true
  branch: main
  tag: {enabled: true, name: "v{version}", push: true, remote: origin, annotate: false}
  after_tag: [{command: "echo {version}"}]
notifications:
  - channel: slack
    webhook_secret: WEBHOOK
    events: [started, succeeded, failed]
    success_payload_command: ./payload.sh
services:
  web:
    deployer: files
    target: production
    enabled: true
    pre: [{command: make}]
    post: [{ssh: "true"}]
    config: {src: dist}
    verify: [{http: {url: "https://app.example.com/"}}]
    logs: {unit: web.service}
  worker:
    deployer: commands
    needs: [web]
    config: {steps: [{ssh: "true"}]}
    logs: {command: "tail {follow} -n {tail} /var/log/worker.log"}
  cron:
    deployer: commands
    config: {steps: [{ssh: "true"}]}
    logs: {files: [/var/log/cron.log]}
"#;

    /// Collect every field path the loaded config serializes, marking whether
    /// any instance of it carried a value. Map keys (target and service names)
    /// become `*`; user-authored blocks (`config`, steps, checks, secrets) stop
    /// the walk, since their keys are the deployer's to define, not `config.rs`'s.
    fn walk(value: &Json, path: String, seen: &mut BTreeSet<String>, set: &mut BTreeSet<String>) {
        const OPAQUE: &[&str] = &[
            "config",
            "pre",
            "post",
            "verify",
            "after_tag",
            "providers",
            "define",
        ];
        if let Json::Object(map) = value {
            for (key, child) in map {
                let dynamic = path == "targets" || path == "services";
                let next = match (path.is_empty(), dynamic) {
                    (true, _) => key.clone(),
                    (false, true) => format!("{path}.*"),
                    (false, false) => format!("{path}.{key}"),
                };
                if !dynamic {
                    seen.insert(next.clone());
                    if !child.is_null() {
                        set.insert(next.clone());
                    }
                }
                if !OPAQUE.contains(&key.as_str()) || dynamic {
                    walk(child, next, seen, set);
                }
            }
        } else if let Json::Array(items) = value {
            for item in items {
                walk(item, format!("{path}[]"), seen, set);
            }
        }
    }

    fn strip_nulls(value: &mut Json) {
        match value {
            Json::Object(map) => {
                map.retain(|_, v| !v.is_null());
                map.values_mut().for_each(strip_nulls);
            }
            Json::Array(items) => items.iter_mut().for_each(strip_nulls),
            _ => {}
        }
    }

    /// The loaded config, serialized back out, names every field `config.rs`
    /// has — defaults included — under its canonical spelling. So a field
    /// added to the structs without the schema learning it fails here twice
    /// over: the schema's `additionalProperties: false` rejects it, and if it
    /// is an `Option` the kitchen sink above never set, the walk reports it.
    #[test]
    fn a_field_added_to_config_rs_must_reach_the_schema() {
        let config = load_text("sink", KITCHEN_SINK).expect("the kitchen sink loads");
        let mut json = serde_json::to_value(&config).unwrap();

        let (mut seen, mut set) = (BTreeSet::new(), BTreeSet::new());
        walk(&json, String::new(), &mut seen, &mut set);
        // Folded into `ssh:` on load, by design.
        let folded = ["targets.*.user", "targets.*.port"];
        let unset: Vec<&String> = seen
            .difference(&set)
            .filter(|p| !folded.contains(&p.as_str()))
            .collect();
        assert!(
            unset.is_empty(),
            "KITCHEN_SINK never sets {unset:?}: give it a value there and describe it in config.schema.json"
        );

        strip_nulls(&mut json);
        let errors = schema_errors(&json);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    /// Older spellings `load` still accepts are ones the schema accepts too.
    #[test]
    fn aliases_and_deprecated_keys_validate() {
        let text = r#"
version: 1
app: old
defaults: {target: prod}
targets:
  prod: {host: old.example.com, user: root, port: 22, dir: /srv/old}
release: {from: tag}
services:
  web:
    deployer: nginx_vhost
    before: [{command: make}]
    after: [{ssh: "true"}]
    config: {conf: nginx/site.conf}
"#;
        load_text("aliases", text).expect("load accepts the old spellings");
        assert_eq!(schema_errors(&yaml_to_json(text)), Vec::<String>::new());
    }

    const MINIMAL: &str = r#"
version: 1
app: typo
defaults: {target: prod}
targets:
  prod: {host: typo.example.com, dir: /srv/typo}
services:
  web: {deployer: files, config: {src: dist}}
"#;

    /// A mistake the loader refuses is one the editor underlines — the point of
    /// the schema is to move `deliver validate`'s error to the keystroke.
    #[test]
    fn typos_the_loader_refuses_are_schema_errors_too() {
        load_text("minimal", MINIMAL).expect("the baseline loads");
        assert!(schema_errors(&yaml_to_json(MINIMAL)).is_empty());

        let cases = [
            (
                "unknown top-level key",
                MINIMAL.replace("defaults:", "default:"),
            ),
            ("unknown target key", MINIMAL.replace("{host:", "{hots:")),
            (
                "bad method",
                MINIMAL.replace("dir: /srv/typo}", "dir: /srv/typo, method: rsync}"),
            ),
            (
                "port is not a number",
                MINIMAL.replace("dir: /srv/typo}", "dir: /srv/typo, ssh: {port: twenty}}"),
            ),
            (
                "unknown service key",
                MINIMAL.replace("{deployer: files,", "{deployer: files, verfy: [],"),
            ),
            (
                "unsupported version",
                MINIMAL.replace("version: 1", "version: 2"),
            ),
            (
                "unsupported notification channel",
                format!("{MINIMAL}notifications: [{{channel: email, webhook_secret: X}}]\n"),
            ),
            (
                "no services",
                MINIMAL.replace("  web: {deployer: files, config: {src: dist}}\n", "  {}\n"),
            ),
        ];
        for (what, text) in cases {
            assert!(
                load_text("typo", &text).is_err(),
                "{what}: load accepted\n{text}"
            );
            assert!(
                !schema_errors(&yaml_to_json(&text)).is_empty(),
                "{what}: the schema accepted what load refuses\n{text}"
            );
        }
    }

    /// Deployer settings the plan refuses, caught before `plan` runs.
    #[test]
    fn a_deployer_config_missing_what_it_requires_is_a_schema_error() {
        let cases = [
            ("unknown deployer", "{deployer: ftp}"),
            ("files without src", "{deployer: files, config: {}}"),
            ("commands without steps", "{deployer: commands, config: {}}"),
            (
                "macos-app without appcast.url",
                "{deployer: macos-app, config: {appcast: {}}}",
            ),
            (
                "a raw step that is neither",
                "{deployer: commands, config: {steps: [{run: ls}]}}",
            ),
            (
                "an unknown verify check",
                "{deployer: files, config: {src: d}, verify: [{ping: x}]}",
            ),
        ];
        for (what, service) in cases {
            let text = MINIMAL.replace("{deployer: files, config: {src: dist}}", service);
            assert!(
                !schema_errors(&yaml_to_json(&text)).is_empty(),
                "{what}: the schema accepted\n{text}"
            );
        }
    }
}
