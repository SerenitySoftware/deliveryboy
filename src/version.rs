//! Two different identities, deliberately separate:
//!
//! * **deploy version** — Delivery Boy's own id for *this deploy*, e.g.
//!   `20260730T151230Z-1a2b3c4`. Sortable, human-readable, offline-computable,
//!   and tied to the commit. It names the release directory on the target, so
//!   `readlink web` answers "what's live?".
//!
//! * **release** — what the *app* calls this version: the git tag when HEAD is
//!   tagged (e.g. `v0.3.2`), otherwise the short sha. This is the number users
//!   and Sparkle see; Delivery Boy never invents it.
//!
//! Both are reported before and after a run.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct GitInfo {
    pub sha: String,
    pub short_sha: String,
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub commit_count: u64,
    pub dirty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployVersion {
    /// Delivery Boy's id for this deploy (also the release directory name).
    pub id: String,
    /// What the app calls this version — the git tag. `None` when HEAD isn't
    /// tagged: an untagged commit has no release number, and inventing one
    /// (a sha, a counter) produces a "version" nothing else agrees with.
    pub release: Option<String>,
    /// Where `release` came from: "tag" | "commit" | "commit-count".
    pub release_source: &'static str,
    pub git: GitInfo,
}

impl DeployVersion {
    /// The release number, or an explanation of why there isn't one.
    pub fn release_display(&self) -> String {
        self.release
            .clone()
            .unwrap_or_else(|| "untagged".to_string())
    }

    /// Use the bare semantic version for user-facing artifacts. Git tags often
    /// carry a `v` prefix, while CFBundleShortVersionString and filenames do not.
    pub fn marketing_version(&self) -> String {
        self.release_display().trim_start_matches('v').to_string()
    }

    pub fn with_release(mut self, release: String, source: &'static str) -> Self {
        self.release = Some(release);
        self.release_source = source;
        self
    }

