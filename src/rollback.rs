//! `deliver rollback --to <deploy-id>` — go back to any *retained* release.
//!
//! Plain `deliver rollback` repoints the live symlink at `.deliver-previous`,
//! which is one step back and no further ([`crate::exec::rollback`]). But the
//! `files`/`hugo` layout keeps `keep_releases` (default 5) prior releases on
//! disk, each directory named by its deploy id — so jumping past a release
//! that was *itself* bad is possible on the target and merely unaddressable
//! from the CLI. This module makes it addressable.
//!
//! The ids come from [`crate::readback`]: `deliver history` lists them, and
//! the same read validates one before anything is touched. That ordering is
//! the point — **resolve every selected service first, and refuse the whole
//! run if any one of them cannot be satisfied**, so a multi-service rollback
//! never half-lands. A deploy id that is not on the target, a deployer with no
//! release layout, or a host that would not answer are all caught while the
//! target is still untouched.
//!
//! Paths are never derived here. The release directory, the live symlink and
//! the previous-release marker all come from the
//! [`ReleaseState`](crate::deployers::ReleaseState) the deployer attached at
//! compile time, for the same reason the read-back does it that way: the swap
//! has to land exactly where the deploy wrote.

use crate::config::Target;
use crate::readback::{Live, Status};
use crate::remote::shell_quote;
use crate::secrets::redact::scrub;
use std::collections::BTreeMap;

/// What one selected service will do for a `--to <deploy-id>` run.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Ready to swap. Nothing has been run yet.
    Ready(Swap),
    /// That deploy id is already what the live symlink points at.
    AlreadyLive { service: String, deploy_id: String },
    /// This service cannot be rolled back to that id, and why.
    Refused(Refusal),
}

/// A validated swap for one service, and the command that performs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Swap {
    pub service: String,
    pub target: String,
    pub host: String,
    /// The deploy id that is live now, when one could be established.
    pub from: Option<String>,
    /// Its release label, for the line the operator reads.
    pub from_release: Option<String>,
    /// The deploy id being restored.
    pub to: String,
    pub to_release: Option<String>,
    /// The release directory the live symlink will point at.
    pub to_path: String,
    pub command: String,
}

/// Why one service was refused, and what that should do to the exit code.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub service: String,
    pub reason: String,
    /// `1` for a target that could not be read — the same code `deliver
    /// status` uses, because it is the same failed read. `2` for a request
    /// that was never valid for this config or this target.
    pub exit_code: i32,
}

