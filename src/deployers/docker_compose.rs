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

use super::{cfg_bool, cfg_str, PlanContext, PlannedStep};
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
            lines.push(format!("{k}={}", expand(&v, ctx)));
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
            Some(found) => lines.push(format!("{name}={}", found.value)),
            // Optional secrets may legitimately be absent.
            None if resolver.definition(name).is_some_and(|d| !d.required) => {}
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        bail!(
            "docker-compose: {} secret(s) not found in any provider ({}): {}",
            missing.len(),
            resolver.describe(),
            missing.join(", ")
        );
    }

    lines.sort();
    Ok(Some((path, format!("{}\n", lines.join("\n")))))
}

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let files = string_list(cfg.get("files"));
    let files = if files.is_empty() {
        vec!["docker-compose.yml".to_string()]
    } else {
        files
    };
    let project = cfg_str(cfg, "project").unwrap_or_else(|| ctx.app.clone());
    let root = ctx.target.dir.trim_end_matches('/').to_string();
    let sudo = ctx.sudo_prefix();
    let compose_files: String = files.iter().map(|f| format!("-f {f} ")).collect();
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

    // Loading or pulling the new image replaces the local `:latest` tag on
    // the target. Record the outgoing image immediately before that happens;
    // doing it later would make `:rollback` point at the release we are trying
    // to undo.
    let mark_rollback = || {
        PlannedStep::ssh(
            "mark the current image as rollback".to_string(),
            format!(
                "{sudo}docker image inspect {tag} >/dev/null 2>&1 && \
                 {sudo}docker tag {tag} {image_name}:rollback || true"
            ),
        )
    };

    let mut steps = Vec::new();

    // --- build ---------------------------------------------------------------
    // Built here, never on the target: a shared box tuned to the edge of its
    // memory can't afford a docker build.
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
            steps.push(mark_rollback());
            steps.push(PlannedStep::ssh(
                "load image on the target".to_string(),
                format!("{sudo}docker load -i {remote_tar}"),
            ));
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
                .context("docker-compose: image.registry is required for transport: registry")?;
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
            steps.push(mark_rollback());
            steps.push(PlannedStep::ssh(
                format!("pull {primary} on the target"),
                format!("{sudo}docker pull {primary} && {sudo}docker tag {primary} {tag}"),
            ));
        }
        other => bail!("docker-compose: unknown image.transport '{other}' (tarball|registry)"),
    }

    // --- config + env --------------------------------------------------------
    steps.push(PlannedStep::ssh(
        format!("ensure {root}"),
        format!("{sudo}mkdir -p {root}"),
    ));
    for file in &files {
        steps.push(PlannedStep::command(
            format!("ship {file}"),
            ctx.copy(file, &root),
        ));
    }

    // Anything else the stack reads off the host — a nats.conf, prometheus
    // rules, an alertmanager template. Compose bind-mounts these by relative
    // path, so without them the project comes up misconfigured or not at all.
    for path in string_list(cfg.get("include")) {
        let local = ctx.repo_root.join(&path);
        if !local.exists() {
            bail!("docker-compose: include '{path}' does not exist in the repo");
        }
        let dest = match path.rsplit_once('/') {
            Some((parent, _)) => format!("{root}/{parent}"),
            None => root.clone(),
        };
        if local.is_dir() {
            steps.push(PlannedStep::ssh(
                format!("ensure {dest}"),
                format!("{sudo}mkdir -p {dest}"),
            ));
            steps.push(PlannedStep::command(
                format!("ship {path}/"),
                ctx.copy_dir(&path, &dest),
            ));
        } else {
            steps.push(PlannedStep::command(
                format!("ship {path}"),
                ctx.copy(&path, &dest),
            ));
        }
    }

    let resolver = ctx.resolver();
    if let Some((env_path, contents)) = render_env(cfg, ctx, resolver)? {
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
            ctx.copy(&local, &root),
        ));
        steps.push(PlannedStep::ssh(
            format!("secure {env_path}"),
            format!("{sudo}chmod 600 {root}/{env_path}"),
        ));
        steps.push(
            PlannedStep::command(format!("remove local {env_path}"), format!("rm -f {local}"))
                .into_cleanup(),
        );
    }

    // --- backup, before anything is replaced ---------------------------------
    if let Some(backup) = cfg.get("backup") {
        let dir = backup
            .get("dir")
            .and_then(|v| v.as_str())
            .unwrap_or("backups");
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
    let up = match cfg_str(cfg, "remote_command") {
        Some(custom) => custom,
        None => format!("{sudo}{compose} up -d --remove-orphans"),
    };
    steps.push(
        PlannedStep::ssh(
            "start services".to_string(),
            format!("set -e; cd {root}; {up}"),
        )
        .with_rollback(format!(
            "set -e; cd {root}; \
                 if {sudo}docker image inspect {0}:rollback >/dev/null 2>&1; then \
                   {sudo}docker tag {0}:rollback {tag}; {sudo}{compose} up -d --remove-orphans; \
                   echo 'rolled back to the previous image'; \
                 else echo 'no rollback image recorded' >&2; exit 1; fi",
            image_name
        )),
    );

    // --- health --------------------------------------------------------------
    if let Some(health) = cfg.get("health") {
        if let Some(url) = health.get("url").and_then(|v| v.as_str()) {
            let retries = health.get("retries").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
            let interval = health.get("interval").and_then(|v| v.as_u64()).unwrap_or(5);
            let expect = health
                .get("expect_status")
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as u16;
            steps.push(PlannedStep::http(
                format!("verify {url}"),
                url.to_string(),
                expect,
                retries,
                interval,
            ));
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
            steps.push(PlannedStep::ssh(
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
            ));
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
    ));

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

    Ok(steps)
}
