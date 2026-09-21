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

/// The locks this run holds. Releasing is the whole point of the type: it
/// happens in `Drop`, so no caller has to remember it on its error path.
pub struct Guard {
    held: Vec<(Target, String, String)>,
    released: bool,
}

impl Guard {
    /// A guard that holds nothing, for the paths that mutate nothing.
    pub fn empty() -> Self {
        Self {
            held: Vec::new(),
            released: true,
        }
    }

    /// Give every lock back. Idempotent, and called for you by `Drop`.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        for (target, host, dir) in std::mem::take(&mut self.held) {
            let sudo = sudo_for(&target);
            let marker = nonce();
            let quoted = shell_quote(&dir);
            // `rmdir`, never `rm -rf`: a path built from config should not be
            // able to take a tree with it, and a lock dir holding anything but
            // our own owner file is not ours to delete.
            let script = format!(
                "{sudo}rm -f {quoted}/owner 2>/dev/null; \
                 {sudo}rmdir {quoted} 2>/dev/null; \
                 if [ ! -d {quoted} ]; then printf '%s RELEASED\\n' {n}; fi",
                n = shell_quote(&marker),
            );
            let freed = capture(&target, &host, &script)
                .map(|out| out.contains(&format!("{marker} RELEASED")))
                .unwrap_or(false);
            if !freed {
                crate::ui::note(format!(
                    "could not release the deploy lock at {host}:{dir} — \
                     remove it by hand, or wait for it to go stale."
                ));
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
         if {sudo}mkdir \"$L\" 2>/dev/null; then :; else\n\
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
                guard.held.push((target.clone(), host.clone(), dir.clone()));
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

    #[test]
    fn each_target_and_host_is_locked_once() {
        let sp = |service: &str, target: &str, host: &str| ServicePlan {
            service: service.into(),
            target: target.into(),
            host: host.into(),
            steps: Vec::new(),
            after_tag: false,
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
