//! Read back what is actually deployed — `deliver status` and `deliver history`.
//!
//! Every release-based deploy already *writes* a durable record on the target:
//! the live path is a symlink named after the deploy id, retained releases sit
//! beside it, and `.deliver/history.tsv` gets one row per deploy (number,
//! deploy id, release, sha, UTC timestamp). Until now nothing read any of it
//! back, so "what version is on prod right now?" meant an ssh session and a
//! `readlink` by hand — for each of ~10 apps.
//!
//! The design mirrors [`crate::configdiff`], for the same reason: the deployer
//! that writes the record declares where it wrote it
//! ([`crate::deployers::ReleaseState`], attached to the step that does the
//! writing), and this module only reads. There is no second copy of the path
//! arithmetic to drift away from the first.
//!
//! Like the config diff, the read is **read-only and batched** — one connection
//! per (target, host), nothing on the target is touched — and everything on the
//! way out goes through [`crate::secrets::redact`].

use crate::config::Target;
use crate::deployers::{IngressState, LogSource, ReleaseState};
use crate::plan::ServicePlan;
use crate::remote::{capture, nonce, shell_quote};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a Compose stack runs on the target, taken from the deployer's
/// [`LogSource::Compose`] so `deliver status` queries the exact same project
/// and files the deploy brought up.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposeTarget {
    pub compose: String,
    pub dir: String,
    pub images: Vec<String>,
}

/// One container reported by `docker compose ps --all --format json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Container {
    pub service: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_drift: Option<String>,
}

/// One service on one host whose deploy state can be read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub service: String,
    pub target: String,
    pub host: String,
    pub state: Option<ReleaseState>,
    pub ingress: Vec<IngressState>,
    pub compose: Option<ComposeTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ingress {
    pub conf_path: String,
    pub enabled_path: String,
    pub installed: Option<bool>,
    pub enabled: Option<bool>,
    pub nginx_test: Option<bool>,
    pub certificates: Vec<Certificate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Certificate {
    pub name: String,
    pub path: String,
    pub health: CertificateHealth,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CertificateHealth {
    Valid { days_remaining: u64 },
    Expired,
    Missing,
    Unreadable,
}

fn ingress_probes(index: usize, states: &[IngressState], sudo: &str) -> Vec<Probe> {
    let mut probes = Vec::new();
    if states.is_empty() {
        return probes;
    }
    probes.push(Probe {
        key: format!("{index}:nginx"),
        snippet: format!("if {sudo}nginx -t >/dev/null 2>&1; then echo yes; else echo no; fi"),
    });
    for (v, state) in states.iter().enumerate() {
        let conf = shell_quote(&state.conf_path);
        let link = shell_quote(&state.enabled_path);
        probes.push(Probe {
            key: format!("{index}:ingress:{v}"),
            // -ef follows relative links too, and rejects dangling or wrong links.
            snippet: format!(
                "if ! {sudo}test -d /; then printf 'unknown\\nunknown\\n'; else \
                   if {sudo}test -f {conf}; then echo yes; else echo no; fi; \
                   if {sudo}test -L {link} && {sudo}test {link} -ef {conf}; then echo yes; else echo no; fi; fi"
            ),
        });
        for (c, name) in state.cert_names.iter().enumerate() {
            let path = shell_quote(&format!("/etc/letsencrypt/live/{name}/fullchain.pem"));
            probes.push(Probe {
                key: format!("{index}:cert:{v}:{c}"),
                // First distinguish missing and malformed/unreadable from expired.
                // Binary search whole days with checkend: no remote date parser,
                // local clock assumptions or writes. X.509 dates end at year 9999;
                // ten thousand years is an upper bound, not a capped answer.
                snippet: format!(
                    "if ! {sudo}test -d /; then echo unreadable; \
                     elif ! {sudo}test -e {path}; then echo missing; \
                     elif ! {sudo}openssl x509 -in {path} -noout >/dev/null 2>&1; then echo unreadable; \
                     elif {sudo}openssl x509 -in {path} -noout -checkend 0 >/dev/null 2>&1; then \
                       LO=0; HI=3652500; while [ $((HI - LO)) -gt 1 ]; do \
                         MID=$(((LO + HI) / 2)); \
                         if {sudo}openssl x509 -in {path} -noout -checkend $((MID * 86400)) >/dev/null 2>&1; \
                         then LO=$MID; else HI=$MID; fi; done; echo \"$LO\"; \
                     else echo expired; fi"
                ),
            });
        }
    }
    probes
}

fn parse_ingress(
    index: usize,
    states: &[IngressState],
    answers: &BTreeMap<String, String>,
) -> Vec<Ingress> {
    let yes_no = |text: Option<&str>| match text {
        Some("yes") => Some(true),
        Some("no") => Some(false),
        _ => None,
    };
    states
        .iter()
        .enumerate()
        .map(|(v, state)| {
            let fields: Vec<_> = answers
                .get(&format!("{index}:ingress:{v}"))
                .map(|text| text.lines().collect())
                .unwrap_or_default();
            Ingress {
                conf_path: state.conf_path.clone(),
                enabled_path: state.enabled_path.clone(),
                installed: yes_no(fields.first().copied()),
                enabled: yes_no(fields.get(1).copied()),
                nginx_test: yes_no(answers.get(&format!("{index}:nginx")).map(|s| s.trim())),
                certificates: state
                    .cert_names
                    .iter()
                    .enumerate()
                    .map(|(c, name)| {
                        let text = answers
                            .get(&format!("{index}:cert:{v}:{c}"))
                            .map(|s| s.trim())
                            .unwrap_or("");
                        Certificate {
                            name: name.clone(),
                            path: format!("/etc/letsencrypt/live/{name}/fullchain.pem"),
                            health: match text {
                                "expired" => CertificateHealth::Expired,
                                "missing" => CertificateHealth::Missing,
                                other => other
                                    .parse()
                                    .map(|days_remaining| CertificateHealth::Valid {
                                        days_remaining,
                                    })
                                    .unwrap_or(CertificateHealth::Unreadable),
                            },
                        }
                    })
                    .collect(),
            }
        })
        .collect()
}

fn render_ingress(ingress: &[Ingress]) -> String {
    let mut out = String::new();
    let flag = |value| match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    for site in ingress {
        out.push_str(&format!(
            "    vhost         {} · installed {} · enabled {} ({})\n",
            site.conf_path,
            flag(site.installed),
            flag(site.enabled),
            site.enabled_path
        ));
        out.push_str(&format!(
            "    nginx -t      {}\n",
            match site.nginx_test {
                Some(true) => "passes",
                Some(false) => "fails (or could not run)",
                None => "unknown",
            }
        ));
        if site.certificates.is_empty() {
            out.push_str("    certificates  none declared\n");
        }
        for cert in &site.certificates {
            let health = match cert.health {
                CertificateHealth::Valid { days_remaining } => {
                    format!("{days_remaining} day(s) remaining")
                }
                CertificateHealth::Expired => "EXPIRED".into(),
                CertificateHealth::Missing => "missing".into(),
                CertificateHealth::Unreadable => "unreadable (or openssl unavailable)".into(),
            };
            out.push_str(&format!("    certificate   {} · {health}\n", cert.name));
        }
    }
    out
}

/// Worst certificate result for one fleet row. Unknown certs must not be hidden
/// behind a healthy certificate's remaining days.
pub fn certificate_summary(statuses: &[Status]) -> Option<String> {
    let certs: Vec<_> = statuses
        .iter()
        .flat_map(|s| &s.ingress)
        .flat_map(|i| &i.certificates)
        .collect();
    if certs.is_empty() {
        return None;
    }
    let expired = certs.iter().any(|c| c.health == CertificateHealth::Expired);
    let unknown = certs.iter().any(|c| {
        matches!(
            c.health,
            CertificateHealth::Missing | CertificateHealth::Unreadable
        )
    });
    let days = certs
        .iter()
        .filter_map(|c| match c.health {
            CertificateHealth::Valid { days_remaining } => Some(days_remaining),
            _ => None,
        })
        .min();
    let mut parts = Vec::new();
    if expired {
        parts.push("EXPIRED".to_string());
    }
    if unknown {
        parts.push("missing/unreadable".to_string());
    }
    if let Some(days) = days {
        parts.push(format!("{days} day(s) remaining (minimum)"));
    }
    Some(format!("certificates: {}", parts.join(" · ")))
}

/// What the live path points at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Live {
    /// The symlink resolves to this release directory.
    Release { path: String },
    /// Something is there, but it is a plain directory — a deploy has not
    /// migrated it into the release layout yet.
    Unmanaged,
    /// Nothing is at the live path.
    Missing,
    /// This deployer has no live symlink (Compose replaces containers in place).
    NotApplicable,
}

/// One row of `.deliver/history.tsv`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deploy {
    pub number: String,
    pub deploy_id: String,
    pub release: String,
    pub sha: String,
    pub at: String,
    /// True for a row `deliver rollback` appended: the deploy id is the
    /// release it restored, not one that was shipped at that time.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rollback: bool,
}

