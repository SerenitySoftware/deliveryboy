//! An advisory deploy lock, held on the target for the length of a release.
//!
//! The whole release model is atomic symlink swaps into `releases/<stamp>` on a
//! *single shared host*, and nothing serialized two deploys against it. A CI
//! release and a hand-run `deliver deploy`, two operators, or a `rollback` fired
//! while a deploy is mid-swap do not produce a clean loser: they interleave
//! symlink swaps, restarts and health checks, and the box can end up running one
//! release's artifact behind another's config — with `verify:` passing against
//! whichever won the race.
//!
//! Three properties this module is built around:
//!
//! * **`mkdir` is the lock.** Creating a directory is atomic on every
//!   filesystem the CLI targets and needs no tool the host might not have, which
//!   matters for the same reason the rest of the CLI shells out to `sh`: the
//!   target is someone else's box.
//! * **It refuses only on positive evidence.** A lock we could not take because
//!   ssh died or the parent is unwritable is *not* proof that someone else is
//!   deploying, and turning an unrelated permission problem into a failed
//!   release would be a worse bug than the one this closes. Only a lock that is
//!   demonstrably held by a live run stops a deploy; everything else prints a
//!   line and carries on, the same discipline [`crate::configdiff`] follows.
//! * **It is released on every exit path.** [`Guard`] releases in `Drop`, so the
//!   `?` paths, the abort paths and the rollback unwind all give the lock back
//!   without each caller remembering to. The one path `Drop` cannot cover is a
//!   Ctrl-C, which kills the process outright — that is why a lock also goes
//!   stale (see [`Target::lock_stale_after`]), and it is the reason the stale
//!   window exists rather than a nicety.
//! * **A live run keeps its lock fresh, and only releases its own.** Staleness
//!   is the age of `started_epoch`, so a release that simply runs longer than
//!   the window — a notarization wait, a slow `docker build`, a long `soak:` —
//!   would be "abandoned" by definition while it was still live. [`Guard::refresh`]
//!   rewrites that stamp before every step and every soak round, and
//!   [`Guard::release`] gives the lock back only while the owner file still
//!   names this run's deploy id: a lock that was taken over anyway belongs to
//!   the run that took it, and removing it would leave *that* run unlocked.
//!
//! The lock lives *beside* the target directory, at `<target.dir>.deliver-lock`,
//! not inside it. For a `files` service that serves the release root, `dir` is
//! itself the live symlink the deploy is about to swap — a lock underneath it
//! would be carried away mid-release and never released. The sibling suffix is
//! the same convention `files.rs` already uses for `<root>.releases`.

use crate::config::Target;
use crate::plan::ServicePlan;
use crate::remote::{capture, nonce, shell_quote};
use std::collections::BTreeMap;

/// Who is asking for the lock — recorded on the target so the next run can name
/// them rather than printing "locked" and leaving the operator to guess.
#[derive(Debug, Clone)]
pub struct Identity {
    /// `user@host` of the machine running `deliver`, not of the target.
    pub owner: String,
    pub pid: u32,
    pub services: String,
    pub release: String,
    pub deploy: String,
}

impl Identity {
    pub fn new(plan: &[ServicePlan], version: &crate::version::DeployVersion) -> Self {
        let mut services: Vec<&str> = plan.iter().map(|sp| sp.service.as_str()).collect();
        services.dedup();
        Self {
            owner: format!("{}@{}", local_user(), local_host()),
            pid: std::process::id(),
            services: services.join(","),
            release: version.release_display(),
            deploy: version.id.clone(),
        }
    }
}

fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn local_host() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// What the owner file on the target said about the run holding the lock.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Holder {
    pub owner: String,
    pub pid: String,
    pub services: String,
    pub release: String,
    pub deploy: String,
    pub started_at: String,
    /// How long the lock has been held, measured on the *target's* clock at both
    /// ends so a laptop running fast cannot make a live lock look abandoned.
    pub age: Option<u64>,
}

