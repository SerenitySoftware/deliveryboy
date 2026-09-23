//! What a deploy is about to ship — the commit range between what is live on
//! the target and the commit being deployed.
//!
//! The confirmation prompt used to ask "Deploy release v1.2.3 (abc123)?" with
//! no indication of what was in it. Every release-based deploy already records
//! its sha in the target's `.deliver/history.tsv`, and [`crate::readback`]
//! already knows how to read that back, so the answer to "what am I about to
//! push?" is one read and one `git log` away.
//!
//! Read-only like the rest of the read-back: nothing on the target is touched,
//! and an unreadable target or an unknown commit is reported as a line rather
//! than a failure — this informs the confirmation, it is not a gate in front of
//! it.

use crate::readback::Status;
use std::path::Path;
use std::process::Command;

/// How many commit lines to list before summarising the rest.
const SHOWN: usize = 20;

/// Where the commit being deployed stands relative to what is live.
#[derive(Debug, Clone, PartialEq)]
pub enum Range {
    /// The target could not be read, so the range is unknown.
    Unreadable,
    /// Nothing recorded on the target: a first deploy.
    FirstDeploy,
    /// Something is live, but its history row is missing or names no commit.
    Unrecorded,
    /// The live sha is not a commit this clone has.
    UnknownCommit { sha: String },
    /// `ahead` are the commits being shipped (oneline, newest first); `behind`
    /// counts commits that are live now and are not in what is being shipped.
    Known { ahead: Vec<String>, behind: usize },
}

/// One group of services that share a live commit, and so a range.
#[derive(Debug, Clone, PartialEq)]
pub struct Shipment {
    pub services: Vec<String>,
    /// The live release and full sha, when recorded.
    pub live: Option<(String, String)>,
    pub range: Range,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// The range between a live sha and `head` in the repo at `root`.
pub fn range(root: &Path, live_sha: &str, head: &str) -> Range {
    let commit = |sha: &str| format!("{sha}^{{commit}}");
    // A sha that is not hex is not a commit ("unknown" when the deploy ran
    // outside git) — and must never reach git as an option or a revision
    // expression.
    let hex = !live_sha.is_empty() && live_sha.chars().all(|c| c.is_ascii_hexdigit());
    if !hex
        || git(
            root,
            &["rev-parse", "--verify", "--quiet", &commit(live_sha)],
        )
        .is_none()
    {
        return Range::UnknownCommit {
            sha: live_sha.to_string(),
        };
    }
    let ahead = git(
        root,
        &[
            "log",
            "--oneline",
            "--no-decorate",
            "--no-show-signature",
            &format!("{live_sha}..{head}"),
        ],
    );
    let behind = git(
        root,
        &["rev-list", "--count", &format!("{head}..{live_sha}")],
    );
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Range::Known {
            ahead: ahead
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect(),
            behind: behind.trim().parse().unwrap_or(0),
        },
        _ => Range::UnknownCommit {
            sha: live_sha.to_string(),
        },
    }
}

