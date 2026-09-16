//! Repo detection for `deliver init`.
//!
//! Answers "how is this app currently deployed, and which deploy strategies does
//! it match?" by inspecting the working tree, then scaffolds a `.deliver.yml`
//! from what it finds. This is the inverse of `plan`: plan reads a config and
//! prints steps; init reads a repo and writes a config.

use std::path::Path;

/// One value in a scaffolded `config:` block.
///
/// The richer deployers are configured with nested blocks (`image:`,
/// `appcast:`, `backup:`) and lists (`files:`), so a flat key/value pair cannot
/// describe what they need — a list written as `files: a, b` is a *string* to
/// every YAML parser, not a sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    Scalar(String),
    List(Vec<String>),
    Block(Vec<(String, ConfigValue)>),
}

impl ConfigValue {
    fn scalar(value: impl Into<String>) -> Self {
        ConfigValue::Scalar(value.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Deployer this maps to (e.g. "hugo"), or None when it's only informational.
    pub deployer: Option<&'static str>,
    /// Suggested service name.
    pub service: String,
    /// What we saw, shown to the operator.
    pub evidence: String,
    /// Config lines for the scaffold.
    pub config: Vec<(String, ConfigValue)>,
    /// What the scaffold cannot know and the operator must supply. Printed by
    /// `deliver init` under the finding, because a config that looks complete
    /// but is missing a credential or a build context is worse than one that
    /// says so.
    pub notes: Vec<String>,
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

/// Find a Hugo project: hugo.toml/config.toml at the root or one level down.
fn hugo_dir(root: &Path) -> Option<String> {
    for candidate in ["", "apps/web", "web", "site", "www"] {
        let dir = if candidate.is_empty() {
            root.to_path_buf()
        } else {
            root.join(candidate)
        };
        if dir.join("hugo.toml").exists()
            || dir.join("hugo.yaml").exists()
            || dir.join("config.toml").exists()
        {
            // Require content/ or layouts/ so we don't match an unrelated config.toml.
            if dir.join("content").exists() || dir.join("layouts").exists() {
                return Some(if candidate.is_empty() {
                    ".".into()
                } else {
                    candidate.into()
                });
            }
        }
    }
    None
}

fn first_nginx_conf(root: &Path) -> Option<String> {
    let dir = root.join("nginx");
    let entries = std::fs::read_dir(dir).ok()?;
    let mut confs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".conf").then(|| format!("nginx/{name}"))
        })
        .collect();
    confs.sort();
    confs.into_iter().next()
}

fn find_nginx_script(root: &Path) -> Option<String> {
    for candidate in ["nginx/bootstrap-tls.sh", "nginx/install.sh"] {
        if exists(root, candidate) {
            return Some(candidate.into());
        }
    }
    None
}

/// What a Compose file says about itself, as far as `init` needs to know.
#[derive(Debug, Default, PartialEq)]
struct ComposeProject {
    /// Service that looks like a Postgres database, with the user and database
    /// name its environment declares.
    database: Option<(String, Option<String>, Option<String>)>,
    /// Top-level named volumes — the ones worth taking with a release.
    volumes: Vec<String>,
    /// Env files the services read.
    env_files: Vec<String>,
    /// True when at least one service builds rather than only pulling.
    builds: bool,
}

impl ComposeProject {
    /// The `backup:` block, when there is anything worth backing up.
    ///
    /// `user` and `name` are only written when the Compose file states them:
    /// the deployer defaults both to the project name, and a guess here would
    /// be a `pg_dump` that fails on the one run that matters.
    fn backup(&self) -> Option<ConfigValue> {
        let mut block = Vec::new();
        if let Some((service, user, name)) = &self.database {
            let mut db = vec![("service".into(), ConfigValue::scalar(service.clone()))];
            if let Some(user) = user {
                db.push(("user".into(), ConfigValue::scalar(user.clone())));
            }
            if let Some(name) = name {
                db.push(("name".into(), ConfigValue::scalar(name.clone())));
            }
            block.push(("database".into(), ConfigValue::Block(db)));
        }
        if !self.volumes.is_empty() {
            block.push(("volumes".into(), ConfigValue::List(self.volumes.clone())));
        }
        (!block.is_empty()).then_some(ConfigValue::Block(block))
    }
}

