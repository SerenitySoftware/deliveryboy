//! `macos_app` — build, sign, notarize and publish a macOS app with a Sparkle
//! appcast.
//!
//! Division of labour (deliberate):
//!
//! * **This deployer** owns the release *lifecycle* — the guards, the appcast,
//!   publishing, and verification. Those are identical across apps.
//! * **The configured build strategy** owns the build *specifics* — pre-build
//!   artifacts, project generation, signing embedded binaries, notarizing, and
//!   packaging the DMG. Those differ per app.
//!
//! The guard that matters most: **Sparkle orders updates by `CFBundleVersion`**.
//! If a release ships a build number at or below what's already published,
//! clients silently stop seeing updates. CI used to supply `github.run_number`,
//! which doesn't exist off GitHub — so the build number comes from the deploy's
//! release identity (commit count) and is checked against the *live* appcast
//! before anything is built.
//!
//! ```yaml
//! macos:
//!   deployer: macos-app
//!   needs: [web]                 # Sparkle's notes URL must already be live
//!   config:
//!     lane: release              # fastlane lane (or another build strategy)
//!     dmg: target/dist/Demo-{version}.dmg
//!     appcast:
//!       url: https://example.com/download/mac/appcast.xml
//!       download_url_prefix: https://example.com/download/mac/
//!       notes: notes/{version}.html
//!       ed_key_keychain: demo-sparkle-private
//!     publish:
//!       remote_subdir: downloads/mac
//!       aliases: [Demo.dmg, latest/Demo.dmg]
//!       extra: [build/helper-{version}.dmg]       # public, checked by URL
//!       archive: [build/Demo-{version}.dSYM.tgz] # private on the target
//!       archive_remote_subdir: .deliver/symbols
//! ```

use super::{cfg_bool, cfg_str, PlanContext, PlannedStep};
use anyhow::{bail, Context, Result};
use serde_yaml::Value;

/// `{version}` / `{build}` placeholders, so config reads naturally.
fn expand(template: &str, ctx: &PlanContext, build: &str) -> String {
    template
        .replace("{version}", &ctx.version.marketing_version())
        .replace("{release}", &ctx.version.release_display())
        .replace("{build}", build)
        .replace("{sha}", &ctx.version.git.short_sha)
        .replace("{deploy}", &ctx.version.id)
        .replace("{work}", &ctx.work_dir())
}

/// Extra paths that should contain the same published DMG. Paths are relative
/// to `publish.remote_subdir`; nested paths are allowed.
fn publish_aliases(
    cfg: &Value,
    ctx: &PlanContext,
    build: &str,
    dmg_name: &str,
) -> Result<Vec<String>> {
    let Some(value) = cfg.get("publish").and_then(|p| p.get("aliases")) else {
        return Ok(Vec::new());
    };
    let aliases = value
        .as_sequence()
        .context("macos-app: publish.aliases must be a list of relative paths")?;
    let mut result = Vec::new();
    for value in aliases {
        let template = value
            .as_str()
            .context("macos-app: each publish.aliases entry must be a string")?;
        let alias = expand(template, ctx, build);
        let path = std::path::Path::new(&alias);
        let safe = !alias.is_empty()
            && !path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)));
        if !safe {
            bail!("macos-app: publish alias '{alias}' must be a relative path without '.' or '..'");
        }
        if alias == dmg_name || result.contains(&alias) {
            bail!("macos-app: duplicate publish output '{alias}'");
        }
        result.push(alias);
    }
    Ok(result)
}

