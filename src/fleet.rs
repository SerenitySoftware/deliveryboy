//! `deliver.fleet.yml` — one command across the repos on one host.
//!
//! The founding problem is ~8–10 apps sharing a box, and every command up to
//! here operates on one repo: "deploy everything" or "what is live across the
//! fleet?" means N invocations and N terminals. This is the thinnest thing that
//! answers those two questions — a list of repo paths, and a loop that runs the
//! per-repo command that already exists, once per repo.
//!
//! Deliberately thin. It is a *loop*, not a scheduler: no state, no daemon, no
//! cross-repo dependency graph, no second host. Each repo is entered and run
//! exactly as if you had `cd`-ed into it and typed the command yourself —
//! including config discovery, which is why the loop changes the process's
//! working directory rather than threading a config path through. A local
//! `command:` step compiles with no `cwd` (`deployers::compile_raw_step`), so
//! it inherits the process's directory; entering the repo is what makes
//! `command: ./scripts/build.sh` mean the same thing under `fleet` as it does
//! on its own.
//!
//! ```yaml
//! version: 1
//! repos:
//!   - ../conduit
//!   - ../toothpick
//! ```

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The canonical fleet file.
pub const FLEET_FILENAME: &str = "deliver.fleet.yml";

/// Fleet file names searched at each directory level, when `--fleet` isn't given.
pub const CANDIDATES: &[&str] = &[FLEET_FILENAME, "deliver.fleet.yaml"];

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFile {
    #[serde(default = "default_version")]
    version: u32,
    repos: Vec<String>,
}

fn default_version() -> u32 {
    SUPPORTED_VERSION
}

/// One repo in the fleet, resolved to an absolute path before anything runs.
#[derive(Debug, Clone)]
pub struct Repo {
    /// What the fleet file said, kept for messages so the operator recognizes it.
    pub declared: String,
    pub path: PathBuf,
    /// The directory name, which is what `--repo` matches and what is printed.
    pub name: String,
}

/// A loaded fleet: where the file was, and the repos it names.
#[derive(Debug)]
pub struct Fleet {
    pub path: PathBuf,
    pub repos: Vec<Repo>,
}

/// Find a fleet file by walking up from `start`.
///
/// Unlike [`crate::config::discover`] this does **not** stop at a `.git`
/// directory: the fleet file sits *above* the repos it lists, so running
/// `deliver fleet status` from inside one of them has to be able to climb out.
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
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    bail!(
        "no fleet file found in {} or any parent (looked for: {}).\n\
         A fleet file lists the repo paths to run across:\n\
         \n    version: 1\n    repos:\n      - ../conduit\n      - ../toothpick\n",
        start.display(),
        CANDIDATES.join(", ")
    )
}

/// Resolve the fleet path: an explicit `--fleet` wins, else discovery.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(path) => {
            if !path.is_file() {
                bail!("no fleet file at {}", path.display());
            }
            Ok(path.to_path_buf())
        }
        None => discover(&std::env::current_dir()?),
    }
}

/// Load and validate a fleet file.
///
/// Paths are relative to the *fleet file's* directory, not the caller's, and
/// are made absolute here — before the loop starts changing directories, which
/// would otherwise move the ground a relative path is measured from.
pub fn load(path: &Path) -> Result<Fleet> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: FleetFile =
        serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if file.version != SUPPORTED_VERSION {
        bail!(
            "{}: unsupported version {} (this build understands {})",
            path.display(),
            file.version,
            SUPPORTED_VERSION
        );
    }
    if file.repos.is_empty() {
        bail!("{}: `repos:` is empty", path.display());
    }

    let base = path.parent().unwrap_or(Path::new("."));
    let mut repos = Vec::new();
    for declared in file.repos {
        let joined = base.join(expand_tilde(&declared));
        // Canonicalize when we can, so `../x` prints as somewhere recognizable
        // and two spellings of one repo are visibly the same path. A path that
        // does not exist yet is kept as written and reported by the loop, which
        // can say which repo is missing rather than failing the whole fleet.
        let path = joined.canonicalize().unwrap_or(joined);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| declared.clone());
        repos.push(Repo {
            declared,
            path,
            name,
        });
    }
    Ok(Fleet {
        path: path.to_path_buf(),
        repos,
    })
}

