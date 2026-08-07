//! `files` — ship a path/tarball to the target.
//!
//! With `unpack: true` this uses an **atomic release layout**, which is what
//! makes a deploy zero-downtime and rollback-able:
//!
//! ```text
//! <target.dir>/releases/<stamp>/     <- new release unpacked here
//! <target.dir>/<subdir>  ->  releases/<stamp>     (symlink, flipped atomically)
//! ```
//!
//! The live path is **never emptied**. Everything is staged into a fresh release
//! directory, sanity-checked, and only then swapped in with a single atomic
//! `mv -T` of the symlink — so nginx never observes a missing or half-written
//! webroot, and the previous release stays on disk to roll back to.
//!
//! (The old behavior — clean the live directory, then untar into it — had a
//! window where a bad tarball left the site empty with nothing to restore.)
//!
//! With `unpack: false` it's a plain copy of a single file; no releases needed.

use super::{cfg_bool, cfg_str, PlanContext, PlannedStep};
use anyhow::{Context, Result};
use serde_yaml::Value;

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let mut src = cfg_str(cfg, "src").context("files: 'src' is required")?;
    let root = ctx.target.dir.trim_end_matches('/').to_string();
    let sudo = ctx.sudo_prefix();
    let mut unpack = cfg_bool(cfg, "unpack", false);
    let mut steps: Vec<PlannedStep> = Vec::new();

    // --- build ---------------------------------------------------------------
    // An SPA that has to be compiled before it can be shipped. Build-time
    // variables are baked into the bundle, so they belong here rather than in
    // the runtime env the containers read.
    if let Some(build) = cfg_str(cfg, "build") {
        let build_dir = cfg_str(cfg, "build_dir").unwrap_or_else(|| ".".into());
        let expand = |value: &str| {
            value
                .replace("{version}", &ctx.version.marketing_version())
                .replace("{release}", &ctx.version.release_display())
                .replace("{deploy}", &ctx.version.id)
                .replace("{sha}", &ctx.version.git.short_sha)
                .replace("{work}", &ctx.work_dir())
        };
        let env: String = cfg
            .get("env")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
                    .map(|(k, v)| format!("{k}='{}' ", expand(v).replace('\'', "'\\''")))
                    .collect()
            })
            .unwrap_or_default();
        steps.push(PlannedStep::command_in(
            format!("build ({build_dir})"),
            format!("{env}{build}"),
            build_dir,
        ));
    }

    // A directory can't travel over scp as one atomic unit, and it's the release
    // archive that makes the swap atomic — so package it, exactly as `hugo` does.
    if ctx.repo_root.join(&src).is_dir() {
        let label = cfg_str(cfg, "remote_subdir")
            .filter(|s| s != "." && !s.is_empty())
            .or_else(|| src.rsplit('/').next().map(str::to_string))
            .unwrap_or_else(|| "files".into());
        let tarball = format!("{}/{}-{label}.tar.gz", ctx.work_dir(), ctx.app);
        steps.push(PlannedStep::command(
            format!("package {src} → {tarball}"),
            // --no-xattrs + COPYFILE_DISABLE stop macOS from embedding
            // LIBARCHIVE.xattr.* entries that GNU tar on the target warns about.
            format!(
                "mkdir -p \"$(dirname {tarball})\" && \
                 COPYFILE_DISABLE=1 tar --no-xattrs -czf {tarball} -C {src} ."
            ),
        ));
        src = tarball;
        unpack = true;
    }
    let name = src.rsplit('/').next().unwrap_or(&src).to_string();

    // `remote_subdir: "."` means the served path *is* the target directory —
    // the case where nginx's root can't be repointed (someone else's vhost, a
    // shared box). The directory becomes the symlink instead, which nginx
    // follows, so no vhost has to change. Releases then live in a sibling
    // directory: nesting them under a served path would publish every old
    // release over HTTP, and the incoming tarball would land inside the site.
    let subdir = cfg_str(cfg, "remote_subdir").unwrap_or_else(|| "current".into());
    let serves_root = subdir == "." || subdir.is_empty();
    let live = if serves_root {
        root.clone()
    } else {
        format!("{root}/{subdir}")
    };
    let releases = cfg_str(cfg, "releases_dir").unwrap_or_else(|| {
        if serves_root {
            format!("{root}.releases")
        } else {
            format!("{root}/releases")
        }
    });
    // Land the archive beside the releases, never inside the served path.
    let landing = if unpack {
        releases.clone()
    } else {
        root.clone()
    };

    // Where deploy state lives. When the served path *is* the symlink, `root`
    // points inside whichever release is current — so state written there is
    // orphaned by the next swap and the history restarts at #1 every time. The
    // releases directory is outside the swap, and outside the web root.
    let state = if serves_root {
        releases.clone()
    } else {
        root.clone()
    };

    steps.push(PlannedStep::ssh(
        format!("ensure {landing}"),
        format!("{sudo}mkdir -p {landing}"),
    ));
    steps.push(PlannedStep::command(
        format!("ship {src} → {}:{landing}/", ctx.dest_label()),
        ctx.copy(&src, &landing),
    ));

    if !unpack {
        return Ok(steps); // plain file ship
    }

    // --- atomic release ------------------------------------------------------
    // The release directory is named after the deploy id, so `readlink` on the
    // live path tells you exactly what's deployed.
    let stamp = ctx.version.id.clone();
    let rel = format!("{releases}/{stamp}");
    let prev_marker = format!("{releases}/.deliver-previous");
    let dir_mode = cfg_str(cfg, "dir_mode").unwrap_or_else(|| "755".into());
    let file_mode = cfg_str(cfg, "file_mode").unwrap_or_else(|| "644".into());
    let owner = cfg_str(cfg, "owner");
    let keep: u32 = cfg
        .get("keep_releases")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(1) as u32;

    let owner_flags = match owner.as_deref().and_then(|o| o.split_once(':')) {
        Some((u, g)) => format!("-o {u} -g {g} "),
        None => String::new(),
    };

    steps.push(PlannedStep::ssh(
        format!("stage release {stamp}"),
        format!("{sudo}install -d -m {dir_mode} {owner_flags}{rel}"),
    ));
    steps.push(PlannedStep::ssh(
        format!("unpack {name} → releases/{stamp}"),
        format!("{sudo}tar -xzf {landing}/{name} -C {rel}"),
    ));
    if let Some(owner) = &owner {
        steps.push(PlannedStep::ssh(
            format!("chown {owner} releases/{stamp}"),
            format!("{sudo}chown -R {owner} {rel}"),
        ));
    }
    steps.push(PlannedStep::ssh(
        format!("chmod releases/{stamp} ({dir_mode}/{file_mode})"),
        format!(
            "{sudo}find {rel} -type d -exec chmod {dir_mode} {{}} + && \
             {sudo}find {rel} -type f -exec chmod {file_mode} {{}} +"
        ),
    ));

    // Guard the disaster case: an empty/corrupt artifact must never be activated.
    steps.push(PlannedStep::ssh(
        format!("check release {stamp} is not empty"),
        format!(
            "set -e; if [ -z \"$({sudo}ls -A {rel} 2>/dev/null)\" ]; then \
               echo 'release {stamp} is empty — refusing to activate' >&2; exit 1; fi; \
             echo \"release {stamp} looks sane\""
        ),
    ));

    // The atomic switch. `ln -sfn` + `mv -T` replaces the symlink in one rename,
    // so readers (nginx) always see a complete tree — zero downtime.
    let activate = format!(
        "set -e; \
         PREV=''; \
         if [ -L {live} ]; then \
           PREV=$(readlink -f {live} || true); \
         elif [ -d {live} ]; then \
           PREV={releases}/premigrate-{stamp}; \
           echo 'migrating pre-existing directory {live} into releases/'; \
           {sudo}mv {live} \"$PREV\"; \
         fi; \
         printf '%s\\n' \"$PREV\" | {sudo}tee {prev_marker} >/dev/null; \
         {sudo}ln -sfn {rel} {live}.new; \
         {sudo}mv -Tf {live}.new {live}; \
         echo \"activated {stamp} (previous: ${{PREV:-none}})\""
    );
    // Undo: point the symlink back at whatever was live before.
    let undo = format!(
        "set -e; \
         PREV=$({sudo}cat {prev_marker} 2>/dev/null || true); \
         if [ -n \"$PREV\" ] && [ -d \"$PREV\" ]; then \
           {sudo}ln -sfn \"$PREV\" {live}.rb; {sudo}mv -Tf {live}.rb {live}; \
           echo \"rolled back {subdir} → $PREV\"; \
         else \
           echo 'no previous release recorded — cannot roll back {subdir}' >&2; exit 1; \
         fi"
    );
    steps.push(
        PlannedStep::ssh(
            format!("activate release {stamp} (atomic symlink swap)"),
            activate,
        )
        .with_rollback(undo),
    );

    // Phase 16 of the lifecycle: a durable record on the target of what was
    // deployed, so "what is live and when did it land?" is answerable without a
    // server. This is the seam the service tier will later ingest.
    let release = ctx.version.release_display();
    let sha = &ctx.version.git.sha;
    steps.push(PlannedStep::ssh(
        format!("record deploy {stamp} (release {release})"),
        format!(
            "set -e; {sudo}mkdir -p {state}/.deliver; \
             N=1; if {sudo}test -f {state}/.deliver/history.tsv; then \
               N=$(( $({sudo}wc -l {state}/.deliver/history.tsv | awk '{{print $1}}') + 1 )); fi; \
             printf '%s\\t%s\\t%s\\t%s\\t%s\\n' \"$N\" \"{stamp}\" \"{release}\" \"{sha}\" \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" \
               | {sudo}tee -a {state}/.deliver/history.tsv >/dev/null; \
             echo \"deploy #$N · {stamp} · release {release}\""
        ),
    ));

    // Cleanup: the shipped archive has served its purpose once unpacked. Left
    // alone it accumulates in the deploy directory forever.
    steps.push(
        PlannedStep::ssh(
            format!("remove shipped {name} from the target"),
            format!("{sudo}rm -f {landing}/{name}"),
        )
        .into_cleanup(),
    );
    if src.starts_with(&ctx.work_dir()) {
        steps.push(
            PlannedStep::command(format!("remove local {src}"), format!("rm -f {src}"))
                .into_cleanup(),
        );
    }

    steps.push(PlannedStep::ssh(
        format!("prune old releases (keep {keep})"),
        format!(
            "cd {releases} && ls -1d */ 2>/dev/null | sort -r | tail -n +{} | \
             xargs -r {sudo}rm -rf; echo \"kept newest {keep}\"",
            keep + 1
        ),
    ));

    Ok(steps)
}