impl Holder {
    /// Parse the `key=value` owner file. Unknown keys are ignored so an older
    /// `deliver` holding the lock still reports something legible.
    fn parse(text: &str) -> Self {
        let mut h = Holder::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "owner" => h.owner = value,
                "pid" => h.pid = value,
                "services" => h.services = value,
                "release" => h.release = value,
                "deploy" => h.deploy = value,
                "started_at" => h.started_at = value,
                _ => {}
            }
        }
        h
    }

    fn describe_owner(&self) -> String {
        match (self.owner.as_str(), self.pid.as_str()) {
            ("", "") => "an unidentified run".into(),
            ("", pid) => format!("pid {pid}"),
            (owner, "") => owner.into(),
            (owner, pid) => format!("{owner} (pid {pid})"),
        }
    }
}

/// A lock this run could not take, and everything known about who has it.
#[derive(Debug, Clone)]
pub struct Blocked {
    pub target: String,
    pub host: String,
    pub dir: String,
    pub holder: Holder,
}

/// Roughly how long ago, for a line a human reads at 2 a.m.
fn human_age(secs: u64) -> String {
    match secs {
        s if s < 90 => format!("{s}s ago"),
        s if s < 5400 => format!("{}m ago", s / 60),
        s => format!("{}h{:02}m ago", s / 3600, (s % 3600) / 60),
    }
}

impl Blocked {
    /// The legible refusal: who, what, and since when.
    pub fn render(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "{} [{}]:{} — a deploy is already running",
            self.target, self.host, self.dir
        )];
        lines.push(format!("holder:   {}", self.holder.describe_owner()));
        if !self.holder.services.is_empty() {
            lines.push(format!(
                "services: {}",
                self.holder.services.replace(',', ", ")
            ));
        }
        if !self.holder.release.is_empty() {
            let release = match self.holder.deploy.as_str() {
                "" => self.holder.release.clone(),
                id => format!("{} ({id})", self.holder.release),
            };
            lines.push(format!("release:  {release}"));
        }
        let started = match (self.holder.started_at.as_str(), self.holder.age) {
            ("", None) => None,
            ("", Some(age)) => Some(human_age(age)),
            (at, None) => Some(at.to_string()),
            (at, Some(age)) => Some(format!("{at} ({})", human_age(age))),
        };
        if let Some(started) = started {
            lines.push(format!("started:  {started}"));
        }
        lines
    }
}

/// One lock this run holds.
struct Held {
    target: Target,
    host: String,
    dir: String,
    /// When this run last wrote `started_epoch` — at acquire, then at each
    /// refresh — so refreshes can be throttled without a second clock read.
    refreshed: std::cell::Cell<std::time::Instant>,
    /// The lock was found held by another run. Said once, not every step.
    lost: std::cell::Cell<bool>,
}

/// The locks this run holds. Releasing is the whole point of the type: it
/// happens in `Drop`, so no caller has to remember it on its error path.
pub struct Guard {
    held: Vec<Held>,
    /// This run's deploy id, which the owner file records: a lock is only
    /// refreshed or released while the file still names it.
    deploy: String,
    released: bool,
}

/// What the target said when asked to refresh or release a lock.
#[derive(Debug, PartialEq)]
enum Ownership {
    /// The owner file named this run; the request was carried out.
    Ours,
    /// Another run's deploy id is in the owner file; nothing was touched.
    Other(String),
    /// There is no lock directory any more.
    Gone,
    /// The target gave no usable answer.
    Unknown,
}

fn parse_ownership(output: &str, marker: &str) -> Ownership {
    for line in output.lines() {
        let Some(rest) = line.strip_prefix(marker) else {
            continue;
        };
        let rest = rest.trim();
        if rest == "RELEASED" || rest.starts_with("REFRESHED") {
            return Ownership::Ours;
        }
        if rest == "GONE" {
            return Ownership::Gone;
        }
        if let Some(id) = rest.strip_prefix("OTHER") {
            return Ownership::Other(id.trim().to_string());
        }
    }
    Ownership::Unknown
}