/// Read a Compose file for the few facts the scaffold can use.
///
/// Best effort by design: an unreadable or unparseable file yields an empty
/// project, which scaffolds the same config minus the evidence-backed extras.
fn read_compose(root: &Path, file: &str) -> ComposeProject {
    let mut project = ComposeProject::default();
    let Ok(text) = std::fs::read_to_string(root.join(file)) else {
        return project;
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return project;
    };

    if let Some(volumes) = doc.get("volumes").and_then(|v| v.as_mapping()) {
        project.volumes = volumes
            .keys()
            .filter_map(|k| k.as_str().map(str::to_string))
            .collect();
    }

    let Some(services) = doc.get("services").and_then(|v| v.as_mapping()) else {
        return project;
    };
    for (name, service) in services {
        let Some(name) = name.as_str() else { continue };
        if service.get("build").is_some() {
            project.builds = true;
        }
        match service.get("env_file") {
            Some(serde_yaml::Value::String(path)) => project.env_files.push(path.clone()),
            Some(serde_yaml::Value::Sequence(seq)) => project.env_files.extend(
                seq.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            ),
            _ => {}
        }
        let image = service.get("image").and_then(|v| v.as_str()).unwrap_or("");
        // The image is the evidence, not the service name: plenty of projects
        // call their database `db`, and plenty call something else `db`.
        if project.database.is_none() && image.split(':').next().unwrap_or("").contains("postgres")
        {
            let env = |key: &str| {
                service
                    .get("environment")
                    .and_then(|e| e.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            project.database = Some((name.to_string(), env("POSTGRES_USER"), env("POSTGRES_DB")));
        }
    }
    project.env_files.sort();
    project.env_files.dedup();
    project
}

/// True when something in this repo can actually be built into an image.
fn has_dockerfile(root: &Path, project: &ComposeProject) -> bool {
    project.builds
        || exists(root, "Dockerfile")
        || exists(root, "docker/Dockerfile")
        || exists(root, "Containerfile")
}

/// The first `.xcodeproj` in the conventional places, as a repo-relative path.
fn find_xcodeproj(root: &Path) -> Option<String> {
    for dir in ["apps/macos", "macos", "."] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".xcodeproj").then_some(name)
            })
            .collect();
        names.sort();
        if let Some(name) = names.into_iter().next() {
            return Some(if dir == "." {
                name
            } else {
                format!("{dir}/{name}")
            });
        }
    }
    None
}

/// A built front-end: a `package.json` with a `build` script, plus the output
/// directory that build writes.
///
/// This is the shape `files` with a `build:` step was written for — the
/// deployer supports it fully, so detecting it is wiring rather than new
/// machinery.
#[derive(Debug, Clone, PartialEq)]
struct WebBuild {
    /// Where `package.json` lives, relative to the repo root ("." at the top).
    dir: String,
    framework: &'static str,
    /// Output directory, relative to `dir`.
    out_dir: String,
    /// `npm ci && npm run build`, or the lockfile's equivalent.
    build: String,
    notes: Vec<String>,
}

/// The install + build pair implied by the lockfile that is actually present.
///
/// Reproducible installs matter more here than anywhere else in a deploy: the
/// bundle is compiled on the operator's machine and then frozen into a
/// release, so a floating dependency resolution is baked in permanently.
fn package_manager(dir: &Path) -> (&'static str, &'static str) {
    for (lock, install, run) in [
        (
            "pnpm-lock.yaml",
            "pnpm install --frozen-lockfile",
            "pnpm run build",
        ),
        ("yarn.lock", "yarn install --frozen-lockfile", "yarn build"),
        (
            "bun.lockb",
            "bun install --frozen-lockfile",
            "bun run build",
        ),
        ("bun.lock", "bun install --frozen-lockfile", "bun run build"),
    ] {
        if dir.join(lock).exists() {
            return (install, run);
        }
    }
    // `npm ci` needs a lockfile; without one only `npm install` works.
    if dir.join("package-lock.json").exists() {
        ("npm ci", "npm run build")
    } else {
        ("npm install", "npm run build")
    }
}

