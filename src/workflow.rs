//! `deliver init --from-workflow` — scaffold a config from a GitHub Actions
//! workflow.
//!
//! An app worth converting almost always has a workflow that already encodes
//! its real deploy recipe: the build commands, the SSH target, the secret
//! names. `detect.rs` infers a project's *type* from the tree; this reads the
//! deploy *intent* the workflow declares, and maps what it can:
//!
//! - `run:` steps become `command:` steps,
//! - `appleboy/ssh-action` scripts become `ssh:` steps, and its host, user and
//!   port become the target,
//! - `appleboy/scp-action` becomes an `scp` command to that target,
//! - `docker/build-push-action` becomes the `docker-compose` service plain
//!   `init` would scaffold, when the repo has a compose file to build it from,
//! - `secrets.*` references become the `secrets:` block,
//! - `on: push: tags:` (or `release:`) becomes a tag release, and a push to a
//!   branch becomes a commit-versioned release from that branch.
//!
//! Everything else is reported by name, with the reason, rather than dropped:
//! a scaffold that silently lost a step looks complete and is not.

use crate::detect::{self, quote_scalar, Finding};
use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};
use std::collections::BTreeSet;

/// One step of a scaffolded `commands` service.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Command(String),
    Ssh(String),
    /// Rendered as an `scp` command once the target is known.
    Scp {
        sources: Vec<String>,
        target: String,
    },
}

/// A workflow job that kept at least one step.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub name: String,
    pub needs: Vec<String>,
    pub steps: Vec<Step>,
}

/// What triggered a release in the workflow.
#[derive(Debug, Clone, PartialEq)]
pub enum Release {
    /// `on: push: tags:` or `on: release:` — the tag is the version.
    Tag,
    /// `on: push: branches:` — every push to this branch shipped.
    Branch(String),
}

/// A step the scaffold could not carry over, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Unmapped {
    pub step: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    pub jobs: Vec<Job>,
    /// Literal connection details from an ssh/scp action.
    pub host: Option<String>,
    pub user: Option<String>,
    pub port: Option<u64>,
    /// Secrets the workflow's own steps read — declared in `secrets:`.
    pub secrets: BTreeSet<String>,
    /// Secrets the workflow only used to reach the box or a registry. `deliver`
    /// uses the operator's own ssh config and docker login, so these are named
    /// in the report and left out of the config.
    pub connection_secrets: BTreeSet<String>,
    pub release: Option<Release>,
    /// A `docker/build-push-action` step, by its label, if there was one.
    pub image_build: Option<String>,
    /// Steps that became config.
    pub mapped: Vec<String>,
    /// CI plumbing with no deploy intent (checkout, toolchain setup, caches).
    pub skipped: Vec<String>,
    pub unmapped: Vec<Unmapped>,
    /// Semantics the mapping changed, said out loud.
    pub notes: Vec<String>,
}

/// Actions that set up the CI runner rather than deploy anything. On a laptop
/// the checkout is the working tree and the toolchain is already installed.
const PLUMBING: &[&str] = &[
    "actions/checkout",
    "actions/cache",
    "actions/upload-artifact",
    "actions/download-artifact",
    "dtolnay/rust-toolchain",
    "swatinem/rust-cache",
    "docker/setup-buildx-action",
    "docker/setup-qemu-action",
    "docker/login-action",
    "webfactory/ssh-agent",
    "shimataro/ssh-key-action",
    "pnpm/action-setup",
    "oven-sh/setup-bun",
    "peaceiris/actions-hugo",
];

/// `with:` keys of the ssh/scp actions that only describe the connection.
const CONNECTION_KEYS: &[&str] = &[
    "host",
    "username",
    "port",
    "key",
    "key_path",
    "password",
    "passphrase",
    "fingerprint",
    "proxy_host",
    "proxy_username",
    "proxy_key",
    "proxy_password",
];

fn is_plumbing(action: &str) -> bool {
    action.starts_with("actions/setup-") || PLUMBING.contains(&action)
}

/// Every `${{ … }}` expression in `text`, trimmed.
fn expressions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else { break };
        found.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    found
}