/// Rewrite `started_epoch` in the owner file, if the file still names `deploy`.
/// Read whole, rewritten whole: a concurrent acquire that reads mid-write sees
/// either the old stamp, the new one, or none — and none is "do not take over".
/// A lock directory with no owner file names nobody; it is left alone rather
/// than reported as another run's or given a stamp with no owner.
fn refresh_script(dir: &str, deploy: &str, sudo: &str, marker: &str) -> String {
    format!(
        "L={l}; N={n}; D={d}\n\
         if [ ! -d \"$L\" ]; then printf '%s GONE\\n' \"$N\"; exit 0; fi\n\
         HELD=$({sudo}cat \"$L/owner\" 2>/dev/null || true)\n\
         OWNED=$(printf '%s\\n' \"$HELD\" | sed -n 's/^deploy=//p' | head -1)\n\
         if [ -z \"$OWNED\" ]; then printf '%s REFRESHED none\\n' \"$N\"; exit 0; fi\n\
         if [ \"$OWNED\" != \"$D\" ]; then printf '%s OTHER %s\\n' \"$N\" \"$OWNED\"; exit 0; fi\n\
         NOW=$(date -u +%s)\n\
         printf '%s\\n' \"$HELD\" | awk -v now=\"$NOW\" \
           '/^started_epoch=/{{print \"started_epoch=\" now; seen=1; next}} {{print}} \
            END{{if (!seen) print \"started_epoch=\" now}}' \
           | {sudo}tee \"$L/owner\" >/dev/null\n\
         printf '%s REFRESHED %s\\n' \"$N\" \"$NOW\"\n",
        l = shell_quote(dir),
        n = shell_quote(marker),
        d = shell_quote(deploy),
    )
}

/// Remove the lock, if the owner file still names `deploy` (or names nobody —
/// a lock with no owner file is a torn write of ours, not someone else's).
/// `rmdir`, never `rm -rf`: a path built from config should not be able to
/// take a tree with it, and a lock dir holding anything but our own owner file
/// is not ours to delete.
fn release_script(dir: &str, deploy: &str, sudo: &str, marker: &str) -> String {
    format!(
        "L={l}; N={n}; D={d}\n\
         if [ ! -d \"$L\" ]; then printf '%s GONE\\n' \"$N\"; exit 0; fi\n\
         OWNED=$({sudo}sed -n 's/^deploy=//p' \"$L/owner\" 2>/dev/null | head -1)\n\
         if [ -n \"$OWNED\" ] && [ \"$OWNED\" != \"$D\" ]; then \
           printf '%s OTHER %s\\n' \"$N\" \"$OWNED\"; exit 0; fi\n\
         {sudo}rm -f \"$L/owner\" 2>/dev/null; {sudo}rmdir \"$L\" 2>/dev/null\n\
         if [ ! -d \"$L\" ]; then printf '%s RELEASED\\n' \"$N\"; fi\n",
        l = shell_quote(dir),
        n = shell_quote(marker),
        d = shell_quote(deploy),
    )
}

/// The line printed when a lock turned out to belong to someone else.
fn taken_over_note(host: &str, dir: &str, by: &Ownership) -> String {
    let who = match by {
        Ownership::Other(id) if !id.is_empty() => format!("another run (deploy {id})"),
        Ownership::Other(_) => "another run".to_string(),
        _ => "nobody — it is gone".to_string(),
    };
    format!(
        "the deploy lock at {host}:{dir} is now held by {who}: it was taken over as \
         abandoned while this release was still running. Left in place. Raise \
         `lock.stale_after` on the target if releases run this long."
    )
}

impl Guard {
    /// A guard that holds nothing, for the paths that mutate nothing.
    pub fn empty() -> Self {
        Self {
            held: Vec::new(),
            deploy: String::new(),
            released: true,
        }
    }