/// A deploy id has to be one path segment: it is joined onto the releases
/// directory, and `deliver history` never prints anything else.
///
/// Checked before the retained set is consulted so `--to ../../etc` gets an
/// answer about its shape rather than a confusing "not retained".
pub fn id_is_addressable(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The release label recorded for a deploy id, when history knows it.
fn release_of(status: &Status, deploy_id: &str) -> Option<String> {
    status
        .history
        .iter()
        .find(|d| d.deploy_id == deploy_id)
        .map(|d| d.release.clone())
}

/// The shell that performs one swap on the target.
///
/// Deliberately the same shape as the activate step's own undo in
/// `deployers::files`: re-read what is live, point `.deliver-previous` at it,
/// then swap through a temporary name so `mv -T` replaces the symlink in a
/// single rename and a reader (nginx) never sees a missing path.
///
/// The `-d` re-check is not redundant with validation: the retained set was
/// read over a separate connection, and a prune or a concurrent deploy can
/// land in between. Checking on the target, inside the same shell that swaps,
/// is the only place the answer is still true.
fn swap_command(
    live: &str,
    to_path: &str,
    previous_marker: Option<&str>,
    service: &str,
    deploy_id: &str,
    sudo: &str,
) -> String {
    let record_previous = match previous_marker {
        Some(marker) => format!(
            "printf '%s\\n' \"$CUR\" | {sudo}tee {m} >/dev/null; ",
            m = shell_quote(marker)
        ),
        // No marker declared: swap anyway rather than refuse, but do not
        // invent a path to write to.
        None => String::new(),
    };
    format!(
        "set -e; \
         if [ ! -d {to} ]; then \
           echo 'release {id} is no longer on the target — refusing to swap' >&2; exit 1; fi; \
         CUR=''; \
         if [ -L {live} ]; then CUR=$({sudo}readlink -f {live} || true); fi; \
         {record_previous}\
         {sudo}ln -sfn {to} {live}.rb; \
         {sudo}mv -Tf {live}.rb {live}; \
         echo \"rolled back {svc} → {id}\"",
        to = shell_quote(to_path),
        live = shell_quote(live),
        id = deploy_id,
        svc = service,
    )
}

/// Resolve every read-back service against a requested deploy id, touching
/// nothing.
///
/// The caller runs the swaps only when no [`Resolution::Refused`] came back.
pub fn resolve(
    statuses: &[Status],
    targets: &BTreeMap<String, Target>,
    deploy_id: &str,
) -> Vec<Resolution> {
    statuses
        .iter()
        .map(|status| resolve_one(status, targets, deploy_id))
        .collect()
}

fn resolve_one(status: &Status, targets: &BTreeMap<String, Target>, deploy_id: &str) -> Resolution {
    let refuse = |reason: String, exit_code: i32| {
        Resolution::Refused(Refusal {
            service: status.service.clone(),
            reason,
            exit_code,
        })
    };

    if !status.reachable {
        return refuse("could not read the target".into(), 1);
    }
    // Compose has no symlink to repoint: its rollback is the `:rollback` image
    // tag the deploy leaves behind, which is one step back by construction.
    let (Some(live_path), Some(releases_dir)) = (&status.live_path, &status.releases_dir) else {
        return refuse(
            "this deployer keeps no release directories, so there is no deploy id to \
             point at — plain `deliver rollback` is the one step it supports"
                .into(),
            2,
        );
    };
    if !status.releases.iter().any(|r| r == deploy_id) {
        let retained = if status.releases.is_empty() {
            "nothing is retained on the target".to_string()
        } else {
            format!("retained: {}", status.releases.join(", "))
        };
        return refuse(
            format!("no release {deploy_id} on the target — {retained}"),
            2,
        );
    }
    if status.live_deploy_id().as_deref() == Some(deploy_id)
        && matches!(status.live, Live::Release { .. })
    {
        return Resolution::AlreadyLive {
            service: status.service.clone(),
            deploy_id: deploy_id.to_string(),
        };
    }

    let sudo = match targets.get(&status.target) {
        Some(target) if target.uses_sudo() => "sudo ",
        // A target missing from the map cannot happen for a status that was
        // read (the read resolves it too), but defaulting to no sudo keeps
        // this total rather than panicking.
        _ => "",
    };
    let to_path = format!("{}/{}", releases_dir.trim_end_matches('/'), deploy_id);
    let from = status.live_deploy_id();
    Resolution::Ready(Swap {
        service: status.service.clone(),
        target: status.target.clone(),
        host: status.host.clone(),
        from_release: from.as_deref().and_then(|id| release_of(status, id)),
        from,
        to_release: release_of(status, deploy_id),
        command: swap_command(
            live_path,
            &to_path,
            status.previous_marker.as_deref(),
            &status.service,
            deploy_id,
            sudo,
        ),
        to: deploy_id.to_string(),
        to_path,
    })
}

/// `web: v0.2.0 · 20260202-1000-bbb2222 → v0.1.0 · 20260101-0900-aaa1111`
pub fn describe(swap: &Swap) -> String {
    let label = |id: Option<&String>, release: Option<&String>| match (id, release) {
        (Some(id), Some(release)) => format!("{release} · {id}"),
        (Some(id), None) => format!("unknown release · {id}"),
        (None, _) => "nothing live".to_string(),
    };
    scrub(&format!(
        "{}: {} → {}",
        swap.service,
        label(swap.from.as_ref(), swap.from_release.as_ref()),
        label(Some(&swap.to), swap.to_release.as_ref()),
    ))
}

/// Where a plain `deliver rollback` takes one service, read off the target
/// before anything is touched, so the operator sees both ends first.
#[derive(Debug, Clone, PartialEq)]
pub struct StepBack {
    pub service: String,
    pub target: String,
    pub host: String,
    pub reachable: bool,
    pub from: Option<String>,
    pub from_release: Option<String>,
    /// The deploy id the undo restores, when the target records one.
    pub to: Option<String>,
    pub to_release: Option<String>,
    pub to_sha: Option<String>,
    pub history_path: String,
}

/// Work out what one step back means for a service that keeps a record.
///
/// With a live symlink, the previous-release marker the activate step wrote
/// is the answer — it is exactly what the undo swaps to. Compose has no
/// symlink: its undo re-tags the image that was running before the newest
/// *deploy*, so the restored release is the row below the newest row that
/// was not itself a rollback.
pub fn step_back(status: &Status) -> StepBack {
    let row = |id: &str| status.history.iter().find(|d| d.deploy_id == id);
    let from = status.live_deploy_id();
    let to = match &status.live {
        Live::NotApplicable => status
            .history
            .iter()
            .position(|d| !d.rollback)
            .and_then(|newest| status.history.get(newest + 1))
            .map(|d| d.deploy_id.clone()),
        _ => status
            .previous
            .as_deref()
            .map(|path| crate::readback::basename(path).to_string()),
    };
    StepBack {
        service: status.service.clone(),
        target: status.target.clone(),
        host: status.host.clone(),
        reachable: status.reachable,
        from_release: from.as_deref().and_then(|id| release_of(status, id)),
        from,
        to_release: to.as_deref().and_then(|id| release_of(status, id)),
        to_sha: to.as_deref().and_then(row).map(|d| d.sha.clone()),
        to,
        history_path: status.history_path.clone(),
    }
}

/// A `--to` swap seen as where it leaves the service, so it is recorded and
/// verified exactly as a one-step rollback is.
pub fn landed(swap: &Swap, status: &Status) -> StepBack {
    StepBack {
        service: swap.service.clone(),
        target: swap.target.clone(),
        host: swap.host.clone(),
        reachable: true,
        from: swap.from.clone(),
        from_release: swap.from_release.clone(),
        to: Some(swap.to.clone()),
        to_release: swap.to_release.clone(),
        to_sha: status
            .history
            .iter()
            .find(|d| d.deploy_id == swap.to)
            .map(|d| d.sha.clone()),
        history_path: status.history_path.clone(),
    }
}

/// `web: live 20260202-bbb2222 (v0.2.0) → back to 20260130-aaa1111 (v0.1.9)`
pub fn describe_step_back(back: &StepBack) -> String {
    let end = |id: Option<&String>, release: Option<&String>| match (id, release) {
        (Some(id), Some(release)) => format!("{id} ({release})"),
        (Some(id), None) => format!("{id} (unknown release)"),
        (None, _) => "nothing recorded".to_string(),
    };
    let line = if !back.reachable {
        format!(
            "{}: could not read the target — cannot say what is live or where this goes",
            back.service
        )
    } else if back.to.is_none() {
        format!(
            "{}: live {} → no previous release recorded on the target",
            back.service,
            end(back.from.as_ref(), back.from_release.as_ref())
        )
    } else {
        format!(
            "{}: live {} → back to {}",
            back.service,
            end(back.from.as_ref(), back.from_release.as_ref()),
            end(back.to.as_ref(), back.to_release.as_ref())
        )
    };
    scrub(&line)
}

/// The shell that appends a rollback row to the target's history: the
/// restored deploy id with its release and sha, and a sixth `rollback` field
/// so `deliver history` can tell it from a re-deploy. Same numbering as the
/// deployers' own record step.
pub fn record_command(
    history_path: &str,
    deploy_id: &str,
    release: &str,
    sha: &str,
    sudo: &str,
) -> String {
    let dir = history_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .filter(|dir| !dir.is_empty())
        .unwrap_or(".");
    format!(
        "set -e; {sudo}mkdir -p {dir}; \
         N=1; if {sudo}test -f {h}; then \
           N=$(( $({sudo}wc -l {h} | awk '{{print $1}}') + 1 )); fi; \
         printf '%s\\t%s\\t%s\\t%s\\t%s\\trollback\\n' \"$N\" {id} {release} {sha} \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" \
           | {sudo}tee -a {h} >/dev/null; \
         echo \"rollback #$N · \"{id}",
        dir = shell_quote(dir),
        h = shell_quote(history_path),
        id = shell_quote(deploy_id),
        release = shell_quote(release),
        sha = shell_quote(sha),
    )
}

/// Shell that records an automatic unwind in `history.tsv`.
///
/// Appended to an undo after it has moved the target back. Without it the
/// failed deploy stays the newest row, so `deliver status` reads a release
/// that was just rolled back as live and `deliver rollback` offers to go
/// "back" to what is already running. The row is the one [`record_command`]
/// writes; it lands only when this run's own deploy row (`stamp`) exists, so a
/// failure before the record step adds nothing. `id_expr` is a shell command
/// printing the restored deploy id; release and sha come from that id's own
/// row, `unknown` when it has none.
pub fn unwind_record_command(history_path: &str, stamp: &str, id_expr: &str, sudo: &str) -> String {
    format!(
        "H={h}; \
         if {sudo}test -f \"$H\" && [ -n \"$({sudo}awk -F'\\t' -v id={stamp} '$2==id{{print 1; exit}}' \"$H\")\" ]; then \
           ID=$({id_expr}); \
           if [ -n \"$ID\" ]; then \
             REL=$({sudo}awk -F'\\t' -v id=\"$ID\" '$2==id{{r=$3}} END{{print (r==\"\" ? \"unknown\" : r)}}' \"$H\"); \
             SHA=$({sudo}awk -F'\\t' -v id=\"$ID\" '$2==id{{r=$4}} END{{print (r==\"\" ? \"unknown\" : r)}}' \"$H\"); \
             N=$(( $({sudo}wc -l \"$H\" | awk '{{print $1}}') + 1 )); \
             printf '%s\\t%s\\t%s\\t%s\\t%s\\trollback\\n' \"$N\" \"$ID\" \"$REL\" \"$SHA\" \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" \
               | {sudo}tee -a \"$H\" >/dev/null; \
             echo \"recorded rollback #$N · $ID\"; \
           fi; \
         fi",
        h = shell_quote(history_path),
        stamp = shell_quote(stamp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readback::Deploy;

    fn deploy(number: &str, id: &str, release: &str) -> Deploy {
        Deploy {
            number: number.into(),
            deploy_id: id.into(),
            release: release.into(),
            sha: "abc1234def5678".into(),
            at: "2026-02-02T10:00:00Z".into(),
            rollback: false,
        }
    }

    fn status() -> Status {
        Status {
            service: "web".into(),
            target: "box".into(),
            host: "localhost".into(),
            reachable: true,
            live: Live::Release {
                path: "/var/app/releases/20260202-bbb".into(),
            },
            releases: vec!["20260202-bbb".into(), "20260101-aaa".into()],
            history: vec![
                deploy("2", "20260202-bbb", "v0.2.0"),
                deploy("1", "20260101-aaa", "v0.1.0"),
            ],
            history_path: "/var/app/.deliver/history.tsv".into(),
            live_path: Some("/var/app/web".into()),
            releases_dir: Some("/var/app/releases".into()),
            previous_marker: Some("/var/app/releases/.deliver-previous".into()),
            backups: None,
            backups_dir: None,
            previous: None,
        }
    }

    fn no_targets() -> BTreeMap<String, Target> {
        BTreeMap::new()
    }

    #[test]
    fn a_retained_release_resolves_to_a_swap_naming_both_ends() {
        let resolved = resolve(&[status()], &no_targets(), "20260101-aaa");
        let Resolution::Ready(swap) = &resolved[0] else {
            panic!("expected a swap, got {:?}", resolved[0]);
        };
        assert_eq!(swap.to_path, "/var/app/releases/20260101-aaa");
        assert_eq!(swap.from.as_deref(), Some("20260202-bbb"));
        assert_eq!(swap.from_release.as_deref(), Some("v0.2.0"));
        assert_eq!(swap.to_release.as_deref(), Some("v0.1.0"));
        assert_eq!(
            describe(swap),
            "web: v0.2.0 · 20260202-bbb → v0.1.0 · 20260101-aaa"
        );
    }

    #[test]
    fn the_swap_rewrites_the_previous_marker_the_deployer_declared() {
        let resolved = resolve(&[status()], &no_targets(), "20260101-aaa");
        let Resolution::Ready(swap) = &resolved[0] else {
            panic!("expected a swap");
        };
        // The marker must name what was live *before this swap*, or the next
        // plain `deliver rollback` walks back to a stale release.
        assert!(
            swap.command
                .contains("'/var/app/releases/.deliver-previous'"),
            "{}",
            swap.command
        );
        assert!(swap.command.contains("readlink -f"), "{}", swap.command);
        // Atomic: through a temporary name, replaced with a single rename.
        assert!(swap.command.contains("mv -Tf"), "{}", swap.command);
        // And it re-checks the directory it was told about.
        assert!(
            swap.command
                .contains("if [ ! -d '/var/app/releases/20260101-aaa' ]"),
            "{}",
            swap.command
        );
    }

    #[test]
    fn an_id_that_is_not_retained_is_refused_and_lists_what_is() {
        let resolved = resolve(&[status()], &no_targets(), "20251212-zzz");
        let Resolution::Refused(refusal) = &resolved[0] else {
            panic!("expected a refusal, got {:?}", resolved[0]);
        };
        assert_eq!(refusal.exit_code, 2);
        assert!(refusal.reason.contains("20260202-bbb"), "{refusal:?}");
        assert!(refusal.reason.contains("20260101-aaa"), "{refusal:?}");
    }

    #[test]
    fn the_release_that_is_already_live_is_a_no_op_not_a_swap() {
        let resolved = resolve(&[status()], &no_targets(), "20260202-bbb");
        assert!(matches!(resolved[0], Resolution::AlreadyLive { .. }));
    }

    #[test]
    fn a_deployer_with_no_release_layout_is_refused_rather_than_guessed_at() {
        let mut compose = status();
        compose.live = Live::NotApplicable;
        compose.live_path = None;
        compose.releases_dir = None;
        compose.previous_marker = None;
        compose.releases.clear();
        let resolved = resolve(&[compose], &no_targets(), "20260101-aaa");
        let Resolution::Refused(refusal) = &resolved[0] else {
            panic!("expected a refusal, got {:?}", resolved[0]);
        };
        assert_eq!(refusal.exit_code, 2);
        assert!(refusal.reason.contains("deliver rollback"), "{refusal:?}");
    }

    #[test]
    fn an_unreadable_target_is_a_failed_read_not_an_invalid_request() {
        let mut unreachable = status();
        unreachable.reachable = false;
        let resolved = resolve(&[unreachable], &no_targets(), "20260101-aaa");
        let Resolution::Refused(refusal) = &resolved[0] else {
            panic!("expected a refusal");
        };
        // Exit 1, matching `deliver status` — the target was never read, so
        // nothing here says the request was wrong.
        assert_eq!(refusal.exit_code, 1);
    }

    #[test]
    fn a_retained_release_with_no_history_row_still_resolves() {
        let mut orphan = status();
        orphan.history.retain(|d| d.deploy_id != "20260101-aaa");
        let resolved = resolve(&[orphan], &no_targets(), "20260101-aaa");
        let Resolution::Ready(swap) = &resolved[0] else {
            panic!("expected a swap, got {:?}", resolved[0]);
        };
        assert_eq!(swap.to_release, None);
        assert!(describe(swap).contains("unknown release"), "{swap:?}");
    }

    #[test]
    fn only_a_single_path_segment_is_addressable() {
        assert!(id_is_addressable("20260101-0900-aaa1111"));
        assert!(id_is_addressable("premigrate-20260101-0900"));
        assert!(!id_is_addressable("../../etc"));
        assert!(!id_is_addressable("a/b"));
        assert!(!id_is_addressable(".."));
        assert!(!id_is_addressable(""));
        assert!(!id_is_addressable("a b"));
        assert!(!id_is_addressable("$(whoami)"));
    }

    #[test]
    fn one_step_back_on_a_symlink_goes_where_the_marker_points() {
        let mut files = status();
        files.previous = Some("/var/app/releases/20260101-aaa".into());
        let back = step_back(&files);
        assert_eq!(back.from.as_deref(), Some("20260202-bbb"));
        assert_eq!(back.to.as_deref(), Some("20260101-aaa"));
        assert_eq!(back.to_release.as_deref(), Some("v0.1.0"));
        assert_eq!(
            describe_step_back(&back),
            "web: live 20260202-bbb (v0.2.0) → back to 20260101-aaa (v0.1.0)"
        );
    }

    #[test]
    fn no_marker_says_so_instead_of_guessing() {
        let back = step_back(&status());
        assert_eq!(back.to, None);
        assert!(describe_step_back(&back).contains("no previous release recorded"));
    }

    #[test]
    fn compose_steps_back_past_its_own_rollback_rows() {
        // The `:rollback` image is whatever ran before the newest *deploy*, so
        // a second rollback lands on the same release as the first.
        let mut compose = status();
        compose.live = Live::NotApplicable;
        let mut again = deploy("3", "20260101-aaa", "v0.1.0");
        again.rollback = true;
        compose.history.insert(0, again);
        let back = step_back(&compose);
        assert_eq!(back.from.as_deref(), Some("20260101-aaa"));
        assert_eq!(back.to.as_deref(), Some("20260101-aaa"));

        let mut fresh = status();
        fresh.live = Live::NotApplicable;
        assert_eq!(step_back(&fresh).to.as_deref(), Some("20260101-aaa"));
    }

    #[test]
    fn the_history_row_quotes_what_it_read_off_the_target() {
        let cmd = record_command(
            "/var/app/.deliver/history.tsv",
            "20260101-aaa",
            "v0.1.0; rm -rf /",
            "abc",
            "",
        );
        assert!(cmd.contains("'v0.1.0; rm -rf /'"), "{cmd}");
        assert!(cmd.contains("\\trollback\\n"), "{cmd}");
    }
}