/// Map a dependency set onto the framework, its default output directory and
/// the prefix its build-time variables need.
///
/// Ordered most specific first: a Next or Astro project also depends on Vite,
/// and answering "Vite" for it would scaffold the wrong output directory.
fn framework_of(
    deps: &serde_json::Map<String, serde_json::Value>,
) -> (&'static str, &'static str, &'static str) {
    for (dep, name, out, prefix) in [
        ("next", "Next.js", "out", "NEXT_PUBLIC_"),
        ("@sveltejs/kit", "SvelteKit", "build", "PUBLIC_"),
        ("astro", "Astro", "dist", "PUBLIC_"),
        ("nuxt", "Nuxt", ".output/public", "NUXT_PUBLIC_"),
        ("@angular/cli", "Angular", "dist", "NG_"),
        ("react-scripts", "Create React App", "build", "REACT_APP_"),
        ("@vue/cli-service", "Vue CLI", "dist", "VUE_APP_"),
        ("parcel", "Parcel", "dist", ""),
        ("vite", "Vite", "dist", "VITE_"),
    ] {
        if deps.contains_key(dep) {
            return (name, out, prefix);
        }
    }
    ("a JavaScript build", "dist", "")
}

/// Where a front-end lives: the repo root, or the one conventional subdirectory
/// that holds one. Deliveryboy's own `apps/web` is this shape.
const WEB_DIRS: [&str; 6] = [".", "apps/web", "web", "frontend", "client", "ui"];