    /// Rewrite `started_epoch` on every lock this run holds, so a release that
    /// outlives `lock.stale_after` is not taken over while it is live. Called
    /// before every step and every soak round; best-effort, so a refresh that
    /// cannot be made prints nothing and the lock is exactly as stale as it
    /// was. Throttled per lock to the smaller of ten seconds and a quarter of
    /// the stale window — a plan is mostly sub-second steps and each refresh is
    /// a round trip to the target. A single step longer than the window can
    /// still be taken over: nothing runs between a step's start and its end.
    pub fn refresh(&self) {
        if self.released {
            return;
        }
        for held in &self.held {
            let stale_after = held.target.lock_stale_after();
            if stale_after == 0 {
                continue; // never taken over, so there is nothing to keep fresh
            }
            let throttle = std::time::Duration::from_millis((stale_after * 1000 / 4).min(10_000));
            if held.refreshed.get().elapsed() < throttle {
                continue;
            }
            held.refreshed.set(std::time::Instant::now());
            let marker = nonce();
            let script = refresh_script(&held.dir, &self.deploy, sudo_for(&held.target), &marker);
            let answer = match capture(&held.target, &held.host, &script) {
                Ok(out) => parse_ownership(&out, &marker),
                Err(_) => Ownership::Unknown,
            };
            match answer {
                Ownership::Ours | Ownership::Unknown => {}
                other => {
                    if !held.lost.replace(true) {
                        crate::ui::note(taken_over_note(&held.host, &held.dir, &other));
                    }
                }
            }
        }
    }

    /// Give every lock back. Idempotent, and called for you by `Drop`.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        for held in std::mem::take(&mut self.held) {
            let Held {
                target,
                host,
                dir,
                lost,
                ..
            } = held;
            let marker = nonce();
            let script = release_script(&dir, &self.deploy, sudo_for(&target), &marker);
            let answer = match capture(&target, &host, &script) {
                Ok(out) => parse_ownership(&out, &marker),
                Err(_) => Ownership::Unknown,
            };
            match answer {
                Ownership::Ours => {}
                Ownership::Unknown => crate::ui::note(format!(
                    "could not release the deploy lock at {host}:{dir} — \
                     remove it by hand, or wait for it to go stale."
                )),
                // Not ours any more. Say so once: the refresh may already have.
                other => {
                    if !lost.get() {
                        crate::ui::note(taken_over_note(&host, &dir, &other));
                    }
                }
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.release();
    }
}

/// The outcome of trying to lock every target the plan touches.
pub enum Taken {
    /// Every target is ours for as long as the guard lives.
    Held(Guard),
    /// Someone else has one of them. Anything already taken has been given back.
    Busy(Box<Blocked>),
    /// The lock could not be created on a target at all. Every mutating step
    /// runs under the same prefix and would fail the same way one step later,
    /// so this aborts rather than continuing as `Unknown` does.
    Failed(Box<Unlockable>),
}

/// A target where the lock directory could not be created.
#[derive(Debug, Clone, PartialEq)]
pub struct Unlockable {
    pub target: String,
    pub host: String,
    pub dir: String,
    pub reason: String,
    pub sudo: bool,
}

impl Unlockable {
    /// What failed, and — when `sudo` is the likely cause — what to change.
    pub fn render(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "could not create the deploy lock on {} [{}]:{} — {}",
            self.target, self.host, self.dir, self.reason
        )];
        if self.sudo {
            lines.push(
                "every step runs under sudo on this target: set `sudo: false` on it for a \
                 root login, or install sudo and allow it without a password"
                    .to_string(),
            );
        }
        lines
    }
}

fn sudo_for(target: &Target) -> &'static str {
    if target.uses_sudo() {
        "sudo "
    } else {
        ""
    }
}

/// One acquire attempt, as the target answered it.
enum Answer {
    Acquired {
        took_over: bool,
    },
    Busy(Holder),
    /// The lock directory could not be created and does not exist — not
    /// contention. `sudo` missing or wanting a password is the usual cause.
    Failed(String),
    /// Could not be determined — not evidence of a concurrent deploy.
    Unknown(String),
}