/// Work out the range for every service read back, grouping services whose
/// live commit is the same so a range is listed once.
pub fn collect(root: &Path, statuses: &[Status], head: &str) -> Vec<Shipment> {
    let mut out: Vec<Shipment> = Vec::new();
    for status in statuses {
        let live = status
            .live_deploy()
            .map(|d| (d.release.clone(), d.sha.clone()));
        let range = if !status.reachable {
            Range::Unreadable
        } else if status.live_deploy_id().is_none() {
            // `Unmanaged` lands here too: a plain directory predates the release
            // layout, so nothing about it was recorded.
            if status.history.is_empty() {
                Range::FirstDeploy
            } else {
                Range::Unrecorded
            }
        } else {
            match &live {
                Some((_, sha)) if !sha.is_empty() => range(root, sha, head),
                _ => Range::Unrecorded,
            }
        };
        match out.iter_mut().find(|s| s.live == live && s.range == range) {
            Some(group) => group.services.push(status.service.clone()),
            None => out.push(Shipment {
                services: vec![status.service.clone()],
                live,
                range,
            }),
        }
    }
    out
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn live_label(live: &Option<(String, String)>) -> String {
    match live {
        Some((release, sha)) if !sha.is_empty() => format!("{release} ({})", short(sha)),
        Some((release, _)) => release.clone(),
        None => "an unrecorded release".into(),
    }
}

/// The lines printed under the "Shipping" phase.
pub fn render(shipments: &[Shipment], dirty: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for s in shipments {
        let who = s.services.join(", ");
        let live = live_label(&s.live);
        match &s.range {
            Range::Unreadable => lines.push(format!(
                "{who}: could not read the target — what is live, and so what ships, is unknown"
            )),
            Range::FirstDeploy => lines.push(format!(
                "{who}: nothing recorded on the target — this is its first deploy"
            )),
            Range::Unrecorded => lines.push(format!(
                "{who}: what is live was not recorded with a commit — the range is unknown"
            )),
            Range::UnknownCommit { sha } => lines.push(format!(
                "{who}: live {live} — {} is not a commit in this clone; \
                 `git fetch` and re-run to see the range",
                short(sha)
            )),
            Range::Known { ahead, behind: 0 } if ahead.is_empty() => lines.push(format!(
                "{who}: live {live} is this commit — nothing new ships, this re-deploys it"
            )),
            Range::Known { ahead, behind } => {
                if ahead.is_empty() {
                    lines.push(format!(
                        "{who}: live {live} is {behind} commit(s) ahead of this one — \
                         this deploy moves the target backwards"
                    ));
                } else if *behind == 0 {
                    lines.push(format!(
                        "{who}: shipping {} commit(s) since live {live}",
                        ahead.len()
                    ));
                } else {
                    lines.push(format!(
                        "{who}: live {live} is not an ancestor of this commit — shipping {} \
                         commit(s) and dropping {behind} that are live now",
                        ahead.len()
                    ));
                }
                for commit in ahead.iter().take(SHOWN) {
                    lines.push(format!("  {commit}"));
                }
                if ahead.len() > SHOWN {
                    lines.push(format!("  … and {} more", ahead.len() - SHOWN));
                }
            }
        }
    }
    if dirty && !shipments.is_empty() {
        lines.push("plus uncommitted changes in the working tree".into());
    }
    lines
}

/// A short clause for the confirmation prompt, when one range covers every
/// service — "shipping 7 commits since live v0.2.0 (def4567)". With several
/// ranges the prompt defers to the lines printed above it.
pub fn summary(shipments: &[Shipment]) -> Option<String> {
    let [only] = shipments else {
        return (shipments.len() > 1).then(|| "see the ranges above".into());
    };
    let live = live_label(&only.live);
    match &only.range {
        Range::Known { ahead, behind: 0 } if ahead.is_empty() => {
            Some(format!("re-deploying live {live}"))
        }
        Range::Known { ahead, behind: 0 } => Some(format!(
            "shipping {} commit(s) since live {live}",
            ahead.len()
        )),
        Range::Known { ahead, behind } if ahead.is_empty() => {
            Some(format!("moving back {behind} commit(s) from live {live}"))
        }
        Range::Known { ahead, behind } => Some(format!(
            "shipping {} commit(s), dropping {behind} from live {live}",
            ahead.len()
        )),
        Range::FirstDeploy => Some("first deploy".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipment(range: Range) -> Shipment {
        Shipment {
            services: vec!["web".into()],
            live: Some(("v0.2.0".into(), "bbb2222deadbeef".into())),
            range,
        }
    }

    fn commits(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:07x} change {i}")).collect()
    }

    #[test]
    fn a_forward_range_lists_the_commits_being_shipped() {
        let lines = render(
            &[shipment(Range::Known {
                ahead: commits(3),
                behind: 0,
            })],
            false,
        );
        assert_eq!(
            lines[0],
            "web: shipping 3 commit(s) since live v0.2.0 (bbb2222)"
        );
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[1], "  0000000 change 0");
    }

    #[test]
    fn a_long_range_is_capped_and_says_how_many_it_hid() {
        let lines = render(
            &[shipment(Range::Known {
                ahead: commits(25),
                behind: 0,
            })],
            false,
        );
        assert_eq!(lines.len(), 1 + SHOWN + 1, "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  … and 5 more");
    }

    #[test]
    fn moving_the_target_backwards_is_called_out() {
        let lines = render(
            &[shipment(Range::Known {
                ahead: Vec::new(),
                behind: 2,
            })],
            false,
        );
        assert!(
            lines[0].contains("2 commit(s) ahead of this one"),
            "{lines:?}"
        );
        assert!(lines[0].contains("moves the target backwards"), "{lines:?}");
    }

    #[test]
    fn a_diverged_target_says_what_is_dropped() {
        let s = shipment(Range::Known {
            ahead: commits(1),
            behind: 3,
        });
        let lines = render(std::slice::from_ref(&s), false);
        assert!(lines[0].contains("not an ancestor"), "{lines:?}");
        assert!(lines[0].contains("dropping 3"), "{lines:?}");
        assert_eq!(
            summary(&[s]).unwrap(),
            "shipping 1 commit(s), dropping 3 from live v0.2.0 (bbb2222)"
        );
    }

    #[test]
    fn a_redeploy_of_the_live_commit_says_nothing_new_ships() {
        let s = shipment(Range::Known {
            ahead: Vec::new(),
            behind: 0,
        });
        assert!(render(std::slice::from_ref(&s), false)[0].contains("nothing new ships"));
        assert_eq!(summary(&[s]).unwrap(), "re-deploying live v0.2.0 (bbb2222)");
    }

    #[test]
    fn unknown_ranges_are_reported_not_guessed() {
        let lines = render(
            &[
                shipment(Range::UnknownCommit {
                    sha: "bbb2222deadbeef".into(),
                }),
                Shipment {
                    services: vec!["api".into()],
                    live: None,
                    range: Range::Unreadable,
                },
            ],
            true,
        );
        assert!(lines[0].contains("`git fetch`"), "{lines:?}");
        assert!(lines[1].contains("could not read the target"), "{lines:?}");
        assert_eq!(lines[2], "plus uncommitted changes in the working tree");
    }

    #[test]
    fn several_ranges_defer_the_prompt_to_the_lines_above() {
        let a = shipment(Range::FirstDeploy);
        let b = shipment(Range::Unreadable);
        assert_eq!(summary(&[a, b]).unwrap(), "see the ranges above");
        assert_eq!(summary(&[shipment(Range::Unreadable)]), None);
        assert_eq!(summary(&[]), None);
    }

    #[test]
    fn a_non_hex_sha_never_reaches_git() {
        let dir = std::env::temp_dir();
        assert_eq!(
            range(&dir, "--output=/tmp/x", "HEAD"),
            Range::UnknownCommit {
                sha: "--output=/tmp/x".into()
            }
        );
        assert_eq!(
            range(&dir, "unknown", "HEAD"),
            Range::UnknownCommit {
                sha: "unknown".into()
            }
        );
    }
}
