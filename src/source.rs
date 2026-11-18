//! A release is built from its commit, not from the folder `deliver` runs in.
//!
//! `deliver deploy --version 1.2.3` ships exactly the commit the release's tag
//! names — or HEAD's commit, which it tags once the release is live. The
//! checkout it was run from may have uncommitted or untracked files, sit on
//! another branch, or have a detached HEAD: none of that reaches the target,
//! because every step runs in a clean copy of that one commit, made with
//! `git worktree add --detach` in Delivery Boy's own scratch directory and
//! removed when the run ends.
//!
//! A worktree rather than `git archive`: it is a real checkout of the commit,
//! so steps that ask git about it (`git rev-parse HEAD`, `git describe`,
//! `cargo publish`) get the release's answers, and it shares the checkout's
//! tags, so an `after_tag` step sees the tag the release just made.
//!
//! What the commit does not hold is read from the checkout instead: secrets
//! providers such as a gitignored `.env.deploy`, a relative ssh key, and git
//! itself (tags are made there).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where `root` sits inside its git repository, as git spells it: `""` at the
/// top, `"services/api/"` below it. `None` when `root` is not in a work tree.
pub fn repo_prefix(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-prefix"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A clean copy of one commit, checked out for this run only.
///
/// Dropping it puts the process back in the directory it was entered from and
/// removes the copy and its worktree registration.
pub struct Export {
    /// The checkout the worktree belongs to.
    checkout: PathBuf,
    /// The worktree itself: the repository's top level at the commit.
    dir: PathBuf,
    /// The checkout root's counterpart in the copy, where the run works.
    pub root: PathBuf,
    entered_from: Option<PathBuf>,
}

impl Export {
    /// Check out `commit` at `dir`. `checkout` is the directory holding the
    /// config; its place in the repository is kept, so a config in a
    /// subdirectory works from the same subdirectory of the copy.
    pub fn create(checkout: &Path, commit: &str, dir: &Path) -> Result<Self> {
        let prefix = repo_prefix(checkout)
            .with_context(|| format!("{} is not in a git work tree", checkout.display()))?;
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("could not clear {}", dir.display()))?;
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(checkout)
            // The committed bytes, not this machine's rendering of them: with
            // `core.autocrlf` (Windows' default) a checkout turns LF into CRLF,
            // and a shell script shipped that way to a Linux target breaks. A
            // `.gitattributes` `eol` rule is the repo's own, and still applies.
            .args(["-c", "core.autocrlf=false"])
            .args(["worktree", "add", "--detach", "--quiet"])
            .arg(dir)
            .arg(commit)
            .stdin(Stdio::null())
            .output()
            .context("could not run git worktree add")?;
        if !out.status.success() {
            bail!(
                "could not check out {} for the release: {}",
                &commit[..7.min(commit.len())],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Self {
            checkout: checkout.to_path_buf(),
            dir: dir.to_path_buf(),
            root: if prefix.is_empty() {
                dir.to_path_buf()
            } else {
                dir.join(prefix)
            },
            entered_from: None,
        })
    }

    /// Make the copy the process's working directory. Every local step runs
    /// with relative paths (`docker build … src`, `./scripts/release.sh`), so
    /// this is what points them at the commit instead of the checkout.
    pub fn enter(&mut self) -> Result<()> {
        let here = std::env::current_dir().ok();
        std::env::set_current_dir(&self.root)
            .with_context(|| format!("could not enter {}", self.root.display()))?;
        self.entered_from = here;
        Ok(())
    }
}

impl Drop for Export {
    fn drop(&mut self) {
        if let Some(previous) = self.entered_from.take() {
            let _ = std::env::set_current_dir(previous);
        }
        let removed = Command::new("git")
            .arg("-C")
            .arg(&self.checkout)
            .args(["worktree", "remove", "--force"])
            .arg(&self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !removed {
            // A step left something git will not remove (a read-only file, a
            // nested repository): take the directory, then the registration.
            let _ = std::fs::remove_dir_all(&self.dir);
            prune(&self.checkout);
        }
    }
}

/// Forget worktrees whose directories are gone — the copies of runs that were
/// killed before they could clean up after themselves.
pub fn prune(checkout: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["worktree", "prune"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