/// `~` and `~/x` relative to the operator's home. A fleet file is hand-written
/// and naming `~/dev/app` is the obvious thing to type.
fn expand_tilde(raw: &str) -> PathBuf {
    let Some(rest) = raw.strip_prefix('~') else {
        return PathBuf::from(raw);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(raw);
    };
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest)
    }
}

impl Fleet {
    /// The repos a run applies to. An empty selection means all of them.
    ///
    /// A selector matches a repo's directory name or its declared path, so both
    /// `--repo conduit` and `--repo ../conduit` work. Returns the selectors
    /// that matched nothing, so a typo is named rather than silently narrowing
    /// the run to nothing.
    pub fn select(&self, only: &[String]) -> (Vec<&Repo>, Vec<String>) {
        if only.is_empty() {
            return (self.repos.iter().collect(), Vec::new());
        }
        let mut unmatched = Vec::new();
        let mut chosen: Vec<&Repo> = Vec::new();
        for selector in only {
            let matches: Vec<&Repo> = self
                .repos
                .iter()
                .filter(|r| r.name == *selector || r.declared == *selector)
                .collect();
            if matches.is_empty() {
                unmatched.push(selector.clone());
                continue;
            }
            for repo in matches {
                if !chosen.iter().any(|c| c.path == repo.path) {
                    chosen.push(repo);
                }
            }
        }
        // Fleet-file order, not the order the selectors were typed: a deploy
        // across several repos should be reproducible from the file.
        let ordered = self
            .repos
            .iter()
            .filter(|r| chosen.iter().any(|c| c.path == r.path))
            .collect();
        (ordered, unmatched)
    }
}

/// What happened to one repo in a fleet run.
pub enum RepoOutcome {
    /// The command ran and gave this exit code (0 is success).
    Ran(i32),
    /// The repo could not be entered or has no config — named, not guessed at.
    Skipped(String),
    /// An earlier repo failed and the run stopped before reaching this one.
    NotAttempted,
}

impl RepoOutcome {
    pub fn ok(&self) -> bool {
        matches!(self, RepoOutcome::Ran(0))
    }

    /// The exit code this outcome contributes to the fleet's own.
    ///
    /// A skipped repo is a usage error (`2`), the same code a missing config
    /// gets in a single-repo run; a repo never attempted contributes nothing,
    /// because the failure that stopped the run is already being reported.
    pub fn exit_code(&self) -> i32 {
        match self {
            RepoOutcome::Ran(code) => *code,
            RepoOutcome::Skipped(_) => 2,
            RepoOutcome::NotAttempted => 0,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            RepoOutcome::Ran(0) => "ok".to_string(),
            RepoOutcome::Ran(code) => format!("failed (exit {code})"),
            RepoOutcome::Skipped(why) => format!("skipped — {why}"),
            RepoOutcome::NotAttempted => "not attempted".to_string(),
        }
    }
}

/// Enter `repo` and run `body` there, exactly as if the operator had `cd`-ed in.
///
/// The working directory is restored afterwards even when the body fails, so
/// one repo's failure cannot leave the next one resolving paths from the wrong
/// place.
///
/// Nothing here returns `Err`: one repo's broken config is that repo's answer,
/// not the fleet's. An error is printed the way `main` prints one and becomes
/// exit `2` for that repo, so a fleet of ten still reports on the other nine.
pub fn run_in(repo: &Repo, body: impl FnOnce() -> Result<i32>) -> RepoOutcome {
    if !repo.path.is_dir() {
        return RepoOutcome::Skipped(format!("{} is not a directory", repo.path.display()));
    }
    if crate::config::discover(&repo.path).is_err() {
        return RepoOutcome::Skipped(format!(
            "no {} in {}",
            crate::config::CONFIG_FILENAME,
            repo.path.display()
        ));
    }
    let previous = std::env::current_dir().ok();
    if std::env::set_current_dir(&repo.path).is_err() {
        return RepoOutcome::Skipped(format!("could not enter {}", repo.path.display()));
    }
    let result = body();
    if let Some(previous) = previous {
        let _ = std::env::set_current_dir(previous);
    }
    match result {
        Ok(code) => RepoOutcome::Ran(code),
        Err(err) => {
            crate::ui::fail(crate::secrets::redact::scrub_error(&err));
            RepoOutcome::Ran(2)
        }
    }
}
