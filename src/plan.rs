//! Compile a config into an ordered plan (topological by `needs`) and render it.

use crate::config::{Config, Service};
use crate::deployers::{compile_service, PlanContext, PlannedStep};
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct ServicePlan {
    pub service: String,
    pub target: String,
    /// Which host of that target these steps run against.
    pub host: String,
    pub steps: Vec<PlannedStep>,
    /// These steps run after Delivery Boy creates and pushes the release tag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_tag: bool,
}

/// Kahn's algorithm over `needs`; errors on cycles.
pub fn topo_order(services: &BTreeMap<String, Service>) -> Result<Vec<String>> {
    let mut pending: BTreeMap<&String, BTreeSet<&String>> = services
        .iter()
        .map(|(name, svc)| (name, svc.needs.iter().collect()))
        .collect();
    let mut order: Vec<String> = Vec::new();

    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| (*name).clone())
            .collect();
        if ready.is_empty() {
            let cyclic: Vec<String> = pending.keys().map(|k| (*k).to_string()).collect();
            bail!("dependency cycle among services: {}", cyclic.join(", "));
        }
        for name in ready {
            pending.remove(&name);
            for deps in pending.values_mut() {
                deps.remove(&name);
            }
            order.push(name);
        }
    }
    Ok(order)
}

/// Every service this run acts on, in dependency order.
///
/// `only` overrides `enabled: false` — naming a service explicitly is how you
/// cut a partial release — and an empty `only` means every enabled service.
pub fn selected(config: &Config, only: &[String]) -> Result<Vec<String>> {
    Ok(topo_order(&config.services)?
        .into_iter()
        .filter(|name| {
            let named = only.contains(name);
            if !only.is_empty() && !named {
                return false;
            }
            config.services[name].enabled || named
        })
        .collect())
}

/// The (target, host) pairs the run selects but `compiled` does not contain —
/// the services whose compile failed.
///
/// Preflight probes these too: a service that will not compile is no reason to
/// hide an unreachable host, and reporting both in one pass is the whole point
/// of the command. Derived from the config, because the plan is what broke.
pub fn unplanned_hosts(
    config: &Config,
    only: &[String],
    compiled: &[ServicePlan],
) -> Vec<(String, String)> {
    let Ok(names) = selected(config, only) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for name in names {
        if compiled.iter().any(|sp| sp.service == name) {
            continue;
        }
        // An unresolvable target is already reported as the compile error.
        if let Ok((target_name, target)) = config.target_for(&config.services[&name]) {
            for host in target.hosts() {
                pairs.push((target_name.clone(), host.clone()));
            }
        }
    }
    pairs
}

/// A compiled plan plus the services that would not compile.
pub struct Compilation {
    pub plan: Vec<ServicePlan>,
    /// One entry per failed service, in plan order. [`build`] turns the first
    /// into its error; `deliver preflight` reports all of them as findings so
    /// the rest of its checks still run.
    pub errors: Vec<String>,
}

pub fn build(
    config: &Config,
    only: &[String],
    repo_root: &std::path::Path,
    version: &crate::version::DeployVersion,
) -> Result<Vec<ServicePlan>> {
    let compiled = compile(config, only, repo_root, version)?;
    match compiled.errors.into_iter().next() {
        // Same message, same first-failure ordering as before this returned a
        // partial plan: every caller but preflight still treats one broken
        // service as a broken run.
        Some(first) => bail!("{first}"),
        None => Ok(compiled.plan),
    }
}

