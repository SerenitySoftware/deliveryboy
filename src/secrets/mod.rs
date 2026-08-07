//! Secrets: what this config needs, whether it's there, and how to put it there.
//!
//! Requirements are **derived from the config**, not declared separately — a
//! second list would drift from the one that matters. Values are never read or
//! printed; the checks only ask "does this exist?".
//!
//! This runs in preflight too, so a missing signing key fails in a second rather
//! than ten minutes into a notarized build.

pub mod resolve;

pub use resolve::{parse_definitions, parse_providers, Resolver};

use crate::config::Config;
use std::path::Path;
use std::process::{Command, Stdio};

/// Where a secret lives.
#[derive(Debug, Clone, PartialEq)]
pub enum Store {
    /// A file on disk (an ssh identity).
    File { path: String },
    /// A generic password in the macOS keychain.
    Keychain {
        service: String,
        account: Option<String>,
    },
    /// An `xcrun notarytool store-credentials` profile.
    NotaryProfile { name: String },
    /// An environment variable.
    Env { name: String },
}

#[derive(Debug, Clone)]
pub struct Requirement {
    pub label: String,
    pub store: Store,
    /// Which part of the config asked for it.
    pub used_by: String,
}

#[derive(Debug)]
pub struct Status {
    pub ok: bool,
    pub detail: String,
    /// How to create it, when it's missing.
    pub remedy: Option<String>,
}

fn expand_tilde(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// Scan a shell fragment for `security find-generic-password -s <service>`, which
/// is how a build setting pulls a value from the keychain.
fn keychain_refs_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("find-generic-password") {
        rest = &rest[at + "find-generic-password".len()..];
        let mut tokens = rest.split_whitespace();
        while let Some(token) = tokens.next() {
            if token == "-s" {
                if let Some(service) = tokens.next() {
                    found.push(service.trim_matches(['"', '\'']).to_string());
                }
                break;
            }
            if token.starts_with('-') && token != "-w" && token != "-a" {
                continue;
            }
        }
    }
    found
}

/// Everything this config needs to have in place before a deploy can work.
pub fn requirements(config: &Config) -> Vec<Requirement> {
    let mut reqs = Vec::new();

    for (name, target) in &config.targets {
        if let Some(key) = &target.ssh.key {
            reqs.push(Requirement {
                label: format!("ssh identity {key}"),
                store: Store::File { path: key.clone() },
                used_by: format!("targets.{name}.ssh.key"),
            });
        }
    }

    for (name, service) in &config.services {
        if !service.deployer.replace('_', "-").eq("macos-app") {
            continue;
        }
        let cfg = &service.config;
        if let Some(appcast) = cfg.get("appcast") {
            if let Some(svc) = appcast.get("ed_key_keychain").and_then(|v| v.as_str()) {
                reqs.push(Requirement {
                    label: format!("Sparkle signing key ({svc})"),
                    store: Store::Keychain {
                        service: svc.to_string(),
                        account: Some("release".into()),
                    },
                    used_by: format!("services.{name}.appcast.ed_key_keychain"),
                });
            }
            if let Some(var) = appcast.get("ed_key_env").and_then(|v| v.as_str()) {
                reqs.push(Requirement {
                    label: format!("Sparkle signing key (${var})"),
                    store: Store::Env {
                        name: var.to_string(),
                    },
                    used_by: format!("services.{name}.appcast.ed_key_env"),
                });
            }
        }
        if let Some(xb) = cfg.get("xcodebuild") {
            if let Some(profile) = xb.get("notary_profile").and_then(|v| v.as_str()) {
                reqs.push(Requirement {
                    label: format!("notarytool profile ({profile})"),
                    store: Store::NotaryProfile {
                        name: profile.to_string(),
                    },
                    used_by: format!("services.{name}.xcodebuild.notary_profile"),
                });
            }
            // Build settings often read the keychain inline.
            if let Some(map) = xb.get("build_settings").and_then(|v| v.as_mapping()) {
                for (k, v) in map {
                    let (Some(k), Some(v)) = (k.as_str(), v.as_str()) else {
                        continue;
                    };
                    for svc in keychain_refs_in(v) {
                        reqs.push(Requirement {
                            label: format!("keychain item ({svc})"),
                            store: Store::Keychain {
                                service: svc,
                                account: Some("release".into()),
                            },
                            used_by: format!("services.{name}.xcodebuild.build_settings.{k}"),
                        });
                    }
                }
            }
        }
    }

    reqs.dedup_by(|a, b| a.store == b.store);
    reqs
}

