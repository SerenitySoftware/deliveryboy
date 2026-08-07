//! Repo detection for `deliver init`.
//!
//! Answers "how is this app currently deployed, and which deploy strategies does
//! it match?" by inspecting the working tree, then scaffolds a `.deliver.yml`
//! from what it finds. This is the inverse of `plan`: plan reads a config and
//! prints steps; init reads a repo and writes a config.

use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Deployer this maps to (e.g. "hugo"), or None when it's only informational.
    pub deployer: Option<&'static str>,
    /// Suggested service name.
    pub service: String,
    /// What we saw, shown to the operator.
    pub evidence: String,
    /// Config lines for the scaffold.
    pub config: Vec<(String, String)>,
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

pub fn detect(root: &Path) -> Vec<Finding> {
    let mut found = Vec::new();

    if let Some(dir) = hugo_dir(root) {
        found.push(Finding {
            deployer: Some("hugo"),
            service: "web".into(),
            evidence: format!("Hugo site at {dir} (hugo config + content/layouts)"),
            config: vec![
                ("source".into(), dir),
                ("minify".into(), "true".into()),
                ("remote_subdir".into(), "web".into()),
                ("owner".into(), "www-data:www-data".into()),
            ],
        });
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
        found.push(Finding {
            deployer: None, // docker_compose deployer not implemented yet
            service: "app".into(),
            evidence: format!("Docker Compose project ({})", files.join(" + ")),
            config: vec![("files".into(), files.join(", "))],
        });
    }

    if let Some(conf) = first_nginx_conf(root) {
        let script = find_nginx_script(root);
        let mut config = vec![("conf".into(), conf.clone())];
        let evidence = match &script {
            Some(s) => {
                config.push(("strategy".into(), "script".into()));
                config.push(("script".into(), format!("{s} activate")));
                format!("nginx vhost {conf} with its own install script ({s})")
            }
            None => {
                config.push(("strategy".into(), "managed".into()));
                format!("nginx vhost {conf} (no install script — Delivery Boy can manage it)")
            }
        };
        found.push(Finding {
            deployer: Some("nginx-vhost"),
            service: "nginx".into(),
            evidence,
            config,
        });
    }

    // Informational: shapes we can see but don't have a deployer for yet.
    if exists(root, "config/deploy.yml") {
        found.push(Finding {
            deployer: None,
            service: "kamal".into(),
            evidence: "Kamal config at config/deploy.yml".into(),
            config: vec![],
        });
    }
    let xcode = std::fs::read_dir(root.join("apps/macos"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().ends_with(".xcodeproj"))
        })
        .unwrap_or(false);
    if xcode || exists(root, "fastlane/Fastfile") || exists(root, "apps/macos/project.yml") {
        found.push(Finding {
            deployer: None,
            service: "macos".into(),
            evidence:
                "macOS app (Xcode project / fastlane) — macos-app deployer (configure it manually)"
                    .into(),
            config: vec![],
        });
    }

    found
}

/// Render a `.deliver.yml` from findings that map to a real deployer.
pub fn scaffold(app: &str, host: &str, dir: &str, findings: &[Finding]) -> String {
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
                let quoted = if v.contains(' ') || v.contains('/') && k == "script" {
                    format!("\"{v}\"")
                } else {
                    v.clone()
                };
                out.push_str(&format!("      {k}: {quoted}\n"));
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