/// Everything read back for one service on one host.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ingress: Vec<Ingress>,
    /// Live containers reported by `docker compose ps --all --format json` when this
    /// service is a Compose stack and the daemon answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<Container>>,
    /// False when `docker compose ps` failed (e.g. Docker daemon unreachable),
    /// kept distinct from `containers: Some([])` ("nothing running").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_reachable: Option<bool>,
    pub service: String,
    pub target: String,
    pub host: String,
    /// False when the target could not be read at all — an unreachable host, a
    /// refused login, a shell that never answered.
    pub reachable: bool,
    pub live: Live,
    /// Retained release directories, newest first.
    pub releases: Vec<String>,
    /// Recorded deploys, newest first.
    pub history: Vec<Deploy>,
    pub history_path: String,
    pub live_path: Option<String>,
    pub releases_dir: Option<String>,
    /// Where the activate step records the outgoing release, carried through
    /// so `deliver rollback --to` can keep it honest after a targeted swap.
    pub previous_marker: Option<String>,
    /// Pre-deploy backups on the target, newest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backups: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backups_dir: Option<String>,
    /// What the previous-release marker names — where a plain `deliver
    /// rollback` would go. Read for the rollback preview, not reported.
    #[serde(skip)]
    pub previous: Option<String>,
}

impl Status {
    /// The deploy id that is live, when one can be established.
    ///
    /// A symlink is the authority: it is what the web server actually follows.
    /// Without one — Compose, which has no symlink — the newest history row is
    /// the best available answer, and [`Status::live_is_recorded`] says which
    /// of the two the caller is looking at — unless that row is the unwind of
    /// a first release, after which nothing is up.
    pub fn live_deploy_id(&self) -> Option<String> {
        match &self.live {
            Live::Release { path } => Some(basename(path).to_string()),
            Live::NotApplicable => self
                .history
                .first()
                .map(|d| d.deploy_id.clone())
                .filter(|id| id != crate::rollback::NOTHING_LIVE),
            _ => None,
        }
    }

    /// True when the live deploy id came from the symlink rather than history.
    pub fn live_is_recorded(&self) -> bool {
        matches!(self.live, Live::Release { .. })
    }

    /// The history row for whatever is live, when the two agree.
    pub fn live_deploy(&self) -> Option<&Deploy> {
        let id = self.live_deploy_id()?;
        self.history.iter().find(|d| d.deploy_id == id)
    }
}

pub(crate) fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Every service in the plan that records deploy state on the target.
///
/// Pure: it walks the plan the deployers already produced, so the paths read
/// are by construction the paths the deploy writes.
pub fn collect(plan: &[ServicePlan]) -> Vec<Request> {
    plan.iter()
        .filter_map(|sp| {
            let state = sp.steps.iter().find_map(|step| step.release_state.clone());
            let ingress: Vec<_> = sp
                .steps
                .iter()
                .filter_map(|step| step.ingress_state.clone())
                .collect();
            let compose = sp.steps.iter().find_map(|step| match &step.log_source {
                Some(LogSource::Compose {
                    compose,
                    dir,
                    images,
                }) => Some(ComposeTarget {
                    compose: compose.clone(),
                    dir: dir.clone(),
                    images: images.clone(),
                }),
                _ => None,
            });
            (state.is_some() || !ingress.is_empty()).then(|| Request {
                service: sp.service.clone(),
                target: sp.target.clone(),
                host: sp.host.clone(),
                state,
                ingress,
                compose,
            })
        })
        .collect()
}

/// Release consumers (shipping and rollback) must not treat ingress as a release.
pub fn collect_releases(plan: &[ServicePlan]) -> Vec<Request> {
    collect(plan)
        .into_iter()
        .filter(|r| r.state.is_some())
        .map(|mut r| {
            r.compose = None;
            r
        })
        .collect()
}

/// One framed probe: a key to find the answer by, and the snippet that answers.
struct Probe {
    key: String,
    snippet: String,
}

/// Build the one script that answers every probe for a host.
///
/// Each answer is framed by a nonce so several of them travel back over a
/// single connection. The script adds exactly one newline before the end
/// marker and the parser removes exactly one, so an answer with no trailing
/// newline round-trips unchanged.
fn probe_script(probes: &[Probe], nonce: &str) -> String {
    let mut script = String::new();
    for probe in probes {
        script.push_str(&format!(
            "printf '%s BEGIN %s\\n' {n} {k}; {{ {s}; }} 2>/dev/null; printf '\\n%s END\\n' {n}; ",
            n = shell_quote(nonce),
            k = shell_quote(&probe.key),
            s = probe.snippet,
        ));
    }
    script
}

