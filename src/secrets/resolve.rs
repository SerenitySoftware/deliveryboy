//! Resolving secret *values* from an ordered chain of providers.
//!
//! `secrets.providers` is a fallback list: the first provider that has a name
//! wins. That's what makes one config work everywhere — `env` covers CI, a
//! gitignored dotenv covers a quick local run, and keychain/1Password/sops cover
//! the real store — without per-machine config forks.
//!
//! Values are held in memory only, never logged, never written to plan output,
//! and only ever land on a target inside a `0600` file.

use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    /// The process environment — how CI supplies values.
    Env,
    /// A dotenv file (gitignored), for a quick local run.
    File { path: String },
    /// macOS keychain. `prefix` builds the service name: `<prefix><NAME>`.
    Keychain {
        prefix: Option<String>,
        service: Option<String>,
        account: Option<String>,
    },
    /// 1Password CLI. `reference` is a template, e.g. op://Vault/item/{name}.
    Op { reference: String },
    /// A sops-encrypted YAML/dotenv file, decrypted once and cached.
    Sops { file: String },
}

impl Provider {
    pub fn label(&self) -> String {
        match self {
            Provider::Env => "env".into(),
            Provider::File { path } => format!("file {path}"),
            Provider::Keychain { .. } => "keychain".into(),
            Provider::Op { .. } => "1password".into(),
            Provider::Sops { file } => format!("sops {file}"),
        }
    }
}

/// Parse `secrets.providers`, accepting both scalar and mapping forms:
///
/// ```yaml
/// providers:
///   - env
///   - file: .env.local
///   - keychain: {prefix: "amplifier-"}
///   - op: "op://Engineering/releases/{name}"
///   - sops: secrets.enc.yaml
/// ```
pub fn parse_providers(value: Option<&Value>) -> Result<Vec<Provider>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_sequence()
        .context("secrets.providers must be a list")?;
    let mut providers = Vec::new();

    for item in items {
        if let Some(name) = item.as_str() {
            providers.push(match name {
                "env" => Provider::Env,
                "keychain" => Provider::Keychain {
                    prefix: None,
                    service: None,
                    account: None,
                },
                other => {
                    bail!("secrets: provider '{other}' needs configuration (e.g. `{other}: …`)")
                }
            });
            continue;
        }
        let map = item
            .as_mapping()
            .context("secrets: each provider is either a name or a single-key mapping")?;
        let (key, cfg) = map.iter().next().context("secrets: empty provider entry")?;
        let key = key
            .as_str()
            .context("secrets: provider name must be a string")?;

        providers.push(match key {
            "env" => Provider::Env,
            "file" => Provider::File {
                path: cfg
                    .as_str()
                    .context("secrets: `file` takes a path")?
                    .to_string(),
            },
            "sops" => Provider::Sops {
                file: cfg
                    .as_str()
                    .context("secrets: `sops` takes a file path")?
                    .to_string(),
            },
            "op" | "1password" => Provider::Op {
                reference: cfg
                    .as_str()
                    .context(
                        "secrets: `op` takes a reference template, e.g. op://Vault/item/{name}",
                    )?
                    .to_string(),
            },
            "keychain" => {
                let get = |k: &str| cfg.get(k).and_then(|v| v.as_str()).map(str::to_string);
                Provider::Keychain {
                    prefix: get("prefix"),
                    service: get("service"),
                    account: get("account"),
                }
            }
            other => bail!("secrets: unknown provider '{other}' (env|file|keychain|op|sops)"),
        });
    }
    Ok(providers)
}

/// Parse `KEY=value` lines. Handles `export `, quotes, comments, and blank lines.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();
        // Strip matching quotes, but keep any inside the value.
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        map.insert(key.to_string(), value.to_string());
    }
    map
}

/// A secret this app needs, optionally sourced from somewhere other than the
/// default chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub name: String,
    /// Overrides the chain for this one secret.
    pub source: Option<Provider>,
    /// Optional secrets may be absent without failing a deploy.
    pub required: bool,
}

