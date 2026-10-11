//! The release contract is separate from the legacy SSH/undo plan.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub release_version: u32,
    pub app: String,
    pub checkout: PathBuf,
    pub artifacts: PathBuf,
    /// Azure Repos (or another Git remote), read only.
    pub source: Source,
    pub build: Vec<Command>,
    pub checks: BTreeMap<String, Command>,
    #[serde(default)]
    pub secret_env: Vec<String>,
    pub targets: BTreeMap<String, Target>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub remote: String,
    pub branch: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// Executed directly, without a shell or value interpolation.
    pub argv: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub account: String,
    pub resource_group: String,
    pub container: String,
    /// Pre-created lease blobs. The environment and any shared sites need locks.
    pub locks: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub provider: String,
    pub production: bool,
    pub subscription: String,
    pub tenant: String,
    pub resource_group: String,
    pub store: Store,
    pub registry: String,
    /// Image name -> ACR repository. images.json supplies archive and image_id.
    pub images: BTreeMap<String, String>,
    /// Every process that can write the database must be listed here or held by guard.
    pub writers: Vec<String>,
    /// Read-only project check for external writers, job schedules and schema safeguards.
    pub guard: Command,
    /// Reuse project Task/Bicep provisioning, using only pinned images from context.json.
    pub infrastructure: Vec<Command>,
    pub migrate: Option<Command>,
    pub rollout: Vec<Command>,
    pub applications: BTreeMap<String, String>,
    #[serde(default)]
    pub migration_jobs: Vec<String>,
    #[serde(default)]
    pub verification_jobs: Vec<String>,
    pub verify: Vec<Command>,
    pub browser: Vec<Command>,
    pub docs: BTreeMap<String, Site>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub name: String,
    pub resource_group: String,
    pub artifact: PathBuf,
    /// Exact committed policy to compare with the artifact before publication.
    pub policy: PathBuf,
    pub access: Access,
    pub validate: Command,
    pub verify: Command,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Internal,
    Public,
}