/// Parse what [`probe_script`] printed back into one answer per key.
///
/// A key the script never reported on is simply absent from the map, which is
/// how the caller tells "nothing there" from "the target never answered".
fn parse_probes(output: &str, nonce: &str) -> BTreeMap<String, String> {
    let begin = format!("{nonce} BEGIN ");
    let end = format!("{nonce} END");
    let mut found = BTreeMap::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in output.split('\n') {
        if let Some(key) = line.strip_prefix(begin.as_str()) {
            current = Some((key.trim().to_string(), Vec::new()));
            continue;
        }
        if line.trim_end() == end {
            if let Some((key, mut body)) = current.take() {
                // The script's own trailing newline is the last empty element.
                if body.last() == Some(&"") {
                    body.pop();
                }
                found.insert(key, body.join("\n"));
            }
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    found
}

/// The three snippets that read one service's state.
fn probes_for(index: usize, state: &ReleaseState, sudo: &str) -> Vec<Probe> {
    let mut probes = vec![Probe {
        key: format!("{index}:history"),
        snippet: format!(
            "if [ -f {h} ]; then {sudo}cat -- {h}; fi",
            h = shell_quote(&state.history_path)
        ),
    }];
    if let Some(live) = &state.live_path {
        probes.push(Probe {
            key: format!("{index}:live"),
            snippet: format!(
                "if [ -L {p} ]; then printf 'link %s\\n' \"$({sudo}readlink {p})\"; \
                 elif [ -d {p} ]; then printf 'dir\\n'; fi",
                p = shell_quote(live)
            ),
        });
    }
    if let Some(releases) = &state.releases_dir {
        probes.push(Probe {
            key: format!("{index}:releases"),
            // The same `ls -1d */` the prune step uses, so what is listed here
            // is what pruning counts.
            snippet: format!(
                "if [ -d {r} ]; then cd {r} && {sudo}ls -1d */; fi",
                r = shell_quote(releases)
            ),
        });
    }
    if let Some(marker) = &state.previous_marker {
        probes.push(Probe {
            key: format!("{index}:previous"),
            snippet: format!(
                "if [ -f {m} ]; then {sudo}cat -- {m}; fi",
                m = shell_quote(marker)
            ),
        });
    }
    if let Some(backups) = &state.backups_dir {
        probes.push(Probe {
            key: format!("{index}:backups"),
            snippet: format!(
                "if [ -d {b} ]; then cd {b} && {sudo}ls -1 -- *.dump *.tar.gz; fi",
                b = shell_quote(backups)
            ),
        });
    }
    probes
}

fn compose_probe(index: usize, target: &ComposeTarget, sudo: &str) -> Probe {
    let dir = shell_quote(&target.dir);
    let compose = &target.compose;
    Probe {
        key: format!("{index}:compose"),
        // Distinguish a not-yet-created project directory (`absent`), a working
        // daemon with zero or more containers (`ok`), and a failed `compose ps`
        // such as an unreachable Docker daemon (`err`). `--all` includes
        // stopped/exited containers so a crashed worker does not disappear from
        // the `running/total` count. Stderr is discarded on the `ps` call so
        // Compose warnings never corrupt the JSON payload.
        snippet: format!(
            "if ! {sudo}test -d {dir}; then printf 'absent\\n'; \
             elif OUT=$(cd {dir} && {sudo}{compose} ps --all --format json 2>/dev/null); then \
               printf 'ok\\n%s' \"$OUT\"; \
             else printf 'err\\n'; fi"
        ),
    }
}

fn compose_restarts_probe(index: usize, target: &ComposeTarget, sudo: &str) -> Probe {
    let dir = shell_quote(&target.dir);
    let compose = &target.compose;
    Probe {
        key: format!("{index}:compose:restarts"),
        // `docker compose ps --format json` does not emit RestartCount, so read
        // it from `docker inspect` over the same SSH connection.
        snippet: format!(
            "if {sudo}test -d {dir}; then \
               IDS=$(cd {dir} && {sudo}{compose} ps --all -q 2>/dev/null); \
               if [ -n \"$IDS\" ]; then \
                 {sudo}docker inspect --format '{{{{.Name}}}} {{{{.RestartCount}}}}' $IDS 2>/dev/null; \
               fi; \
             fi"
        ),
    }
}

#[derive(Deserialize)]
struct RawContainer {
    #[serde(default, alias = "Service")]
    service: String,
    #[serde(default, alias = "Name")]
    name: String,
    #[serde(default, alias = "Image")]
    image: String,
    #[serde(default, alias = "State")]
    state: String,
    #[serde(default, alias = "Status")]
    status: String,
    #[serde(default, alias = "Health")]
    health: String,
    #[serde(default, alias = "ExitCode")]
    exit_code: Option<i64>,
}

impl From<RawContainer> for Container {
    fn from(raw: RawContainer) -> Self {
        let service = if raw.service.is_empty() {
            raw.name.clone()
        } else {
            raw.service
        };
        let health = if !raw.health.is_empty() {
            Some(raw.health.to_lowercase())
        } else if raw.status.contains("(unhealthy)") {
            Some("unhealthy".into())
        } else if raw.status.contains("(healthy)") {
            Some("healthy".into())
        } else if raw.status.contains("(health: starting)") {
            Some("starting".into())
        } else {
            None
        };
        let exit_code =
            if raw.state.eq_ignore_ascii_case("exited") || raw.exit_code.is_some_and(|c| c != 0) {
                raw.exit_code
            } else {
                None
            };
        Container {
            service,
            name: raw.name,
            image: raw.image,
            state: raw.state,
            status: raw.status,
            health,
            exit_code,
            restart_count: None,
            image_drift: None,
        }
    }
}

fn parse_compose_restarts(raw: &str) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(count), None) = (parts.next(), parts.next(), parts.next()) {
            if let Ok(n) = count.parse::<u64>() {
                let name = name.trim_start_matches('/').to_string();
                if !name.is_empty() {
                    map.insert(name, n);
                }
            }
        }
    }
    map
}

fn apply_restart_counts(containers: &mut [Container], restarts: &BTreeMap<String, u64>) {
    for container in containers.iter_mut() {
        if let Some(&n) = restarts.get(&container.name) {
            if n > 0 {
                container.restart_count = Some(n);
            }
        }
    }
}

/// Parse the framed `{index}:compose` probe output into `(containers, docker_reachable)`.
///
/// Accepts both Compose v2.21+ NDJSON (one object per line) and older JSON
/// array output. Any `err` response or unparseable non-empty payload is
/// reported as `docker_reachable = Some(false)` with `containers = None` so an
/// unreachable daemon is never mistaken for "nothing running".
fn parse_compose_ps(raw: &str) -> (Option<Vec<Container>>, Option<bool>) {
    let trimmed = raw.trim();
    if trimmed == "absent" {
        return (None, None);
    }
    if trimmed == "err" || trimmed.starts_with("err\n") {
        return (None, Some(false));
    }
    let Some(rest) = raw
        .strip_prefix("ok\n")
        .or_else(|| (trimmed == "ok").then_some(""))
    else {
        return (None, Some(false));
    };
    let payload = rest.trim();
    if payload.is_empty() {
        return (Some(Vec::new()), Some(true));
    }
    if payload.starts_with('[') {
        return match serde_json::from_str::<Vec<RawContainer>>(payload) {
            Ok(items) => (
                Some(items.into_iter().map(Container::from).collect()),
                Some(true),
            ),
            Err(_) => (None, Some(false)),
        };
    }
    let mut containers = Vec::new();
    for line in payload.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<RawContainer>(line) {
            Ok(item) => containers.push(Container::from(item)),
            Err(_) => return (None, Some(false)),
        }
    }
    (Some(containers), Some(true))
}

