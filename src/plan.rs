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

pub fn build(
    config: &Config,
    only: &[String],
    repo_root: &std::path::Path,
    version: &crate::version::DeployVersion,
) -> Result<Vec<ServicePlan>> {
    let resolver = std::rc::Rc::new(crate::secrets::resolver(config, repo_root)?);
    let mut plan = Vec::new();
    for name in topo_order(&config.services)? {
        let service = &config.services[&name];
        let named = only.contains(&name);
        if !only.is_empty() && !named {
            continue;
        }
        // `enabled: false` is the default-off switch; naming a service explicitly
        // with --service overrides it (that's how you cut a release).
        if !service.enabled && !named {
            continue;
        }
        let (target_name, target) = config.target_for(service)?;
        // A target may name several hosts; the service is planned once per host.
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
            let steps = compile_service(config, service, &ctx)
                .map_err(|e| anyhow::anyhow!("service '{name}': {e}"))?;
            plan.push(ServicePlan {
                service: name.clone(),
                target: target_name.clone(),
                host,
                steps,
            });
        }
    }
    Ok(plan)
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
    out.trim_end().to_string()
}