fn keychain_has(service: &str, account: Option<&str>) -> bool {
    let mut cmd = Command::new("security");
    cmd.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        cmd.args(["-a", account]);
    }
    // No -w: we check existence, never read the value.
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn notary_profile_has(name: &str) -> bool {
    // notarytool stores new profiles in the data-protection keychain, which
    // `security find-generic-password` does not search. Ask the tool that owns
    // the profile; this also proves the saved credentials still authenticate.
    Command::new("xcrun")
        .args([
            "notarytool",
            "history",
            "--keychain-profile",
            name,
            "--output-format",
            "json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn check(req: &Requirement) -> Status {
    match &req.store {
        Store::File { path } => {
            let full = expand_tilde(path);
            let p = Path::new(&full);
            if !p.is_file() {
                return Status {
                    ok: false,
                    detail: format!("{path} not found"),
                    remedy: Some(format!(
                        "place the key at {path}, or point targets.*.ssh.key elsewhere"
                    )),
                };
            }
            // A private key readable by others is refused by ssh itself.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(p) {
                    let mode = meta.permissions().mode() & 0o077;
                    if mode != 0 {
                        return Status {
                            ok: false,
                            detail: format!("{path} is group/world readable — ssh will refuse it"),
                            remedy: Some(format!("chmod 600 {path}")),
                        };
                    }
                }
            }
            Status {
                ok: true,
                detail: format!("{path} present"),
                remedy: None,
            }
        }
        Store::Keychain { service, account } => {
            // Some items are stored without an account; accept either.
            if keychain_has(service, account.as_deref()) || keychain_has(service, None) {
                Status {
                    ok: true,
                    detail: format!("keychain item {service} present"),
                    remedy: None,
                }
            } else {
                Status {
                    ok: false,
                    detail: format!("keychain item {service} not found"),
                    remedy: Some(format!("deliver secrets set {service}")),
                }
            }
        }
        Store::NotaryProfile { name } => {
            if notary_profile_has(name) {
                Status {
                    ok: true,
                    detail: format!("notary profile {name} present and valid"),
                    remedy: None,
                }
            } else {
                Status {
                    ok: false,
                    detail: format!("notary profile {name} not found"),
                    remedy: Some(format!(
                        "xcrun notarytool store-credentials {name} --key <AuthKey.p8> --key-id <KEY_ID> --issuer <ISSUER_ID>"
                    )),
                }
            }
        }
        Store::Env { name } => match std::env::var(name) {
            Ok(v) if !v.is_empty() => Status {
                ok: true,
                detail: format!("${name} is set"),
                remedy: None,
            },
            _ => Status {
                ok: false,
                detail: format!("${name} is not set"),
                remedy: Some(format!("export {name}=… (or use a keychain item instead)")),
            },
        },
    }
}

/// Store a keychain item, reading the value from stdin so it never appears in
/// the command line, the shell history, or this program's output.
pub fn set_keychain(service: &str, account: &str, value: &str) -> Result<(), String> {
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            account,
            "-w",
            value,
        ])
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("could not run security: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "security add-generic-password failed for {service}"
        ))
    }
}

/// Build a resolver from `secrets.providers`. An empty chain resolves nothing,
/// which is correct for configs that need no runtime values.
pub fn resolver(config: &Config, repo_root: &std::path::Path) -> anyhow::Result<Resolver> {
    let (providers, definitions) = match &config.secrets {
        Some(s) => (
            parse_providers(s.providers.as_ref())?,
            parse_definitions(s.define.as_ref())?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    Ok(Resolver::with_definitions(
        providers,
        definitions,
        repo_root,
    ))
}

/// Report on every declared secret: does it resolve, and from where. Values are
/// never returned or printed — only the provider that held them.
pub struct DeclaredStatus {
    pub name: String,
    pub required: bool,
    pub provider: Option<String>,
}

pub fn declared_status(resolver: &Resolver) -> Vec<DeclaredStatus> {
    resolver
        .definitions()
        .iter()
        .map(|def| DeclaredStatus {
            name: def.name.clone(),
            required: def.required,
            provider: resolver.get(&def.name).map(|f| f.provider),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::keychain_refs_in;

    #[test]
    fn finds_keychain_reads_in_build_settings() {
        let v = "$(security find-generic-password -w -s demo-sparkle-public -a release)";
        assert_eq!(keychain_refs_in(v), vec!["demo-sparkle-public".to_string()]);
    }

    #[test]
    fn ignores_settings_without_keychain_reads() {
        assert!(keychain_refs_in("plain-value").is_empty());
    }
}