fn short_uptime(status: &str) -> Option<String> {
    let rest = status
        .strip_prefix("Up ")
        .or_else(|| status.strip_prefix("up "))?;
    let rest = rest.split(" (").next().unwrap_or(rest).trim();
    if rest.is_empty() {
        return None;
    }
    let lower = rest.to_lowercase();
    if lower == "less than a second" {
        return Some("up <1s".into());
    }
    if lower == "about a minute" {
        return Some("up 1m".into());
    }
    if lower == "about an hour" {
        return Some("up 1h".into());
    }
    let mut parts = lower.split_whitespace();
    if let (Some(n), Some(unit), None) = (parts.next(), parts.next(), parts.next()) {
        if n.chars().all(|c| c.is_ascii_digit()) {
            let suffix = match unit {
                "second" | "seconds" => Some("s"),
                "minute" | "minutes" => Some("m"),
                "hour" | "hours" => Some("h"),
                "day" | "days" => Some("d"),
                "week" | "weeks" => Some("w"),
                "month" | "months" => Some("mo"),
                "year" | "years" => Some("y"),
                _ => None,
            };
            if let Some(suffix) = suffix {
                return Some(format!("up {n}{suffix}"));
            }
        }
    }
    Some(format!("up {lower}"))
}

impl Container {
    fn short_state(&self) -> String {
        let state = self.state.to_lowercase();
        match state.as_str() {
            "running" => match self.health.as_deref() {
                Some("healthy") => "healthy".into(),
                Some("unhealthy") => "unhealthy".into(),
                Some("starting") => "starting".into(),
                _ => short_uptime(&self.status).unwrap_or_else(|| "running".into()),
            },
            "restarting" => match self.restart_count {
                Some(n) if n > 0 => format!("restarting ×{n}"),
                _ => "restarting".into(),
            },
            "exited" => match self.exit_code {
                Some(code) => format!("exited ({code})"),
                None => "exited".into(),
            },
            "" => "unknown".into(),
            other => other.into(),
        }
    }
}

fn split_image_tag(image: &str) -> Option<(&str, &str)> {
    let without_digest = image.split('@').next().unwrap_or(image);
    let last_slash = without_digest.rfind('/');
    let colon = without_digest.rfind(':')?;
    if last_slash.is_some_and(|slash| colon < slash) {
        return None;
    }
    let repo = &without_digest[..colon];
    let tag = &without_digest[colon + 1..];
    if repo.is_empty() || tag.is_empty() {
        return None;
    }
    Some((repo, tag))
}

fn is_deploy_id_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    bytes.len() >= 9 && bytes[..8].iter().all(u8::is_ascii_digit) && bytes[8] == b'T'
}

fn is_hex_sha(tag: &str) -> bool {
    (7..=40).contains(&tag.len())
        && tag.chars().all(|c| c.is_ascii_hexdigit())
        && tag.chars().any(|c| matches!(c, 'a'..='f' | 'A'..='F'))
}

fn is_v_semver(tag: &str) -> bool {
    let mut chars = tag.chars();
    matches!(chars.next(), Some('v'))
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && tag.contains('.')
}

fn tag_matches_release(tag: &str, release: &str) -> bool {
    !release.is_empty()
        && release != "untagged"
        && (tag == release || tag.trim_start_matches('v') == release.trim_start_matches('v'))
}

fn tag_matches_sha(tag: &str, sha: &str) -> bool {
    !sha.is_empty()
        && sha != "unknown"
        && is_hex_sha(tag)
        && (sha.starts_with(tag) || tag.starts_with(sha))
}

fn tag_matches_deploy(tag: &str, deploy: &Deploy) -> bool {
    tag == deploy.deploy_id
        || tag_matches_release(tag, &deploy.release)
        || tag_matches_sha(tag, &deploy.sha)
}

fn detect_image_drift(
    container: &Container,
    recorded: &Deploy,
    history: &[Deploy],
    app_repos: &std::collections::BTreeSet<String>,
) -> Option<String> {
    let (repo, tag) = split_image_tag(&container.image)?;
    if !app_repos.contains(repo) || tag == "latest" || tag_matches_deploy(tag, recorded) {
        return None;
    }
    let expected = if is_deploy_id_tag(tag) {
        recorded.deploy_id.clone()
    } else if let Some(kind) = history.iter().find_map(|old| {
        if tag == old.deploy_id {
            Some(recorded.deploy_id.clone())
        } else if tag_matches_release(tag, &old.release) {
            Some(
                if recorded.release != "untagged" && !recorded.release.is_empty() {
                    recorded.release.clone()
                } else {
                    recorded.deploy_id.clone()
                },
            )
        } else if tag_matches_sha(tag, &old.sha) {
            Some(short_sha(&recorded.sha))
        } else {
            None
        }
    }) {
        kind
    } else if is_v_semver(tag) && recorded.release != "untagged" && !recorded.release.is_empty() {
        recorded.release.clone()
    } else if is_hex_sha(tag) && !recorded.sha.is_empty() && recorded.sha != "unknown" {
        short_sha(&recorded.sha)
    } else if recorded.release != "untagged" && !recorded.release.is_empty() {
        recorded.release.clone()
    } else {
        recorded.deploy_id.clone()
    };
    Some(format!(
        "{} runs {repo}:{tag} but the record says {repo}:{expected}",
        container.service
    ))
}

fn apply_image_drift(
    containers: &mut [Container],
    recorded: &Deploy,
    history: &[Deploy],
    owned_images: &[String],
) {
    let mut app_repos: std::collections::BTreeSet<String> = owned_images.iter().cloned().collect();
    for c in containers.iter() {
        if let Some((repo, tag)) = split_image_tag(&c.image) {
            if tag_matches_deploy(tag, recorded) {
                app_repos.insert(repo.to_string());
            }
        }
    }
    for container in containers.iter_mut() {
        container.image_drift = detect_image_drift(container, recorded, history, &app_repos);
    }
}

fn render_compose(status: &Status) -> String {
    let mut out = String::new();
    if status.docker_reachable == Some(false) {
        out.push_str("    docker        daemon unreachable — could not query containers\n");
        return out;
    }
    let Some(containers) = &status.containers else {
        return out;
    };
    if containers.is_empty() {
        out.push_str("    services      nothing running\n");
        return out;
    }
    let running = containers
        .iter()
        .filter(|c| c.state.eq_ignore_ascii_case("running"))
        .count();
    let details: Vec<String> = containers
        .iter()
        .map(|c| format!("{} {}", c.service, c.short_state()))
        .collect();
    out.push_str(&format!(
        "    services      {running}/{} running ({})\n",
        containers.len(),
        details.join(", ")
    ));
    for container in containers {
        if let Some(drift) = &container.image_drift {
            out.push_str(&format!("    drift         {drift}\n"));
        }
    }
    out
}

/// Backup file names, newest first by the UTC stamp every backup carries
/// (`predeploy-20260918T220521Z.dump`, `media-20260918T220521Z.tar.gz`) —
/// not by name, since the kinds would otherwise sort by their prefix.
pub fn sort_backups(text: &str) -> Vec<String> {
    let stamp = |name: &str| -> String {
        let stem = name.split('.').next().unwrap_or(name);
        stem.rsplit('-').next().unwrap_or(stem).to_string()
    };
    let mut names: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    names.sort_by(|a, b| stamp(b).cmp(&stamp(a)).then_with(|| a.cmp(b)));
    names
}