/// The `secrets.NAME` references in `text`. `GITHUB_TOKEN` is the runner's own
/// credential, not the app's.
fn secret_refs(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for expr in expressions(text) {
        let mut rest = expr.as_str();
        while let Some(at) = rest.find("secrets.") {
            let tail = &rest[at + "secrets.".len()..];
            let name: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !name.is_empty() && name != "GITHUB_TOKEN" {
                names.insert(name.clone());
            }
            rest = &tail[name.len()..];
        }
    }
    names
}

/// Every string anywhere inside `value`, concatenated — for collecting secret
/// references without caring where in a step they sit.
fn all_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Sequence(items) => items.iter().map(all_text).collect::<Vec<_>>().join("\n"),
        Value::Mapping(map) => map
            .iter()
            .map(|(k, v)| format!("{}\n{}", all_text(k), all_text(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Tagged(tagged) => all_text(&tagged.value),
        _ => String::new(),
    }
}

fn get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.to_string()))
}

fn get_str<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    get(map, key).and_then(Value::as_str)
}

/// A scalar `with:` value as text — `port: 2222` is a number in YAML.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `on:` — YAML 1.1 readers turn a bare `on` key into `true`, so accept both.
fn triggers(doc: &Mapping) -> Option<&Value> {
    get(doc, "on").or_else(|| doc.get(Value::Bool(true)))
}

fn release_of(on: &Value) -> Option<Release> {
    let map = on.as_mapping()?;
    if get(map, "release").is_some() {
        return Some(Release::Tag);
    }
    let push = get(map, "push")?.as_mapping()?;
    if get(push, "tags").is_some() {
        return Some(Release::Tag);
    }
    let branch = get(push, "branches")
        .and_then(Value::as_sequence)
        .and_then(|b| b.first())
        .and_then(Value::as_str)?;
    // A glob is a set of branches, not one to require.
    (!branch.contains(['*', '?', '['])).then(|| Release::Branch(branch.to_string()))
}

/// A short, recognisable name for a step in the report.
fn step_label(job: &str, step: &Mapping) -> String {
    let what = get_str(step, "name")
        .map(str::to_string)
        .or_else(|| get_str(step, "uses").map(str::to_string))
        .or_else(|| {
            get_str(step, "run").map(|run| {
                let first = run.trim().lines().next().unwrap_or("").trim();
                let short: String = first.chars().take(48).collect();
                if short.len() < first.len() {
                    format!("run: {short}…")
                } else {
                    format!("run: {short}")
                }
            })
        })
        .unwrap_or_else(|| "(unnamed step)".to_string());
    format!("{job} · {what}")
}