/// Find a built front-end and work out how to build and ship it.
///
/// Returns `None` when there is no `package.json`, or when it declares no
/// `build` script — a package with nothing to build is a library or a tooling
/// manifest (a Hugo site's Tailwind pipeline, say), not a deployable site.
fn web_build(root: &Path) -> Option<WebBuild> {
    for dir in WEB_DIRS {
        let base = if dir == "." {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        let Ok(text) = std::fs::read_to_string(base.join("package.json")) else {
            continue;
        };
        let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if pkg
            .get("scripts")
            .and_then(|s| s.get("build"))
            .and_then(|b| b.as_str())
            .is_none()
        {
            continue;
        }
        let mut deps = serde_json::Map::new();
        for key in ["dependencies", "devDependencies"] {
            if let Some(map) = pkg.get(key).and_then(|d| d.as_object()) {
                deps.extend(map.clone());
            }
        }
        let (framework, default_out, env_prefix) = framework_of(&deps);
        let (install, run) = package_manager(&base);
        let mut notes = Vec::new();

        // Prefer an output directory that is actually on disk: a repo that has
        // been built once is telling us where its build lands, which beats the
        // framework default whenever the project has overridden it.
        let candidates = [default_out, "dist", "build", "out", "public"];
        let out_dir = candidates
            .iter()
            .find(|c| base.join(c).is_dir())
            .map(|c| c.to_string())
            .unwrap_or_else(|| {
                notes.push(format!(
                    "`src` assumes this build writes to {default_out}/ — nothing is \
                         built yet, so check it against one real `{run}` before deploying"
                ));
                default_out.to_string()
            });

        if framework == "Next.js" {
            notes.push(
                "Next.js only writes a static `out/` with `output: 'export'` in \
                 next.config — a default `next build` produces a server application, \
                 which this deployer cannot serve"
                    .to_string(),
            );
        }
        if framework == "SvelteKit" {
            notes.push(
                "SvelteKit's output depends on its adapter — `build/` here assumes \
                 adapter-static"
                    .to_string(),
            );
        }
        // The failure this scaffold exists to prevent. `files` runs the build
        // locally, so anything the bundler reads has to be declared here or it
        // falls back to a development default and ships that.
        notes.push(match env_prefix {
            "" => "build-time variables are baked into the bundle — declare every one the \
                   build reads in an `env:` block, or it ships whatever default it \
                   falls back to"
                .to_string(),
            prefix => format!(
                "build-time variables are baked into the bundle — add an `env:` block for \
                 every `{prefix}*` the build reads (a missing one does not fail the \
                 build, it silently ships the development default)"
            ),
        });

        return Some(WebBuild {
            dir: dir.to_string(),
            framework,
            out_dir,
            build: format!("{install} && {run}"),
            notes,
        });
    }
    None
}

pub fn detect(root: &Path) -> Vec<Finding> {
    let mut found = Vec::new();

    if let Some(dir) = hugo_dir(root) {
        found.push(Finding {
            deployer: Some("hugo"),
            service: "web".into(),
            evidence: format!("Hugo site at {dir} (hugo config + content/layouts)"),
            config: vec![
                ("source".into(), ConfigValue::Scalar(dir)),
                ("minify".into(), ConfigValue::scalar("true")),
                ("remote_subdir".into(), ConfigValue::scalar("web")),
                ("owner".into(), ConfigValue::scalar("www-data:www-data")),
            ],
            notes: Vec::new(),
        });
    }

    // A built front-end, but only when Hugo has not already claimed the web
    // service: a Hugo site with a `package.json` is its asset pipeline, not a
    // second site to deploy.
    if found.is_empty() {
        if let Some(web) = web_build(root) {
            let src = if web.dir == "." {
                web.out_dir.clone()
            } else {
                format!("{}/{}", web.dir, web.out_dir)
            };
            let mut config = vec![("build".into(), ConfigValue::Scalar(web.build.clone()))];
            // `build_dir` defaults to the repo root, so writing "." would be a
            // scaffolded copy of a default.
            if web.dir != "." {
                config.push(("build_dir".into(), ConfigValue::Scalar(web.dir.clone())));
            }
            config.push(("src".into(), ConfigValue::Scalar(src.clone())));
            config.push(("remote_subdir".into(), ConfigValue::scalar("web")));
            config.push(("owner".into(), ConfigValue::scalar("www-data:www-data")));
            found.push(Finding {
                deployer: Some("files"),
                service: "web".into(),
                evidence: format!(
                    "{} front-end at {} (package.json + build script → {src})",
                    web.framework,
                    if web.dir == "." {
                        "the repo root"
                    } else {
                        &web.dir
                    },
                ),
                config,
                notes: web.notes,
            });
        }
    }

    let compose: Vec<&str> = ["docker-compose.yml", "compose.yaml", "docker-compose.yaml"]
        .into_iter()
        .filter(|f| exists(root, f))
        .collect();
    if !compose.is_empty() {
        let mut files = compose.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if exists(root, "docker-compose.prod.yml") {
            files.push("docker-compose.prod.yml".into());
        }
        let project = read_compose(root, &files[0]);
        // Only the keys this repo gives evidence for. `project` and `image.tag`
        // are deliberately absent: the deployer already defaults them to the
        // app name, and a scaffolded copy of a default is one more line to
        // drift.
        let mut config = vec![
            ("files".into(), ConfigValue::List(files.clone())),
            (
                "image".into(),
                ConfigValue::Block(vec![
                    ("context".into(), ConfigValue::scalar(".")),
                    ("platform".into(), ConfigValue::scalar("linux/amd64")),
                ]),
            ),
        ];
        if let Some(backup) = project.backup() {
            config.push(("backup".into(), backup));
        }
        let mut notes = Vec::new();
        // The deployer always builds the image locally, so a project with
        // nothing to build fails at `docker build` rather than at compile time.
        if !has_dockerfile(root, &project) {
            notes.push(
                "no Dockerfile found — this deployer always builds the image locally, so point \
                 image.context (and image.dockerfile) at whatever builds this app"
                    .to_string(),
            );
        }
        // An env file is deliberately *not* scaffolded. `env_file:` makes
        // Delivery Boy render the file from literals and resolved secrets, so
        // an empty block would ship an empty `.env` over a working one.
        if !project.env_files.is_empty() || exists(root, ".env.example") {
            notes.push(format!(
                "services read an env file ({}) — add an `env_file:` block with `literals:` and \
                 `from_secrets:` so it is rendered from your secret store; without one, no env \
                 file is shipped and the existing one on the target is left alone",
                if project.env_files.is_empty() {
                    ".env".to_string()
                } else {
                    project.env_files.join(", ")
                }
            ));
        }
        found.push(Finding {
            deployer: Some("docker-compose"),
            service: "app".into(),
            evidence: format!("Docker Compose project ({})", files.join(" + ")),
            config,
            notes,
        });
    }

    if let Some(conf) = first_nginx_conf(root) {
        let script = find_nginx_script(root);
        let mut config = vec![("conf".into(), ConfigValue::scalar(conf.clone()))];
        let evidence = match &script {
            Some(s) => {
                config.push(("strategy".into(), ConfigValue::scalar("script")));
                config.push((
                    "script".into(),
                    ConfigValue::Scalar(format!("{s} activate")),
                ));
                format!("nginx vhost {conf} with its own install script ({s})")
            }
            None => {
                config.push(("strategy".into(), ConfigValue::scalar("managed")));
                format!("nginx vhost {conf} (no install script — Delivery Boy can manage it)")
            }
        };
        found.push(Finding {
            deployer: Some("nginx-vhost"),
            service: "nginx".into(),
            evidence,
            config,
            notes: Vec::new(),
        });
    }

    // Informational: shapes we can see but don't have a deployer for yet.
    if exists(root, "config/deploy.yml") {
        found.push(Finding {
            deployer: None,
            service: "kamal".into(),
            evidence: "Kamal config at config/deploy.yml".into(),
            config: vec![],
            notes: Vec::new(),
        });
    }
    let xcodeproj = find_xcodeproj(root);
    let fastlane_dir = ["fastlane/Fastfile", "apps/macos/fastlane/Fastfile"]
        .into_iter()
        .find(|f| exists(root, f))
        .map(|f| f.trim_end_matches("/fastlane/Fastfile").to_string());
    if xcodeproj.is_some() || fastlane_dir.is_some() || exists(root, "apps/macos/project.yml") {
        let mut config = Vec::new();
        let evidence = match (&fastlane_dir, &xcodeproj) {
            // fastlane first: when a lane exists it already encodes the signing
            // and notarization this repo actually uses.
            (Some(dir), _) => {
                config.push(("strategy".into(), ConfigValue::scalar("fastlane")));
                config.push(("lane".into(), ConfigValue::scalar("release")));
                if !dir.is_empty() {
                    config.push((
                        "fastlane".into(),
                        ConfigValue::Block(vec![("dir".into(), ConfigValue::scalar(dir.clone()))]),
                    ));
                }
                format!(
                    "macOS app with a fastlane lane ({}fastlane/Fastfile)",
                    if dir.is_empty() {
                        String::new()
                    } else {
                        format!("{dir}/")
                    }
                )
            }
            (None, Some(project)) => {
                let scheme = project
                    .rsplit('/')
                    .next()
                    .unwrap_or(project)
                    .trim_end_matches(".xcodeproj")
                    .to_string();
                let dir = project.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
                config.push(("strategy".into(), ConfigValue::scalar("xcodebuild")));
                config.push((
                    "xcodebuild".into(),
                    ConfigValue::Block(vec![
                        ("project".into(), ConfigValue::scalar(project.clone())),
                        ("scheme".into(), ConfigValue::scalar(scheme)),
                        (
                            "export_options".into(),
                            ConfigValue::Scalar(format!("{dir}/ExportOptions.plist")),
                        ),
                    ]),
                ));
                format!("macOS app with an Xcode project ({project})")
            }
            (None, None) => {
                config.push(("strategy".into(), ConfigValue::scalar("fastlane")));
                config.push(("lane".into(), ConfigValue::scalar("release")));
                "macOS app (apps/macos/project.yml — XcodeGen)".to_string()
            }
        };
        // Sparkle's feed is how updates reach users, so it is the one block
        // that has to be there even as a stub. `{host}` and `{app}` are filled
        // in by `scaffold`.
        config.push((
            "appcast".into(),
            ConfigValue::Block(vec![
                (
                    "url".into(),
                    ConfigValue::scalar("https://{host}/download/mac/appcast.xml"),
                ),
                (
                    "download_url_prefix".into(),
                    ConfigValue::scalar("https://{host}/download/mac/"),
                ),
                (
                    "ed_key_keychain".into(),
                    ConfigValue::scalar("{app}-sparkle-private"),
                ),
            ]),
        ));
        config.push((
            "publish".into(),
            ConfigValue::Block(vec![(
                "remote_subdir".into(),
                ConfigValue::scalar("downloads/mac"),
            )]),
        ));
        found.push(Finding {
            deployer: Some("macos-app"),
            service: "macos".into(),
            evidence,
            config,
            notes: vec![
                "check appcast.url and download_url_prefix — Sparkle clients stop updating if \
                 they are wrong"
                    .to_string(),
                "store the Sparkle EdDSA private key before deploying: \
                 `deliver secrets set {app}-sparkle-private < key`"
                    .to_string(),
            ],
        });
    }

    found
}

/// Quote a scalar only where a bare one would be read as something else.
///
/// A plain YAML scalar copes with `/`, with `:` inside a word
/// (`www-data:www-data`) and with a URL; what it cannot carry unquoted is a
/// leading indicator character, a `key: value` pair inside the value, or a
/// trailing comment.
fn quote_scalar(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.contains(": ")
        || value.contains(" #")
        || value.ends_with(':')
        || value.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`', ' ',
        ]);
    if needs_quotes {
        format!("{:?}", value)
    } else {
        value.to_string()
    }
}

/// Write one `config:` entry, recursing into nested blocks.
fn render_value(
    out: &mut String,
    indent: usize,
    key: &str,
    value: &ConfigValue,
    fill: &dyn Fn(&str) -> String,
) {
    let pad = " ".repeat(indent);
    match value {
        ConfigValue::Scalar(v) => {
            out.push_str(&format!("{pad}{key}: {}\n", quote_scalar(&fill(v))));
        }
        ConfigValue::List(items) => {
            let rendered: Vec<String> = items.iter().map(|i| quote_scalar(&fill(i))).collect();
            out.push_str(&format!("{pad}{key}: [{}]\n", rendered.join(", ")));
        }
        ConfigValue::Block(entries) => {
            out.push_str(&format!("{pad}{key}:\n"));
            for (k, v) in entries {
                render_value(out, indent + 2, k, v, fill);
            }
        }
    }
}

/// Render a `.deliver.yml` from findings that map to a real deployer.
pub fn scaffold(app: &str, host: &str, dir: &str, findings: &[Finding]) -> String {
    let fill = |value: &str| value.replace("{app}", app).replace("{host}", host);
    let mut out = String::new();
    out.push_str(&format!(
        "# Generated by `deliver init` — review before deploying.\nversion: 1\napp: {app}\n\ndefaults:\n  target: production\n\ntargets:\n  production:\n    host: {host}\n    user: root\n    port: 22\n    dir: {dir}\n\nservices:\n"
    ));

    let usable: Vec<&Finding> = findings.iter().filter(|f| f.deployer.is_some()).collect();
    if usable.is_empty() {
        out.push_str("  # No supported deploy strategy detected — see docs/cli-plan.md.\n");
        return out;
    }

    let mut previous: Option<String> = None;
    for f in usable {
        let deployer = f.deployer.unwrap();
        out.push_str(&format!("  {}:\n    deployer: {deployer}\n", f.service));
        if let Some(prev) = &previous {
            out.push_str(&format!("    needs: [{prev}]\n"));
        }
        if !f.config.is_empty() {
            out.push_str("    config:\n");
            for (k, v) in &f.config {
                render_value(&mut out, 6, k, v, &fill);
            }
        }
        if deployer == "hugo" {
            out.push_str("    verify:\n      - http:\n          url: https://EXAMPLE/\n          expect_status: 200\n          retries: 5\n          interval: 10\n");
        }
        if deployer == "nginx-vhost" {
            out.push_str("    verify:\n      - remote_command: nginx -t\n");
        }
        previous = Some(f.service.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("deliver-detect-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_postgres_service_is_found_by_its_image_not_its_name() {
        let dir = tmp("pg");
        std::fs::write(
            dir.join("docker-compose.yml"),
            "services:\n  db:\n    image: redis:7\n  \
             store:\n    image: docker.io/library/postgres:16\n    \
             environment:\n      POSTGRES_USER: u\n      POSTGRES_DB: d\n",
        )
        .unwrap();
        let project = read_compose(&dir, "docker-compose.yml");
        assert_eq!(
            project.database,
            Some(("store".into(), Some("u".into()), Some("d".into())))
        );
    }

    #[test]
    fn a_database_with_no_declared_user_leaves_the_deployer_to_default_it() {
        let dir = tmp("pg-bare");
        std::fs::write(
            dir.join("docker-compose.yml"),
            "services:\n  db:\n    image: postgres:16\n",
        )
        .unwrap();
        let project = read_compose(&dir, "docker-compose.yml");
        assert_eq!(project.database, Some(("db".into(), None, None)));
        let backup = project.backup().unwrap();
        // Only the service — a guessed user is a `pg_dump` that fails.
        assert_eq!(
            backup,
            ConfigValue::Block(vec![(
                "database".into(),
                ConfigValue::Block(vec![("service".into(), ConfigValue::scalar("db"))])
            )])
        );
    }

    #[test]
    fn env_files_are_collected_from_both_spellings() {
        let dir = tmp("envfiles");
        std::fs::write(
            dir.join("docker-compose.yml"),
            "services:\n  a:\n    env_file: .env\n  b:\n    env_file: [.env, .env.prod]\n",
        )
        .unwrap();
        let project = read_compose(&dir, "docker-compose.yml");
        assert_eq!(project.env_files, vec![".env", ".env.prod"]);
    }

    #[test]
    fn an_unparseable_compose_file_yields_no_evidence_rather_than_an_error() {
        let dir = tmp("broken");
        std::fs::write(dir.join("docker-compose.yml"), "services: [oh: no: :\n").unwrap();
        let project = read_compose(&dir, "docker-compose.yml");
        assert_eq!(project, ComposeProject::default());
        assert!(project.backup().is_none());
    }

    #[test]
    fn a_nested_block_renders_as_indented_yaml() {
        let mut out = String::new();
        let fill = |v: &str| v.to_string();
        render_value(
            &mut out,
            6,
            "image",
            &ConfigValue::Block(vec![
                ("context".into(), ConfigValue::scalar(".")),
                (
                    "extra_tags".into(),
                    ConfigValue::List(vec!["{version}".into(), "{sha}".into()]),
                ),
            ]),
            &fill,
        );
        assert_eq!(
            out,
            "      image:\n        context: .\n        extra_tags: [\"{version}\", \"{sha}\"]\n"
        );
    }

    #[test]
    fn placeholders_are_filled_from_the_app_and_host_init_was_given() {
        let mut out = String::new();
        let fill = |v: &str| {
            v.replace("{app}", "demo")
                .replace("{host}", "demo.example.com")
        };
        render_value(
            &mut out,
            6,
            "url",
            &ConfigValue::scalar("https://{host}/download/{app}.xml"),
            &fill,
        );
        assert_eq!(
            out,
            "      url: https://demo.example.com/download/demo.xml\n"
        );
    }

    #[test]
    fn a_plain_scalar_is_only_quoted_where_yaml_would_misread_it() {
        // Valid bare: a path, a URL, and a colon inside a word.
        assert_eq!(quote_scalar("apps/web"), "apps/web");
        assert_eq!(quote_scalar("www-data:www-data"), "www-data:www-data");
        assert_eq!(quote_scalar("https://x/y"), "https://x/y");
        assert_eq!(quote_scalar("true"), "true");
        // Not valid bare: a mapping inside the value, a leading indicator, a
        // trailing comment, and the empty string.
        assert_eq!(quote_scalar("a: b"), "\"a: b\"");
        assert_eq!(quote_scalar("[x]"), "\"[x]\"");
        assert_eq!(quote_scalar("x #y"), "\"x #y\"");
        assert_eq!(quote_scalar(""), "\"\"");
    }

    #[test]
    fn an_xcodeproj_is_found_in_the_conventional_places() {
        let dir = tmp("xcode");
        std::fs::create_dir_all(dir.join("apps/macos/Demo.xcodeproj")).unwrap();
        assert_eq!(
            find_xcodeproj(&dir).as_deref(),
            Some("apps/macos/Demo.xcodeproj")
        );
        assert_eq!(find_xcodeproj(&tmp("xcode-none")), None);
    }
}
