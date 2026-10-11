//! Azure CLI adapter. No Azure DevOps producer or hosted agent is involved.
use super::{
    config::{Access, Site, Target},
    evidence::{self, Image},
    runner::Runner,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

/// Storage, coordination and hosting boundary. The coordinator owns acceptance.
pub trait Provider {
    fn preflight(&self) -> Result<()>;
    fn put(&self, name: &str, file: &Path) -> Result<()>;
    fn get(&self, name: &str, file: &Path) -> Result<()>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    fn acquire(&self, name: &str) -> Result<String>;
    fn renew(&self, name: &str, lease: &str) -> Result<()>;
    fn release(&self, name: &str, lease: &str) -> Result<()>;
    fn publish(
        &self,
        id: &str,
        directory: &Path,
        images: &BTreeMap<String, Image>,
    ) -> Result<BTreeMap<String, String>>;
    fn snapshot(&self) -> Result<Value>;
    fn hold_writers(&self) -> Result<()>;
    fn job(
        &self,
        name: &str,
        images: &BTreeMap<String, String>,
        record: &dyn Fn(&Value) -> Result<()>,
    ) -> Result<()>;
    fn verify_apps(&self, images: &BTreeMap<String, String>) -> Result<Value>;
    fn publish_site(&self, site: &Site, directory: &Path) -> Result<()>;
}
pub struct Azure<'a> {
    pub target: &'a Target,
    pub runner: &'a Runner,
    pub root: &'a Path,
}
impl Azure<'_> {
    fn az(&self, args: &[&str]) -> Result<Value> {
        let mut argv: Vec<String> = std::iter::once("az")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        argv.extend([
            "--subscription".into(),
            self.target.subscription.clone(),
            "--only-show-errors".into(),
            "--output".into(),
            "json".into(),
        ]);
        let output = self.runner.capture(&argv, self.root)?;
        if output.iter().all(u8::is_ascii_whitespace) {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&output).context("Azure returned invalid JSON (output withheld)")
        }
    }
    fn blob(&self, args: &[&str]) -> Result<Value> {
        let mut args = args.to_vec();
        args.extend([
            "--account-name",
            &self.target.store.account,
            "--container-name",
            &self.target.store.container,
            "--auth-mode",
            "login",
        ]);
        self.az(&args)
    }
    fn revisions(&self, app: &str) -> Result<Vec<Value>> {
        let result = self.az(&[
            "containerapp",
            "revision",
            "list",
            "--resource-group",
            &self.target.resource_group,
            "--name",
            app,
        ])?;
        Ok(result
            .as_array()
            .context("Azure returned no revision list")?
            .clone())
    }
    fn docker(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.runner.capture(
            &std::iter::once("docker")
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            self.root,
        )
    }
}
impl Provider for Azure<'_> {
    fn preflight(&self) -> Result<()> {
        let account = self.az(&["account", "show"])?;
        ensure!(
            account["id"] == self.target.subscription
                && account["tenantId"] == self.target.tenant
                && account["environmentName"] == "AzureCloud",
            "Azure subscription, tenant or cloud mismatch"
        );
        let store = self.az(&[
            "storage",
            "account",
            "show",
            "--resource-group",
            &self.target.store.resource_group,
            "--name",
            &self.target.store.account,
        ])?;
        ensure!(
            store["allowBlobPublicAccess"] == false,
            "artifact storage must disable public blob access"
        );
        let container = self.az(&[
            "storage",
            "container",
            "show",
            "--name",
            &self.target.store.container,
            "--account-name",
            &self.target.store.account,
            "--auth-mode",
            "login",
        ])?;
        ensure!(
            container["properties"]["publicAccess"].is_null()
                || container["properties"]["publicAccess"] == "None",
            "artifact container is public"
        );
        let registry = self.az(&[
            "acr",
            "show",
            "--name",
            &self.target.registry,
            "--resource-group",
            &self.target.resource_group,
        ])?;
        ensure!(
            registry["loginServer"] == format!("{}.azurecr.io", self.target.registry),
            "unexpected registry server"
        );
        for lock in &self.target.store.locks {
            self.blob(&["storage", "blob", "show", "--name", lock])?;
        }
        for site in self.target.docs.values() {
            let actual = self.az(&[
                "staticwebapp",
                "show",
                "--resource-group",
                &site.resource_group,
                "--name",
                &site.name,
            ])?;
            let expected = format!(
                "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Web/staticSites/{}",
                self.target.subscription, site.resource_group, site.name
            );
            ensure!(
                actual["id"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&expected)),
                "documentation site identity mismatch"
            );
        }
        Ok(())
    }
    fn put(&self, name: &str, file: &Path) -> Result<()> {
        self.blob(&[
            "storage",
            "blob",
            "upload",
            "--name",
            name,
            "--file",
            file.to_str().context("invalid file path")?,
            "--overwrite",
            "false",
            "--if-none-match",
            "*",
        ])?;
        Ok(())
    }
    fn get(&self, name: &str, file: &Path) -> Result<()> {
        fs::create_dir_all(file.parent().unwrap())?;
        self.blob(&[
            "storage",
            "blob",
            "download",
            "--name",
            name,
            "--file",
            file.to_str().context("invalid file path")?,
            "--overwrite",
            "false",
        ])?;
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let rows = self.blob(&[
            "storage",
            "blob",
            "list",
            "--prefix",
            prefix,
            "--num-results",
            "*",
        ])?;
        rows.as_array()
            .context("invalid blob list")?
            .iter()
            .map(|v| Ok(v["name"].as_str().context("missing blob name")?.to_owned()))
            .collect()
    }
    fn acquire(&self, name: &str) -> Result<String> {
        let lease = self.blob(&[
            "storage",
            "blob",
            "lease",
            "acquire",
            "--blob-name",
            name,
            "--lease-duration",
            "-1",
        ])?;
        Ok(lease
            .as_str()
            .or_else(|| lease["leaseId"].as_str())
            .context("Azure did not return a lease ID")?
            .to_owned())
    }
    fn renew(&self, name: &str, lease: &str) -> Result<()> {
        self.blob(&[
            "storage",
            "blob",
            "lease",
            "renew",
            "--blob-name",
            name,
            "--lease-id",
            lease,
        ])?;
        Ok(())
    }
    fn release(&self, name: &str, lease: &str) -> Result<()> {
        self.blob(&[
            "storage",
            "blob",
            "lease",
            "release",
            "--blob-name",
            name,
            "--lease-id",
            lease,
        ])?;
        Ok(())
    }
    fn publish(
        &self,
        id: &str,
        directory: &Path,
        images: &BTreeMap<String, Image>,
    ) -> Result<BTreeMap<String, String>> {
        self.runner.capture(
            &[
                "az".into(),
                "acr".into(),
                "login".into(),
                "--name".into(),
                self.target.registry.clone(),
                "--subscription".into(),
                self.target.subscription.clone(),
                "--only-show-errors".into(),
            ],
            self.root,
        )?;
        let repositories =
            self.az(&["acr", "repository", "list", "--name", &self.target.registry])?;
        let repositories = repositories
            .as_array()
            .context("invalid registry repository list")?;
        let mut published = BTreeMap::new();
        for (name, image) in images {
            let repo = &self.target.images[name];
            let tag = format!("deliver-{id}");
            if repositories.iter().any(|v| v == repo) {
                let tags = self.az(&[
                    "acr",
                    "repository",
                    "show-tags",
                    "--name",
                    &self.target.registry,
                    "--repository",
                    repo,
                ])?;
                ensure!(
                    !tags
                        .as_array()
                        .context("invalid registry tags")?
                        .iter()
                        .any(|v| v == &tag),
                    "image tag already exists; use a new release ID"
                );
            }
            eprintln!("publishing image {name}");
            self.docker(&[
                "load",
                "--input",
                directory
                    .join(&image.archive)
                    .to_str()
                    .context("invalid image path")?,
            ])?;
            let inspect: Value = serde_json::from_slice(&self.docker(&[
                "image",
                "inspect",
                image.local_ref.as_deref().unwrap_or(&image.image_id),
            ])?)?;
            ensure!(
                inspect[0]["Id"] == image.image_id || inspect[0]["Id"] == image.config_digest(),
                "loaded image differs from tested image ID"
            );
            let reference = format!("{}.azurecr.io/{repo}:{tag}", self.target.registry);
            self.docker(&[
                "tag",
                inspect[0]["Id"]
                    .as_str()
                    .context("loaded image ID missing")?,
                &reference,
            ])?;
            self.docker(&["push", &reference])?;
            let info = self.az(&[
                "acr",
                "repository",
                "show",
                "--name",
                &self.target.registry,
                "--image",
                &format!("{repo}:{tag}"),
            ])?;
            let digest = info["digest"].as_str().context("registry digest missing")?;
            ensure!(evidence::image_digest(digest), "invalid image digest");
            let pinned = format!("{}.azurecr.io/{repo}@{digest}", self.target.registry);
            let manifest: Value =
                serde_json::from_slice(&self.docker(&["manifest", "inspect", &pinned])?)?;
            ensure!(manifest["config"]["digest"] == image.config_digest(),"registry config digest differs from tested image; multi-platform indexes are not supported");
            self.az(&[
                "acr",
                "repository",
                "update",
                "--name",
                &self.target.registry,
                "--image",
                &format!("{repo}:{tag}"),
                "--write-enabled",
                "false",
                "--delete-enabled",
                "false",
            ])?;
            published.insert(name.clone(), pinned);
        }
        Ok(published)
    }
    fn snapshot(&self) -> Result<Value> {
        let mut apps = BTreeMap::new();
        for app in self.target.applications.keys() {
            // Read only selected fields: full revisions may include environment secrets.
            let rows = self.revisions(app)?;
            apps.insert(app,rows.iter().map(|row|json!({"name":row["name"],"active":row["properties"]["active"],"replicas":row["properties"]["replicas"],"images":row["properties"]["template"]["containers"].as_array().map(|cs|cs.iter().map(|c|c["image"].clone()).collect::<Vec<_>>())})).collect::<Vec<_>>());
        }
        Ok(json!({"applications":apps,"database_rollback_automatic":false}))
    }
    fn hold_writers(&self) -> Result<()> {
        // Attempt every writer even when one fails.
        let mut errors = Vec::new();
        for app in &self.target.writers {
            let result = (|| -> Result<()> {
                for row in self.revisions(app)? {
                    if row["properties"]["active"] == true {
                        self.az(&[
                            "containerapp",
                            "revision",
                            "deactivate",
                            "--resource-group",
                            &self.target.resource_group,
                            "--name",
                            app,
                            "--revision",
                            row["name"].as_str().context("missing revision name")?,
                        ])?;
                    }
                }
                for _ in 0..90 {
                    let rows = self.revisions(app)?;
                    if rows.iter().all(|r| {
                        r["properties"]["active"] == false
                            && r["properties"]["replicas"].as_u64() == Some(0)
                    }) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                anyhow::bail!("writer replicas have not stopped");
            })();
            if result.is_err() {
                errors.push(app.clone());
            }
        }
        ensure!(
            errors.is_empty(),
            "cannot confirm stopped writers: {}",
            errors.join(", ")
        );
        Ok(())
    }
    fn job(
        &self,
        name: &str,
        images: &BTreeMap<String, String>,
        record: &dyn Fn(&Value) -> Result<()>,
    ) -> Result<()> {
        let definition = self.az(&[
            "containerapp",
            "job",
            "show",
            "--resource-group",
            &self.target.resource_group,
            "--name",
            name,
        ])?;
        let containers = definition["properties"]["template"]["containers"]
            .as_array()
            .context("job containers missing")?;
        ensure!(
            definition["properties"]["configuration"]["triggerType"] == "Manual"
                && containers.len() == 1
                && images.values().any(|v| containers[0]["image"] == *v),
            "job must be manual and use an accepted image digest"
        );
        let started = self.az(&[
            "containerapp",
            "job",
            "start",
            "--resource-group",
            &self.target.resource_group,
            "--name",
            name,
        ])?;
        let execution = started["name"]
            .as_str()
            .context("job execution name missing")?;
        record(&json!({"job":name,"execution":execution,"status":"started"}))?;
        for _ in 0..180 {
            let rows = self.az(&[
                "containerapp",
                "job",
                "execution",
                "list",
                "--resource-group",
                &self.target.resource_group,
                "--name",
                name,
            ])?;
            let current = rows
                .as_array()
                .context("invalid job executions")?
                .iter()
                .find(|r| r["name"] == execution);
            match current.and_then(|r| r["properties"]["status"].as_str()) {
                Some("Succeeded") => {
                    record(&json!({"job":name,"execution":execution,"status":"succeeded"}))?;
                    return Ok(());
                }
                Some("Failed" | "Stopped" | "Canceled") => anyhow::bail!("Azure job {name} failed"),
                _ => {}
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        anyhow::bail!("Azure job {name} did not succeed before timeout; it may still be running")
    }
    fn verify_apps(&self, images: &BTreeMap<String, String>) -> Result<Value> {
        for (app, image) in &self.target.applications {
            let rows = self.revisions(app)?;
            let active: Vec<_> = rows
                .iter()
                .filter(|r| r["properties"]["active"] == true)
                .collect();
            ensure!(
                !active.is_empty(),
                "application {app} has no active revision"
            );
            for row in active {
                let containers = row["properties"]["template"]["containers"]
                    .as_array()
                    .context("missing revision containers")?;
                ensure!(containers.len()==1 && containers[0]["image"]==images[image],"application {app} runs unexpected image bytes; every active revision must use the accepted digest");
            }
        }
        self.snapshot()
    }
    fn publish_site(&self, site: &Site, directory: &Path) -> Result<()> {
        let token = self.az(&[
            "staticwebapp",
            "secrets",
            "list",
            "--resource-group",
            &site.resource_group,
            "--name",
            &site.name,
        ])?;
        let token = token["properties"]["apiKey"]
            .as_str()
            .context("missing SWA deployment token")?;
        ensure!(
            !token.is_empty() && !token.contains(['\n', '\r']),
            "invalid SWA deployment token"
        );
        let mut runner = self.runner.clone();
        runner.secrets.push(token.into());
        runner
            .env
            .insert("SWA_CLI_DEPLOYMENT_TOKEN".into(), token.into());
        // SWA receives its token only through the environment. Never store or log its output.
        let isolated = evidence::workspace()?;
        runner.capture(
            &[
                "swa".into(),
                "deploy".into(),
                directory.to_string_lossy().into_owned(),
                "--app-location".into(),
                directory.to_string_lossy().into_owned(),
                "--output-location".into(),
                ".".into(),
                "--swa-config-location".into(),
                directory.to_string_lossy().into_owned(),
                "--env".into(),
                "production".into(),
                "--no-use-keychain".into(),
            ],
            &isolated,
        )?;
        fs::remove_dir_all(isolated)?;
        Ok(())
    }
}
/// Policy bytes come from the pinned checkout. Application content/access checks remain required.
pub fn site_policy(site: &Site, root: &Path, artifacts: &Path) -> Result<()> {
    // Validate the portable config path before joining native path separators.
    let directory = super::config::contained(artifacts, &site.artifact)?;
    let artifact = super::config::contained(&directory, Path::new("staticwebapp.config.json"))?;
    ensure!(
        fs::read(&artifact)? == fs::read(super::config::contained(root, &site.policy)?)?,
        "documentation access policy changed"
    );
    let policy: Value = evidence::read(&artifact)?;
    if site.access == Access::Internal {
        let routes = policy["routes"]
            .as_array()
            .context("internal docs require access routes")?;
        let catchall = routes
            .iter()
            .find(|r| r["route"] == "/*")
            .context("internal docs require a restricted /* route")?;
        let roles = catchall["allowedRoles"]
            .as_array()
            .context("internal docs require allowedRoles")?;
        ensure!(
            !roles.is_empty()
                && roles.iter().all(|r| r
                    .as_str()
                    .is_some_and(|s| s != "anonymous" && s != "authenticated" && !s.is_empty())),
            "internal docs must require a reader role"
        );
    }
    Ok(())
}
