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
use crate::deployers::{IngressState, ReleaseState};
use crate::plan::ServicePlan;
use crate::remote::{capture, nonce, shell_quote};
use serde::Serialize;
use std::collections::BTreeMap;

/// One service on one host whose deploy state can be read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub service: String,
    pub target: String,
    pub host: String,
    pub state: Option<ReleaseState>,
    pub ingress: Vec<IngressState>,
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
            (state.is_some() || !ingress.is_empty()).then(|| Request {
                service: sp.service.clone(),
                target: sp.target.clone(),
                host: sp.host.clone(),
                state,
                ingress,
            })
        })
        .collect()
}

/// Release consumers (shipping and rollback) must not treat ingress as a release.
pub fn collect_releases(plan: &[ServicePlan]) -> Vec<Request> {
    collect(plan)
        .into_iter()
        .filter(|r| r.state.is_some())
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
            Status {
                ingress,
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
            }
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
}