/// Parse `secrets.define`, accepting either form:
///
/// ```yaml
/// define: [POSTGRES_PASSWORD, SECRET_KEY]
/// # or
/// define:
///   POSTGRES_PASSWORD: {}                       # use the default chain
///   STRIPE_SECRET_KEY: {keychain: amp-stripe}   # override for this one
///   SENTRY_DSN: {op: "op://V/amplifier/dsn"}
///   LEGACY_FLAG: {required: false}
/// ```
pub fn parse_definitions(value: Option<&Value>) -> Result<Vec<Definition>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    if let Some(seq) = value.as_sequence() {
        return seq
            .iter()
            .map(|v| {
                let name = v
                    .as_str()
                    .context("secrets.define: list entries must be secret names")?;
                Ok(Definition {
                    name: name.to_string(),
                    source: None,
                    required: true,
                })
            })
            .collect();
    }

    let map = value
        .as_mapping()
        .context("secrets.define must be a list of names or a mapping")?;
    let mut defs = Vec::new();
    for (name, spec) in map {
        let name = name
            .as_str()
            .context("secrets.define: names must be strings")?
            .to_string();
        let required = spec
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Any provider-shaped key present makes this secret's own source.
        let source = match spec.as_mapping() {
            Some(m) => {
                let entries: Vec<_> = m
                    .iter()
                    .filter(|(k, _)| k.as_str().is_some_and(|k| k != "required"))
                    .collect();
                match entries.len() {
                    0 => None,
                    1 => {
                        let mut one = serde_yaml::Mapping::new();
                        one.insert(entries[0].0.clone(), entries[0].1.clone());
                        let wrapped = Value::Sequence(vec![Value::Mapping(one)]);
                        Some(
                            parse_providers(Some(&wrapped))?
                                .into_iter()
                                .next()
                                .expect("one provider"),
                        )
                    }
                    _ => bail!("secrets.define.{name}: give at most one source"),
                }
            }
            None => None,
        };
        defs.push(Definition {
            name,
            source,
            required,
        });
    }
    Ok(defs)
}

/// Resolves names against the provider chain, caching whole-file providers so a
/// 29-variable config doesn't shell out 29 times.
pub struct Resolver {
    providers: Vec<Provider>,
    definitions: Vec<Definition>,
    repo_root: std::path::PathBuf,
    files: std::cell::RefCell<BTreeMap<String, BTreeMap<String, String>>>,
}

/// Where a value came from — for reporting, never with the value itself.
pub struct Found {
    pub value: String,
    pub provider: String,
}

impl Resolver {
    #[cfg(test)]
    pub fn new(providers: Vec<Provider>, repo_root: &std::path::Path) -> Self {
        Self::with_definitions(providers, Vec::new(), repo_root)
    }

    pub fn with_definitions(
        providers: Vec<Provider>,
        definitions: Vec<Definition>,
        repo_root: &std::path::Path,
    ) -> Self {
        Self {
            providers,
            definitions,
            repo_root: repo_root.to_path_buf(),
            files: std::cell::RefCell::new(BTreeMap::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.definitions.iter().all(|d| d.source.is_none())
    }

    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// Every declared secret name — what a deployer gets when it doesn't list
    /// its own subset.
    pub fn declared_names(&self) -> Vec<String> {
        self.definitions.iter().map(|d| d.name.clone()).collect()
    }

    pub fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|d| d.name == name)
    }