/// The script that takes the lock, or reports who has it, in one round trip.
///
/// The timestamps are taken from the *target's* `date`, at both write and read,
/// so staleness is measured on one clock. `started_epoch` in the owner file is
/// what the next run compares against.
fn acquire_script(
    dir: &str,
    id: &Identity,
    sudo: &str,
    stale_after: u64,
    force: bool,
    marker: &str,
) -> String {
    let l = shell_quote(dir);
    let n = shell_quote(marker);
    let force = if force { "yes" } else { "no" };
    format!(
        "L={l}; N={n}; NOW=$(date -u +%s); TOOK=fresh\n\
         {sudo}mkdir -p \"$(dirname \"$L\")\" 2>/dev/null || true\n\
         if {sudo}mkdir \"$L\" 2>/dev/null; then :\n\
         elif ! {sudo}test -d \"$L\" && ERR=$({sudo}mkdir \"$L\" 2>&1); then :\n\
         elif ! {sudo}test -d \"$L\"; then\n\
           printf '%s ERROR\\n' \"$N\"\n\
           printf '%s\\n' \"$ERR\"\n\
           printf '%s END\\n' \"$N\"\n\
           exit 0\n\
         else\n\
           HELD=$({sudo}cat \"$L/owner\" 2>/dev/null || true)\n\
           AGE=$(printf '%s\\n' \"$HELD\" | sed -n 's/^started_epoch=//p' | head -1)\n\
           BREAK={force}\n\
           if [ -n \"$AGE\" ] && [ {stale_after} -gt 0 ] && \
              [ $((NOW - AGE)) -gt {stale_after} ]; then BREAK=yes; TOOK=stale; fi\n\
           if [ \"$BREAK\" = yes ]; then\n\
             {sudo}rm -f \"$L/owner\" 2>/dev/null || true\n\
             {sudo}rmdir \"$L\" 2>/dev/null || true\n\
           fi\n\
           if [ \"$BREAK\" != yes ] || ! {sudo}mkdir \"$L\" 2>/dev/null; then\n\
             printf '%s BUSY\\n' \"$N\"\n\
             printf '%s\\n' \"$HELD\"\n\
             if [ -n \"$AGE\" ]; then printf '%s AGE %s\\n' \"$N\" \"$((NOW - AGE))\"; fi\n\
             printf '%s END\\n' \"$N\"\n\
             exit 0\n\
           fi\n\
         fi\n\
         printf 'owner=%s\\npid=%s\\nservices=%s\\nrelease=%s\\ndeploy=%s\\n\
started_at=%s\\nstarted_epoch=%s\\n' \
           {owner} {pid} {services} {release} {deploy} \
           \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" \"$NOW\" \
           | {sudo}tee \"$L/owner\" >/dev/null\n\
         printf '%s OK %s\\n' \"$N\" \"$TOOK\"\n",
        owner = shell_quote(&id.owner),
        pid = shell_quote(&id.pid.to_string()),
        services = shell_quote(&id.services),
        release = shell_quote(&id.release),
        deploy = shell_quote(&id.deploy),
    )
}

fn parse_answer(output: &str, marker: &str) -> Answer {
    if let Some(rest) = output
        .split('\n')
        .find_map(|l| l.strip_prefix(&format!("{marker} OK ")))
    {
        return Answer::Acquired {
            took_over: rest.trim() == "stale",
        };
    }
    let error = format!("{marker} ERROR");
    if let Some(start) = output.find(&error) {
        let body = &output[start + error.len()..];
        let body = body.split(&format!("{marker} END")).next().unwrap_or("");
        let reason = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        return Answer::Failed(if reason.is_empty() {
            "mkdir failed".into()
        } else {
            reason
        });
    }
    let busy = format!("{marker} BUSY");
    let Some(start) = output.find(&busy) else {
        return Answer::Unknown("the target gave no answer".into());
    };
    let body = &output[start + busy.len()..];
    let body = body.split(&format!("{marker} END")).next().unwrap_or("");
    let mut holder = Holder::parse(body);
    holder.age = body
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{marker} AGE ")))
        .and_then(|s| s.trim().parse().ok());
    Answer::Busy(holder)
}

/// Every distinct (target, host) the plan will mutate, in plan order.
fn contended(plan: &[ServicePlan]) -> Vec<(String, String)> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for sp in plan {
        let pair = (sp.target.clone(), sp.host.clone());
        if !seen.contains(&pair) {
            seen.push(pair);
        }
    }
    seen
}

