//! `docker-compose` — deploy an existing Compose project without modifying it.
//!
//! The compose files are treated as **immutable inputs**. They routinely encode
//! hard-won operational knowledge (memory limits, `oom_score_adj`, autovacuum
//! tuning, restart policies) that a translation layer would quietly discard, so
//! this deployer ships them byte-for-byte and drives `docker compose` over them.
//!
//! The image is built locally, because building on the target is exactly what a
//! memory-tight shared box can't afford.
//!
//! ```yaml
//! app:
//!   deployer: docker-compose
//!   config:
//!     files: [docker-compose.yml, docker-compose.prod.yml]
//!     project: amplifier
//!     image:
//!       tag: amplifier:latest
//!       context: .
//!       platform: linux/amd64
//!       transport: tarball          # tarball (default) | registry
//!       extra_tags: ["{version}", "{sha}"]
//!     env_file:
//!       path: .env
//!       literals: {ENVIRONMENT: production}
//!       from_secrets: [POSTGRES_PASSWORD, SECRET_KEY]
//!     backup:
//!       database: {service: db, user: amplifier, name: amplifier}
//!       volumes: [amplifier_media_volume]
//!     health:
//!       service: app
//!       command: curl -fsS http://localhost:8000/healthz
//! ```

use super::{cfg_bool, cfg_str, ComposeProject, LogSource, PlanContext, PlannedStep, ReleaseState};
use crate::remote::shell_quote;
use crate::secrets::Resolver;
use anyhow::{bail, Context, Result};
use serde_yaml::Value;

fn expand(template: &str, ctx: &PlanContext) -> String {
    template
        .replace("{version}", &ctx.version.release_display())
        .replace("{sha}", &ctx.version.git.short_sha)
        .replace("{deploy}", &ctx.version.id)
        .replace("{work}", &ctx.work_dir())
        .replace("{app}", &ctx.app)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One `NAME=value` line, quoted so Compose reads the value back byte-for-byte.
///
/// Unquoted, Compose interpolates `$` and treats ` #` as a comment, so a
/// password like `x7$Kq9pL` reached the container as `x7`. Single quotes are
/// literal; a value that itself holds a single quote is double-quoted with
/// `\`, `"` and `$` escaped instead.
fn env_line(name: &str, value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        bail!("docker-compose: env_file value {name} contains a newline — Compose cannot read it");
    }
    if !value.contains('\'') {
        return Ok(format!("{name}='{value}'"));
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "$$");
    Ok(format!("{name}=\"{escaped}\""))
}