/// The `export` lines for a step's environment, or why it cannot have one.
///
/// Every literal is exported, read by name or not: tools read their
/// environment implicitly (`npm ci` under `NODE_ENV=production` skips dev
/// dependencies, `aws` finds its keys by name). A secret-valued entry has no
/// equivalent in a `command:` step, so one on the step itself makes the step
/// unmappable; one inherited from the job or workflow does only when the
/// script names it, and is otherwise a note.
fn env_prelude(
    inherited: &[(String, String)],
    own: &[(String, String)],
    script: &str,
    label: &str,
    notes: &mut Vec<String>,
) -> Result<Vec<String>, String> {
    let reads = |name: &str| {
        script.contains(&format!("${name}")) || script.contains(&format!("${{{name}}}"))
    };
    let from_secret = |name: &str, value: &str| {
        format!(
            "sets ${name} from `{}` — `command:` steps have no secret substitution",
            expressions(value).join("`, `")
        )
    };
    let mut lines = Vec::new();
    for (name, value) in inherited {
        if own.iter().any(|(n, _)| n == name) {
            continue;
        }
        if value.contains("${{") {
            if reads(name) {
                return Err(from_secret(name, value));
            }
            notes.push(format!(
                "`{label}` inherited ${name} from `{}`; it is not set in the scaffold, so a tool \
                 that reads it implicitly will not see it",
                expressions(value).join("`, `")
            ));
            continue;
        }
        lines.push(format!("export {name}={}", shell_quote(value)));
    }
    for (name, value) in own {
        if value.contains("${{") {
            return Err(from_secret(name, value));
        }
        lines.push(format!("export {name}={}", shell_quote(value)));
    }
    Ok(lines)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

fn env_of(map: &Mapping) -> Vec<(String, String)> {
    get(map, "env")
        .and_then(Value::as_mapping)
        .map(|env| {
            env.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), scalar_text(v)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// A service name `deliver` accepts: the job id, lowercased, with anything
/// outside `[a-z0-9_-]` turned into `-`.
fn service_name(job: &str) -> String {
    job.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Read a workflow and map what it declares.
pub fn import(text: &str) -> Result<Import> {
    let doc: Value = serde_yaml::from_str(text).context("the workflow is not valid YAML")?;
    let Some(doc) = doc.as_mapping() else {
        bail!("the workflow is not a YAML mapping");
    };
    let Some(jobs) = get(doc, "jobs").and_then(Value::as_mapping) else {
        bail!("the workflow has no `jobs:`");
    };
    let mut out = Import {
        release: triggers(doc).and_then(release_of),
        ..Import::default()
    };
    let workflow_env = env_of(doc);
    if let Some(env) = get(doc, "env") {
        out.secrets.extend(secret_refs(&all_text(env)));
    }

    for (id, job) in jobs {
        let Some(id) = id.as_str() else { continue };
        let Some(job) = job.as_mapping() else {
            continue;
        };
        let name = service_name(id);

        if let Some(uses) = get_str(job, "uses") {
            out.unmapped.push(Unmapped {
                step: format!("{id} · {uses}"),
                reason: "calls a reusable workflow — point --from-workflow at that file instead"
                    .into(),
            });
            out.secrets
                .extend(secret_refs(&all_text(&Value::Mapping(job.clone()))));
            continue;
        }
        if get(job, "strategy").is_some() {
            out.unmapped.push(Unmapped {
                step: format!("{id} (whole job)"),
                reason: "runs as a `strategy:` matrix — a release runs each step once".into(),
            });
            continue;
        }
        if let Some(cond) = get(job, "if").and_then(scalar_text) {
            out.notes.push(format!(
                "job `{id}` ran only `if: {cond}` in the workflow; `deliver` runs it every time"
            ));
        }

        let mut env = workflow_env.clone();
        env.extend(env_of(job));
        if let Some(job_env) = get(job, "env") {
            out.secrets.extend(secret_refs(&all_text(job_env)));
        }
        let job_dir = get(job, "defaults")
            .and_then(Value::as_mapping)
            .and_then(|d| get(d, "run"))
            .and_then(Value::as_mapping)
            .and_then(|r| get_str(r, "working-directory"))
            .map(str::to_string);
        let needs = match get(job, "needs") {
            Some(Value::String(one)) => vec![service_name(one)],
            Some(Value::Sequence(many)) => many
                .iter()
                .filter_map(Value::as_str)
                .map(service_name)
                .collect(),
            _ => Vec::new(),
        };

        let mut steps = Vec::new();
        let listed = get(job, "steps").and_then(Value::as_sequence);
        for step in listed.into_iter().flatten() {
            let Some(step) = step.as_mapping() else {
                continue;
            };
            let label = step_label(id, step);
            match map_step(step, &label, &env, job_dir.as_deref(), &mut out) {
                Some(mapped) => {
                    if let Some(cond) = get(step, "if").and_then(scalar_text) {
                        out.notes.push(format!(
                            "`{label}` ran only `if: {cond}` in the workflow; `deliver` runs it \
                             every time"
                        ));
                    }
                    out.mapped.push(label);
                    steps.push(mapped);
                }
                None => continue,
            }
        }
        if !steps.is_empty() {
            out.jobs.push(Job { name, needs, steps });
        }
    }

    // `needs:` can only name a job that became a service.
    let kept: BTreeSet<String> = out.jobs.iter().map(|j| j.name.clone()).collect();
    for job in &mut out.jobs {
        job.needs.retain(|n| kept.contains(n));
    }
    Ok(out)
}

/// Map one step. `None` means it was skipped or reported as unmapped (and
/// recorded on `out` either way).
fn map_step(
    step: &Mapping,
    label: &str,
    env: &[(String, String)],
    job_dir: Option<&str>,
    out: &mut Import,
) -> Option<Step> {
    if let Some(uses) = get_str(step, "uses") {
        let action = uses.split('@').next().unwrap_or(uses).to_ascii_lowercase();
        let with = get(step, "with").and_then(Value::as_mapping);
        if is_plumbing(&action) {
            out.connection_secrets
                .extend(secret_refs(&all_text(&Value::Mapping(step.clone()))));
            out.skipped.push(label.to_string());
            return None;
        }
        match action.as_str() {
            "appleboy/ssh-action" | "appleboy/scp-action" => {
                let Some(with) = with else {
                    out.unmapped.push(Unmapped {
                        step: label.into(),
                        reason: "has no `with:` block".into(),
                    });
                    return None;
                };
                let mut body = Mapping::new();
                for (k, v) in with {
                    let key = k.as_str().unwrap_or_default();
                    if CONNECTION_KEYS.contains(&key) {
                        out.connection_secrets.extend(secret_refs(&all_text(v)));
                    } else {
                        body.insert(k.clone(), v.clone());
                    }
                }
                out.secrets
                    .extend(secret_refs(&all_text(&Value::Mapping(body.clone()))));
                // The first literal connection detail wins; one read from a
                // secret is for the operator to supply with --host.
                let literal = |key: &str| {
                    get(with, key)
                        .and_then(scalar_text)
                        .filter(|v| !v.contains("${{") && !v.is_empty())
                };
                if out.host.is_none() {
                    out.host = literal("host")
                        .map(|h| h.split(',').next().unwrap_or(&h).trim().to_string());
                }
                if out.user.is_none() {
                    out.user = literal("username");
                }
                if out.port.is_none() {
                    out.port = literal("port").and_then(|p| p.parse().ok());
                }
                if action == "appleboy/ssh-action" {
                    ssh_action(&body, label, out)
                } else {
                    scp_action(&body, label, out)
                }
            }
            "docker/build-push-action" => {
                out.secrets
                    .extend(secret_refs(&all_text(&Value::Mapping(step.clone()))));
                out.image_build = Some(label.to_string());
                None
            }
            _ => {
                out.secrets
                    .extend(secret_refs(&all_text(&Value::Mapping(step.clone()))));
                out.unmapped.push(Unmapped {
                    step: label.into(),
                    reason: format!("`{action}` has no `deliver` equivalent"),
                });
                None
            }
        }
    } else if let Some(run) = get_str(step, "run") {
        out.secrets
            .extend(secret_refs(&all_text(&Value::Mapping(step.clone()))));
        if let Some(shell) = get_str(step, "shell") {
            if !["bash", "sh"].contains(&shell.split_whitespace().next().unwrap_or("")) {
                out.unmapped.push(Unmapped {
                    step: label.into(),
                    reason: format!("runs under `shell: {shell}` — `command:` steps run in `sh`"),
                });
                return None;
            }
        }
        let exprs = expressions(run);
        if !exprs.is_empty() {
            out.unmapped.push(Unmapped {
                step: label.into(),
                reason: format!(
                    "uses GitHub expressions (`{}`) that only exist on the runner",
                    exprs.join("`, `")
                ),
            });
            return None;
        }
        let prelude = match env_prelude(env, &env_of(step), run, label, &mut out.notes) {
            Ok(lines) => lines,
            Err(reason) => {
                out.unmapped.push(Unmapped {
                    step: label.into(),
                    reason,
                });
                return None;
            }
        };
        let dir = get_str(step, "working-directory").or(job_dir);
        let body = run.trim();
        let mut lines: Vec<String> = Vec::new();
        // GitHub runs a `run:` block under `bash -e`; `sh -c` alone carries on
        // past a failed line, which would turn a failed build into a deploy.
        if body.contains('\n') {
            lines.push("set -e".into());
        }
        lines.extend(prelude);
        if let Some(dir) = dir {
            lines.push(format!("cd {}", shell_quote(dir)));
        }
        lines.push(body.to_string());
        let command = if lines.len() == 1 {
            lines.remove(0)
        } else if !body.contains('\n') {
            lines.join(" && ")
        } else {
            lines.join("\n")
        };
        Some(Step::Command(command))
    } else {
        out.unmapped.push(Unmapped {
            step: label.into(),
            reason: "has neither `run:` nor `uses:`".into(),
        });
        None
    }
}

fn ssh_action(body: &Mapping, label: &str, out: &mut Import) -> Option<Step> {
    let Some(script) = get_str(body, "script") else {
        out.unmapped.push(Unmapped {
            step: label.into(),
            reason: "has no inline `script:` (a `script_path:` lives on the runner)".into(),
        });
        return None;
    };
    let exprs = expressions(script);
    if !exprs.is_empty() {
        out.unmapped.push(Unmapped {
            step: label.into(),
            reason: format!(
                "its script uses GitHub expressions (`{}`) — `ssh:` steps run literally",
                exprs.join("`, `")
            ),
        });
        return None;
    }
    Some(Step::Ssh(script.trim().to_string()))
}

fn scp_action(body: &Mapping, label: &str, out: &mut Import) -> Option<Step> {
    let (Some(source), Some(target)) = (get_str(body, "source"), get_str(body, "target")) else {
        out.unmapped.push(Unmapped {
            step: label.into(),
            reason: "needs both `source:` and `target:`".into(),
        });
        return None;
    };
    if source.contains("${{") || target.contains("${{") {
        out.unmapped.push(Unmapped {
            step: label.into(),
            reason: "its source or target uses a GitHub expression".into(),
        });
        return None;
    }
    let extra: Vec<&str> = body
        .keys()
        .filter_map(Value::as_str)
        .filter(|k| !["source", "target"].contains(k))
        .collect();
    if !extra.is_empty() {
        out.notes.push(format!(
            "`{label}` also set `{}`, which the scp command does not reproduce",
            extra.join("`, `")
        ));
    }
    Some(Step::Scp {
        sources: source
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        target: target.to_string(),
    })
}

/// A string as a YAML value under a `- command:` / `- ssh:` key at `indent`:
/// a plain scalar when it fits on one line, a literal block when it does not.
fn step_value(text: &str, indent: usize) -> String {
    if !text.contains('\n') {
        return quote_scalar(text);
    }
    let pad = " ".repeat(indent);
    let mut out = String::from("|\n");
    for line in text.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str(&format!("{pad}{}\n", line.trim_end()));
        }
    }
    out.pop();
    out
}

/// Render the scaffold. `compose` is the Compose service plain `init` found,
/// added when the workflow built an image.
#[allow(clippy::too_many_arguments)]
pub fn scaffold(
    source: &str,
    app: &str,
    host: &str,
    user: &str,
    port: u64,
    dir: &str,
    import: &Import,
    compose: Option<&Finding>,
) -> String {
    let mut out = String::new();
    out.push_str(&crate::config::schema_modeline());
    out.push('\n');
    out.push_str(&format!(
        "# Generated by `deliver init --from-workflow {source}` — review before deploying.\n\
         version: 1\napp: {app}\n\ndefaults:\n  target: production\n\ntargets:\n  production:\n    \
         host: {}\n    user: {}\n    port: {port}\n    dir: {}\n",
        quote_scalar(host),
        quote_scalar(user),
        quote_scalar(dir)
    ));

    if !import.secrets.is_empty() {
        out.push_str(
            "\n# Read from the environment, the way the workflow's runner supplied them.\n",
        );
        out.push_str("secrets:\n  providers:\n    - env\n  define:\n");
        for name in &import.secrets {
            out.push_str(&format!("    - {}\n", quote_scalar(name)));
        }
    }

    match &import.release {
        Some(Release::Tag) => out.push_str(
            "\n# The workflow released on a pushed tag; so does `deliver`.\nversioning:\n  from: tag\n",
        ),
        Some(Release::Branch(branch)) => out.push_str(&format!(
            "\n# The workflow shipped every push to {branch}; the commit is the version.\n\
             versioning:\n  from: commit\n  branch: {}\n",
            quote_scalar(branch)
        )),
        None => {}
    }

    out.push_str("\nservices:\n");
    if import.jobs.is_empty() && compose.is_none() {
        out.push_str("  # No workflow step could be mapped — see the report above.\n");
        return out;
    }
    let dest = |path: &str| format!("{user}@{host}:{path}");
    for job in &import.jobs {
        out.push_str(&format!(
            "  {}:\n    deployer: commands\n",
            quote_scalar(&job.name)
        ));
        if !job.needs.is_empty() {
            out.push_str(&format!("    needs: [{}]\n", job.needs.join(", ")));
        }
        out.push_str("    config:\n      steps:\n");
        for step in &job.steps {
            let line = match step {
                Step::Command(cmd) => format!("        - command: {}\n", step_value(cmd, 12)),
                Step::Ssh(cmd) => format!("        - ssh: {}\n", step_value(cmd, 12)),
                Step::Scp { sources, target } => {
                    let sources: Vec<String> = sources.iter().map(|s| shell_quote(s)).collect();
                    let cmd = format!(
                        "scp -r -P {port} {} {}",
                        sources.join(" "),
                        shell_quote(&dest(target))
                    );
                    format!("        - command: {}\n", step_value(&cmd, 12))
                }
            };
            out.push_str(&line);
        }
    }
    if let Some(finding) = compose {
        let needs = import.jobs.last().map(|j| j.name.as_str());
        detect::render_service(&mut out, app, host, dir, finding, needs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_references_are_collected_but_the_runner_token_is_not() {
        let refs = secret_refs(
            "echo ${{ secrets.API_KEY }} ${{secrets.DB_URL}} ${{ secrets.GITHUB_TOKEN }}",
        );
        assert_eq!(
            refs.into_iter().collect::<Vec<_>>(),
            vec!["API_KEY".to_string(), "DB_URL".to_string()]
        );
    }

    #[test]
    fn a_bare_on_key_read_as_yaml_1_1_true_still_finds_the_trigger() {
        let mut doc = Mapping::new();
        let on: Value = serde_yaml::from_str("push: {tags: ['v*']}").unwrap();
        doc.insert(Value::Bool(true), on);
        assert_eq!(triggers(&doc).and_then(release_of), Some(Release::Tag));
    }

    #[test]
    fn a_branch_glob_is_not_turned_into_a_required_branch() {
        let on: Value = serde_yaml::from_str("push: {branches: ['release/*']}").unwrap();
        assert_eq!(release_of(&on), None);
        let on: Value = serde_yaml::from_str("push: {branches: [main]}").unwrap();
        assert_eq!(release_of(&on), Some(Release::Branch("main".into())));
    }

    #[test]
    fn a_multi_line_run_fails_fast_the_way_the_runner_did() {
        let import = import(
            "on: push\njobs:\n  build:\n    steps:\n      - run: |\n          npm ci\n          npm run build\n        working-directory: web\n",
        )
        .unwrap();
        assert_eq!(
            import.jobs[0].steps,
            vec![Step::Command(
                "set -e\ncd web\nnpm ci\nnpm run build".into()
            )]
        );
    }

    #[test]
    fn literal_env_is_exported_and_a_secret_one_is_never_silently_dropped() {
        let import = import(
            "on: push\nenv:\n  NODE_ENV: production\njobs:\n  build:\n    env:\n      NPM_TOKEN: ${{ secrets.NPM }}\n    steps:\n      - run: npm run build\n      - run: aws s3 sync dist s3://bucket\n        env:\n          AWS_ACCESS_KEY_ID: ${{ secrets.AWS_KEY }}\n      - run: npm publish --token $NPM_TOKEN\n",
        )
        .unwrap();
        // The literal is exported although the script never names it: npm
        // reads NODE_ENV on its own.
        assert_eq!(
            import.jobs[0].steps,
            vec![Step::Command(
                "export NODE_ENV=production && npm run build".into()
            )]
        );
        // aws reads its key implicitly, so a step whose own env holds a
        // secret is never mapped as if it did not need one.
        assert_eq!(import.unmapped.len(), 2, "{:?}", import.unmapped);
        assert!(import.unmapped[0].reason.contains("secrets.AWS_KEY"));
        assert!(import.unmapped[1].reason.contains("$NPM_TOKEN"));
        // The inherited secret the first step never names is a note.
        assert!(import.notes.iter().any(|n| n.contains("$NPM_TOKEN")));
        assert!(import.secrets.contains("AWS_KEY"));
    }

    #[test]
    fn a_multi_line_step_renders_as_a_literal_block() {
        assert_eq!(step_value("a\nb", 4), "|\n    a\n    b");
        assert_eq!(step_value("npm ci", 4), "npm ci");
    }
}
