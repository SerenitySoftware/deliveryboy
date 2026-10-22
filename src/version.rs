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
    /// Where `release` came from: "tag" | "commit" | "commit-count", "argument"
    /// (`--version` or the prompt), or "live" (`verify`, read from the target).
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
    let name = tag_name(version, Some(cfg));

    // Already tagged? Checked up front by `check_tag`, before anything ships;
    // this is the last line, for a tag created while the release was running.
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

/// The tag a release gets, rendered exactly as `tag_release` renders it:
/// the `tag.name` template (default `{release}`) over this release.
pub fn tag_name(version: &DeployVersion, cfg: Option<&crate::config::TagConfig>) -> String {
    cfg.and_then(|c| c.name.clone())
        .unwrap_or_else(|| "{release}".to_string())
        .replace("{version}", &version.marketing_version())
        .replace("{release}", &version.release_display())
        .replace("{deploy}", &version.id)
        .replace("{sha}", &version.git.short_sha)
}

/// The commit a local tag points at, if the tag exists.
pub fn tag_commit(root: &Path, name: &str) -> Option<String> {
    git(root, &["rev-list", "-n", "1", &format!("refs/tags/{name}")])
}

fn short(sha: &str) -> &str {
    &sha[..7.min(sha.len())]
}

/// Where a tag of this name already sits, when that is not HEAD.
#[derive(Debug, Clone, PartialEq)]
pub enum TagClash {
    /// In this clone, on another commit.
    Local { commit: String },
    /// On the configured remote, on another commit, and not fetched.
    Remote { remote: String, commit: String },
}

impl TagClash {
    /// Why the release number cannot be used, and what to do instead.
    pub fn refusal(&self, name: &str) -> String {
        match self {
            TagClash::Local { commit } => {
                format!(
                    "tag {name} already exists on {} — pick another version",
                    short(commit)
                )
            }
            TagClash::Remote { remote, commit } => format!(
                "tag {name} already exists on {remote} ({}) — run git fetch --tags and pick \
                 another version",
                short(commit)
            ),
        }
    }
}

/// What asking git about a release's tag found.
#[derive(Debug, Default)]
pub struct TagCheck {
    pub clash: Option<TagClash>,
    /// The remote could not be asked; the run goes on without that answer.
    pub warning: Option<String>,
}

/// Ask, before anything ships, whether `name` is free to tag `head` with. A
/// tag already on `head` is fine (`tag_release` reports it rather than
/// recreating it). When `tag.push` is set the remote is asked once as well,
/// since a tag a colleague pushed is invisible here until a fetch; a remote
/// that does not answer is a warning, not a refusal.
pub fn check_tag(
    root: &Path,
    name: &str,
    head: &str,
    cfg: Option<&crate::config::TagConfig>,
) -> TagCheck {
    if let Some(commit) = tag_commit(root, name) {
        if commit != head {
            return TagCheck {
                clash: Some(TagClash::Local { commit }),
                warning: None,
            };
        }
    }
    let Some(cfg) = cfg.filter(|c| c.push) else {
        return TagCheck::default();
    };
    let remote = cfg.remote.clone().unwrap_or_else(|| "origin".into());
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-remote", "--tags", &remote])
        // Both spellings: the tag itself, and the commit an annotated tag
        // names (`^{}`), which a pattern on the bare name does not match.
        .arg(format!("refs/tags/{name}"))
        .arg(format!("refs/tags/{name}^{{}}"))
        .output();
    let out = match out {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            let why = String::from_utf8_lossy(&out.stderr);
            return TagCheck {
                clash: None,
                warning: Some(format!(
                    "could not ask {remote} whether tag {name} exists: {}",
                    why.lines().next().unwrap_or("git ls-remote failed").trim()
                )),
            };
        }
        Err(e) => {
            return TagCheck {
                clash: None,
                warning: Some(format!(
                    "could not ask {remote} whether tag {name} exists: {e}"
                )),
            }
        }
    };
    // An annotated tag lists twice: the tag object, then the commit it names
    // (`refs/tags/<name>^{}`). The commit is what a release is.
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut tag_object = None;
    let mut peeled = None;
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(r)) = (parts.next(), parts.next()) else {
            continue;
        };
        if r == format!("refs/tags/{name}^{{}}") {
            peeled = Some(sha.to_string());
        } else if r == format!("refs/tags/{name}") {
            tag_object = Some(sha.to_string());
        }
    }
    match peeled.or(tag_object) {
        Some(commit) if commit != head => TagCheck {
            clash: Some(TagClash::Remote { remote, commit }),
            warning: None,
        },
        _ => TagCheck::default(),
    }
}

/// One version the prompt offers: the bump kind, the bare number, and — when
/// the first candidate of that kind was taken — why this is the one offered.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub kind: &'static str,
    pub version: String,
    pub note: Option<String>,
}