pub fn component(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && value != "."
            && value != "..",
        "invalid release identifier"
    );
    Ok(())
}
pub fn relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_)))
            && !path.to_string_lossy().contains('\\'),
        "expected a plain relative path"
    );
    Ok(())
}
/// Resolve an input without following symlinked parents, even inside the checkout.
pub fn contained(root: &Path, path: &Path) -> Result<PathBuf> {
    relative(path)?;
    let mut selected = root.to_path_buf();
    for part in path.components() {
        selected.push(part);
        ensure!(
            !std::fs::symlink_metadata(&selected)?
                .file_type()
                .is_symlink(),
            "input path contains a symlink"
        );
    }
    Ok(selected)
}
pub fn stable_version(value: &str) -> Result<(u64, u64, u64)> {
    let parts: Vec<_> = value
        .strip_prefix('v')
        .context("production version must be vMAJOR.MINOR.PATCH")?
        .split('.')
        .collect();
    ensure!(
        parts.len() == 3
            && parts.iter().all(|s| !s.is_empty()
                && (s.len() == 1 || !s.starts_with('0'))
                && s.bytes().all(|b| b.is_ascii_digit())),
        "production version must be vMAJOR.MINOR.PATCH"
    );
    Ok((parts[0].parse()?, parts[1].parse()?, parts[2].parse()?))
}
impl Command {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.argv.is_empty()
                && self.argv.iter().all(|s| !s.contains(['\0', '\n', '\r']))
                && !self.argv[0].is_empty(),
            "invalid command argv"
        );
        ensure!(
            self.platforms
                .iter()
                .all(|s| ["linux", "macos", "windows"].contains(&s.as_str())),
            "unknown command platform"
        );
        Ok(())
    }
    pub fn available(&self) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|s| s == std::env::consts::OS)
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<(Self, PathBuf, String)> {
        let raw = std::fs::read(path)?;
        let cfg: Self = serde_yaml::from_slice(&raw).context("invalid release config")?;
        cfg.validate()?;
        let root = path
            .canonicalize()?
            .parent()
            .unwrap()
            .join(&cfg.checkout)
            .canonicalize()?;
        // Canonical parsed values keep Windows CRLF checkouts compatible.
        let identity = super::evidence::hash(&serde_json::to_vec(&cfg)?);
        Ok((cfg, root, identity))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.release_version == 1, "unsupported release_version");
        component(&self.app)?;
        component(&self.source.remote)?;
        relative(Path::new(&self.source.branch))?;
        relative(&self.artifacts)?;
        ensure!(
            !self.build.is_empty() && !self.checks.is_empty() && !self.targets.is_empty(),
            "build, checks and targets are required"
        );
        for command in &self.build {
            command.validate()?;
        }
        for (name, command) in &self.checks {
            component(name)?;
            command.validate()?;
        }
        for secret in &self.secret_env {
            ensure!(
                !secret.starts_with("DELIVER_")
                    && secret
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    && !secret.is_empty(),
                "invalid secret environment name"
            );
        }
        let mut groups = std::collections::BTreeSet::new();
        for (name, target) in &self.targets {
            component(name)?;
            ensure!(target.provider == "azure", "unsupported release provider");
            for value in [
                &target.subscription,
                &target.tenant,
                &target.resource_group,
                &target.registry,
                &target.store.account,
                &target.store.resource_group,
                &target.store.container,
            ] {
                component(value)?;
            }
            ensure!(
                groups.insert((&target.subscription, &target.resource_group)),
                "targets must use separate resource groups"
            );
            ensure!(
                !target.store.locks.is_empty()
                    && !target.images.is_empty()
                    && !target.writers.is_empty()
                    && !target.rollout.is_empty()
                    && !target.verify.is_empty()
                    && !target.browser.is_empty(),
                "locks, images, writers, rollout, application and browser checks are required"
            );
            let mut locks = std::collections::BTreeSet::new();
            for lock in &target.store.locks {
                relative(Path::new(lock))?;
                ensure!(locks.insert(lock), "duplicate lock");
            }
            for (image, repo) in &target.images {
                component(image)?;
                relative(Path::new(repo))?;
                ensure!(
                    repo.bytes().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || b"/_-.".contains(&c)),
                    "invalid registry repository"
                );
            }
            for writer in &target.writers {
                component(writer)?;
                ensure!(
                    target.applications.contains_key(writer),
                    "writer must be a declared application"
                );
            }
            ensure!(!target.applications.is_empty(), "applications are required");
            for (app, image) in &target.applications {
                component(app)?;
                ensure!(
                    target.images.contains_key(image),
                    "application references an unknown image"
                );
            }
            for job in target
                .migration_jobs
                .iter()
                .chain(&target.verification_jobs)
            {
                component(job)?;
            }
            for command in target
                .infrastructure
                .iter()
                .chain(&target.rollout)
                .chain(&target.verify)
                .chain(&target.browser)
                .chain(std::iter::once(&target.guard))
                .chain(target.migrate.iter())
            {
                command.validate()?;
            }
            let mut sites = std::collections::BTreeSet::new();
            for (name, site) in &target.docs {
                component(name)?;
                component(&site.name)?;
                component(&site.resource_group)?;
                ensure!(
                    sites.insert((&site.resource_group, &site.name)),
                    "duplicate documentation site"
                );
                relative(&site.artifact)?;
                relative(&site.policy)?;
                site.validate.validate()?;
                site.verify.validate()?;
            }
        }
        // Shared documentation destinations must share a lease in the same store.
        for a in self.targets.values() {
            for b in self.targets.values() {
                for site in a.docs.values() {
                    if b.docs
                        .values()
                        .any(|s| s.name == site.name && s.resource_group == site.resource_group)
                        && a.subscription == b.subscription
                    {
                        ensure!(
                            a.store.account == b.store.account
                                && a.store.container == b.store.container
                                && a.store.locks.iter().any(|l| b.store.locks.contains(l)),
                            "shared documentation sites require a shared lease"
                        );
                    }
                }
            }
        }
        Ok(())
    }
    pub fn target(&self, name: &str) -> Result<&Target> {
        self.targets
            .get(name)
            .with_context(|| format!("unknown release target {name}"))
    }
}
pub fn check_platform(command: &Command) -> Result<()> {
    if !command.available() {
        bail!("required command needs {:?}; this host is {}. Run the check on a matching host; it is not skipped", command.platforms, std::env::consts::OS);
    }
    Ok(())
}