    /// One-line summary for logs.
    pub fn describe(&self) -> String {
        match &self.release {
            Some(r) => format!(
                "release {r} ({}) · sha {}{}",
                self.release_source,
                self.git.short_sha,
                if self.git.dirty { " · DIRTY" } else { "" }
            ),
            None => format!(
                "no release — HEAD is not tagged · sha {}{}",
                self.git.short_sha,
                if self.git.dirty { " · DIRTY" } else { "" }
            ),
        }
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub fn git_info(root: &Path) -> GitInfo {
    let short_sha =
        git(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    GitInfo {
        sha: git(root, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into()),
        short_sha,
        tag: git(root, &["describe", "--tags", "--exact-match"]),
        branch: git(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        commit_count: git(root, &["rev-list", "--count", "HEAD"])
            .and_then(|c| c.parse().ok())
            .unwrap_or(0),
        // `--porcelain` prints nothing for a clean tree.
        dirty: git(root, &["status", "--porcelain"]).is_some(),
    }
}

pub fn release_gate(
    root: &Path,
    cfg: Option<&crate::config::VersioningConfig>,
) -> Result<(), String> {
    let Some(cfg) = cfg else { return Ok(()) };
    let info = git_info(root);
    if cfg.require_clean && info.dirty {
        return Err("working tree is dirty — commit or stash before releasing".into());
    }
    if let Some(required) = &cfg.branch {
        if info.branch.as_deref() != Some(required.as_str()) {
            return Err(format!(
                "release branch is {} — expected {required}",
                info.branch.as_deref().unwrap_or("detached")
            ));
        }
    }
    if cfg.require_pushed {
        let upstream = git(root, &["rev-parse", "@{upstream}"])
            .ok_or_else(|| "release branch has no upstream".to_string())?;
        if info.sha != upstream {
            return Err(format!(
                "HEAD {} does not match its upstream {} — push and pull before releasing",
                info.short_sha,
                &upstream[..upstream.len().min(8)]
            ));
        }
    }
    Ok(())
}

/// Civil date from a unix timestamp (Howard Hinnant's algorithm) so we can build
/// a readable UTC stamp without pulling in a date crate.
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{year:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Compute both identities. `release_from` comes from config (`release.version_from`).
pub fn resolve(root: &Path, release_from: Option<&str>) -> DeployVersion {
    let git = git_info(root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (release, release_source) = match release_from.unwrap_or("tag") {
        "commit-count" => (Some(git.commit_count.to_string()), "commit-count"),
        "commit" => (Some(git.short_sha.clone()), "commit"),
        // Default: the tag. No tag means no release — deliberately not faked.
        _ => (git.tag.clone(), "tag"),
    };

    let id = format!("{}-{}", utc_stamp(now), git.short_sha);
    DeployVersion {
        id,
        release,
        release_source,
        git,
    }
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;

    #[test]
    fn stamp_formats_known_epochs() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        // 2026-07-30T15:12:30Z
        assert_eq!(utc_stamp(1_785_424_350), "20260730T151230Z");
    }
}

/// Scratch space for an app's build artifacts: `<temp>/deliver/<app>`.
/// Per-run subdirectories keep concurrent deploys apart.
pub fn scratch_root(app: &str) -> std::path::PathBuf {
    std::env::temp_dir().join("deliver").join(app)
}

pub fn run_scratch(app: &str, deploy_id: &str) -> std::path::PathBuf {
    scratch_root(app).join(deploy_id)
}

/// Outcome of tagging a successful deploy.
pub struct TagResult {
    pub name: String,
    pub created: bool,
    pub pushed: bool,
    pub note: Option<String>,
}

/// Tag the commit that shipped. Idempotent: an existing tag on the same commit
/// is reported, not recreated; on a *different* commit it's an error rather than
/// a silent move, since tags are how you answer "what shipped?".
pub fn tag_release(
    root: &Path,
    version: &DeployVersion,
    cfg: &crate::config::TagConfig,
) -> Result<TagResult, String> {
    if version.git.dirty {
        return Err(
            "working tree is dirty — refusing to tag a commit that isn't what shipped".into(),
        );
    }
    let name = cfg
        .name
        .clone()
        .unwrap_or_else(|| "{release}".to_string())
        .replace("{version}", &version.marketing_version())
        .replace("{release}", &version.release_display())
        .replace("{deploy}", &version.id)
        .replace("{sha}", &version.git.short_sha);

    // Already tagged?
    if let Some(existing) = git(root, &["rev-list", "-n", "1", &name]) {
        if existing != version.git.sha {
            return Err(format!(
                "tag {name} already exists on a different commit ({}) — pick another name",
                &existing[..7.min(existing.len())]
            ));
        }
        let mut pushed = false;
        let mut note = Some("already tagged".into());
        if cfg.push {
            let remote = cfg.remote.clone().unwrap_or_else(|| "origin".into());
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["push", &remote, &name])
                .output()
                .map_err(|e| format!("git push failed: {e}"))?;
            if out.status.success() {
                pushed = true;
                note = Some("existing local tag pushed".into());
            } else {
                note = Some(format!(
                    "tag exists locally but push failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        return Ok(TagResult {
            name,
            created: false,
            pushed,
            note,
        });
    }

    let mut args: Vec<String> = vec!["tag".into()];
    if cfg.annotate {
        args.push("-a".into());
        args.push(name.clone());
        args.push("-m".into());
        args.push(format!(
            "{} (deploy {})",
            version.release_display(),
            version.id
        ));
    } else {
        args.push(name.clone());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&args)
        .output()
        .map_err(|e| format!("git tag failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git tag failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let mut pushed = false;
    let mut note = None;
    if cfg.push {
        let remote = cfg.remote.clone().unwrap_or_else(|| "origin".into());
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["push", &remote, &name])
            .output()
            .map_err(|e| format!("git push failed: {e}"))?;
        if out.status.success() {
            pushed = true;
        } else {
            // The deploy already succeeded; a failed push isn't fatal.
            note = Some(format!(
                "tag created locally but push failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(TagResult {
        name,
        created: true,
        pushed,
        note,
    })
}

/// The most recent tag reachable from HEAD (the previous release), if any.
pub fn previous_tag(root: &Path) -> Option<String> {
    git(root, &["describe", "--tags", "--abbrev=0"])
}

/// A semantic version parsed out of a tag, keeping any prefix (`v`) so new tags
/// match the repo's existing convention.
#[derive(Debug, Clone, PartialEq)]
pub struct SemVer {
    pub prefix: String,
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    pub fn parse(tag: &str) -> Option<Self> {
        let (prefix, rest) = match tag.strip_prefix('v') {
            Some(rest) => ("v".to_string(), rest),
            None => (String::new(), tag),
        };
        // Ignore any pre-release/build suffix when bumping.
        let core = rest.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self {
            prefix,
            major,
            minor,
            patch,
        })
    }

    /// The bare version (no prefix) — the tag name template adds that back.
    pub fn bump(&self, kind: Bump) -> String {
        match kind {
            Bump::Major => format!("{}.0.0", self.major + 1),
            Bump::Minor => format!("{}.{}.0", self.major, self.minor + 1),
            Bump::Patch => format!("{}.{}.{}", self.major, self.minor, self.patch + 1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bump {
    Major,
    Minor,
    Patch,
}

#[cfg(test)]
mod version_choice_tests {
    use super::{Bump, SemVer};

    #[test]
    fn parses_and_bumps() {
        let v = SemVer::parse("v1.4.9").unwrap();
        assert_eq!(v.prefix, "v");
        assert_eq!(v.bump(Bump::Patch), "1.4.10");
        assert_eq!(v.bump(Bump::Minor), "1.5.0");
        assert_eq!(v.bump(Bump::Major), "2.0.0");
    }

    #[test]
    fn keeps_an_unprefixed_convention() {
        let v = SemVer::parse("0.2.0").unwrap();
        assert_eq!(v.prefix, "");
        assert_eq!(v.bump(Bump::Minor), "0.3.0");
    }

    #[test]
    fn tolerates_prerelease_suffixes_and_short_forms() {
        assert_eq!(
            SemVer::parse("v2.1.0-rc.1").unwrap().bump(Bump::Patch),
            "2.1.1"
        );
        assert_eq!(SemVer::parse("v3").unwrap().bump(Bump::Minor), "3.1.0");
    }

    #[test]
    fn rejects_non_versions() {
        assert!(SemVer::parse("nightly").is_none());
    }
}