/// Take the deploy lock on every target this plan touches.
///
/// Announces its own phase, so the operator sees the lock being taken rather
/// than discovering it only when it refuses.
pub fn acquire(
    plan: &[ServicePlan],
    targets: &BTreeMap<String, Target>,
    id: &Identity,
    force: bool,
) -> Taken {
    let pairs = contended(plan);
    if pairs.is_empty() {
        return Taken::Held(Guard::empty());
    }
    let mut guard = Guard {
        held: Vec::new(),
        deploy: id.deploy.clone(),
        released: false,
    };
    for (target_name, host) in pairs {
        let Some(target) = targets.get(&target_name) else {
            continue;
        };
        let dir = target.lock_dir();
        let marker = nonce();
        let script = acquire_script(
            &dir,
            id,
            sudo_for(target),
            target.lock_stale_after(),
            force,
            &marker,
        );
        let answer = match capture(target, &host, &script) {
            Ok(out) => parse_answer(&out, &marker),
            Err(e) => Answer::Unknown(e.to_string()),
        };
        match answer {
            Answer::Acquired { took_over } => {
                guard.held.push(Held {
                    target: target.clone(),
                    host: host.clone(),
                    dir: dir.clone(),
                    refreshed: std::cell::Cell::new(std::time::Instant::now()),
                    lost: std::cell::Cell::new(false),
                });
                if took_over {
                    crate::ui::note(format!(
                        "took over an abandoned lock on {host}:{dir} \
                         (older than {}s — no run has held it since)",
                        target.lock_stale_after()
                    ));
                }
                crate::ui::ok(format!("{target_name} [{host}] locked"));
            }
            Answer::Busy(holder) => {
                // Whatever this run already took goes back when `guard` drops
                // at the end of this function.
                return Taken::Busy(Box::new(Blocked {
                    target: target_name,
                    host,
                    dir,
                    holder,
                }));
            }
            Answer::Failed(reason) => {
                return Taken::Failed(Box::new(Unlockable {
                    target: target_name,
                    host,
                    dir,
                    reason,
                    sudo: target.uses_sudo(),
                }));
            }
            // Not evidence of a concurrent deploy — see the module note.
            Answer::Unknown(why) => {
                crate::ui::note(format!(
                    "could not take the deploy lock on {target_name} [{host}]: {why}. \
                     Continuing — this is not proof that another deploy is running."
                ));
            }
        }
    }
    Taken::Held(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_acquired_lock_is_recognised() {
        let out = "__M__ OK fresh\n";
        assert!(matches!(
            parse_answer(out, "__M__"),
            Answer::Acquired { took_over: false }
        ));
    }

    #[test]
    fn a_stale_takeover_is_recognised() {
        assert!(matches!(
            parse_answer("__M__ OK stale\n", "__M__"),
            Answer::Acquired { took_over: true }
        ));
    }

    #[test]
    fn a_busy_answer_carries_the_holder_and_its_age() {
        let out = "__M__ BUSY\nowner=jo@laptop\npid=51234\nservices=web,nginx\n\
                   release=v0.4.1\ndeploy=20260921-abc1234\n\
                   started_at=2026-09-21T14:02:11Z\nstarted_epoch=1758463331\n\
                   __M__ AGE 420\n__M__ END\n";
        let Answer::Busy(holder) = parse_answer(out, "__M__") else {
            panic!("expected busy");
        };
        assert_eq!(holder.owner, "jo@laptop");
        assert_eq!(holder.pid, "51234");
        assert_eq!(holder.services, "web,nginx");
        assert_eq!(holder.age, Some(420));
        let lines = Blocked {
            target: "box".into(),
            host: "h".into(),
            dir: "/d".into(),
            holder,
        }
        .render();
        assert!(
            lines[0].contains("a deploy is already running"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("jo@laptop (pid 51234)")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("7m ago")), "{lines:?}");
    }

    #[test]
    fn a_held_lock_with_no_readable_owner_still_refuses() {
        let Answer::Busy(holder) = parse_answer("__M__ BUSY\n\n__M__ END\n", "__M__") else {
            panic!("expected busy");
        };
        assert_eq!(holder, Holder::default());
        assert_eq!(holder.describe_owner(), "an unidentified run");
    }

    /// A failed `mkdir` with no lock directory behind it is not contention.
    /// It used to read as `BUSY` with an empty owner — "a deploy is already
    /// running … an unidentified run" — on a box that simply had no `sudo`.
    #[test]
    fn an_error_answer_is_a_failure_not_a_holder() {
        let out = "__M__ ERROR\nsh: sudo: command not found\n__M__ END\n";
        let Answer::Failed(reason) = parse_answer(out, "__M__") else {
            panic!("expected a failure");
        };
        assert_eq!(reason, "sh: sudo: command not found");
        let lines = Unlockable {
            target: "box".into(),
            host: "h".into(),
            dir: "/d.deliver-lock".into(),
            reason,
            sudo: true,
        }
        .render();
        assert!(!lines.join("\n").contains("already running"), "{lines:?}");
        assert!(
            lines[0].contains("could not create the deploy lock"),
            "{lines:?}"
        );
        assert!(lines.join("\n").contains("sudo: false"), "{lines:?}");
    }

    /// The script itself, with a `sudo` prefix that does not exist: it reports
    /// the failure, with force and without, and never claims a holder.
    #[cfg(unix)]
    #[test]
    fn a_missing_sudo_is_reported_as_a_failure_by_the_script() {
        let dir = std::env::temp_dir().join(format!("deliver-lock-nosudo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("app.deliver-lock");
        let id = Identity {
            owner: "jo@laptop".into(),
            pid: 1,
            services: "web".into(),
            release: "v1".into(),
            deploy: "d".into(),
        };
        for force in [false, true] {
            let script = acquire_script(
                &lock.to_string_lossy(),
                &id,
                "deliver-no-such-sudo ",
                3600,
                force,
                "__M__",
            );
            let out = sh(&script);
            let Answer::Failed(reason) = parse_answer(&out, "__M__") else {
                panic!("expected a failure (force={force}):\n{out}");
            };
            assert!(reason.contains("deliver-no-such-sudo"), "{reason}");
            assert!(!lock.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lock directory that exists is still contention, whatever `mkdir`
    /// said — the failure path must not swallow a real holder.
    #[test]
    fn an_existing_lock_is_still_busy_not_a_failure() {
        let dir = held_lock("still-busy", "d-other", "1");
        let id = Identity {
            owner: "me@laptop".into(),
            pid: 2,
            services: "web".into(),
            release: "v2".into(),
            deploy: "d-mine".into(),
        };
        let script = acquire_script(&dir.to_string_lossy(), &id, "", 0, false, "__M__");
        let out = sh(&script);
        assert!(
            matches!(parse_answer(&out, "__M__"), Answer::Busy(_)),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn silence_from_the_target_is_not_evidence_of_a_deploy() {
        assert!(matches!(parse_answer("", "__M__"), Answer::Unknown(_)));
    }

    /// The identity is built from the environment and the config, so it is not
    /// trusted input: it is run through `sh` on a machine the operator cares
    /// about. The payload here is benign on purpose — the assertion is that the
    /// shell treated it as data, not that nothing bad happened this time.
    #[test]
    fn the_script_never_lets_a_config_value_become_a_command() {
        let dir = std::env::temp_dir().join("deliver-lock-quoting");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("app.deliver-lock");
        let id = Identity {
            owner: "jo'; echo PWNED; '@laptop".into(),
            pid: 1,
            services: "web".into(),
            release: "v1".into(),
            deploy: "d".into(),
        };
        let script = acquire_script(&lock.to_string_lossy(), &id, "", 3600, false, "__M__");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(text.contains("__M__ OK fresh"), "{text}");
        assert!(!text.contains("PWNED"), "the value was executed:\n{text}");
        let owner = std::fs::read_to_string(lock.join("owner")).unwrap();
        assert!(
            owner.contains("owner=jo'; echo PWNED; '@laptop"),
            "the value was not stored verbatim:\n{owner}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sh(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn held_lock(name: &str, deploy: &str, epoch: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deliver-lock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("owner"),
            format!(
                "owner=jo@laptop\npid=1\nservices=web\nrelease=v1\ndeploy={deploy}\n\
                 started_at=2026-01-01T00:00:00Z\nstarted_epoch={epoch}\n"
            ),
        )
        .unwrap();
        dir
    }

    fn epoch_in(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("owner"))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("started_epoch=").map(str::to_string))
            .unwrap_or_default()
    }

    #[test]
    fn a_refresh_rewrites_only_the_stamp_and_only_on_our_own_lock() {
        let dir = held_lock("refresh-ours", "d1", "1000");
        let text = sh(&refresh_script(&dir.to_string_lossy(), "d1", "", "__M__"));
        assert_eq!(parse_ownership(&text, "__M__"), Ownership::Ours, "{text}");
        let epoch: u64 = epoch_in(&dir).parse().unwrap();
        assert!(
            epoch > 1_700_000_000,
            "stamp was not rewritten to now: {epoch}"
        );
        let owner = std::fs::read_to_string(dir.join("owner")).unwrap();
        assert!(
            owner.contains("owner=jo@laptop\n"),
            "the rest survived:\n{owner}"
        );
        assert!(owner.contains("deploy=d1\n"), "{owner}");
        assert_eq!(owner.matches("started_epoch=").count(), 1, "{owner}");

        // Someone else's lock is left exactly as it was.
        let theirs = held_lock("refresh-theirs", "d2", "1000");
        let text = sh(&refresh_script(
            &theirs.to_string_lossy(),
            "d1",
            "",
            "__M__",
        ));
        assert_eq!(
            parse_ownership(&text, "__M__"),
            Ownership::Other("d2".into()),
            "{text}"
        );
        assert_eq!(epoch_in(&theirs), "1000");

        // A lock with no owner file names nobody: not ours to stamp, not
        // another run's to report.
        std::fs::remove_file(theirs.join("owner")).unwrap();
        let text = sh(&refresh_script(
            &theirs.to_string_lossy(),
            "d1",
            "",
            "__M__",
        ));
        assert_eq!(parse_ownership(&text, "__M__"), Ownership::Ours, "{text}");
        assert!(
            !theirs.join("owner").exists(),
            "a refresh must never write an owner file"
        );

        // And a lock that is gone says so rather than inventing one.
        let _ = std::fs::remove_dir_all(&theirs);
        let text = sh(&refresh_script(
            &theirs.to_string_lossy(),
            "d1",
            "",
            "__M__",
        ));
        assert_eq!(parse_ownership(&text, "__M__"), Ownership::Gone, "{text}");
        assert!(!theirs.exists(), "a refresh must never create a lock");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_release_gives_back_only_the_lock_this_run_still_holds() {
        let ours = held_lock("release-ours", "d1", "1000");
        let text = sh(&release_script(&ours.to_string_lossy(), "d1", "", "__M__"));
        assert_eq!(parse_ownership(&text, "__M__"), Ownership::Ours, "{text}");
        assert!(!ours.exists(), "our lock was released");

        let theirs = held_lock("release-theirs", "d2", "1000");
        let text = sh(&release_script(
            &theirs.to_string_lossy(),
            "d1",
            "",
            "__M__",
        ));
        assert_eq!(
            parse_ownership(&text, "__M__"),
            Ownership::Other("d2".into()),
            "{text}"
        );
        assert!(
            theirs.join("owner").exists(),
            "a lock another run took over is theirs to release"
        );
        let _ = std::fs::remove_dir_all(&theirs);

        let text = sh(&release_script(&ours.to_string_lossy(), "d1", "", "__M__"));
        assert_eq!(parse_ownership(&text, "__M__"), Ownership::Gone, "{text}");
    }

    #[test]
    fn silence_on_release_is_unknown_not_released() {
        assert_eq!(parse_ownership("", "__M__"), Ownership::Unknown);
        assert_eq!(
            parse_ownership("__M__ RELEASED\n", "__M__"),
            Ownership::Ours
        );
        assert_eq!(
            parse_ownership("__M__ REFRESHED 1758463331\n", "__M__"),
            Ownership::Ours
        );
    }

    #[test]
    fn each_target_and_host_is_locked_once() {
        let sp = |service: &str, target: &str, host: &str| ServicePlan {
            service: service.into(),
            target: target.into(),
            host: host.into(),
            steps: Vec::new(),
            after_tag: false,
            soak: None,
        };
        let plan = vec![
            sp("web", "box", "a"),
            sp("nginx", "box", "a"),
            sp("api", "box", "b"),
        ];
        assert_eq!(
            contended(&plan),
            vec![
                ("box".to_string(), "a".to_string()),
                ("box".to_string(), "b".to_string())
            ]
        );
    }
}
