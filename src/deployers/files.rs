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
//! For established symlink releases, the live path is never emptied. Everything
//! is staged into a fresh release
//! directory, sanity-checked, and only then swapped in with a single atomic
//! rename of the symlink — so nginx never observes a missing or half-written
//! webroot, and the previous release stays on disk to roll back to.
//!
//! (The old behavior — clean the live directory, then untar into it — had a
//! window where a bad tarball left the site empty with nothing to restore.)
//!
//! With `unpack: false` it's a plain copy of a single file; no releases needed.

use super::{cfg_bool, cfg_str, PlanContext, PlannedStep, ReleaseState};
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
        // `export`, not a `VAR=value cmd` prefix. A prefix binds to exactly
        // one command, so with the near-universal
        // `npm ci && npm run build` it reached `npm ci` and never the build
        // that needs it — and a Vite app whose `VITE_*` variable is missing
        // does not fail, it silently falls back to its localhost default
        // and ships a bundle pointing at a developer's machine. That is
        // how Meridian's panel went to production broken while
        // `.deliver.yml` plainly declared the right value.
        let env: String = cfg
            .get("env")
            .and_then(|v| v.as_mapping())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.as_str()?, v.as_str()?)))
                    .map(|(k, v)| format!("export {k}='{}'; ", expand(v).replace('\'', "'\\''")))
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
    //
    // `build:` is what *creates* `src`, and it runs long after the plan is
    // compiled — so on a clean checkout `src` is not there to test. A build
    // output is a directory (`dist/`, `build/`) unless it is already an
    // archive; deciding from disk instead compiled a plain `scp` of a path
    // that did not exist yet.
    let built_later = cfg.get("build").is_some()
        && !ctx.repo_root.join(&src).exists()
        && !(src.ends_with(".tar.gz") || src.ends_with(".tgz"));
    if ctx.repo_root.join(&src).is_dir() || built_later {
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
    steps.push(
        PlannedStep::ssh(
            format!("unpack {name} → releases/{stamp}"),
            format!("{sudo}tar -xzf {landing}/{name} -C {rel}"),
        )
        .needs_remote(&["tar"]),
    );
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

    // The atomic switch replaces the symlink in one rename,
    // so readers (nginx) always see a complete tree — zero downtime.
    let swap = crate::symlink_swap::SymlinkSwap::for_target(&ctx.target);
    let guard = swap.guard(&live, sudo);
    let mv = swap.command();
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
         {guard}\
         {sudo}ln -sfn {rel} {live}.new; \
         {sudo}{mv} {live}.new {live}; \
         printf '%s\\n' \"$PREV\" | {sudo}tee {prev_marker} >/dev/null; \
         echo \"activated {stamp} (previous: ${{PREV:-none}})\""
    );
    // Undo: point the symlink back at whatever was live before. An empty
    // marker is the activate step saying nothing was live — a first release —
    // so going back means serving nothing, not refusing and leaving the failed
    // release up. The history row then names `none`, which the read-back
    // takes as "nothing deployed".
    let history = format!("{state}/.deliver/history.tsv");
    let undo = format!(
        "set -e; \
         {guard}\
         PREV=$({sudo}cat {prev_marker} 2>/dev/null || true); \
         if [ -n \"$PREV\" ] && [ -d \"$PREV\" ]; then \
           {sudo}ln -sfn \"$PREV\" {live}.rb; {sudo}{mv} {live}.rb {live}; \
           echo \"rolled back {subdir} → $PREV\"; {record}; \
         elif [ -z \"$PREV\" ] && {sudo}test -f {prev_marker}; then \
           {sudo}rm -f {live}; \
           echo 'first release: removed {subdir} — nothing is served until a release passes its checks'; {first}; \
         else \
           echo 'no previous release recorded — cannot roll back {subdir}' >&2; exit 1; \
         fi",
        record =
            crate::rollback::unwind_record_command(&history, &stamp, "basename \"$PREV\"", sudo),
        first = crate::rollback::unwind_record_command(
            &history,
            &stamp,
            &format!("echo {}", crate::rollback::NOTHING_LIVE),
            sudo
        ),
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
        )
        // The read-back looks exactly where this step writes: `deliver status`
        // and `deliver history` are this record read back off the target, and
        // `deliver rollback --to` swaps the same symlink this step's activate
        // swapped, rewriting the same marker.
        .with_release_state(ReleaseState {
            history_path: format!("{state}/.deliver/history.tsv"),
            live_path: Some(live.clone()),
            releases_dir: Some(releases.clone()),
            previous_marker: Some(prev_marker.clone()),
            backups_dir: None,
        }),
    );

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

    // Prune as a *cleanup* step, so it runs after the service's `verify:`
    // checks and is skipped when the release fails — and never deletes what
    // the activate undo needs. As an ordinary step it ran before `verify:`,
    // so with `keep_releases: 1` (or after a `rollback --to` had landed on
    // the oldest retained release) it removed the directory
    // `.deliver-previous` names one step before the check that would need
    // it, and a failed verify left the failed release live. "Keep N" still
    // means the newest N by name; the live release and the one the marker
    // names are spared on top of that, never counted against it.
    steps.push(
        PlannedStep::ssh(
            format!("prune old releases (keep {keep})"),
            format!(
                "cd {releases} && \
                 LIVE=$(basename \"$({sudo}readlink -f {live} 2>/dev/null || echo /)\"); \
                 PREV=$(basename \"$({sudo}cat {prev_marker} 2>/dev/null || echo /)\"); \
                 ls -1d */ 2>/dev/null | sed 's:/$::' | sort -r | tail -n +{} | \
                 awk -v live=\"$LIVE\" -v prev=\"$PREV\" '$0 != live && $0 != prev' | \
                 xargs -r {sudo}rm -rf; echo \"kept newest {keep}\"",
                keep + 1
            ),
        )
        .into_cleanup(),
    );

    Ok(steps)
}