    pub fn describe(&self) -> String {
        self.providers
            .iter()
            .map(|p| p.label())
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// A secret's own source wins; otherwise the first provider in the chain
    /// that has the name.
    pub fn get(&self, name: &str) -> Option<Found> {
        if let Some(source) = self.definition(name).and_then(|d| d.source.as_ref()) {
            let value = self.get_from(source, name)?;
            return (!value.is_empty()).then(|| Found {
                value,
                provider: source.label(),
            });
        }
        for provider in &self.providers {
            if let Some(value) = self.get_from(provider, name) {
                if !value.is_empty() {
                    return Some(Found {
                        value,
                        provider: provider.label(),
                    });
                }
            }
        }
        None
    }

    fn get_from(&self, provider: &Provider, name: &str) -> Option<String> {
        match provider {
            Provider::Env => std::env::var(name).ok(),

            Provider::File { path } => self.resolve_from_file(path, name, false),

            Provider::Sops { file } => self.resolve_from_file(file, name, true),

            Provider::Keychain {
                prefix,
                service,
                account,
            } => {
                let service = service
                    .clone()
                    .unwrap_or_else(|| format!("{}{name}", prefix.clone().unwrap_or_default()));
                let mut cmd = Command::new("security");
                cmd.args(["find-generic-password", "-w", "-s", &service]);
                if let Some(account) = account {
                    cmd.args(["-a", account]);
                }
                let out = cmd.stderr(Stdio::null()).output().ok()?;
                out.status.success().then(|| {
                    String::from_utf8_lossy(&out.stdout)
                        .trim_end_matches('\n')
                        .to_string()
                })
            }

            Provider::Op { reference } => {
                let reference = reference.replace("{name}", name);
                let out = Command::new("op")
                    .args(["read", "--no-newline", &reference])
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).to_string())
            }
        }
    }

    /// Read (and cache) a dotenv or sops-decrypted file.
    fn resolve_from_file(&self, path: &str, name: &str, encrypted: bool) -> Option<String> {
        let key = format!("{}:{path}", if encrypted { "sops" } else { "plain" });
        if !self.files.borrow().contains_key(&key) {
            let full = self.repo_root.join(path);
            let text = if encrypted {
                let out = Command::new("sops")
                    .args(["-d", &full.to_string_lossy()])
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                String::from_utf8_lossy(&out.stdout).to_string()
            } else {
                std::fs::read_to_string(&full).ok()?
            };
            // sops files are usually YAML; fall back to dotenv parsing either way.
            let parsed = match serde_yaml::from_str::<BTreeMap<String, Value>>(&text) {
                Ok(map) if encrypted => map
                    .into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                    .collect(),
                _ => parse_dotenv(&text),
            };
            self.files.borrow_mut().insert(key.clone(), parsed);
        }
        self.files.borrow().get(&key)?.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_forms() {
        let yaml: Value = serde_yaml::from_str(
            "- env\n- file: .env.local\n- keychain: {prefix: 'amp-'}\n- op: op://V/i/{name}\n- sops: s.enc.yaml\n",
        )
        .unwrap();
        let providers = parse_providers(Some(&yaml)).unwrap();
        assert_eq!(providers.len(), 5);
        assert_eq!(providers[0], Provider::Env);
        assert_eq!(
            providers[1],
            Provider::File {
                path: ".env.local".into()
            }
        );
        assert!(matches!(&providers[2], Provider::Keychain { prefix: Some(p), .. } if p == "amp-"));
        assert!(
            matches!(&providers[3], Provider::Op { reference } if reference.contains("{name}"))
        );
        assert!(matches!(&providers[4], Provider::Sops { file } if file == "s.enc.yaml"));
    }

    #[test]
    fn rejects_unknown_providers() {
        let yaml: Value = serde_yaml::from_str("- vault: x\n").unwrap();
        assert!(parse_providers(Some(&yaml)).is_err());
    }

    #[test]
    fn dotenv_handles_export_quotes_and_comments() {
        let map = parse_dotenv(
            "# comment\nexport A=1\nB=\"two words\"\nC='three'\n\nD=has=equals\nBAD_LINE\n",
        );
        assert_eq!(map.get("A").unwrap(), "1");
        assert_eq!(map.get("B").unwrap(), "two words");
        assert_eq!(map.get("C").unwrap(), "three");
        assert_eq!(map.get("D").unwrap(), "has=equals");
        assert!(!map.contains_key("BAD_LINE"));
    }

    #[test]
    fn first_provider_with_the_name_wins() {
        let dir = std::env::temp_dir().join("deliver-resolver-order");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env.a"), "SHARED=from-a\nONLY_A=a\n").unwrap();
        std::fs::write(dir.join(".env.b"), "SHARED=from-b\nONLY_B=b\n").unwrap();

        let r = Resolver::new(
            vec![
                Provider::File {
                    path: ".env.a".into(),
                },
                Provider::File {
                    path: ".env.b".into(),
                },
            ],
            &dir,
        );
        assert_eq!(
            r.get("SHARED").unwrap().value,
            "from-a",
            "earlier provider wins"
        );
        // Later providers still fill gaps the earlier one doesn't have.
        assert_eq!(r.get("ONLY_B").unwrap().value, "b");
        assert!(r.get("NOWHERE").is_none());
    }

    #[test]
    fn env_provider_reads_the_process_environment() {
        // Safety: single-threaded test process, restored immediately.
        unsafe { std::env::set_var("DELIVER_TEST_SECRET", "s3cret") };
        let r = Resolver::new(vec![Provider::Env], std::path::Path::new("."));
        let found = r.get("DELIVER_TEST_SECRET").unwrap();
        assert_eq!(found.value, "s3cret");
        assert_eq!(found.provider, "env");
        unsafe { std::env::remove_var("DELIVER_TEST_SECRET") };
    }

    #[test]
    fn empty_values_are_treated_as_absent() {
        let dir = std::env::temp_dir().join("deliver-resolver-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env.empty"), "BLANK=\n").unwrap();
        std::fs::write(dir.join(".env.real"), "BLANK=filled\n").unwrap();
        let r = Resolver::new(
            vec![
                Provider::File {
                    path: ".env.empty".into(),
                },
                Provider::File {
                    path: ".env.real".into(),
                },
            ],
            &dir,
        );
        assert_eq!(r.get("BLANK").unwrap().value, "filled");
    }
}
