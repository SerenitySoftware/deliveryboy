//! `hugo` — build the site, package it, hand off to `files`.
//! Builds a Hugo site, then packages its output for the file deployer.

use super::{cfg_bool, cfg_str, files, PlanContext, PlannedStep};
use anyhow::Result;
use serde_yaml::{Mapping, Value};

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let source = cfg_str(cfg, "source").unwrap_or_else(|| ".".into());
    // Intermediates live under .deliver/work so the repo stays clean and one
    // gitignore line covers them.
    let tarball = cfg_str(cfg, "tarball")
        .unwrap_or_else(|| format!("{}/{}-web.tar.gz", ctx.work_dir(), ctx.app));
    let build = if cfg_bool(cfg, "minify", true) {
        "hugo --minify"
    } else {
        "hugo"
    };

    let mut steps = vec![PlannedStep::command_in(
        format!("hugo build ({source})"),
        build,
        source.clone(),
    )];
    steps.push(PlannedStep::command(
        format!("package → {tarball}"),
        // --no-xattrs + COPYFILE_DISABLE stop macOS from embedding
        // LIBARCHIVE.xattr.* / AppleDouble entries, which GNU tar on the
        // target can't read and warns about for every single file.
        format!(
            "mkdir -p \"$(dirname {tarball})\" && \
                 COPYFILE_DISABLE=1 tar --no-xattrs -czf {tarball} -C {source}/public ."
        ),
    ));

    // Delegate shipping + install to the files deployer.
    let mut fc = Mapping::new();
    fc.insert("src".into(), tarball.into());
    fc.insert("unpack".into(), true.into());
    fc.insert("clean".into(), true.into());
    fc.insert(
        "remote_subdir".into(),
        cfg_str(cfg, "remote_subdir")
            .unwrap_or_else(|| "web".into())
            .into(),
    );
    fc.insert(
        "owner".into(),
        cfg_str(cfg, "owner")
            .unwrap_or_else(|| "www-data:www-data".into())
            .into(),
    );
    if let Some(rd) = cfg_str(cfg, "releases_dir") {
        fc.insert("releases_dir".into(), rd.into());
    }
    fc.insert(
        "dir_mode".into(),
        cfg_str(cfg, "dir_mode")
            .unwrap_or_else(|| "755".into())
            .into(),
    );
    fc.insert(
        "file_mode".into(),
        cfg_str(cfg, "file_mode")
            .unwrap_or_else(|| "644".into())
            .into(),
    );

    steps.extend(files::compile(&Value::Mapping(fc), ctx)?);
    Ok(steps)
}