fn publish_files(
    cfg: &Value,
    key: &str,
    ctx: &PlanContext,
    build: &str,
) -> Result<Vec<(String, String)>> {
    let Some(value) = cfg.get("publish").and_then(|p| p.get(key)) else {
        return Ok(Vec::new());
    };
    let values = value.as_sequence().with_context(|| {
        format!("macos-app: publish.{key} must be a list of local artifact paths")
    })?;
    let mut result = Vec::new();
    for value in values {
        let template = value
            .as_str()
            .with_context(|| format!("macos-app: each publish.{key} entry must be a string"))?;
        let path = expand(template, ctx, build);
        let name = std::path::Path::new(&path)
            .file_name()
            .and_then(|value| value.to_str())
            .with_context(|| format!("macos-app: publish.{key} path must end in a filename"))?
            .to_string();
        if result.iter().any(|(_, existing)| existing == &name) {
            bail!("macos-app: duplicate publish.{key} artifact '{name}'");
        }
        result.push((path, name));
    }
    Ok(result)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    // Fail at compile time, not halfway through a release: this only runs on macOS.
    if std::env::consts::OS != "macos" {
        bail!(
            "macos_app requires macOS (running on {}). Build the Mac app from a Mac; \
             a remote macOS runner is a later milestone.",
            std::env::consts::OS
        );
    }

    // Sparkle, the DMG name, and the appcast all need a release number. `plan`
    // may compile an untagged commit, while `deploy --version` supplies the
    // number without creating a tag until the release succeeds.
    let Some(_) = ctx.version.release.as_ref() else {
        return Ok(vec![PlannedStep::command(
            "guard: HEAD must be tagged to cut a Mac release".to_string(),
            "echo 'macos-app: HEAD is not tagged, so there is no release number. \
Tag the commit (e.g. `git tag -a v0.4.1 -m v0.4.1`) or run `deliver deploy --version 0.4.1`.' >&2; exit 1"
                .to_string(),
        )]);
    };
    let version = ctx.version.marketing_version();
    // Sparkle's CFBundleVersion: monotonic, and independent of any CI counter.
    let build = ctx.version.git.commit_count.to_string();
    let sudo = ctx.sudo_prefix();
    let root = ctx.target.dir.trim_end_matches('/').to_string();

    let appcast = cfg.get("appcast");
    let appcast_url = appcast
        .and_then(|a| a.get("url"))
        .and_then(|v| v.as_str())
        .context("macos-app: appcast.url is required (the live feed to seed from)")?
        .to_string();
    // Default the artifact into scratch — a DMG is a build output, not repo content.
    let dmg =
        cfg_str(cfg, "dmg").unwrap_or_else(|| format!("{{work}}/{}-{{version}}.dmg", ctx.app));
    let dmg = expand(&dmg, ctx, &build);
    let dmg_name = dmg.rsplit('/').next().unwrap_or(&dmg).to_string();
    let aliases = publish_aliases(cfg, ctx, &build, &dmg_name)?;
    let extras = publish_files(cfg, "extra", ctx, &build)?;
    let archives = publish_files(cfg, "archive", ctx, &build)?;

    let mut steps = Vec::new();

    // --- guards, before anything is built ------------------------------------
    steps.push(PlannedStep::command(
        format!("guard: release {version} is well-formed and the tree is clean"),
        format!(
            "set -e; \
             echo '{version}' | grep -Eq '^v?[0-9]+\\.[0-9]+\\.[0-9]+([.-][0-9A-Za-z.-]+)?$' \
               || {{ echo 'macos-app: release \"{version}\" is not a version number — tag the commit or set release.version' >&2; exit 1; }}; \
             {}",
            if cfg_bool(cfg, "allow_dirty", false) {
                "echo 'skipping clean-tree check (allow_dirty)'".to_string()
            } else {
                "test -z \"$(git status --porcelain)\" \
                   || { echo 'macos-app: working tree is dirty — commit or stash; a release must match a commit' >&2; exit 1; }"
                    .to_string()
            }
        ),
    ));

    // THE guard: never ship a build number Sparkle would ignore.
    steps.push(PlannedStep::command(
        format!("guard: build {build} is newer than the published appcast"),
        format!(
            "set -e; \
             PUBLISHED=$(curl -fsS {appcast_url} 2>/dev/null \
               | grep -oE '<sparkle:version>[0-9]+' | grep -oE '[0-9]+' | sort -n | tail -1); \
             PUBLISHED=${{PUBLISHED:-0}}; \
             if [ '{build}' -le \"$PUBLISHED\" ]; then \
               echo \"macos-app: build {build} is not greater than published $PUBLISHED — \
Sparkle orders updates by this number, so clients would never see the update\" >&2; exit 1; fi; \
             echo \"build {build} > published $PUBLISHED\""
        ),
    ));

    // Release notes: the sidecar Sparkle attaches, and the live URL it links to.
    let notes = appcast
        .and_then(|a| a.get("notes"))
        .and_then(|v| v.as_str())
        .map(|n| expand(n, ctx, &build));
    if let Some(notes) = &notes {
        steps.push(PlannedStep::command(
            format!("guard: release notes {notes} exist"),
            format!(
                "test -s {notes} || {{ echo 'macos-app: missing release notes at {notes}' >&2; exit 1; }}"
            ),
        ));
    }
    if let Some(url) = appcast
        .and_then(|a| a.get("notes_url"))
        .and_then(|v| v.as_str())
    {
        let url = expand(url, ctx, &build);
        steps.push(PlannedStep::http(
            format!("guard: notes are live at {url}"),
            url,
            200,
            1,
            0,
        ));
    }

    // --- build ---------------------------------------------------------------
    steps.extend(build_steps(cfg, ctx, &version, &build, &dmg)?);

    steps.push(PlannedStep::command(
        format!("check {dmg} was produced"),
        format!(
            "test -s {dmg} || {{ echo 'macos-app: the build did not produce {dmg}' >&2; exit 1; }}"
        ),
    ));

    // --- appcast -------------------------------------------------------------
    // generate_appcast must see the *existing* feed, or previously published
    // items vanish from it.
    let stage = format!("{}/appcast", ctx.work_dir());
    let ed_keychain = appcast
        .and_then(|a| a.get("ed_key_keychain"))
        .and_then(|v| v.as_str());
    let ed_env = appcast
        .and_then(|a| a.get("ed_key_env"))
        .and_then(|v| v.as_str());
    let key_expr = match (ed_keychain, ed_env) {
        (Some(service), _) => format!(
            "security find-generic-password -w -s {service} -a release 2>/dev/null || \
             security find-generic-password -w -s {service} 2>/dev/null"
        ),
        (None, Some(var)) => format!("printf '%s' \"${var}\""),
        (None, None) => bail!(
            "macos-app: set appcast.ed_key_keychain (local) or appcast.ed_key_env — \
             generate_appcast needs the Sparkle EdDSA private key"
        ),
    };
    let notes_name = notes
        .as_ref()
        .map(|_| format!("{}.html", dmg_name.trim_end_matches(".dmg")));
    let notes_copy = match (&notes, &notes_name) {
        (Some(n), Some(name)) => format!("cp {n} {stage}/{name}; "),
        _ => String::new(),
    };
    let download_prefix = appcast
        .and_then(|a| a.get("download_url_prefix"))
        .and_then(|v| v.as_str())
        .map(|p| format!("--download-url-prefix {p} "))
        .unwrap_or_default();
    let alias_copies = aliases
        .iter()
        .map(|alias| {
            let source = shell_quote(&format!("{stage}/{dmg_name}"));
            let destination = shell_quote(&format!("{stage}/{alias}"));
            format!("mkdir -p \"$(dirname {destination})\"; cp {source} {destination}; ")
        })
        .collect::<String>();
    let extra_copies = extras
        .iter()
        .map(|(path, name)| {
            format!(
                "test -s {} || {{ echo 'macos-app: missing extra artifact {name}' >&2; exit 1; }}; cp {} {}; ",
                shell_quote(path),
                shell_quote(path),
                shell_quote(&format!("{stage}/{name}"))
            )
        })
        .collect::<String>();

    steps.push(PlannedStep::command(
        format!("appcast: seed from live, add {dmg_name}, sign"),
        format!(
            "set -e; rm -rf {stage}; mkdir -p {stage}; \
             cp {dmg} {stage}/{dmg_name}; \
             {notes_copy}\
             curl -fsS {appcast_url} -o {stage}/appcast.xml || echo 'no published appcast yet — creating a new feed'; \
             GEN=$(find target -path '*/SourcePackages/artifacts/sparkle/Sparkle/bin/generate_appcast' -type f 2>/dev/null | head -1); \
             [ -n \"$GEN\" ] || GEN=$(command -v generate_appcast 2>/dev/null || true); \
             [ -n \"$GEN\" ] || GEN=$(find \"$(brew --prefix)/Caskroom/sparkle\" -type f -name generate_appcast 2>/dev/null | head -1); \
             [ -n \"$GEN\" ] || {{ echo 'macos-app: generate_appcast not found in built Swift packages, on PATH, or in the Homebrew cask' >&2; exit 1; }}; \
             ( {key_expr} ) | \"$GEN\" --ed-key-file - {download_prefix}{stage}; \
             test -s {stage}/appcast.xml; \
             {alias_copies}{extra_copies}true"
        ),
    ));

    // --- publish -------------------------------------------------------------
    let remote_subdir = cfg
        .get("publish")
        .and_then(|p| p.get("remote_subdir"))
        .and_then(|v| v.as_str())
        .unwrap_or("downloads/mac")
        .to_string();
    let dest = format!("{root}/{remote_subdir}");
    let owner = cfg
        .get("publish")
        .and_then(|p| p.get("owner"))
        .and_then(|v| v.as_str())
        .unwrap_or("www-data:www-data")
        .to_string();

    steps.push(PlannedStep::ssh(
        format!("ensure {dest}"),
        format!(
            "{sudo}install -d -m 755 -o {} -g {} {dest}",
            owner.split(':').next().unwrap_or("root"),
            owner.split(':').nth(1).unwrap_or("root")
        ),
    ));
    // Additive on purpose: previously published DMGs must stay downloadable, or
    // older clients lose their update path.
    let mut publish_names = vec![dmg_name.clone()];
    publish_names.extend(aliases.iter().cloned());
    if let Some(name) = &notes_name {
        publish_names.push(name.clone());
    }
    publish_names.extend(extras.iter().map(|(_, name)| name.clone()));
    publish_names.push("appcast.xml".to_string());
    steps.push(PlannedStep::command(
        format!(
            "publish {} → {}",
            publish_names.join(" + "),
            ctx.dest_label()
        ),
        ctx.copy_many(
            &publish_names
                .iter()
                .map(|n| format!("{stage}/{n}"))
                .collect::<Vec<_>>(),
            &dest,
            false,
        ),
    ));
    steps.push(PlannedStep::ssh(
        format!("fix perms on {remote_subdir}"),
        format!(
            "{sudo}chown -R {owner} {dest} && \
             {sudo}find {dest} -type d -exec chmod 755 {{}} + && \
             {sudo}find {dest} -type f -exec chmod 644 {{}} +"
        ),
    ));

    // Debug symbols and similar operator-only files belong on the target but
    // not under the public download URL.
    if !archives.is_empty() {
        let archive_subdir = cfg
            .get("publish")
            .and_then(|p| p.get("archive_remote_subdir"))
            .and_then(|v| v.as_str())
            .unwrap_or(".deliver/macos-archives");
        let archive_dest = format!("{root}/{archive_subdir}");
        let archive_owner = cfg
            .get("publish")
            .and_then(|p| p.get("archive_owner"))
            .and_then(|v| v.as_str())
            .unwrap_or("root:root");
        let archive_paths = archives
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        steps.push(PlannedStep::ssh(
            format!("ensure private archive {archive_dest}"),
            format!(
                "{sudo}install -d -m 750 -o {} -g {} {archive_dest}",
                archive_owner.split(':').next().unwrap_or("root"),
                archive_owner.split(':').nth(1).unwrap_or("root")
            ),
        ));
        steps.push(PlannedStep::command(
            format!(
                "archive {} → {}",
                archives
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(" + "),
                ctx.dest_label()
            ),
            ctx.copy_many(&archive_paths, &archive_dest, false),
        ));
        steps.push(PlannedStep::ssh(
            "lock private Mac archives to the release operator",
            format!(
                "{sudo}chown {archive_owner} {} && {sudo}chmod 640 {}",
                archives
                    .iter()
                    .map(|(_, name)| shell_quote(&format!("{archive_dest}/{name}")))
                    .collect::<Vec<_>>()
                    .join(" "),
                archives
                    .iter()
                    .map(|(_, name)| shell_quote(&format!("{archive_dest}/{name}")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ));
        for (_, name) in &archives {
            steps.push(PlannedStep::ssh(
                format!("verify private archive {name}"),
                format!("test -s {}", shell_quote(&format!("{archive_dest}/{name}"))),
            ));
        }
    }

    // --- verify --------------------------------------------------------------
    steps.push(PlannedStep::http(
        "verify appcast is live".to_string(),
        appcast_url.clone(),
        200,
        5,
        5,
    ));
    if let (Some(prefix), Some(name)) = (
        appcast
            .and_then(|a| a.get("download_url_prefix"))
            .and_then(|v| v.as_str()),
        notes_name,
    ) {
        steps.push(PlannedStep::http(
            "verify Sparkle release notes are downloadable",
            format!("{}/{}", prefix.trim_end_matches('/'), name),
            200,
            3,
            5,
        ));
    }
    steps.push(PlannedStep::command(
        format!("verify appcast advertises {version}"),
        format!(
            "curl -fsS {appcast_url} | grep -Fq '{version}' \
             || {{ echo 'macos-app: published appcast does not mention {version}' >&2; exit 1; }}"
        ),
    ));
    steps.push(
        PlannedStep::command(format!("remove {stage}"), format!("rm -rf {stage}")).into_cleanup(),
    );

    if let Some(prefix) = appcast
        .and_then(|a| a.get("download_url_prefix"))
        .and_then(|v| v.as_str())
    {
        let prefix = prefix.trim_end_matches('/').to_string() + "/";
        steps.push(PlannedStep::http(
            format!("verify {dmg_name} is downloadable"),
            format!("{prefix}{dmg_name}"),
            200,
            3,
            5,
        ));
        for alias in &aliases {
            let artifact_url = format!("{prefix}{dmg_name}");
            let alias_url = format!("{prefix}{alias}");
            steps.push(PlannedStep::http(
                format!("verify alias {alias} is downloadable"),
                alias_url.clone(),
                200,
                3,
                5,
            ));
            steps.push(PlannedStep::command(
                format!("verify alias {alias} matches {dmg_name}"),
                format!(
                    "set -e; \
                     set -- $(curl -fsS {} | shasum -a 256); ARTIFACT_SHA=$1; \
                     set -- $(curl -fsS {} | shasum -a 256); ALIAS_SHA=$1; \
                     [ \"$ARTIFACT_SHA\" = \"$ALIAS_SHA\" ] \
                       || {{ echo 'macos-app: published alias {alias} does not match {dmg_name}' >&2; exit 1; }}",
                    shell_quote(&artifact_url),
                    shell_quote(&alias_url),
                ),
            ));
        }
        for (_, name) in &extras {
            steps.push(PlannedStep::http(
                format!("verify extra artifact {name} is downloadable"),
                format!("{prefix}{name}"),
                200,
                3,
                5,
            ));
        }
    }

    Ok(steps)
}

/// Compile the build phase for the chosen strategy.
///
/// * `xcodebuild` — Apple's own toolchain, no extra dependencies. What the
///   fleet already uses.
/// * `fastlane`   — for repos that want it; the lane owns the build.
/// * `command`    — an escape hatch (any shell command that produces the dmg).
fn build_steps(
    cfg: &Value,
    ctx: &PlanContext,
    version: &str,
    build: &str,
    dmg: &str,
) -> Result<Vec<PlannedStep>> {
    // Default to whichever block is present, so config stays terse.
    let strategy = cfg_str(cfg, "strategy").unwrap_or_else(|| {
        if cfg.get("xcodebuild").is_some() {
            "xcodebuild".into()
        } else if cfg.get("command").is_some() || cfg.get("build_command").is_some() {
            "command".into()
        } else {
            "fastlane".into()
        }
    });

    match strategy.as_str() {
        "command" => {
            let command = cfg_str(cfg, "command")
                .or_else(|| cfg_str(cfg, "build_command"))
                .context("macos-app: strategy 'command' needs `command:`")?;
            let command = expand(&command, ctx, build);
            Ok(vec![PlannedStep::command(
                format!("build: {command}"),
                command,
            )])
        }
        "fastlane" => {
            let fl = cfg.get("fastlane");
            let lane = fl
                .and_then(|f| f.get("lane"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| cfg_str(cfg, "lane"))
                .unwrap_or_else(|| "release".into());
            let label = format!("fastlane {lane} (build · sign · notarize · staple · dmg)");
            // The lane is told the versions; it must not invent its own.
            let command = format!(
                "VERSION={version} BUILD={build} fastlane {lane} version:{version} build:{build}"
            );
            Ok(vec![
                match fl.and_then(|f| f.get("dir")).and_then(|v| v.as_str()) {
                    Some(dir) => PlannedStep::command_in(label, command, dir),
                    None => PlannedStep::command(label, command),
                },
            ])
        }
        "xcodebuild" => xcodebuild_steps(cfg, ctx, version, build, dmg),
        other => bail!("macos-app: unknown strategy '{other}' (xcodebuild|fastlane|command)"),
    }
}

/// The raw Apple toolchain path: archive → export → sign → dmg → notarize → staple.
///
/// Order matters and is not incidental: an embedded Mach-O (a bundled server
/// binary, a helper) must be signed with the hardened runtime **before** the
/// outer bundle, or notarization rejects the DMG.
fn xcodebuild_steps(
    cfg: &Value,
    _ctx: &PlanContext,
    version: &str,
    build: &str,
    dmg: &str,
) -> Result<Vec<PlannedStep>> {
    let xb = cfg
        .get("xcodebuild")
        .context("macos-app: strategy 'xcodebuild' needs an `xcodebuild:` block")?;
    let get = |key: &str| xb.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let project = get("project").context("macos-app: xcodebuild.project is required")?;
    let scheme = get("scheme").context("macos-app: xcodebuild.scheme is required")?;
    let configuration = get("configuration").unwrap_or_else(|| "Release".into());
    let destination = get("destination").unwrap_or_else(|| "generic/platform=macOS".into());
    let identity = get("identity").unwrap_or_else(|| "Developer ID Application".into());
    let dist = get("dist_dir").unwrap_or_else(|| "target/dist".into());
    let app_name = get("app_name").unwrap_or_else(|| format!("{scheme}.app"));
    let volname = get("volname").unwrap_or_else(|| scheme.clone());
    let archive = format!("{dist}/{scheme}.xcarchive");
    let export_dir = format!("{dist}/export");
    let app = format!("{export_dir}/{app_name}");
    let staged_dmg = format!("{dist}/{scheme}.dmg");

    let mut steps = Vec::new();

    // Optional pre-build (e.g. an embedded runtime built from source).
    if let Some(prebuild) = get("prebuild") {
        let prebuild = expand(&prebuild, _ctx, build);
        steps.push(PlannedStep::command(
            format!("prebuild: {prebuild}"),
            prebuild,
        ));
    }
    if let Some(generate) = get("generate") {
        steps.push(PlannedStep::command(
            format!("generate project: {generate}"),
            generate,
        ));
    }

    // Extra build settings, verbatim (values may contain shell, e.g. a keychain read).
    let mut settings = vec![
        format!("MARKETING_VERSION='{version}'"),
        format!("CURRENT_PROJECT_VERSION={build}"),
        format!("CODE_SIGN_IDENTITY='{identity}'"),
        "CODE_SIGN_STYLE=Manual".to_string(),
    ];
    if let Some(team) = get("team") {
        settings.push(format!("DEVELOPMENT_TEAM={team}"));
    }
    if let Some(archs) = get("archs") {
        settings.push(format!("ARCHS={archs}"));
    }
    if let Some(map) = xb.get("build_settings").and_then(|v| v.as_mapping()) {
        for (k, v) in map {
            if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                settings.push(format!("{k}=\"{v}\""));
            }
        }
    }

    steps.push(PlannedStep::command(
        format!("xcodebuild archive {scheme} ({version} build {build})"),
        format!(
            "set -e; rm -rf {dist} && mkdir -p {dist}; \
             xcodebuild archive -project {project} -scheme {scheme} \
               -configuration {configuration} -destination '{destination}' \
               -archivePath {archive} -clonedSourcePackagesDirPath {dist}/SourcePackages \
               {}",
            settings.join(" ")
        ),
    ));

    if let Some(symbols) = get("symbols") {
        let symbols = expand(&symbols, _ctx, build);
        let dsym = format!("{archive}/dSYMs/{scheme}.app.dSYM");
        steps.push(PlannedStep::command(
            format!("archive dSYM → {symbols}"),
            format!(
                "set -e; test -d '{dsym}'; mkdir -p \"$(dirname '{symbols}')\"; \
                 COPYFILE_DISABLE=1 tar --no-xattrs -czf '{symbols}' -C '{archive}/dSYMs' '{scheme}.app.dSYM'"
            ),
        ));
    }

    let export_options = get("export_options")
        .context("macos-app: xcodebuild.export_options (an ExportOptions plist) is required")?;
    steps.push(PlannedStep::command(
        format!("export {app_name}"),
        format!(
            "xcodebuild -exportArchive -archivePath {archive} \
               -exportPath {export_dir} -exportOptionsPlist {export_options}"
        ),
    ));

    if let Some(seconds) = xb
        .get("smoke_test_seconds")
        .and_then(|v| v.as_u64())
        .filter(|v| *v > 0)
    {
        let log = format!("{dist}/startup.log");
        steps.push(PlannedStep::command(
            format!("smoke test {app_name} startup ({seconds}s)"),
            format!(
                "set -e; '{app}/Contents/MacOS/{scheme}' >'{log}' 2>&1 & PID=$!; \
                 for _ in $(seq 1 {seconds}); do sleep 1; if ! kill -0 $PID 2>/dev/null; then \
                   wait $PID || STATUS=$?; sed -n '1,200p' '{log}'; \
                   echo 'macos-app: exported app exited during startup' >&2; exit ${{STATUS:-1}}; fi; done; \
                 kill $PID; wait $PID || true"
            ),
        ));
    }

    // Inner binaries first — hardened runtime — then the outer bundle.
    let embedded: Vec<String> = xb
        .get("sign_embedded")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for rel in &embedded {
        steps.push(PlannedStep::command(
            format!("sign embedded {rel} (hardened runtime)"),
            format!(
                "set -e; test -e '{app}/{rel}' || {{ echo 'macos-app: embedded binary not found: {app}/{rel}' >&2; exit 1; }}; \
                 codesign --force --timestamp --options runtime --sign '{identity}' '{app}/{rel}'"
            ),
        ));
    }
    if cfg_bool(xb, "resign_app", true) {
        steps.push(PlannedStep::command(
            format!("sign + verify {app_name}"),
            format!(
                "set -e; codesign --force --timestamp --options runtime --sign '{identity}' '{app}'; \
                 codesign --verify --deep --strict --verbose=2 '{app}'"
            ),
        ));
    } else {
        steps.push(PlannedStep::command(
            format!("verify exported signature for {app_name}"),
            format!("codesign --verify --deep --strict --verbose=2 '{app}'"),
        ));
    }

    // DMG with the conventional /Applications drop target.
    steps.push(PlannedStep::command(
        format!("package {scheme}.dmg"),
        format!(
            "set -e; STAGE={dist}/dmg-stage; rm -rf \"$STAGE\"; mkdir -p \"$STAGE\"; \
             cp -R '{app}' \"$STAGE/\"; ln -sf /Applications \"$STAGE/Applications\"; \
             hdiutil create -volname {volname} -srcfolder \"$STAGE\" -ov -format UDZO {staged_dmg}; \
             codesign --force --timestamp --sign '{identity}' {staged_dmg}"
        ),
    ));

    let profile = get("notary_profile").context(
        "macos-app: xcodebuild.notary_profile is required (xcrun notarytool store-credentials)",
    )?;
    steps.push(PlannedStep::command(
        format!("notarize + staple {scheme}.dmg"),
        format!(
            "set -e; xcrun notarytool submit {staged_dmg} --keychain-profile {profile} --wait; \
             xcrun stapler staple {staged_dmg}; xcrun stapler validate {staged_dmg}"
        ),
    ));

    // Hand the artifact to the rest of the lifecycle under its published name.
    steps.push(PlannedStep::command(
        format!("stage {dmg}"),
        format!("set -e; mkdir -p \"$(dirname {dmg})\"; cp {staged_dmg} {dmg}"),
    ));

    Ok(steps)
}