/// Render the `.env` the containers will read.
///
/// Values come from the resolver's provider chain. A missing one is fatal here
/// rather than at container start, where it surfaces as an opaque crash loop.
fn render_env(
    cfg: &Value,
    ctx: &PlanContext,
    resolver: &Resolver,
) -> Result<Option<(String, String)>> {
    let Some(env) = cfg.get("env_file") else {
        return Ok(None);
    };
    let path = env
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".env")
        .to_string();

    let mut lines = Vec::new();
    if let Some(map) = env.get("literals").and_then(|v| v.as_mapping()) {
        for (k, v) in map {
            let (Some(k), Some(v)) = (
                k.as_str(),
                v.as_str().map(str::to_string).or_else(|| {
                    // Numbers and bools are common in config; render them faithfully.
                    match v {
                        Value::Bool(b) => Some(b.to_string()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    }
                }),
            ) else {
                continue;
            };
            lines.push(env_line(k, &expand(&v, ctx))?);
        }
    }

    // Declared once under `secrets.define`; a deployer only lists names when it
    // wants a subset.
    let wanted = match env.get("from_secrets") {
        Some(v) => string_list(Some(v)),
        None => resolver.declared_names(),
    };
    if !wanted.is_empty() && resolver.is_empty() {
        bail!(
            "docker-compose: env_file.from_secrets needs a provider chain — add a top-level \
             `secrets: {{providers: [...]}}` block"
        );
    }
    let mut missing = Vec::new();
    for name in &wanted {
        match resolver.get(name) {
            Some(found) => lines.push(env_line(name, &found.value)?),
            // Optional secrets may legitimately be absent.
            None if resolver.definition(name).is_some_and(|d| !d.required) => {}
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(crate::secrets::Unresolved(format!(
            "docker-compose: {} secret(s) not found in any provider ({}): {}",
            missing.len(),
            resolver.describe(),
            missing.join(", ")
        ))
        .into());
    }

    lines.sort();
    Ok(Some((path, format!("{}\n", lines.join("\n")))))
}

/// Whether this deploy has to build an image at all.
///
/// The deployer builds locally and ships the result, so it used to push a
/// `docker build` unconditionally — which a project whose services only *pull*
/// published images never asked for: it fails when there is no Dockerfile, and
/// ships a meaningless image when one happens to be lying around.
///
/// Evidence, in order, and deliberately conservative — a build is skipped only
/// when every Compose file parsed and none of them wanted one:
///
/// * an explicit `build: true` / `build: false` in the service config wins;
/// * an `image:`/`images:` block is a statement of intent — something is built;
/// * a Compose service with a `build:` key builds;
/// * a Compose service whose `image:` is the tag this deploy would produce is
///   waiting for that image, which is the shape the deployer was written for
///   (the build happens here, not in Compose) even with no `build:` anywhere;
/// * a file that cannot be read or parsed means we do not know, so we build.
fn builds_image(cfg: &Value, ctx: &PlanContext, files: &[String], tag: &str) -> bool {
    if let Some(explicit) = cfg.get("build").and_then(|v| v.as_bool()) {
        return explicit;
    }
    if cfg.get("image").is_some() || cfg.get("images").is_some() {
        return true;
    }
    let name = tag.split(':').next().unwrap_or(tag);
    for file in files {
        let Ok(text) = std::fs::read_to_string(ctx.repo_root.join(file)) else {
            return true;
        };
        let Ok(doc) = serde_yaml::from_str::<Value>(&text) else {
            return true;
        };
        let Some(services) = doc.get("services").and_then(|v| v.as_mapping()) else {
            continue;
        };
        for (_, service) in services {
            if service.get("build").is_some() {
                return true;
            }
            if let Some(image) = service.get("image").and_then(|v| v.as_str()) {
                if image == tag || image.split(':').next().unwrap_or(image) == name {
                    return true;
                }
            }
        }
    }
    false
}

/// The Compose files, in `-f` order (`docker-compose.yml` when unset).
/// Where on the target the config a deploy is about to overwrite is kept, one
/// directory per deploy id, relative to the project root.
const SNAPSHOTS: &str = ".deliver/config";

/// Use a portable, unambiguous relative destination on both local and SSH targets.
fn relative_path(path: &str) -> Result<String> {
    let path = path.trim_end_matches('/');
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty()
        || path.starts_with(['/', '-'])
        || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || matches!(
            path.split('/').next(),
            Some(".deliver" | ".first" | ".layout-v1")
        )
    {
        bail!("docker-compose: unsafe repository-relative path {path:?}");
    }
    Ok(path.to_string())
}

/// Validate all destinations before producing any executable steps. Includes
/// cannot cover Compose inputs, each other, or the rendered root env file.
fn validated_config(cfg: &Value) -> Result<Value> {
    let mut cfg = cfg.clone();
    let files: Vec<String> = compose_files(&cfg)
        .iter()
        .map(|p| relative_path(p))
        .collect::<Result<_>>()?;
    let includes: Vec<String> = string_list(cfg.get("include"))
        .iter()
        .map(|p| relative_path(p))
        .collect::<Result<_>>()?;
    let mut destinations = files.clone();
    destinations.extend(includes.iter().cloned());
    if let Some(env) = cfg.get("env_file") {
        let path = relative_path(env.get("path").and_then(Value::as_str).unwrap_or(".env"))?;
        let basename = path.rsplit('/').next().unwrap().to_string();
        destinations.push(basename.clone());
        cfg["env_file"]["path"] = Value::String(basename);
    }
    for (i, path) in destinations.iter().enumerate() {
        for other in &destinations[..i] {
            if path == other
                || path.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{path}/"))
            {
                bail!("docker-compose: overlapping destinations {other:?} and {path:?}");
            }
        }
    }
    cfg["files"] = serde_yaml::to_value(files)?;
    cfg["include"] = serde_yaml::to_value(includes)?;
    Ok(cfg)
}

fn path_arg(path: &str) -> String {
    if path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
    {
        path.to_string()
    } else {
        shell_quote(path)
    }
}

/// Compose files and includes retain their repository paths. The rendered env
/// retains the existing root-basename rule.
fn shipped_config(cfg: &Value, files: &[String]) -> Vec<String> {
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let mut paths: Vec<String> = files.to_vec();
    paths.extend(
        string_list(cfg.get("include"))
            .into_iter()
            .map(|p| p.trim_end_matches('/').to_string()),
    );
    if let Some(env) = cfg.get("env_file") {
        let path = env.get("path").and_then(|v| v.as_str()).unwrap_or(".env");
        paths.push(basename(path));
    }
    paths.retain(|p| {
        !p.is_empty() && !p.starts_with('/') && p.split('/').all(|seg| seg != ".." && seg != ".")
    });
    paths.dedup();
    paths
}

/// The marker a snapshot carries when the target had no live Compose file:
/// nothing was running from this root before the deploy, so the undo brings
/// the stack down instead of looking for a previous release that never was.
const FIRST_MARKER: &str = ".first";

const LAYOUT_MARKER: &str = ".layout-v1";

fn layout_manifest(paths: &[String], files: &[String]) -> String {
    serde_json::json!({"version": 1, "files": files, "paths": paths}).to_string()
}

/// S names the candidate snapshot. Never infer nested paths from a legacy
/// basename snapshot, and never use a snapshot for a different ordered layout.
fn snapshot_guard(sudo: &str, paths: &[String], files: &[String]) -> String {
    let expected = shell_quote(&layout_manifest(paths, files));
    let absent = if files.iter().any(|p| p.contains('/')) {
        "echo 'docker-compose: legacy nested config snapshot needs explicit migration' >&2; exit 1"
    } else {
        ":"
    };
    format!(
        "if [ -e \"$S/{LAYOUT_MARKER}\" ]; then \
        if [ \"$({sudo}cat \"$S/{LAYOUT_MARKER}\")\" != {expected} ]; then \
          echo 'docker-compose: config snapshot layout mismatch' >&2; exit 1; fi; \
        else {absent}; fi"
    )
}

/// Runs before any target mutation or Docker command. Legacy flat files stay
/// untouched: operators must choose a migration rather than silently lose them.
fn legacy_target_guard(root: &str, sudo: &str, files: &[String]) -> String {
    files.iter().filter(|p| p.contains('/')).map(|p| {
        let nested = shell_quote(&format!("{root}/{p}"));
        let flat = shell_quote(&format!("{root}/{}", p.rsplit('/').next().unwrap()));
        format!("if {sudo}test -e {flat} && ! {sudo}test -e {nested}; then \
          echo 'docker-compose: legacy flattened nested layout needs explicit migration' >&2; exit 1; fi")
    }).collect::<Vec<_>>().join("; ")
}

/// Copy the live config aside before the deploy overwrites it. A path the
/// target does not have yet is skipped: there is nothing to put back. Two
/// deploys of one commit inside a second share an id, so a taken one gets a
/// suffix that still sorts after it. With no live Compose file (`compose_file`,
/// the first `-f`) this is a first release and the snapshot gets
/// [`FIRST_MARKER`].
fn snapshot_command(
    root: &str,
    sudo: &str,
    id: &str,
    paths: &[String],
    files: &[String],
) -> String {
    let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
    format!(
        "set -e; cd {root}; S={SNAPSHOTS}/{id}; \
         if [ -e \"$S\" ]; then I=2; while [ -e \"$S-$I\" ]; do I=$((I+1)); done; S=\"$S-$I\"; fi; \
         {sudo}mkdir -p \"$S\"; N=0; \
         for p in {list}; do if [ -e \"$p\" ]; then \
           {sudo}mkdir -p \"$S/$(dirname \"$p\")\"; {sudo}cp -Rp \"$p\" \"$S/$p\"; N=$((N+1)); fi; \
         done; printf '%s\\n' {manifest} | {sudo}tee \"$S/{LAYOUT_MARKER}\" >/dev/null; \
         echo \"saved $N live config path(s) to $S\"; \
         if [ ! -e {compose_file} ]; then {sudo}touch \"$S/{FIRST_MARKER}\"; \
           echo 'first release: nothing was running from {root} before it'; fi",
        list = quoted.join(" "),
        compose_file = shell_quote(&files[0]),
        manifest = shell_quote(&layout_manifest(paths, files)),
    )
}

/// Restore the paths present in the already validated snapshot named by S.
fn restore_paths_command(sudo: &str, paths: &[String]) -> String {
    let list = paths
        .iter()
        .map(|p| shell_quote(p))
        .collect::<Vec<_>>()
        .join(" ");
    format!("for p in {list}; do \
        if [ -d \"$S/$p\" ]; then {sudo}mkdir -p \"$p\"; {sudo}cp -Rp \"$S/$p/.\" \"$p/\"; \
        elif [ -e \"$S/$p\" ]; then {sudo}mkdir -p \"$(dirname \"$p\")\"; {sudo}cp -p \"$S/$p\" \"$p\"; fi; done")
}

/// Restore the newest snapshot, including for a fresh `deliver rollback` plan.
/// Refuse an incompatible layout or failed copy before any image undo runs.
fn restore_command(root: &str, sudo: &str, paths: &[String], files: &[String]) -> String {
    format!(
        "set -e; cd {root}; RESTORED=; S=$(ls -1d {SNAPSHOTS}/*/ 2>/dev/null | sed 's:/*$::' | LC_ALL=C sort | tail -n 1); \
         if [ -n \"$S\" ]; then {guard}; {copy}; \
           RESTORED=1; echo \"restored the config saved in $S\"; \
         else echo 'no config snapshot on the target — the shipped config stays' >&2; {missing}; fi",
        guard = snapshot_guard(sudo, paths, files),
        copy = restore_paths_command(sudo, paths),
        missing = if files.iter().any(|p| p.contains('/')) { "exit 1" } else { ":" },
    )
}

fn compose_files(cfg: &Value) -> Vec<String> {
    let files = string_list(cfg.get("files"));
    if files.is_empty() {
        vec!["docker-compose.yml".to_string()]
    } else {
        files
    }
}

/// Every repo file this deployer ships as-is — the Compose files and each
/// `include:` — so preflight can check them before anything is built.
pub fn input_files(cfg: &Value) -> Vec<String> {
    let mut files = compose_files(cfg);
    files.extend(string_list(cfg.get("include")));
    files
}

/// Where each input lands under the target's root, mirroring the ship steps:
/// Compose files and includes at their repo-relative paths; rendered env at root.
fn compose_project(
    cfg: &Value,
    project: &str,
    files: &[String],
    env: Option<(String, String)>,
) -> ComposeProject {
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let mut layout: Vec<(String, String)> = files.iter().map(|f| (f.clone(), f.clone())).collect();
    layout.extend(string_list(cfg.get("include")).into_iter().map(|p| {
        let dest = p.trim_end_matches('/').to_string();
        (p, dest)
    }));
    ComposeProject {
        project: project.to_string(),
        files: files.to_vec(),
        layout,
        env: env.map(|(path, contents)| (basename(&path), contents)),
    }
}

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let validated = validated_config(cfg)?;
    let cfg = &validated;
    let files = compose_files(cfg);
    let project = cfg_str(cfg, "project").unwrap_or_else(|| ctx.app.clone());
    let root = ctx.target.dir.trim_end_matches('/').to_string();
    let sudo = ctx.sudo_prefix();
    let compose_files: String = files
        .iter()
        .map(|f| format!("-f {} ", path_arg(f)))
        .collect();
    let compose = format!("docker compose {compose_files}-p {project}");

    // A project can build several images (api, web, exporter). `image:` is the
    // single-image shorthand; `images:` is the general form.
    let image_specs: Vec<&Value> = match cfg.get("images").and_then(|v| v.as_sequence()) {
        Some(seq) => seq.iter().collect(),
        None => cfg.get("image").into_iter().collect(),
    };
    let image = image_specs.first().copied();
    let tag = image
        .and_then(|i| i.get("tag"))
        .and_then(|v| v.as_str())
        .map(|t| expand(t, ctx))
        .unwrap_or_else(|| format!("{}:latest", ctx.app));
    let context = image
        .and_then(|i| i.get("context"))
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .to_string();
    let dockerfile = image
        .and_then(|i| i.get("dockerfile"))
        .and_then(|v| v.as_str())
        .map(|f| format!("-f {f} "))
        .unwrap_or_default();
    let platform = image
        .and_then(|i| i.get("platform"))
        .and_then(|v| v.as_str())
        .unwrap_or("linux/amd64")
        .to_string();
    let transport = image
        .and_then(|i| i.get("transport"))
        .and_then(|v| v.as_str())
        .unwrap_or("tarball")
        .to_string();
    let extra_tags: Vec<String> = string_list(image.and_then(|i| i.get("extra_tags")))
        .iter()
        .map(|t| expand(t, ctx))
        .collect();
    let image_name = tag.split(':').next().unwrap_or(&tag);
    // Every image the project ships, as `(name, tag)`: the primary and each
    // further `images:` entry. `extra_tags` are the primary under other names
    // and need no rollback of their own. A rollback has to cover all of them,
    // or the previous release's `api` comes back up beside the new `web`.
    let shipped_images: Vec<(String, String)> = std::iter::once(tag.clone())
        .chain(image_specs.iter().skip(1).filter_map(|spec| {
            spec.get("tag")
                .and_then(|v| v.as_str())
                .map(|t| expand(t, ctx))
        }))
        .map(|t| (t.split(':').next().unwrap_or(&t).to_string(), t))
        .collect();
    let image_names = shipped_images
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();

    // Loading or pulling the new image replaces the local `:latest` tag on
    // the target. Record the outgoing image immediately before that happens;
    // doing it later would make `:rollback` point at the release we are trying
    // to undo.
    let mark_rollback = || {
        if shipped_images.len() == 1 {
            return PlannedStep::ssh(
                "mark the current image as rollback".to_string(),
                format!(
                    "{sudo}docker image inspect {tag} >/dev/null 2>&1 && \
                     {sudo}docker tag {tag} {image_name}:rollback || true"
                ),
            );
        }
        PlannedStep::ssh(
            format!(
                "mark the current images as rollback ({})",
                image_names.join(", ")
            ),
            shipped_images
                .iter()
                .map(|(name, t)| {
                    format!(
                        "{{ {sudo}docker image inspect {t} >/dev/null 2>&1 && \
                         {sudo}docker tag {t} {name}:rollback || true; }}"
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    let mut steps = Vec::new();
    let legacy_guard = legacy_target_guard(&root, sudo, &files);
    if !legacy_guard.is_empty() {
        steps.push(PlannedStep::ssh(
            "check Compose target layout".to_string(),
            legacy_guard,
        ));
    }

    // A project whose services only pull published images has nothing to build,
    // nothing to save and nothing to load; shipping the files, backing up,
    // `up -d`, health and record are the whole deploy.
    let builds = builds_image(cfg, ctx, &files, &tag);

    // --- build ---------------------------------------------------------------
    // Built here, never on the target: a shared box tuned to the edge of its
    // memory can't afford a docker build.
    // On a `method: local` target the build runs on the target's own daemon
    // and moves `:latest` to the new image itself, so the outgoing image has
    // to be marked before the build, not before the load.
    let mark_before_build = ctx.target.is_local();
    if builds {
        if mark_before_build {
            steps.push(mark_rollback());
        }
        let all_tags: Vec<String> = std::iter::once(tag.clone())
            .chain(extra_tags.iter().map(|t| {
                if t.contains(':') {
                    t.clone()
                } else {
                    format!("{}:{t}", tag.split(':').next().unwrap_or(&tag))
                }
            }))
            .collect();
        let tag_flags: String = all_tags.iter().map(|t| format!("-t {t} ")).collect();
        steps.push(PlannedStep::command(
            format!("build {tag} ({platform})"),
            format!("docker build --platform {platform} {dockerfile}{tag_flags}{context}"),
        ));

        // Any additional images in the project. They ride in the same archive, so
        // the whole project lands on the target in one transfer and one load.
        let mut extra_image_tags: Vec<String> = Vec::new();
        for spec in image_specs.iter().skip(1) {
            let spec_tag = spec
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|t| expand(t, ctx))
                .context("docker-compose: each entry in `images` needs a `tag`")?;
            let spec_context = spec.get("context").and_then(|v| v.as_str()).unwrap_or(".");
            let dockerfile = spec
                .get("dockerfile")
                .and_then(|v| v.as_str())
                .map(|f| format!("-f {f} "))
                .unwrap_or_default();
            let spec_platform = spec
                .get("platform")
                .and_then(|v| v.as_str())
                .unwrap_or(&platform);
            steps.push(PlannedStep::command(
                format!("build {spec_tag} ({spec_platform})"),
                format!(
                "docker build --platform {spec_platform} {dockerfile}-t {spec_tag} {spec_context}"
            ),
            ));
            extra_image_tags.push(spec_tag);
        }
        let shipped_tags: Vec<String> = all_tags
            .iter()
            .cloned()
            .chain(extra_image_tags.iter().cloned())
            .collect();

        // --- ship the image ------------------------------------------------------
        match transport.as_str() {
            "tarball" => {
                let tar = format!("{}/{}-image.tar.gz", ctx.work_dir(), ctx.app);
                steps.push(PlannedStep::command(
                    format!("save {tag} → tarball"),
                    format!(
                        "mkdir -p \"$(dirname {tar})\" && docker save {} | gzip > {tar}",
                        shipped_tags.join(" ")
                    ),
                ));
                steps.push(PlannedStep::command(
                    format!("ship image → {}", ctx.dest_label()),
                    ctx.copy(&tar, &root),
                ));
                let remote_tar = format!("{root}/{}-image.tar.gz", ctx.app);
                if !mark_before_build {
                    steps.push(mark_rollback());
                }
                steps.push(
                    PlannedStep::ssh(
                        "load image on the target".to_string(),
                        format!("{sudo}docker load -i {remote_tar}"),
                    )
                    .needs_remote(&["docker"]),
                );
                steps.push(
                    PlannedStep::ssh(
                        "remove the shipped image tarball".to_string(),
                        format!("{sudo}rm -f {remote_tar}"),
                    )
                    .into_cleanup(),
                );
                steps.push(
                    PlannedStep::command(format!("remove local {tar}"), format!("rm -f {tar}"))
                        .into_cleanup(),
                );
            }
            "registry" => {
                let registry = image
                    .and_then(|i| i.get("registry"))
                    .and_then(|v| v.as_str())
                    .context(
                        "docker-compose: image.registry is required for transport: registry",
                    )?;
                for t in &shipped_tags {
                    let name = t.rsplit('/').next().unwrap_or(t);
                    let remote = format!("{}/{name}", registry.trim_end_matches('/'));
                    steps.push(PlannedStep::command(
                        format!("push {remote}"),
                        format!("docker tag {t} {remote} && docker push {remote}"),
                    ));
                }
                let primary = format!(
                    "{}/{}",
                    registry.trim_end_matches('/'),
                    tag.rsplit('/').next().unwrap_or(&tag)
                );
                if !mark_before_build {
                    steps.push(mark_rollback());
                }
                if shipped_images.len() == 1 {
                    steps.push(
                        PlannedStep::ssh(
                            format!("pull {primary} on the target"),
                            format!(
                                "{sudo}docker pull {primary} && {sudo}docker tag {primary} {tag}"
                            ),
                        )
                        .needs_remote(&["docker"]),
                    );
                } else {
                    // Every image was pushed above; every one has to come down.
                    let pulls = shipped_images
                        .iter()
                        .map(|(_, t)| {
                            let remote = format!(
                                "{}/{}",
                                registry.trim_end_matches('/'),
                                t.rsplit('/').next().unwrap_or(t)
                            );
                            format!("{sudo}docker pull {remote} && {sudo}docker tag {remote} {t}")
                        })
                        .collect::<Vec<_>>()
                        .join(" && ");
                    steps.push(
                        PlannedStep::ssh(
                            format!("pull {} image(s) on the target", shipped_images.len()),
                            format!("set -e; {pulls}"),
                        )
                        .needs_remote(&["docker"]),
                    );
                }
            }
            other => bail!("docker-compose: unknown image.transport '{other}' (tarball|registry)"),
        }
    }

    // --- config + env --------------------------------------------------------
    steps.push(PlannedStep::ssh(
        format!("ensure {root}"),
        format!("{sudo}mkdir -p {root}"),
    ));
    // Everything below overwrites the live config in place, and the image undo
    // alone would bring the previous image back up under the new files. So the
    // live copies go aside first, and putting them back is this step's undo —
    // the last thing an unwind does, after which the snapshot is spent.
    let config_paths = shipped_config(cfg, &files);
    let restore = restore_command(&root, sudo, &config_paths, &files);
    steps.push(
        PlannedStep::ssh(
            format!("snapshot live config → {SNAPSHOTS}/{}", ctx.version.id),
            snapshot_command(&root, sudo, &ctx.version.id, &config_paths, &files),
        )
        .with_rollback(format!(
            "{restore}; if [ -n \"$S\" ]; then {sudo}rm -rf \"$S\"; fi"
        )),
    );
    for file in &files {
        let dest = file
            .rsplit_once('/')
            .map(|(parent, _)| format!("{root}/{parent}"))
            .unwrap_or_else(|| root.clone());
        if dest != root {
            steps.push(PlannedStep::ssh(
                format!("ensure {dest}"),
                format!("{sudo}mkdir -p {}", shell_quote(&dest)),
            ));
        }
        let step = PlannedStep::command(
            format!("ship {file}"),
            ctx.copy(&path_arg(file), &path_arg(&dest)),
        );
        // The compose file is shipped byte-for-byte, so what is in the repo is
        // what lands on the target — which makes it diffable against the one
        // currently running there. Unreadable is not a failure here; the copy
        // step itself will say so, and preflight runs first.
        steps.push(match std::fs::read_to_string(ctx.repo_root.join(file)) {
            Ok(text) => step.with_live_config(format!("{root}/{file}"), text),
            Err(_) => step,
        });
    }

    // Anything else the stack reads off the host — a nats.conf, prometheus
    // rules, an alertmanager template. Compose bind-mounts these by relative
    // path, so without them the project comes up misconfigured or not at all.
    for path in string_list(cfg.get("include")) {
        let local = ctx.repo_root.join(&path);
        if !local.exists() {
            return Err(crate::deployers::MissingInput::missing(format!(
                "docker-compose: include '{path}' does not exist in the repo"
            ))
            .into());
        }
        let dest = match path.rsplit_once('/') {
            Some((parent, _)) => format!("{root}/{parent}"),
            None => root.clone(),
        };
        steps.push(PlannedStep::ssh(
            format!("ensure {dest}"),
            format!("{sudo}mkdir -p {}", shell_quote(&dest)),
        ));
        if local.is_dir() {
            steps.push(PlannedStep::command(
                format!("ship {path}/"),
                ctx.copy_dir(&path_arg(&path), &path_arg(&dest)),
            ));
        } else {
            steps.push(PlannedStep::command(
                format!("ship {path}"),
                ctx.copy(&path_arg(&path), &path_arg(&dest)),
            ));
        }
    }

    let resolver = ctx.resolver();
    let rendered_env = render_env(cfg, ctx, resolver)?;
    if let Some((env_path, contents)) = rendered_env.clone() {
        let local = format!("{}/{env_path}", ctx.work_dir());
        // Written locally at 0600, shipped with permissions preserved: the values
        // never appear in a command line, plan output, or shell history.
        steps.push(PlannedStep::write_file(
            format!("render {env_path} ({} lines)", contents.lines().count()),
            &local,
            0o600,
            contents,
        ));
        steps.push(PlannedStep::command(
            format!("ship {env_path} (0600)"),
            ctx.copy(&path_arg(&local), &path_arg(&root)),
        ));
        steps.push(PlannedStep::ssh(
            format!("secure {env_path}"),
            format!(
                "{sudo}chmod 600 {}",
                path_arg(&format!("{root}/{env_path}"))
            ),
        ));
        steps.push(
            PlannedStep::command(
                format!("remove local {env_path}"),
                format!("rm -f {}", path_arg(&local)),
            )
            .into_cleanup(),
        );
    }

    // --- backup, before anything is replaced ---------------------------------
    // Every backup is stamped, so each kind is pruned by name to the newest
    // `keep` once the deploy has succeeded — the same shape as `keep_releases`.
    // (absolute dir, keep, one glob per kind)
    let mut backups: Option<(String, u64, Vec<String>)> = None;
    if let Some(backup) = cfg.get("backup") {
        let dir = backup
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or("backups");
        let keep = backup
            .get("keep")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .max(1);
        let mut kinds = Vec::new();
        if backup.get("database").is_some() {
            kinds.push("predeploy-[0-9]*Z.dump".to_string());
        }
        for volume in string_list(backup.get("volumes")) {
            // The stamp starts with a digit, so volume `app` never matches
            // `app-media`'s archives.
            kinds.push(format!("{volume}-[0-9]*Z.tar.gz"));
        }
        let absolute = if dir.starts_with('/') {
            dir.to_string()
        } else {
            format!("{root}/{dir}")
        };
        backups = Some((absolute, keep, kinds));
        if let Some(db) = backup.get("database") {
            let service = db.get("service").and_then(|v| v.as_str()).unwrap_or("db");
            let user = db
                .get("user")
                .and_then(|v| v.as_str())
                .unwrap_or(&project)
                .to_string();
            let name = db
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&project)
                .to_string();
            steps.push(PlannedStep::ssh(
                format!("back up database ({service})"),
                format!(
                    "set -e; cd {root}; {sudo}mkdir -p {dir}; \
                     STAMP=$(date -u +%Y%m%dT%H%M%SZ); \
                     if {sudo}{compose} ps -q {service} | grep -q .; then \
                       {sudo}{compose} exec -T {service} pg_dump -U {user} -Fc {name} \
                         > {dir}/predeploy-$STAMP.dump && echo \"saved {dir}/predeploy-$STAMP.dump\"; \
                     else echo 'database not running yet — nothing to back up'; fi"
                ),
            ));
        }
        for volume in string_list(backup.get("volumes")) {
            steps.push(PlannedStep::ssh(
                format!("back up volume {volume}"),
                format!(
                    "set -e; cd {root}; {sudo}mkdir -p {dir}; \
                     STAMP=$(date -u +%Y%m%dT%H%M%SZ); \
                     if {sudo}docker volume inspect {volume} >/dev/null 2>&1; then \
                       {sudo}docker run --rm --user 0 --entrypoint tar \
                         -v {volume}:/data:ro -v \"$PWD/{dir}\":/backup alpine:3 \
                         -czf /backup/{volume}-$STAMP.tar.gz -C /data . \
                       && echo \"saved {dir}/{volume}-$STAMP.tar.gz\"; \
                     else echo 'volume {volume} does not exist yet — skipping'; fi"
                ),
            ));
        }
    }

    // --- infrastructure first ------------------------------------------------
    // Databases and brokers come up before one-off release commands, because
    // migrations need a live database, and they must land before new app
    // containers start serving with the new code.
    if let Some(infra) = cfg.get("infra") {
        let services = string_list(infra.get("services"));
        if !services.is_empty() {
            steps.push(PlannedStep::ssh(
                format!("start infrastructure ({})", services.join(", ")),
                format!(
                    "set -e; cd {root}; {sudo}{compose} up -d {}",
                    services.join(" ")
                ),
            ));
        }
        if let Some(wait) = infra.get("wait") {
            let service = wait.get("service").and_then(|v| v.as_str()).unwrap_or("db");
            let command = wait
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("pg_isready");
            let retries = wait.get("retries").and_then(|v| v.as_u64()).unwrap_or(30);
            steps.push(PlannedStep::ssh(
                format!("wait for {service}"),
                format!(
                    "set -e; cd {root}; \
                     for i in $(seq 1 {retries}); do \
                       if {sudo}{compose} exec -T {service} {command} >/dev/null 2>&1; then \
                         echo '{service} ready'; exit 0; fi; \
                       sleep 1; \
                     done; \
                     echo '{service} did not become ready' >&2; exit 1"
                ),
            ));
        }
    }

    // --- release commands ----------------------------------------------------
    // Migrations and data syncs, run in a throwaway container against the new
    // image while the old containers are still serving.
    if let Some(release) = cfg.get("release") {
        let service = release
            .get("service")
            .and_then(|v| v.as_str())
            .unwrap_or("app");
        // `-w` is a flag to `compose run`, not part of the command: it has to
        // precede the service name or the container's entrypoint receives it.
        let workdir = release
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(|d| format!("-w {d} "))
            .unwrap_or_default();
        for command in string_list(release.get("commands")) {
            steps.push(PlannedStep::ssh(
                format!("release: {command}"),
                format!("set -e; cd {root}; {sudo}{compose} run --rm {workdir}{service} {command}"),
            ));
        }
    }

    // --- bring it up ---------------------------------------------------------
    let custom_up = cfg_str(cfg, "remote_command");
    let up = match &custom_up {
        Some(custom) => custom.clone(),
        None => format!("{sudo}{compose} up -d --remove-orphans"),
    };
    let start = PlannedStep::ssh(
        "start services".to_string(),
        format!("set -e; cd {root}; {up}"),
    );
    // A custom command is the operator's own; what it runs is theirs to know.
    let start = match custom_up {
        Some(_) => start,
        None => start
            .needs_remote(&["docker", "docker compose"])
            .brings_up(compose_project(cfg, &project, &files, rendered_env)),
    };
    // Once the previous release is back up, say so in the history: the restored
    // deploy is the row just above this one's.
    let history = format!("{root}/.deliver/history.tsv");
    let unwind_record = crate::rollback::unwind_record_command(
        &history,
        &ctx.version.id,
        &format!(
            "{sudo}awk -F'\\t' -v id={} '$2==id{{print p; exit}} {{p=$2}}' \"$H\"",
            shell_quote(&ctx.version.id)
        ),
        sudo,
    );
    // The undo this deployer owns is the image swap. With nothing shipped there
    // is none, and saying so is better than an undo that silently does nothing:
    // the project is running whatever its Compose file pins.
    // The config goes back first, so the previous image comes up under the
    // files and `.env` it was released with.
    let undo = if builds && shipped_images.len() > 1 {
        // Every image goes back or none does: a mix of the previous and the
        // new images is neither release, so it is never brought up as one.
        let names = image_names.join(" ");
        let retag: String = shipped_images
            .iter()
            .map(|(name, t)| format!("{sudo}docker tag {name}:rollback {t}; "))
            .collect();
        format!(
            "{restore}; set -e; cd {root}; MISSING=; \
                 for i in {names}; do \
                   {sudo}docker image inspect \"$i:rollback\" >/dev/null 2>&1 || MISSING=\"$MISSING $i\"; \
                 done; \
                 if [ -z \"$MISSING\" ]; then \
                   {retag}{sudo}{compose} up -d --remove-orphans; \
                   echo 'rolled back to the previous images ({})'; \
                 else echo \"no rollback image recorded for:$MISSING\" >&2; exit 1; fi; {unwind_record}",
            image_names.join(", ")
        )
    } else if builds {
        format!(
            "{restore}; set -e; cd {root}; \
                 if {sudo}docker image inspect {0}:rollback >/dev/null 2>&1; then \
                   {sudo}docker tag {0}:rollback {tag}; {sudo}{compose} up -d --remove-orphans; \
                   echo 'rolled back to the previous image'; \
                 else echo 'no rollback image recorded' >&2; exit 1; fi; {unwind_record}",
            image_name
        )
    } else {
        // A pull-only project's release *is* its config: the files pin the
        // images, so restoring them and coming back up is the whole undo.
        format!(
            "{restore}; set -e; cd {root}; \
                 if [ -n \"$RESTORED\" ]; then {sudo}{compose} up -d --remove-orphans; \
                   echo 'rolled back to the previous config'; \
                 else echo 'no image was shipped and no config snapshot was recorded — \
                   nothing to restore' >&2; exit 1; fi; {unwind_record}"
        )
    };
    // A first release has nothing to go back to: the snapshot this deploy took
    // carries the first-release marker, and the undo takes the stack down and
    // removes the config the deploy shipped where none was before, so the
    // target is as it was and the next deploy is a first release again. Only
    // this run's own snapshot counts — two deploys inside one second sort by
    // sha, not by age — so `deliver rollback`, whose fresh plan has an id no
    // snapshot carries, always takes the ordinary undo.
    let shipped: Vec<String> = config_paths.iter().map(|p| shell_quote(p)).collect();
    let first_record = crate::rollback::unwind_record_command(
        &history,
        &ctx.version.id,
        &format!("echo {}", crate::rollback::NOTHING_LIVE),
        sudo,
    );
    steps.push(start.with_rollback(format!(
        "set -e; {legacy}; cd {root}; S=; for d in {SNAPSHOTS}/{id} {SNAPSHOTS}/{id}-*; do if [ -d \"$d\" ]; then S=$d; fi; done; \
         if [ -n \"$S\" ]; then {guard}; fi; \
         if [ -n \"$S\" ] && [ -e \"$S/{FIRST_MARKER}\" ]; then \
           set -e; {sudo}{compose} down --remove-orphans; \
           for p in {shipped}; do [ -e \"$S/$p\" ] || {sudo}rm -rf \"$p\"; done; {copy_saved}; \
           echo 'first release: brought {project} down — nothing is served until a release passes its checks'; \
           {first_record}; \
         else {undo}; fi",
        shipped = shipped.join(" "),
        copy_saved = restore_paths_command(sudo, &config_paths),
        legacy = if files.iter().any(|p| p.contains('/')) { legacy_target_guard(&root, sudo, &files) } else { ":".to_string() },
        guard = snapshot_guard(sudo, &config_paths, &files),
        id = ctx.version.id,
    )));

    // --- health --------------------------------------------------------------
    if let Some(health) = cfg.get("health") {
        if let Some(url) = health.get("url").and_then(|v| v.as_str()) {
            let retries = health.get("retries").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            let interval = health.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);
            let expect = health
                .get("expect_status")
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as u16;
            steps.push(
                PlannedStep::http(
                    format!("verify {url}"),
                    url.to_string(),
                    expect,
                    retries,
                    interval,
                )
                .with_follow_redirects(cfg_bool(health, "follow_redirects", false)),
            );
        }
        // A loopback probe on the target: tests the published port directly,
        // without depending on DNS, nginx, or TLS being right yet.
        if let Some(url) = health.get("remote_url").and_then(|v| v.as_str()) {
            let retries = health.get("retries").and_then(|v| v.as_u64()).unwrap_or(12);
            let interval = health.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);
            let host_header = health
                .get("host")
                .and_then(|v| v.as_str())
                .map(|host| format!(" -H 'Host: {host}'"))
                .unwrap_or_default();
            // A 2xx is usually proof enough, but an endpoint that reports
            // degraded dependencies in a 200 body needs the body checked too.
            let body_check = match health.get("contains").and_then(|v| v.as_str()) {
                Some(needle) => {
                    format!(" && curl -s{host_header} {url} 2>/dev/null | grep -q '{needle}'")
                }
                None => String::new(),
            };
            steps.push(
                PlannedStep::ssh(
                    format!("health check {url} (on the target)"),
                    format!(
                        "for i in $(seq 1 {retries}); do \
                           STATUS=$(curl -s{host_header} {url} -o /dev/null -w '%{{http_code}}' 2>/dev/null) || STATUS=000; \
                           case \"$STATUS\" in 2*) if true{body_check}; then \
                             echo \"healthy (HTTP $STATUS)\"; exit 0; fi;; esac; \
                           echo \"attempt $i/{retries}: HTTP $STATUS\"; sleep {interval}; \
                         done; \
                         echo 'health check failed' >&2; exit 1"
                    ),
                )
                // A failed curl reads as HTTP 000, so a missing one would
                // fail every attempt and look like an unhealthy release.
                .needs_remote(&["curl"]),
            );
        }
        if let Some(command) = health.get("command").and_then(|v| v.as_str()) {
            let service = health
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or("app");
            let retries = health.get("retries").and_then(|v| v.as_u64()).unwrap_or(10);
            let interval = health.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);
            steps.push(PlannedStep::ssh(
                format!("health check ({service})"),
                format!(
                    "set -e; cd {root}; \
                     for i in $(seq 1 {retries}); do \
                       if {sudo}{compose} exec -T {service} {command} >/dev/null 2>&1; then \
                         echo 'healthy'; exit 0; fi; \
                       echo \"waiting for {service} ($i/{retries})\"; sleep {interval}; \
                     done; \
                     echo 'health check failed' >&2; exit 1"
                ),
            ));
        }
    }

    // Keep the same durable target-side record as atomic file releases. This
    // makes the live Compose release and its source commit clear without
    // depending on local output or a CI provider.
    let release = ctx.version.release_display();
    let sha = &ctx.version.git.sha;
    let stamp = &ctx.version.id;
    steps.push(PlannedStep::ssh(
        format!("record deploy {stamp} (release {release})"),
        format!(
            "set -e; {sudo}mkdir -p {root}/.deliver; \
             N=1; if {sudo}test -f {root}/.deliver/history.tsv; then \
               N=$(( $({sudo}wc -l {root}/.deliver/history.tsv | awk '{{print $1}}') + 1 )); fi; \
             printf '%s\\t%s\\t%s\\t%s\\t%s\\n' \"$N\" \"{stamp}\" \"{release}\" \"{sha}\" \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" \
               | {sudo}tee -a {root}/.deliver/history.tsv >/dev/null; \
             echo \"deploy #$N · {stamp} · release {release}\""
        ),
        )
        // Compose replaces containers in place, so there is no live symlink and
        // no retained release directory — the history is the whole record.
        // No release layout: Compose replaces containers in place, so there is
        // no symlink to repoint and nothing for `rollback --to` to address.
        .with_release_state(ReleaseState {
            history_path: format!("{root}/.deliver/history.tsv"),
            live_path: None,
            releases_dir: None,
            previous_marker: None,
            backups_dir: backups.as_ref().map(|(dir, _, _)| dir.clone()),
        })
        // The same invocation that brought the project up, so `deliver logs`
        // cannot tail a different project than the one that was deployed.
        .with_log_source(LogSource::Compose {
            compose: compose.clone(),
            dir: root.clone(),
        }),
    );

    // --- tidy ----------------------------------------------------------------
    if cfg_bool(cfg, "prune", true) {
        steps.push(
            PlannedStep::ssh(
                "prune dangling images".to_string(),
                format!("{sudo}docker image prune -f >/dev/null && echo pruned"),
            )
            .into_cleanup(),
        );
    }
    // Config snapshots follow the backups' `keep`, newest by deploy id.
    let keep_snapshots = backups.as_ref().map(|(_, keep, _)| *keep).unwrap_or(10);
    steps.push(
        PlannedStep::ssh(
            format!("prune old config snapshots (keep {keep_snapshots})"),
            format!(
                "cd {root}/{SNAPSHOTS} 2>/dev/null || exit 0; \
                 ls -1d -- */ 2>/dev/null | sed 's:/*$::' | LC_ALL=C sort -r | tail -n +{next} \
                   | xargs -r {sudo}rm -rf; \
                 echo \"kept newest {keep_snapshots}\"",
                next = keep_snapshots + 1,
            ),
        )
        .into_cleanup(),
    );
    if let Some((dir, keep, kinds)) = &backups {
        if !kinds.is_empty() {
            steps.push(
                PlannedStep::ssh(
                    format!("prune old backups (keep {keep} of each)"),
                    format!(
                        "cd {dir} 2>/dev/null || exit 0; for kind in {kinds}; do \
                           ls -1d -- $kind 2>/dev/null | sort -r | tail -n +{next} | xargs -r {sudo}rm -f; \
                         done; echo \"kept newest {keep} of each\"",
                        dir = crate::remote::shell_quote(dir),
                        kinds = kinds
                            .iter()
                            .map(|k| format!("'{k}'"))
                            .collect::<Vec<_>>()
                            .join(" "),
                        next = keep + 1,
                    ),
                )
                .into_cleanup(),
            );
        }
    }

    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::env_line;

    #[test]
    fn values_are_single_quoted_so_compose_reads_them_literally() {
        assert_eq!(env_line("A", "x7$Kq9pL").unwrap(), "A='x7$Kq9pL'");
        assert_eq!(env_line("A", "a #b").unwrap(), "A='a #b'");
        assert_eq!(env_line("A", "").unwrap(), "A=''");
    }

    #[test]
    fn a_single_quote_falls_back_to_escaped_double_quotes() {
        assert_eq!(
            env_line("A", r#"it's $5 "now" \o/"#).unwrap(),
            r#"A="it's $$5 \"now\" \\o/""#
        );
    }

    #[test]
    fn a_newline_is_refused_by_name() {
        let err = env_line("CERT", "a\nb").unwrap_err().to_string();
        assert!(
            err.contains("env_file value CERT contains a newline"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod http_verification_tests {
    use super::*;
    use crate::deployers::StepKind;

    #[test]
    fn http_verification_compose_local_only_and_defaults() {
        let fixture = crate::deployers::http_test_support::Context::new();
        std::fs::write(
            fixture.ctx.repo_root.join("compose.yml"),
            "services:\n  api:\n    image: example:latest\n",
        )
        .unwrap();
        let mut baseline = None;
        for (option, expected) in [
            ("", false),
            ("follow_redirects: false,", false),
            ("follow_redirects: true,", true),
            ("follow_redirects: invalid,", false),
        ] {
            let cfg = serde_yaml::from_str(&format!(r#"
build: false
files: [compose.yml]
health: {{{option} url: https://example.com/health, remote_url: http://127.0.0.1/health, command: echo healthy}}
"#)).unwrap();
            let steps = compile(&cfg, &fixture.ctx).unwrap();
            let http: Vec<_> = steps
                .iter()
                .filter_map(|step| match &step.kind {
                    StepKind::Http {
                        follow_redirects,
                        expect_status,
                        retries,
                        interval,
                        ..
                    } => Some((*follow_redirects, *expect_status, *retries, *interval)),
                    _ => None,
                })
                .collect();
            assert!(http == vec![(expected, 200, 10, 5)]);
            let ssh: Vec<_> = steps
                .iter()
                .filter_map(|step| match &step.kind {
                    StepKind::Ssh { command } => Some(command.clone()),
                    _ => None,
                })
                .collect();
            if let Some(prior) = &baseline {
                assert!(prior == &ssh);
            } else {
                baseline = Some(ssh);
            }
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use crate::deployers::{http_test_support, StepKind};

    #[test]
    fn unsafe_and_overlapping_paths_fail_before_a_plan_exists() {
        for path in [
            "/tmp/compose.yml",
            "../compose.yml",
            "deploy/../compose.yml",
            "a//b",
            "C:/compose.yml",
            "a\\b",
            ".deliver/history.tsv",
            ".first",
            ".layout-v1",
            "-compose.yml",
            "a\nb",
        ] {
            let cfg = serde_yaml::to_value(serde_json::json!({"files": [path]})).unwrap();
            assert!(validated_config(&cfg).is_err(), "accepted {path:?}");
        }
        for cfg in [
            "files: [compose.yml, ./compose.yml]",
            "files: [deploy/compose.yml]\ninclude: [deploy]",
            "files: [compose.yml]\ninclude: [a, a/b]",
            "files: [.env]\nenv_file: {literals: {A: one}}",
        ] {
            assert!(
                validated_config(&serde_yaml::from_str(cfg).unwrap()).is_err(),
                "{cfg}"
            );
        }
    }

    #[test]
    fn nested_local_and_ssh_plans_share_paths_and_quote_shell_arguments() {
        let mut fixture = http_test_support::Context::new();
        let file = "deploy dir/it's compose.yml";
        std::fs::create_dir_all(fixture.ctx.repo_root.join("deploy dir")).unwrap();
        std::fs::write(fixture.ctx.repo_root.join(file), "services: {}\n").unwrap();
        let cfg = serde_yaml::to_value(
            serde_json::json!({"files": [format!("./{file}")], "build": false}),
        )
        .unwrap();
        for method in ["local", "ssh"] {
            fixture.ctx.target = serde_yaml::from_str(&format!(
                "{{method: {method}, host: example.com, user: deploy, dir: /fixture}}"
            ))
            .unwrap();
            let steps = compile(&cfg, &fixture.ctx).unwrap();
            assert!(steps[0].label.contains("layout"));
            let ship = steps
                .iter()
                .find(|s| s.label == format!("ship {file}"))
                .unwrap();
            let command = match &ship.kind {
                StepKind::Command { command, .. } => command,
                _ => panic!("copy must be a command"),
            };
            assert!(command.contains(&shell_quote(file)), "{command}");
            assert!(
                command.contains(&shell_quote("/fixture/deploy dir")),
                "{command}"
            );
            assert_eq!(
                ship.live_config.as_ref().unwrap().remote_path,
                format!("/fixture/{file}")
            );
            let project = steps
                .iter()
                .find_map(|s| s.compose_project.as_ref())
                .unwrap();
            assert_eq!(project.files, [file]);
            assert_eq!(project.layout, [(file.to_string(), file.to_string())]);
            let start = steps.iter().find(|s| s.label == "start services").unwrap();
            let command = match &start.kind {
                StepKind::Ssh { command } => command,
                _ => panic!("start"),
            };
            assert!(
                command.contains(&format!("-f {}", shell_quote(file))),
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_only_legacy_snapshots_remain_restorable() {
        let fixture = http_test_support::Context::new();
        let root = fixture.ctx.repo_root.to_str().unwrap();
        let snap = fixture.ctx.repo_root.join(".deliver/config/old");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("compose.yml"), "old").unwrap();
        std::fs::write(fixture.ctx.repo_root.join("compose.yml"), "new").unwrap();
        let paths = vec!["compose.yml".to_string()];
        let out = std::process::Command::new("sh")
            .args(["-c", &restore_command(root, "", &paths, &paths)])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(fixture.ctx.repo_root.join("compose.yml")).unwrap(),
            "old"
        );
    }
}