#[cfg(all(test, unix))]
mod swap_tests {
    use super::*;
    use crate::deployers::http_test_support::Context;
    use std::{fs, os::unix::fs::symlink, process::Command};

    fn run(command: &str) -> std::process::Output {
        Command::new("sh").args(["-c", command]).output().unwrap()
    }

    #[test]
    fn activation_and_undo_keep_sudo_and_ssh_gnu_flags() {
        let mut fixture = Context::new();
        fixture.ctx.sudo = true;
        for method in ["local", "ssh"] {
            fixture.ctx.target.method = method.into();
            let cfg = serde_yaml::from_str("{src: fixture.tgz, unpack: true}").unwrap();
            let steps = compile(&cfg, &fixture.ctx).unwrap();
            let activate = steps
                .iter()
                .find(|s| s.label.starts_with("activate release"))
                .unwrap();
            let expected = if method == "local" && cfg!(target_os = "macos") {
                "sudo /bin/mv -hf"
            } else {
                "sudo mv -Tf"
            };
            assert!(activate.detail().contains(expected));
            assert!(activate.rollback.as_ref().unwrap().contains(expected));
        }
    }

    #[test]
    fn native_release_activation_and_undo_cover_all_destination_shapes() {
        for shape in ["absent", "dangling", "link", "directory", "file"] {
            let mut fixture = Context::new();
            let root = fixture.ctx.repo_root.join("target");
            fs::create_dir_all(root.join("releases/old")).unwrap();
            fs::create_dir_all(root.join("releases/fixture-release")).unwrap();
            fs::write(root.join("releases/old/index"), "old").unwrap();
            fs::write(root.join("releases/fixture-release/index"), "new").unwrap();
            fixture.ctx.target.dir = root.to_string_lossy().into();
            let live = root.join("web");
            match shape {
                "dangling" => symlink(root.join("missing"), &live).unwrap(),
                "link" => symlink(root.join("releases/old"), &live).unwrap(),
                "directory" => {
                    fs::create_dir(&live).unwrap();
                    fs::write(live.join("index"), "old").unwrap();
                }
                "file" => fs::write(&live, "leave me").unwrap(),
                _ => {}
            }
            let cfg = serde_yaml::from_str("{src: fixture.tgz, unpack: true, remote_subdir: web}")
                .unwrap();
            let steps = compile(&cfg, &fixture.ctx).unwrap();
            let activate = steps
                .iter()
                .find(|s| s.label.starts_with("activate release"))
                .unwrap();
            let out = run(&activate.detail());
            // Preserve GNU replacement of regular files; the scoped BSD guard
            // deliberately refuses every existing non-symlink.
            if shape == "file" && cfg!(target_os = "macos") {
                assert!(!out.status.success());
                assert_eq!(fs::read_to_string(&live).unwrap(), "leave me");
                continue;
            }
            assert!(
                out.status.success(),
                "{shape}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(fs::read_to_string(live.join("index")).unwrap(), "new");
            assert!(!root.join("web.new").exists());
            let out = run(activate.rollback.as_ref().unwrap());
            if shape == "dangling" {
                assert!(!out.status.success());
                assert_eq!(fs::read_to_string(live.join("index")).unwrap(), "new");
                continue;
            }
            assert!(
                out.status.success(),
                "{shape}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if shape == "absent" || shape == "file" {
                assert!(!live.exists());
                assert!(!live.is_symlink());
            } else {
                assert_eq!(fs::read_to_string(live.join("index")).unwrap(), "old");
            }
            assert!(!root.join("web.rb").exists());
        }
    }

    #[test]
    fn failed_activation_and_undo_moves_keep_live_marker_and_history() {
        for stage in ["new", "rb"] {
            let mut fixture = Context::new();
            let root = fixture.ctx.repo_root.join("target");
            fs::create_dir_all(root.join("releases/old")).unwrap();
            fs::create_dir_all(root.join("releases/fixture-release")).unwrap();
            fs::create_dir_all(root.join(".deliver")).unwrap();
            fs::write(root.join("releases/old/index"), "old").unwrap();
            fs::write(root.join(".deliver/history.tsv"), "history stays").unwrap();
            fs::write(
                root.join("releases/.deliver-previous"),
                root.join("releases/old").to_str().unwrap(),
            )
            .unwrap();
            symlink(root.join("releases/old"), root.join("web")).unwrap();
            fs::create_dir(root.join(format!("web.{stage}"))).unwrap();
            fixture.ctx.target.dir = root.to_str().unwrap().into();
            let cfg = serde_yaml::from_str("{src: fixture.tgz, unpack: true, remote_subdir: web}")
                .unwrap();
            let steps = compile(&cfg, &fixture.ctx).unwrap();
            let activate = steps
                .iter()
                .find(|s| s.label.starts_with("activate release"))
                .unwrap();
            let marker = fs::read(root.join("releases/.deliver-previous")).unwrap();
            let command = if stage == "new" {
                activate.detail()
            } else {
                activate.rollback.clone().unwrap()
            };
            let out = run(&command);
            assert!(!out.status.success(), "{stage}: {command}");
            assert_eq!(fs::read_to_string(root.join("web/index")).unwrap(), "old");
            assert_eq!(
                fs::read(root.join("releases/.deliver-previous")).unwrap(),
                marker
            );
            assert_eq!(
                fs::read_to_string(root.join(".deliver/history.tsv")).unwrap(),
                "history stays"
            );
        }
    }

    #[test]
    fn undo_refuses_a_real_directory_without_changing_state() {
        let mut fixture = Context::new();
        let root = fixture.ctx.repo_root.join("target");
        fs::create_dir_all(root.join("releases/old")).unwrap();
        fs::create_dir_all(root.join("web")).unwrap();
        fs::write(root.join("web/index"), "untouched").unwrap();
        fixture.ctx.target.dir = root.to_string_lossy().into();
        let marker = root.join("releases/.deliver-previous");
        fs::write(
            &marker,
            root.join("releases/old").to_string_lossy().as_bytes(),
        )
        .unwrap();
        let cfg =
            serde_yaml::from_str("{src: fixture.tgz, unpack: true, remote_subdir: web}").unwrap();
        let steps = compile(&cfg, &fixture.ctx).unwrap();
        let undo = steps.iter().find_map(|s| s.rollback.as_ref()).unwrap();
        let before = fs::read(&marker).unwrap();
        let out = run(undo);
        assert!(!out.status.success());
        assert_eq!(fs::read(&marker).unwrap(), before);
        assert_eq!(
            fs::read_to_string(root.join("web/index")).unwrap(),
            "untouched"
        );
        assert!(!root.join("web/web.rb").exists());
        // The first-release removal branch must refuse it too.
        fs::write(&marker, "").unwrap();
        assert!(!run(undo).status.success());
        assert_eq!(
            fs::read_to_string(root.join("web/index")).unwrap(),
            "untouched"
        );
    }
}