/// Compile every selected service, collecting per-service failures instead of
/// stopping at the first. Errors that are not a service's fault — an
/// unreadable secret provider, a dependency cycle — still abort.
pub fn compile(
    config: &Config,
    only: &[String],
    repo_root: &std::path::Path,
    version: &crate::version::DeployVersion,
) -> Result<Compilation> {
    let resolver = std::rc::Rc::new(crate::secrets::resolver(config, repo_root)?);
    // Learn this run's secret values before any of them can reach stdout.
    crate::secrets::prime_redaction(&resolver);
    let mut plan = Vec::new();
    let mut errors = Vec::new();
    for name in selected(config, only)? {
        let service = &config.services[&name];
        let (target_name, target) = match config.target_for(service) {
            Ok(pair) => pair,
            Err(e) => {
                errors.push(format!("service '{name}': {e}"));
                continue;
            }
        };
        // A target may name several hosts; the service is planned once per host.
        // One host's failure is the service's failure — the config is the same
        // for all of them — so it is recorded once and the rest are skipped.
        let mut compiled_hosts = Vec::new();
        let mut failed = None;
        for host in target.hosts().to_vec() {
            let ctx = PlanContext {
                secrets: resolver.clone(),
                work_dir: crate::version::run_scratch(&config.app, &version.id)
                    .to_string_lossy()
                    .to_string(),
                app: config.app.clone(),
                target: target.clone(),
                host: host.clone(),
                sudo: target.uses_sudo(),
                repo_root: repo_root.to_path_buf(),
                version: version.clone(),
            };
            match compile_service(config, service, &ctx) {
                Ok(steps) => compiled_hosts.push(ServicePlan {
                    service: name.clone(),
                    target: target_name.clone(),
                    host,
                    steps,
                    after_tag: false,
                }),
                Err(e) => {
                    failed = Some(format!("service '{name}': {e}"));
                    break;
                }
            }
        }
        match failed {
            // A half-planned service is worse than none: preflight would probe
            // its hosts as if they were covered.
            Some(problem) => errors.push(problem),
            None => plan.extend(compiled_hosts),
        }
    }

    // A partial service deploy must not publish a whole-app release. Full
    // deploys append these steps to the plan so they remain visible in `plan`
    // and preflight, but the command runner holds them until the tag exists.
    // Skipped once a service has failed: the run cannot reach a tag anyway, and
    // this keeps `build`'s error the first service's, exactly as it always was.
    if only.is_empty() && errors.is_empty() {
        if let Some(versioning) = &config.versioning {
            if !versioning.after_tag.is_empty() {
                let target_name = config
                    .defaults
                    .target
                    .clone()
                    .or_else(|| config.targets.keys().next().cloned())
                    .ok_or_else(|| anyhow::anyhow!("versioning.after_tag needs a target"))?;
                let target = config
                    .targets
                    .get(&target_name)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown target '{target_name}'"))?;
                let host = target.hosts()[0].clone();
                let ctx = PlanContext {
                    secrets: resolver,
                    work_dir: crate::version::run_scratch(&config.app, &version.id)
                        .to_string_lossy()
                        .to_string(),
                    app: config.app.clone(),
                    target,
                    host: host.clone(),
                    sudo: false,
                    repo_root: repo_root.to_path_buf(),
                    version: version.clone(),
                };
                let steps = versioning
                    .after_tag
                    .iter()
                    .map(|raw| crate::deployers::compile_raw_step(raw, &ctx))
                    .collect::<Result<Vec<_>>>()?;
                plan.push(ServicePlan {
                    service: "after tag".to_string(),
                    target: target_name,
                    host,
                    steps,
                    after_tag: true,
                });
            }
        }
    }
    Ok(Compilation { plan, errors })
}

pub fn render(plan: &[ServicePlan]) -> String {
    let mut out = String::new();
    for sp in plan {
        out.push_str(&format!(
            "▸ {}  → {}  ({} steps)\n",
            sp.service,
            sp.target,
            sp.steps.len()
        ));
        for (i, step) in sp.steps.iter().enumerate() {
            let detail = step.detail();
            let label = &step.label;
            let suffix = if detail.is_empty() || label.trim_start_matches("$ ") == detail {
                String::new()
            } else {
                format!("  — {detail}")
            };
            out.push_str(&format!(
                "  {:>2}. [{}] {label}{suffix}\n",
                i + 1,
                step.type_name()
            ));
        }
        out.push('\n');
    }
    crate::secrets::redact::scrub(out.trim_end())
}