/// Parse `.deliver/history.tsv`, newest first.
///
/// Rows that are not five tab-separated fields are skipped rather than guessed
/// at: the file is appended to by a shell `printf` on the target and a
/// half-written line should not turn into a confident wrong answer. A sixth
/// field of `rollback` marks a row `deliver rollback` wrote; older readers
/// ignore it.
pub fn parse_history(text: &str) -> Vec<Deploy> {
    let mut rows: Vec<Deploy> = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 5 || fields[1].is_empty() {
                return None;
            }
            Some(Deploy {
                number: fields[0].trim().to_string(),
                deploy_id: fields[1].trim().to_string(),
                release: fields[2].trim().to_string(),
                sha: fields[3].trim().to_string(),
                at: fields[4].trim().to_string(),
                rollback: fields.get(5).is_some_and(|f| f.trim() == "rollback"),
            })
        })
        .collect();
    rows.reverse();
    rows
}

/// Read every request off its target, one connection per (target, host).
pub fn read(requests: Vec<Request>, targets: &BTreeMap<String, Target>) -> Vec<Status> {
    // Group by where the read happens, keeping each request's index so the
    // answers can be matched back up.
    let mut by_host: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (i, request) in requests.iter().enumerate() {
        by_host
            .entry((request.target.clone(), request.host.clone()))
            .or_default()
            .push(i);
    }

    let mut answers: BTreeMap<usize, Option<BTreeMap<String, String>>> = BTreeMap::new();
    for ((target_name, host), indexes) in by_host {
        let Some(target) = targets.get(&target_name) else {
            for i in indexes {
                answers.insert(i, None);
            }
            continue;
        };
        // The deploy writes these paths under sudo, so a 0600 history or a
        // root-owned releases directory is only readable the same way.
        let sudo = if target.uses_sudo() { "sudo " } else { "" };
        let probes: Vec<Probe> = indexes
            .iter()
            .flat_map(|i| {
                let request = &requests[*i];
                let mut probes = request
                    .state
                    .as_ref()
                    .map(|state| probes_for(*i, state, sudo))
                    .unwrap_or_default();
                if let Some(compose) = &request.compose {
                    probes.push(compose_probe(*i, compose, sudo));
                    probes.push(compose_restarts_probe(*i, compose, sudo));
                }
                probes.extend(ingress_probes(
                    *i,
                    &request.ingress,
                    if target.uses_sudo() { "sudo -n " } else { "" },
                ));
                probes
            })
            .collect();
        let marker = nonce();
        let script = probe_script(&probes, &marker);
        let found = match capture(target, &host, &script) {
            Ok(output) => parse_probes(&output, &marker),
            Err(_) => BTreeMap::new(),
        };
        for i in indexes {
            // No answer at all for a service means the host never ran the
            // script — unreachable, not "nothing is deployed".
            let mine: BTreeMap<String, String> = found
                .iter()
                .filter(|(k, _)| k.starts_with(&format!("{i}:")))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            answers.insert(i, if mine.is_empty() { None } else { Some(mine) });
        }
    }

    requests
        .into_iter()
        .enumerate()
        .map(|(i, request)| {
            let answer = answers.remove(&i).flatten();
            let reachable = answer.is_some();
            let answer = answer.unwrap_or_default();
            let ingress = parse_ingress(i, &request.ingress, &answer);
            // Ingress has no release history; these empty fields preserve the
            // release readback shape without inventing deploy records.
            let state = request.state.unwrap_or(ReleaseState {
                history_path: String::new(),
                live_path: None,
                releases_dir: None,
                previous_marker: None,
                backups_dir: None,
            });
            let live = match answer.get(&format!("{i}:live")) {
                None if state.live_path.is_none() => Live::NotApplicable,
                None => Live::Missing,
                Some(text) => match text.trim() {
                    "" => Live::Missing,
                    "dir" => Live::Unmanaged,
                    other => match other.strip_prefix("link ") {
                        Some(path) if !path.trim().is_empty() => Live::Release {
                            path: path.trim().to_string(),
                        },
                        _ => Live::Missing,
                    },
                },
            };
            let mut releases: Vec<String> = answer
                .get(&format!("{i}:releases"))
                .map(|text| {
                    text.lines()
                        .map(|l| l.trim().trim_end_matches('/').to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            releases.reverse();
            let history = answer
                .get(&format!("{i}:history"))
                .map(|text| parse_history(text))
                .unwrap_or_default();
            let (mut containers, docker_reachable) = if reachable {
                answer
                    .get(&format!("{i}:compose"))
                    .map(|raw| parse_compose_ps(raw))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };
            let mut status = Status {
                ingress,
                containers: None,
                docker_reachable,
                service: request.service,
                target: request.target,
                host: request.host,
                reachable,
                live,
                releases,
                history,
                history_path: state.history_path,
                live_path: state.live_path,
                releases_dir: state.releases_dir,
                previous_marker: state.previous_marker,
                backups: state.backups_dir.as_ref().map(|_| {
                    answer
                        .get(&format!("{i}:backups"))
                        .map(|text| sort_backups(text))
                        .unwrap_or_default()
                }),
                backups_dir: state.backups_dir,
                previous: answer
                    .get(&format!("{i}:previous"))
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty()),
            };
            let owned_images = request
                .compose
                .as_ref()
                .map(|c| c.images.as_slice())
                .unwrap_or(&[]);
            if let Some(list) = containers.as_mut() {
                if let Some(raw_restarts) = answer.get(&format!("{i}:compose:restarts")) {
                    let restarts = parse_compose_restarts(raw_restarts);
                    apply_restart_counts(list, &restarts);
                }
                if let Some(recorded) = status.live_deploy() {
                    apply_image_drift(list, recorded, &status.history, owned_images);
                }
            }
            status.containers = containers;
            status
        })
        .collect()
}

fn heading(status: &Status) -> String {
    if status.host.is_empty() {
        format!("▸ {}  → {}", status.service, status.target)
    } else {
        format!(
            "▸ {}  → {} ({})",
            status.service, status.target, status.host
        )
    }
}

/// `deliver status` — one block per service, answering "what is live?".
pub fn render_status(statuses: &[Status]) -> String {
    let mut out = String::new();
    for status in statuses {
        out.push_str(&heading(status));
        out.push('\n');
        if !status.reachable {
            out.push_str("    could not read the target\n\n");
            continue;
        }
        if !status.ingress.is_empty() {
            out.push_str(&render_ingress(&status.ingress));
            if status.history_path.is_empty() {
                out.push('\n');
                continue;
            }
        }
        match (&status.live, status.live_deploy_id()) {
            (Live::Unmanaged, _) => {
                out.push_str("    live          a plain directory, not a release symlink\n");
                out.push_str("                  the next deploy migrates it into releases/\n");
            }
            (_, None) => {
                out.push_str("    live          nothing deployed yet\n");
            }
            (_, Some(id)) => {
                let recorded = status.live_deploy();
                let release = recorded
                    .map(|d| d.release.clone())
                    .unwrap_or_else(|| "unknown release".into());
                let source = if status.live_is_recorded() {
                    ""
                } else {
                    "  (newest recorded deploy — this deployer has no live symlink)"
                };
                out.push_str(&format!("    live          {release} · {id}{source}\n"));
                match recorded {
                    Some(d) => out.push_str(&format!(
                        "    deployed      {} · sha {}\n",
                        d.at,
                        short_sha(&d.sha)
                    )),
                    None => out.push_str(&format!(
                        "    deployed      not recorded in {}\n",
                        status.history_path
                    )),
                }
            }
        }
        if let (Some(live_path), Live::Release { path }) = (&status.live_path, &status.live) {
            out.push_str(&format!("    path          {live_path} → {path}\n"));
        }
        out.push_str(&render_compose(status));
        if status.releases_dir.is_some() {
            out.push_str(&format!(
                "    retained      {} release(s)\n",
                status.releases.len()
            ));
        }
        if let Some(backups) = &status.backups {
            match backups.first() {
                Some(newest) => out.push_str(&format!(
                    "    backups       {} file(s) in {} · newest {newest}\n",
                    backups.len(),
                    status.backups_dir.as_deref().unwrap_or_default()
                )),
                None => out.push_str(&format!(
                    "    backups       none yet in {}\n",
                    status.backups_dir.as_deref().unwrap_or_default()
                )),
            }
        }
        out.push_str(&format!(
            "    history       {} deploy(s) recorded\n\n",
            status.history.len()
        ));
    }
    crate::secrets::redact::scrub(out.trim_end())
}

/// Shorten a full sha for a column, leaving anything else alone.
fn short_sha(sha: &str) -> String {
    if sha.len() > 10 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        sha[..10].to_string()
    } else {
        sha.to_string()
    }
}

/// `deliver history` — the recorded deploys, newest first.
///
/// `limit` of 0 means every row.
pub fn render_history(statuses: &[Status], limit: usize) -> String {
    let mut out = String::new();
    for status in statuses {
        out.push_str(&heading(status));
        out.push('\n');
        if !status.reachable {
            out.push_str("    could not read the target\n\n");
            continue;
        }
        if status.history_path.is_empty() && !status.ingress.is_empty() {
            out.push_str(&render_ingress(&status.ingress));
            out.push_str("    nginx installs do not record release history\n\n");
            continue;
        }
        if status.history.is_empty() {
            out.push_str(&format!(
                "    no deploys recorded in {}\n\n",
                status.history_path
            ));
            continue;
        }
        let live = status.live_deploy_id();
        out.push_str("       #  deploy                    release       sha         when\n");
        let shown = if limit == 0 {
            status.history.len()
        } else {
            limit.min(status.history.len())
        };
        // A rollback row repeats the id it restored, so only the newest row
        // for the live id carries the marker.
        let mut marked = false;
        for deploy in status.history.iter().take(shown) {
            let marker = if !marked && Some(&deploy.deploy_id) == live.as_ref() {
                marked = true;
                "  ← live"
            } else {
                ""
            };
            let kind = if deploy.rollback { "  (rollback)" } else { "" };
            out.push_str(&format!(
                "    {:>4}  {:<24}  {:<12}  {:<10}  {}{kind}{marker}\n",
                deploy.number,
                deploy.deploy_id,
                deploy.release,
                short_sha(&deploy.sha),
                deploy.at,
            ));
        }
        if shown < status.history.len() {
            out.push_str(&format!(
                "          … {} older deploy(s) — pass --limit 0 for all\n",
                status.history.len() - shown
            ));
        }
        out.push('\n');
    }
    crate::secrets::redact::scrub(out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_answers_preserve_failures_and_the_worst_certificate() {
        let states = vec![IngressState {
            conf_path: "/etc/nginx/sites-available/example".into(),
            enabled_path: "/etc/nginx/sites-enabled/example".into(),
            cert_names: vec![
                "example.com".into(),
                "api.example.com".into(),
                "old.example.com".into(),
                "missing.example.com".into(),
            ],
        }];
        let answers = BTreeMap::from([
            ("0:ingress:0".into(), "yes\nno".into()),
            ("0:nginx".into(), "no".into()),
            ("0:cert:0:0".into(), "61".into()),
            ("0:cert:0:1".into(), "7".into()),
            ("0:cert:0:2".into(), "expired".into()),
            ("0:cert:0:3".into(), "missing".into()),
        ]);
        let ingress = parse_ingress(0, &states, &answers);
        assert_eq!(ingress[0].installed, Some(true));
        assert_eq!(ingress[0].enabled, Some(false));
        let text = render_ingress(&ingress);
        assert!(text.contains("nginx -t      fails"), "{text}");
        assert!(text.contains("7 day(s) remaining"), "{text}");
        assert!(text.contains("EXPIRED"), "{text}");
        let mut status = Status {
            ingress,
            containers: None,
            docker_reachable: None,
            service: "nginx".into(),
            target: "box".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::NotApplicable,
            releases: Vec::new(),
            history: Vec::new(),
            history_path: String::new(),
            live_path: None,
            releases_dir: None,
            previous_marker: None,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        let summary = certificate_summary(std::slice::from_ref(&status)).unwrap();
        assert!(
            summary.contains("EXPIRED")
                && summary.contains("missing/unreadable")
                && summary.contains("7 day(s)"),
            "{summary}"
        );
        assert!(!render_status(std::slice::from_ref(&status)).contains("nothing deployed"));
        assert!(render_history(std::slice::from_ref(&status), 0)
            .contains("do not record release history"));
        status.ingress = parse_ingress(0, &states, &BTreeMap::new());
        assert_eq!(status.ingress[0].installed, None);
        assert_eq!(
            status.ingress[0].certificates[0].health,
            CertificateHealth::Unreadable
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkend_probe_reads_a_real_certificate_without_changing_it() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("deliver-checkend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let result = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "3",
                "-subj",
                "/CN=example.com",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let original = std::fs::read(&cert).unwrap();
        let states = [IngressState {
            conf_path: "/unused".into(),
            enabled_path: "/unused".into(),
            cert_names: vec!["example.com".into()],
        }];
        let mut probes = ingress_probes(0, &states, "");
        let probe = probes.pop().unwrap();
        let script = probe.snippet.replace(
            "/etc/letsencrypt/live/example.com/fullchain.pem",
            cert.to_str().unwrap(),
        );
        let run = || Command::new("sh").args(["-c", &script]).output().unwrap();
        let output = run();
        let days: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        assert!((2..=3).contains(&days), "{days}");
        assert_eq!(std::fs::read(&cert).unwrap(), original);
        std::fs::write(&cert, "not a certificate").unwrap();
        assert_eq!(String::from_utf8_lossy(&run().stdout).trim(), "unreadable");
        std::fs::remove_file(&cert).unwrap();
        assert_eq!(String::from_utf8_lossy(&run().stdout).trim(), "missing");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn state() -> ReleaseState {
        ReleaseState {
            history_path: "/var/app/.deliver/history.tsv".into(),
            live_path: Some("/var/app/web".into()),
            releases_dir: Some("/var/app/releases".into()),
            previous_marker: Some("/var/app/releases/.deliver-previous".into()),
            backups_dir: None,
        }
    }

    #[test]
    fn backups_sort_newest_first_by_stamp_not_by_name() {
        let listed = "media-20260401T000000Z.tar.gz\n\
                      media-thumbs-20260101T000000Z.tar.gz\n\
                      predeploy-20260301T000000Z.dump\n\
                      predeploy-20260501T000000Z.dump\n";
        assert_eq!(
            sort_backups(listed),
            vec![
                "predeploy-20260501T000000Z.dump",
                "media-20260401T000000Z.tar.gz",
                "predeploy-20260301T000000Z.dump",
                "media-thumbs-20260101T000000Z.tar.gz",
            ]
        );
    }

    #[test]
    fn history_rows_come_back_newest_first() {
        let text = "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n\
                    2\t20260202-b\tv0.2.0\tdef\t2026-02-02T00:00:00Z\n";
        let rows = parse_history(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].deploy_id, "20260202-b");
        assert_eq!(rows[0].release, "v0.2.0");
        assert_eq!(rows[1].deploy_id, "20260101-a");
    }

    #[test]
    fn a_half_written_row_is_skipped_rather_than_guessed_at() {
        let text = "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n2\t2026020";
        let rows = parse_history(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].deploy_id, "20260101-a");
    }

    #[test]
    fn framed_answers_round_trip_through_one_batched_read() {
        let marker = "__NONCE__";
        let probes = vec![
            Probe {
                key: "0:history".into(),
                snippet: "true".into(),
            },
            Probe {
                key: "0:live".into(),
                snippet: "true".into(),
            },
        ];
        let script = probe_script(&probes, marker);
        assert!(script.contains("'0:history'"), "{script}");
        // What the target would print for that script.
        let output = "__NONCE__ BEGIN 0:history\na\tb\n\n__NONCE__ END\n\
                      __NONCE__ BEGIN 0:live\nlink /var/app/releases/x\n__NONCE__ END\n";
        let found = parse_probes(output, marker);
        assert_eq!(found.get("0:history").map(String::as_str), Some("a\tb"));
        assert_eq!(
            found.get("0:live").map(String::as_str),
            Some("link /var/app/releases/x")
        );
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_its_last_line() {
        let found = parse_probes("N BEGIN 0:history\nonly line\nN END\n", "N");
        assert_eq!(
            found.get("0:history").map(String::as_str),
            Some("only line")
        );
    }

    #[test]
    fn the_live_symlink_names_the_deploy_that_is_live() {
        let status = Status {
            ingress: Vec::new(),
            containers: None,
            docker_reachable: None,
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::Release {
                path: "/var/app/releases/20260202-b".into(),
            },
            releases: vec!["20260202-b".into(), "20260101-a".into()],
            history: parse_history(
                "1\t20260101-a\tv0.1.0\tabc\t2026-01-01T00:00:00Z\n\
                 2\t20260202-b\tv0.2.0\tdef0123456789\t2026-02-02T00:00:00Z\n",
            ),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        assert_eq!(status.live_deploy_id().as_deref(), Some("20260202-b"));
        assert_eq!(status.live_deploy().unwrap().release, "v0.2.0");
        let text = render_status(std::slice::from_ref(&status));
        assert!(text.contains("v0.2.0 · 20260202-b"), "{text}");
        assert!(text.contains("2 release(s)"), "{text}");
        assert!(text.contains("sha def0123456"), "{text}");
        let history = render_history(&[status], 1);
        assert!(history.contains("← live"), "{history}");
        assert!(history.contains("1 older deploy(s)"), "{history}");
    }

    #[test]
    fn a_live_release_missing_from_history_says_so_rather_than_inventing_one() {
        let status = Status {
            ingress: Vec::new(),
            containers: None,
            docker_reachable: None,
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::Release {
                path: "/var/app/releases/20260303-c".into(),
            },
            releases: vec!["20260303-c".into()],
            history: Vec::new(),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        let text = render_status(&[status]);
        assert!(text.contains("unknown release · 20260303-c"), "{text}");
        assert!(text.contains("not recorded in"), "{text}");
    }

    #[test]
    fn an_unreachable_target_is_reported_not_read_as_empty() {
        let status = Status {
            ingress: Vec::new(),
            containers: None,
            docker_reachable: None,
            service: "web".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: false,
            live: Live::Missing,
            releases: Vec::new(),
            history: Vec::new(),
            history_path: state().history_path,
            live_path: state().live_path,
            releases_dir: state().releases_dir,
            previous_marker: state().previous_marker,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        let text = render_status(&[status]);
        assert!(text.contains("could not read the target"), "{text}");
        assert!(!text.contains("nothing deployed yet"), "{text}");
    }

    #[test]
    fn a_deployer_with_no_symlink_falls_back_to_the_newest_recorded_deploy() {
        let status = Status {
            ingress: Vec::new(),
            containers: None,
            docker_reachable: None,
            service: "stack".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::NotApplicable,
            releases: Vec::new(),
            history: parse_history("7\t20260404-d\tv1.0.0\tfeed\t2026-04-04T00:00:00Z\n"),
            history_path: state().history_path,
            live_path: None,
            releases_dir: None,
            previous_marker: None,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        assert!(!status.live_is_recorded());
        let text = render_status(&[status]);
        assert!(text.contains("v1.0.0 · 20260404-d"), "{text}");
        assert!(text.contains("no live symlink"), "{text}");
        // No release directory means no retained count to claim.
        assert!(!text.contains("retained"), "{text}");
    }

    #[test]
    fn a_sixth_field_marks_a_rollback_row() {
        let rows = parse_history(
            "1\ta\tv1\tsha1\t2026-01-01T00:00:00Z\n\
             2\tb\tv2\tsha2\t2026-01-02T00:00:00Z\n\
             3\ta\tv1\tsha1\t2026-01-03T00:00:00Z\trollback\n",
        );
        assert!(rows[0].rollback);
        assert!(!rows[1].rollback);
        assert!(!rows[2].rollback);
    }

    fn compose_status(
        history_tsv: &str,
        containers: Option<Vec<Container>>,
        docker_reachable: Option<bool>,
    ) -> Status {
        compose_status_with_restarts(history_tsv, containers, docker_reachable, "")
    }

    fn compose_status_with_restarts(
        history_tsv: &str,
        containers: Option<Vec<Container>>,
        docker_reachable: Option<bool>,
        restarts_raw: &str,
    ) -> Status {
        let history = parse_history(history_tsv);
        let mut status = Status {
            ingress: Vec::new(),
            containers: None,
            docker_reachable,
            service: "stack".into(),
            target: "production".into(),
            host: "example.com".into(),
            reachable: true,
            live: Live::NotApplicable,
            releases: Vec::new(),
            history,
            history_path: "/var/app/.deliver/history.tsv".into(),
            live_path: None,
            releases_dir: None,
            previous_marker: None,
            backups: None,
            backups_dir: None,
            previous: None,
        };
        let mut containers = containers;
        if let Some(list) = containers.as_mut() {
            let restarts = parse_compose_restarts(restarts_raw);
            apply_restart_counts(list, &restarts);
            if let Some(recorded) = status.live_deploy() {
                let owned = vec!["myapp".to_string()];
                apply_image_drift(list, recorded, &status.history, &owned);
            }
        }
        status.containers = containers;
        status
    }

    #[test]
    fn compose_ps_parses_ndjson_and_renders_running_services() {
        let raw = "ok\n\
            {\"Service\":\"api\",\"Name\":\"app-api-1\",\"Image\":\"myapp:20261015T120000Z-bbbbbbb\",\"State\":\"running\",\"Status\":\"Up 2 hours (healthy)\",\"Health\":\"healthy\"}\n\
            {\"Service\":\"worker\",\"Name\":\"app-worker-1\",\"Image\":\"myapp:20261015T120000Z-bbbbbbb\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n\
            {\"Service\":\"db\",\"Name\":\"app-db-1\",\"Image\":\"postgres:16-alpine\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n\
            {\"Service\":\"cache\",\"Name\":\"app-cache-1\",\"Image\":\"redis:v7.4\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n";
        let (containers, reachable) = parse_compose_ps(raw);
        assert_eq!(reachable, Some(true));
        let status = compose_status(
            "1\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            containers,
            reachable,
        );
        let text = render_status(&[status]);
        assert!(
            text.contains(
                "services      4/4 running (api healthy, worker up 2h, db up 2h, cache up 2h)"
            ),
            "{text}"
        );
        // Unchanged sidecar `redis:v7.4` must not be flagged as needing app release `v1.2.0`.
        assert!(!text.contains("drift"), "{text}");
    }

    #[test]
    fn compose_ps_parses_json_array_and_reports_restarting_exited_and_drift() {
        let raw = "ok\n[\
            {\"Service\":\"api\",\"Name\":\"app-api-1\",\"Image\":\"myapp:20261001T120000Z-aaaaaaa\",\"State\":\"running\",\"Status\":\"Up 3 days (healthy)\"},\
            {\"Service\":\"worker\",\"Name\":\"app-worker-1\",\"Image\":\"myapp:20261015T120000Z-bbbbbbb\",\"State\":\"restarting\",\"Status\":\"Restarting (1) 5 seconds ago\"},\
            {\"Service\":\"jobs\",\"Name\":\"app-jobs-1\",\"Image\":\"myapp:20261015T120000Z-bbbbbbb\",\"State\":\"exited\",\"Status\":\"Exited (1) 2 minutes ago\",\"ExitCode\":1},\
            {\"Service\":\"cache\",\"Name\":\"app-cache-1\",\"Image\":\"redis:v7.4\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\
        ]";
        let (containers, reachable) = parse_compose_ps(raw);
        assert_eq!(reachable, Some(true));
        let status = compose_status_with_restarts(
            "1\t20261001T120000Z-aaaaaaa\tv1.1.0\taaaaaaa1234\t2026-10-01T12:00:00Z\n\
             2\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            containers,
            reachable,
            "/app-api-1 0\n/app-worker-1 4\n/app-jobs-1 0\n/app-cache-1 0\n",
        );
        let text = render_status(std::slice::from_ref(&status));
        assert!(
            text.contains(
                "services      2/4 running (api healthy, worker restarting ×4, jobs exited (1), cache up 2h)"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "drift         api runs myapp:20261001T120000Z-aaaaaaa but the record says myapp:20261015T120000Z-bbbbbbb"
            ),
            "{text}"
        );
        // Sidecar image (`redis:v7.4`) and matching `worker`/`jobs` must not be flagged.
        assert!(!text.contains("cache runs"), "{text}");
        assert!(!text.contains("worker runs"), "{text}");
        assert!(!text.contains("jobs runs"), "{text}");
        let json: serde_json::Value = serde_json::to_value(&status).unwrap();
        assert_eq!(json["docker_reachable"], true);
        assert_eq!(json["containers"].as_array().unwrap().len(), 4);
        assert_eq!(json["containers"][1]["restart_count"], 4);
        assert_eq!(json["containers"][2]["exit_code"], 1);
        assert!(json["containers"][0]["image_drift"]
            .as_str()
            .unwrap()
            .contains("20261001T120000Z-aaaaaaa"));
    }

    #[test]
    fn compose_restarting_without_inspect_count_renders_cleanly() {
        let raw = "ok\n\
            {\"Service\":\"worker\",\"Name\":\"app-worker-1\",\"Image\":\"myapp:20261015T120000Z-bbbbbbb\",\"State\":\"restarting\",\"Status\":\"Restarting (1) 5 seconds ago\"}\n";
        let (containers, reachable) = parse_compose_ps(raw);
        let status = compose_status(
            "1\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            containers,
            reachable,
        );
        let text = render_status(std::slice::from_ref(&status));
        assert!(
            text.contains("services      0/1 running (worker restarting)"),
            "{text}"
        );
        let json: serde_json::Value = serde_json::to_value(&status).unwrap();
        assert!(json["containers"][0].get("restart_count").is_none());
    }

    #[test]
    fn compose_drift_ignores_sidecars_with_semver_sha_or_deploy_id_tags() {
        let raw = "ok\n\
            {\"Service\":\"api\",\"Name\":\"app-api-1\",\"Image\":\"myapp:v1.1.0\",\"State\":\"running\",\"Status\":\"Up 1 hour (healthy)\",\"Health\":\"healthy\"}\n\
            {\"Service\":\"cache\",\"Name\":\"app-cache-1\",\"Image\":\"redis:v7.4\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n\
            {\"Service\":\"exporter\",\"Name\":\"app-exporter-1\",\"Image\":\"vendor/exporter:deadbeef\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n\
            {\"Service\":\"cron\",\"Name\":\"app-cron-1\",\"Image\":\"vendor/cron:20261001T120000Z-aaaaaaa\",\"State\":\"running\",\"Status\":\"Up 2 hours\"}\n";
        let (containers, reachable) = parse_compose_ps(raw);
        let status = compose_status(
            "1\t20261001T120000Z-aaaaaaa\tv1.1.0\taaaaaaa1234\t2026-10-01T12:00:00Z\n\
             2\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            containers,
            reachable,
        );
        let text = render_status(&[status]);
        assert!(
            text.contains("drift         api runs myapp:v1.1.0 but the record says myapp:v1.2.0"),
            "{text}"
        );
        assert!(!text.contains("cache runs"), "{text}");
        assert!(!text.contains("exporter runs"), "{text}");
        assert!(!text.contains("cron runs"), "{text}");
    }

    #[test]
    fn compose_ps_distinguishes_nothing_running_from_unreachable_docker_daemon() {
        let (empty_containers, empty_reachable) = parse_compose_ps("ok\n");
        assert_eq!(empty_reachable, Some(true));
        assert_eq!(empty_containers, Some(Vec::new()));
        let empty = compose_status(
            "1\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            empty_containers,
            empty_reachable,
        );
        let empty_text = render_status(std::slice::from_ref(&empty));
        assert!(
            empty_text.contains("services      nothing running"),
            "{empty_text}"
        );
        assert!(!empty_text.contains("daemon unreachable"), "{empty_text}");

        let (arr_empty, arr_reachable) = parse_compose_ps("ok\n[]\n");
        assert_eq!(arr_reachable, Some(true));
        assert_eq!(arr_empty, Some(Vec::new()));

        let (err_containers, err_reachable) = parse_compose_ps("err\n");
        assert_eq!(err_reachable, Some(false));
        assert_eq!(err_containers, None);
        let down = compose_status(
            "1\t20261015T120000Z-bbbbbbb\tv1.2.0\tbbbbbbb1234\t2026-10-15T12:00:00Z\n",
            err_containers,
            err_reachable,
        );
        let down_text = render_status(std::slice::from_ref(&down));
        assert!(
            down_text.contains("docker        daemon unreachable"),
            "{down_text}"
        );
        assert!(!down_text.contains("nothing running"), "{down_text}");
        let json: serde_json::Value = serde_json::to_value(&down).unwrap();
        assert_eq!(json["docker_reachable"], false);
        assert!(json.get("containers").is_none());
    }
}