/// Patch, minor and major bumps off `previous`, each advanced past any number
/// whose tag already exists. `taken` answers for a bare version with the tag
/// name and the commit it is on.
pub fn offer_bumps(
    previous: &SemVer,
    taken: &dyn Fn(&str) -> Option<(String, String)>,
) -> Vec<Offer> {
    [
        ("patch", Bump::Patch),
        ("minor", Bump::Minor),
        ("major", Bump::Major),
    ]
    .into_iter()
    .map(|(kind, bump)| {
        let mut candidate = previous.bump(bump);
        let mut skipped: Vec<(String, String)> = Vec::new();
        // Bounded: a project with a hundred consecutive taken numbers of one
        // kind gets the custom option, not a hang.
        while skipped.len() < 100 {
            let Some(hit) = taken(&candidate) else { break };
            skipped.push(hit);
            // The next number of the same kind.
            candidate = match SemVer::parse(&format!("{}{candidate}", previous.prefix)) {
                Some(next) => next.bump(bump),
                None => break,
            };
        }
        let note = match skipped.as_slice() {
            [] => None,
            [(name, commit)] => Some(format!("{name} is already on {}", short(commit))),
            [(first, _), .., (last, _)] => Some(format!("{first} … {last} are already tagged")),
        };
        Offer {
            kind,
            version: candidate,
            note,
        }
    })
    .collect()
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

#[cfg(test)]
mod tag_check_tests {
    use super::{check_tag, offer_bumps, tag_commit, tag_name, SemVer, TagClash};
    use crate::config::TagConfig;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Two commits: the first tagged `v1.0.0`; HEAD on the second, untagged.
    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deliver-tagcheck-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "T"]);
        std::fs::write(dir.join("a"), "a").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "one"]);
        git(&dir, &["tag", "v1.0.0"]);
        std::fs::write(dir.join("b"), "b").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "two"]);
        dir
    }

    fn head(dir: &Path) -> String {
        git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn a_tag_on_another_commit_is_a_local_clash_and_one_on_head_is_not() {
        let dir = repo("local");
        let first = git(&dir, &["rev-parse", "HEAD~1"]);
        let check = check_tag(&dir, "v1.0.0", &head(&dir), None);
        assert_eq!(check.clash, Some(TagClash::Local { commit: first }));
        assert!(check.warning.is_none());
        git(&dir, &["tag", "v1.0.1"]);
        let check = check_tag(&dir, "v1.0.1", &head(&dir), None);
        assert_eq!(check.clash, None);
        let check = check_tag(&dir, "v9.9.9", &head(&dir), None);
        assert_eq!(check.clash, None);
        assert_eq!(tag_commit(&dir, "v9.9.9"), None);
    }

    #[test]
    fn a_tag_held_only_by_origin_is_a_remote_clash_when_pushing() {
        let dir = repo("remote");
        let origin = std::env::temp_dir().join("deliver-tagcheck-remote-origin");
        let _ = std::fs::remove_dir_all(&origin);
        git(&dir, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&dir, &["push", "-q", "origin", "HEAD"]);
        let first = git(&dir, &["rev-parse", "HEAD~1"]);
        // An annotated tag, so ls-remote lists the tag object and the peeled commit.
        git(&dir, &["tag", "-a", "v1.0.1", "-m", "v1.0.1", &first]);
        git(&dir, &["push", "-q", "origin", "v1.0.1"]);
        git(&dir, &["tag", "-d", "v1.0.1"]);

        let push = TagConfig {
            enabled: true,
            push: true,
            ..TagConfig::default()
        };
        let check = check_tag(&dir, "v1.0.1", &head(&dir), Some(&push));
        assert_eq!(
            check.clash,
            Some(TagClash::Remote {
                remote: "origin".into(),
                commit: first.clone()
            })
        );
        assert!(check.clash.unwrap().refusal("v1.0.1").starts_with(&format!(
            "tag v1.0.1 already exists on origin ({})",
            &first[..7]
        )));

        // Without `push`, origin is not asked.
        let local_only = TagConfig {
            enabled: true,
            ..TagConfig::default()
        };
        assert_eq!(
            check_tag(&dir, "v1.0.1", &head(&dir), Some(&local_only)).clash,
            None
        );

        // A remote that does not answer is a warning, and the run goes on.
        let elsewhere = TagConfig {
            enabled: true,
            push: true,
            remote: Some("nowhere".into()),
            ..TagConfig::default()
        };
        let check = check_tag(&dir, "v1.0.1", &head(&dir), Some(&elsewhere));
        assert_eq!(check.clash, None);
        assert!(check.warning.unwrap().contains("could not ask nowhere"));
    }

    #[test]
    fn the_name_checked_is_the_templated_one() {
        let dir = repo("template");
        let v = super::DeployVersion {
            id: "20260101T000000Z-abc1234".into(),
            release: Some("v1.0.1".into()),
            release_source: "argument",
            git: super::git_info(&dir),
        };
        let cfg = TagConfig {
            enabled: true,
            name: Some("release-{version}".into()),
            ..TagConfig::default()
        };
        assert_eq!(tag_name(&v, Some(&cfg)), "release-1.0.1");
        assert_eq!(tag_name(&v, None), "v1.0.1");
    }

    #[test]
    fn offers_skip_taken_numbers_and_say_why() {
        let previous = SemVer::parse("v1.0.0").unwrap();
        let taken = |bare: &str| match bare {
            "1.0.1" => Some(("v1.0.1".to_string(), "d67382e0000".to_string())),
            "1.1.0" => Some(("v1.1.0".to_string(), "aaaaaaa0000".to_string())),
            "1.2.0" => Some(("v1.2.0".to_string(), "bbbbbbb0000".to_string())),
            _ => None,
        };
        let offers = offer_bumps(&previous, &taken);
        assert_eq!(offers[0].version, "1.0.2");
        assert_eq!(
            offers[0].note.as_deref(),
            Some("v1.0.1 is already on d67382e")
        );
        assert_eq!(offers[1].version, "1.3.0");
        assert_eq!(
            offers[1].note.as_deref(),
            Some("v1.1.0 … v1.2.0 are already tagged")
        );
        assert_eq!(offers[2].version, "2.0.0");
        assert_eq!(offers[2].note, None);
    }
}
