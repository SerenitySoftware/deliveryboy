//! Local producers, retained evidence and a fail-closed Azure release coordinator.
mod azure;
mod config;
mod evidence;
mod runner;

use crate::interrupt;
use anyhow::{ensure, Context, Result};
use azure::{Azure, Provider};
use clap::Subcommand;
use config::{Config, Target};
use evidence::{CheckReceipt, Manifest};
use runner::Runner;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Print the contract; do not run project commands or contact Azure
    Plan,
    /// Check local prerequisites; --online adds read-only Azure checks
    Preflight {
        #[arg(long)]
        online: bool,
    },
    /// Run the build/check entry point once and retain its exact output
    Prepare {
        #[arg(long)]
        id: String,
        #[arg(long)]
        version: Option<String>,
        /// Optional HTTPS ticket links recorded in the private release notes
        #[arg(long)]
        ticket: Vec<String>,
    },
    /// Run one required check on this host against the retained release
    Check {
        #[arg(long)]
        id: String,
        #[arg(long)]
        receipt_sha256: String,
        #[arg(long)]
        name: String,
    },
    /// Deploy only retained bytes after every required check has passed
    Deploy {
        #[arg(long)]
        id: String,
        #[arg(long)]
        receipt_sha256: String,
    },
    /// Read immutable prepared, failed and accepted release events
    History,
}
struct Session<'a> {
    cfg: &'a Config,
    target: &'a Target,
    target_name: &'a str,
    root: &'a Path,
    config_hash: &'a str,
    work: PathBuf,
    runner: Runner,
}
impl Session<'_> {
    fn azure(&self) -> Azure<'_> {
        Azure {
            target: self.target,
            runner: &self.runner,
            root: self.root,
        }
    }
    fn prefix(&self, id: &str) -> String {
        format!("releases/{}/{}/{id}", self.cfg.app, self.target_name)
    }
    fn save(
        &self,
        provider: &dyn Provider,
        name: &str,
        value: &impl serde::Serialize,
    ) -> Result<()> {
        let path = self.work.join(format!("{}.json", evidence::nonce()));
        evidence::write(&path, value)?;
        provider.put(name, &path)
    }
    fn event(
        &self,
        provider: &dyn Provider,
        id: &str,
        status: &str,
        phase: &str,
        details: Value,
    ) -> Result<()> {
        self.save(provider,&format!("{}/events/{}-{}.json",self.prefix(id),evidence::now(),evidence::nonce()),&json!({"format":1,"at":evidence::now(),"app":self.cfg.app,"target":self.target_name,"id":id,"status":status,"phase":phase,"details":details}))
    }
    fn source(&self) -> Result<String> {
        let top = git(self.root, &["rev-parse", "--show-toplevel"])?;
        ensure!(
            Path::new(top.trim()).canonicalize()? == self.root,
            "checkout must be the Git root"
        );
        ensure!(
            git(
                self.root,
                &["status", "--porcelain", "--untracked-files=all"]
            )?
            .is_empty(),
            "release requires a clean tracked and untracked tree"
        );
        ensure!(
            git(self.root, &["ls-files", "--stage"])?
                .lines()
                .all(|l| !l.starts_with("160000 ")),
            "release source with submodules is not supported in v1"
        );
        Ok(git(self.root, &["rev-parse", "HEAD"])?.trim().into())
    }
    fn assert_source(&self, commit: &str) -> Result<()> {
        ensure!(
            self.source()? == commit,
            "source changed or mismatches retained release"
        );
        Ok(())
    }
    fn version(&self, version: Option<&str>, commit: &str) -> Result<()> {
        if self.target.production {
            let version = version.context("production requires --version vMAJOR.MINOR.PATCH")?;
            config::stable_version(version)?;
            ensure!(
                git(
                    self.root,
                    &["rev-parse", &format!("refs/tags/{version}^{{commit}}")]
                )?
                .trim()
                    == commit,
                "production tag does not point to release source"
            );
            let tag_ref = format!("refs/tags/{version}");
            let peeled = format!("{tag_ref}^{{}}");
            let branch_ref = format!("refs/heads/{}", self.cfg.source.branch);
            let remote = git(
                self.root,
                &[
                    "ls-remote",
                    &self.cfg.source.remote,
                    &tag_ref,
                    &peeled,
                    &branch_ref,
                ],
            )?;
            let refs: BTreeMap<_, _> = remote
                .lines()
                .filter_map(|line| line.split_once('\t').map(|(sha, name)| (name, sha)))
                .collect();
            ensure!(
                refs.get(peeled.as_str())
                    .or_else(|| refs.get(tag_ref.as_str()))
                    .copied()
                    == Some(commit),
                "remote production tag is missing or moved"
            );
            let main = refs
                .get(branch_ref.as_str())
                .context("remote source branch missing")?;
            git(self.root,&["merge-base","--is-ancestor",commit,main]).context("production source must be on the remote source branch; fetch that branch before retrying")?;
        } else {
            ensure!(
                version.is_none(),
                "Dev releases use an ID, not a production version"
            );
        }
        Ok(())
    }
    fn runner_for(&self, artifacts: &Path, context: &Path) -> Runner {
        let mut runner = self.runner.clone();
        runner.env.extend([
            (
                "DELIVER_ARTIFACTS".into(),
                artifacts.to_string_lossy().into_owned(),
            ),
            (
                "DELIVER_CONTEXT".into(),
                context.to_string_lossy().into_owned(),
            ),
            (
                "DELIVER_EVIDENCE".into(),
                self.work.join("evidence").to_string_lossy().into_owned(),
            ),
            ("DELIVER_TARGET".into(), self.target_name.into()),
            (
                "DELIVER_SUBSCRIPTION".into(),
                self.target.subscription.clone(),
            ),
            (
                "DELIVER_RESOURCE_GROUP".into(),
                self.target.resource_group.clone(),
            ),
        ]);
        if self.work.join("leases.json").exists() {
            runner.env.insert(
                "DELIVER_LOCKS".into(),
                self.work.join("leases.json").to_string_lossy().into_owned(),
            );
        }
        runner
    }
    fn fetch(
        &self,
        provider: &dyn Provider,
        id: &str,
        digest: &str,
    ) -> Result<(Manifest, PathBuf)> {
        ensure!(evidence::sha256(digest), "receipt SHA-256 is required");
        let path = self.work.join("manifest.json");
        provider.get(&format!("{}/manifest.json", self.prefix(id)), &path)?;
        ensure!(
            evidence::hash_file(&path)? == digest,
            "receipt hash differs from the independently retained prepare result"
        );
        let manifest: Manifest = evidence::read(&path)?;
        ensure!(
            manifest.format == 1
                && manifest.app == self.cfg.app
                && manifest.target == self.target_name
                && manifest.id == id
                && manifest.clean
                && manifest.config_sha256 == self.config_hash,
            "release source, target or configuration identity mismatch"
        );
        ensure!(
            manifest.required_checks == self.cfg.checks.keys().cloned().collect::<Vec<_>>()
                && manifest.build.len() == self.cfg.build.len(),
            "required checks changed or are missing"
        );
        for (actual, expected) in manifest.build.iter().zip(&self.cfg.build) {
            ensure!(
                actual.status == "passed"
                    && actual.exit_code == Some(0)
                    && serde_json::to_value(&actual.command)? == serde_json::to_value(expected)?,
                "build did not pass the declared commands"
            );
        }
        self.assert_source(&manifest.commit)?;
        self.version(manifest.version.as_deref(), &manifest.commit)?;
        let directory = self.work.join("artifacts");
        fs::create_dir(&directory)?;
        for (name, hash) in &manifest.files {
            config::relative(Path::new(name))?;
            ensure!(evidence::sha256(hash), "invalid artifact hash");
            provider.get(
                &format!("{}/files/{name}", self.prefix(id)),
                &directory.join(name),
            )?;
        }
        evidence::verify(&directory, &manifest.files)?;
        validate_images(self.target, &directory, &manifest.files, &manifest.images)?;
        Ok((manifest, directory))
    }
    fn checks(
        &self,
        provider: &dyn Provider,
        id: &str,
        digest: &str,
        manifest: &Manifest,
    ) -> Result<()> {
        for (name, spec) in &self.cfg.checks {
            let file = self.work.join(format!("check-{name}.json"));
            provider
                .get(&format!("{}/checks/{name}.json", self.prefix(id)), &file)
                .with_context(|| format!("required check {name} is missing"))?;
            let receipt: CheckReceipt = evidence::read(&file)?;
            ensure!(
                receipt.manifest_sha256 == digest
                    && receipt.commit == manifest.commit
                    && receipt.config_sha256 == self.config_hash
                    && receipt.target == self.target_name
                    && receipt.check.name == *name
                    && receipt.check.status == "passed"
                    && receipt.check.exit_code == Some(0),
                "required check {name} failed or belongs to a different release"
            );
            ensure!(
                serde_json::to_value(&receipt.check.command)? == serde_json::to_value(spec)?
                    && (spec.platforms.is_empty()
                        || spec.platforms.contains(&receipt.check.platform)),
                "check {name} used the wrong command or platform"
            );
        }
        Ok(())
    }
    fn changelog(&self, provider: &dyn Provider, commit: &str) -> Result<String> {
        let prefix = format!("accepted/{}/{}/", self.cfg.app, self.target_name);
        let mut records = Vec::new();
        for name in provider.list(&prefix)? {
            let path = self
                .work
                .join(format!("baseline-{}.json", evidence::nonce()));
            provider.get(&name, &path)?;
            let record: Value = evidence::read(&path)?;
            records.push(record);
        }
        records.sort_by_key(|r| r["at"].as_u64().unwrap_or(0));
        if self.target.production {
            records.sort_by_key(|r| {
                r["version"]
                    .as_str()
                    .and_then(|v| config::stable_version(v).ok())
            });
        }
        let range = if let Some(last) = records.last() {
            let previous = last["commit"]
                .as_str()
                .context("accepted baseline lacks a commit")?;
            ensure!(
                previous.len() == 40 && previous.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid accepted source commit"
            );
            git(
                self.root,
                &["merge-base", "--is-ancestor", previous, commit],
            )
            .context("accepted baseline is not an ancestor; recovery needs a separate review")?;
            format!("{previous}..{commit}")
        } else {
            commit.to_string()
        };
        git(self.root, &["log", "--format=%h %s", &range])
    }
    fn context(
        &self,
        manifest: &Manifest,
        digest: &str,
        images: &BTreeMap<String, String>,
        held: bool,
    ) -> Result<PathBuf> {
        let path = self.work.join("context.json");
        evidence::write(
            &path,
            &json!({"format":1,"producer":"deliveryboy-local","manifest_sha256":digest,"release":manifest,"subscription":self.target.subscription,"resource_group":self.target.resource_group,"images":images,"locks_held":held,"lock_blobs":self.target.store.locks,"database_rollback_automatic":false}),
        )?;
        Ok(path)
    }
    fn retain_evidence(&self, provider: &dyn Provider, id: &str, phase: &str) -> Result<()> {
        let directory = self.work.join("evidence");
        if fs::read_dir(&directory)?.next().is_some() {
            let files = evidence::inventory(&directory)?;
            scan_secrets(&directory, &files, &self.runner.secrets)?;
            let prefix = format!("{}/evidence/{phase}-{}", self.prefix(id), evidence::nonce());
            for name in files.keys() {
                provider.put(&format!("{prefix}/{name}"), &directory.join(name))?;
            }
            self.save(provider, &format!("{prefix}/hashes.json"), &files)?;
        }
        Ok(())
    }
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut argv = vec!["git".into()];
    argv.extend(args.iter().map(|s| s.to_string()));
    Ok(String::from_utf8(Runner::default().capture(&argv, root)?)?)
}
fn validate_images(
    target: &Target,
    directory: &Path,
    files: &BTreeMap<String, String>,
    images: &BTreeMap<String, evidence::Image>,
) -> Result<()> {
    ensure!(
        images.keys().eq(target.images.keys()),
        "image inventory must include every configured image"
    );
    let saved: BTreeMap<String, evidence::Image> = evidence::read(&directory.join("images.json"))?;
    ensure!(
        serde_json::to_value(&saved)? == serde_json::to_value(images)?,
        "image inventory changed"
    );
    for image in images.values() {
        config::relative(Path::new(&image.archive))?;
        if let Some(reference) = &image.local_ref {
            ensure!(
                !reference.is_empty()
                    && !reference.starts_with('-')
                    && reference
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/.:_-".contains(&b)),
                "invalid saved image reference"
            );
        }
        ensure!(
            files.contains_key(&image.archive)
                && evidence::image_digest(&image.image_id)
                && evidence::image_digest(image.config_digest()),
            "image archive or SHA-256 config digest missing"
        );
    }
    Ok(())
}
/// Check known secret bytes across block boundaries before writing private artifacts.
fn scan_secrets(root: &Path, files: &BTreeMap<String, String>, secrets: &[String]) -> Result<()> {
    use std::io::Read;
    if secrets.is_empty() {
        return Ok(());
    }
    let overlap = secrets.iter().map(String::len).max().unwrap();
    for name in files.keys() {
        let mut file = fs::File::open(root.join(name))?;
        let mut tail = Vec::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buffer[..n]);
            ensure!(
                !secrets
                    .iter()
                    .any(|s| tail.windows(s.len()).any(|w| w == s.as_bytes())),
                "artifact contains a declared secret value; upload refused"
            );
            if tail.len() > overlap {
                tail.drain(..tail.len() - overlap);
            }
        }
    }
    Ok(())
}
fn prepare(
    session: &Session<'_>,
    provider: &dyn Provider,
    id: &str,
    version: Option<&str>,
    tickets: &[String],
) -> Result<()> {
    let commit = session.source()?;
    session.version(version, &commit)?;
    for command in &session.cfg.build {
        config::check_platform(command)?;
    }
    for ticket in tickets {
        ensure!(
            ticket.starts_with("https://") && !ticket.contains(['\n', '\r', '@']),
            "tickets must be HTTPS links without credentials"
        );
    }
    provider.preflight()?;
    // Conditional creation prevents two producers or a moved version from reusing an identity.
    let binding =
        json!({"id":id,"commit":commit,"config_sha256":session.config_hash,"version":version});
    session.save(
        provider,
        &format!("{}/reservation.json", session.prefix(id)),
        &binding,
    )?;
    let operation = (|| -> Result<()> {
        if let Some(version) = version {
            session.save(
                provider,
                &format!(
                    "versions/{}/{}/{version}.json",
                    session.cfg.app, session.target_name
                ),
                &binding,
            )?;
        }
        session.event(provider, id, "preparing", "build", binding)?;
        let mut build = Vec::new();
        let mut runner = session.runner.clone();
        runner
            .env
            .insert("DELIVER_TARGET".into(), session.target_name.into());
        runner.env.insert("DELIVER_RELEASE_ID".into(), id.into());
        runner
            .env
            .insert("DELIVER_SOURCE_COMMIT".into(), commit.clone());
        for (index, spec) in session.cfg.build.iter().enumerate() {
            let check = runner.run(&format!("build-{index}"), spec, session.root)?;
            session.event(
                provider,
                id,
                &check.status,
                "build",
                serde_json::to_value(&check)?,
            )?;
            ensure!(
                check.status == "passed",
                "build failed with exit {:?}",
                check.exit_code
            );
            build.push(check);
        }
        session.assert_source(&commit)?;
        let source = config::contained(session.root, &session.cfg.artifacts)?;
        let files = evidence::inventory(&source)?;
        let directory = session.work.join("artifacts");
        evidence::copy(&source, &directory, &files)?;
        scan_secrets(&directory, &files, &session.runner.secrets)?;
        let images = evidence::read(&directory.join("images.json"))?;
        validate_images(session.target, &directory, &files, &images)?;
        for site in session.target.docs.values() {
            azure::site_policy(site, session.root, &directory)?;
        }
        let manifest = Manifest {
            format: 1,
            app: session.cfg.app.clone(),
            target: session.target_name.into(),
            id: id.into(),
            version: version.map(str::to_owned),
            commit: commit.clone(),
            clean: true,
            config_sha256: session.config_hash.into(),
            cli_version: env!("CARGO_PKG_VERSION").into(),
            built_at: evidence::now(),
            build,
            required_checks: session.cfg.checks.keys().cloned().collect(),
            files,
            images,
            changelog: session.changelog(provider, &commit)?,
            tickets: tickets.to_vec(),
        };
        let file = session.work.join("manifest.json");
        evidence::write(&file, &manifest)?;
        // Scan command records and commit subjects as well as payloads.
        scan_secrets(
            &session.work,
            &BTreeMap::from([("manifest.json".into(), evidence::hash_file(&file)?)]),
            &session.runner.secrets,
        )?;
        for name in manifest.files.keys() {
            provider.put(
                &format!("{}/files/{name}", session.prefix(id)),
                &directory.join(name),
            )?;
        }
        evidence::verify(&directory, &manifest.files)?;
        provider.put(&format!("{}/manifest.json", session.prefix(id)), &file)?;
        let digest = evidence::hash_file(&file)?;
        session.event(provider,id,"prepared","build",json!({"manifest_sha256":digest,"pending_checks":manifest.required_checks,"version":version,"commit":commit}))?;
        println!("prepared {id}\nreceipt_sha256: {digest}\nRequired checks remain pending; this is not an accepted release.");
        Ok(())
    })();
    if operation.is_err() {
        session.event(
            provider,
            id,
            "failed",
            "prepare",
            json!({"commit":commit,"recovery":"retain local evidence; use a new release ID"}),
        )?;
    }
    operation
}
fn check(
    session: &Session<'_>,
    provider: &dyn Provider,
    id: &str,
    digest: &str,
    name: &str,
) -> Result<()> {
    let spec = session
        .cfg
        .checks
        .get(name)
        .context("unknown required check")?;
    config::check_platform(spec)?;
    provider.preflight()?;
    let (manifest, directory) = session.fetch(provider, id, digest)?;
    let context = session.context(&manifest, digest, &BTreeMap::new(), false)?;
    let result = (|| -> Result<()> {
        let checked = session
            .runner_for(&directory, &context)
            .run(name, spec, session.root)?;
        let receipt = CheckReceipt {
            manifest_sha256: digest.into(),
            commit: manifest.commit.clone(),
            config_sha256: session.config_hash.into(),
            target: session.target_name.into(),
            check: checked,
        };
        session.event(
            provider,
            id,
            &receipt.check.status,
            "check",
            serde_json::to_value(&receipt)?,
        )?;
        ensure!(
            receipt.check.status == "passed",
            "check {name} failed with exit {:?}",
            receipt.check.exit_code
        );
        session.assert_source(&manifest.commit)?;
        evidence::verify(&directory, &manifest.files)?;
        session.retain_evidence(provider, id, name)?;
        session.save(
            provider,
            &format!("{}/checks/{name}.json", session.prefix(id)),
            &receipt,
        )?;
        println!("check {name} passed for {id}");
        Ok(())
    })();
    if result.is_err() {
        session.event(provider, id, "failed", "check", json!({"name":name}))?;
    }
    result
}
struct Locks<'a> {
    provider: &'a dyn Provider,
    leases: Vec<(String, String)>,
}
impl<'a> Locks<'a> {
    fn acquire(provider: &'a dyn Provider, names: &[String]) -> Result<Self> {
        let mut locks = Self {
            provider,
            leases: Vec::new(),
        };
        let mut names = names.to_vec();
        names.sort();
        for name in names {
            match provider.acquire(&name) {
                Ok(lease) => locks.leases.push((name, lease)),
                Err(error) => {
                    locks.release()?;
                    return Err(error.context("release lock unavailable; no takeover attempted"));
                }
            }
        }
        Ok(locks)
    }
    /// Renew between phases, and stop there if Ctrl-C was pressed.
    fn checkpoint(&self) -> Result<()> {
        ensure!(interrupt::count() == 0, "interrupted (Ctrl-C)");
        self.renew()
    }
    fn renew(&self) -> Result<()> {
        for (name, lease) in &self.leases {
            self.provider.renew(name, lease)?;
        }
        Ok(())
    }
    fn release(&self) -> Result<()> {
        let mut failed = false;
        for (name, lease) in self.leases.iter().rev() {
            if self.provider.release(name, lease).is_err() {
                failed = true;
            }
        }
        ensure!(
            !failed,
            "could not release every lease; operator recovery is required"
        );
        Ok(())
    }
}
fn deploy(session: &Session<'_>, provider: &dyn Provider, id: &str, digest: &str) -> Result<()> {
    provider.preflight()?;
    let (manifest, directory) = session.fetch(provider, id, digest)?;
    session.checks(provider, id, digest, &manifest)?;
    for command in session
        .target
        .infrastructure
        .iter()
        .chain(&session.target.rollout)
        .chain(&session.target.verify)
        .chain(&session.target.browser)
        .chain(std::iter::once(&session.target.guard))
        .chain(session.target.migrate.iter())
        .chain(
            session
                .target
                .docs
                .values()
                .flat_map(|s| [&s.validate, &s.verify]),
        )
    {
        config::check_platform(command)?;
    }
    let locks = Locks::acquire(provider, &session.target.store.locks)?;
    if let Err(error) = evidence::write(&session.work.join("leases.json"), &locks.leases) {
        locks.release()?;
        return Err(error);
    }
    // Ctrl-C is a failure like any other from here on: the child it also
    // reaches dies, and `checkpoint` stops at the next phase boundary.
    let _armed = interrupt::arm();
    // `mutated`: images may have been pushed and the release ID is spent.
    // `writers_stopping`: writers were told to stop, so a failure must keep
    // them stopped. Before that, the running release is still serving.
    let mut mutated = false;
    let mut writers_stopping = false;
    let mut database_started = false;
    let operation = (|| -> Result<()> {
        // One deployment attempt per release ID, including failed attempts.
        session.save(
            provider,
            &format!("{}/deployment.json", session.prefix(id)),
            &json!({"manifest_sha256":digest,"at":evidence::now()}),
        )?;
        if let Some(version) = &manifest.version {
            let candidate = config::stable_version(version)?;
            let prefix = format!("accepted/{}/{}/", session.cfg.app, session.target_name);
            for path in provider.list(&prefix)? {
                let file = session
                    .work
                    .join(format!("accepted-{}.json", evidence::nonce()));
                provider.get(&path, &file)?;
                let record: Value = evidence::read(&file)?;
                let old = record["version"]
                    .as_str()
                    .context("accepted production version missing")?;
                ensure!(
                    candidate > config::stable_version(old)?,
                    "production version must exceed every accepted version"
                );
            }
        }
        let context = session.context(&manifest, digest, &BTreeMap::new(), true)?;
        let mut runner = session.runner_for(&directory, &context);
        for (name, site) in &session.target.docs {
            azure::site_policy(site, session.root, &directory)?;
            run_hook(
                session,
                provider,
                id,
                &runner,
                &format!("docs-validate-{name}"),
                &site.validate,
            )?;
        }
        let snapshot = provider.snapshot()?;
        session.save(
            provider,
            &format!("{}/recovery-before.json", session.prefix(id)),
            &snapshot,
        )?;
        locks.checkpoint()?;
        session.assert_source(&manifest.commit)?;
        evidence::verify(&directory, &manifest.files)?;
        session.event(
            provider,
            id,
            "deploying",
            "publish-images",
            json!({"manifest_sha256":digest}),
        )?;
        mutated = true;
        let images = provider.publish(id, &directory, &manifest.images)?;
        session.save(
            provider,
            &format!("{}/images.json", session.prefix(id)),
            &images,
        )?;
        session.context(&manifest, digest, &images, true)?;
        runner = session.runner_for(&directory, &context);
        for (index, spec) in session.target.infrastructure.iter().enumerate() {
            locks.checkpoint()?;
            run_hook(
                session,
                provider,
                id,
                &runner,
                &format!("infrastructure-{index}"),
                spec,
            )?;
        }
        locks.checkpoint()?;
        session.event(
            provider,
            id,
            "deploying",
            "stop-writers",
            json!({"database_rollback_automatic":false}),
        )?;
        writers_stopping = true;
        provider.hold_writers()?;
        run_hook(
            session,
            provider,
            id,
            &runner,
            "writer-guard",
            &session.target.guard,
        )?;
        session.save(
            provider,
            &format!("{}/writers-stopped.json", session.prefix(id)),
            &provider.snapshot()?,
        )?;
        if session.target.migrate.is_some() || !session.target.migration_jobs.is_empty() {
            locks.checkpoint()?;
            session.event(
                provider,
                id,
                "deploying",
                "database-starting",
                json!({"automatic_rollback":false}),
            )?;
            database_started = true;
            if let Some(migrate) = &session.target.migrate {
                run_hook(session, provider, id, &runner, "migrate", migrate)?;
            }
            for job in &session.target.migration_jobs {
                locks.checkpoint()?;
                provider.job(job, &images, &|record| {
                    session.event(provider, id, "running", "azure-job", record.clone())
                })?;
                session.event(provider, id, "passed", "migration-job", json!({"name":job}))?;
            }
            session.retain_evidence(provider, id, "database")?;
        }
        for (index, spec) in session.target.rollout.iter().enumerate() {
            locks.checkpoint()?;
            run_hook(
                session,
                provider,
                id,
                &runner,
                &format!("rollout-{index}"),
                spec,
            )?;
        }
        session.save(
            provider,
            &format!("{}/revisions.json", session.prefix(id)),
            &provider.verify_apps(&images)?,
        )?;
        for job in &session.target.verification_jobs {
            locks.checkpoint()?;
            provider.job(job, &images, &|record| {
                session.event(provider, id, "running", "azure-job", record.clone())
            })?;
            session.event(
                provider,
                id,
                "passed",
                "verification-job",
                json!({"name":job}),
            )?;
        }
        for (kind, commands) in [
            ("application", &session.target.verify),
            ("browser", &session.target.browser),
        ] {
            for (index, spec) in commands.iter().enumerate() {
                locks.checkpoint()?;
                run_hook(
                    session,
                    provider,
                    id,
                    &runner,
                    &format!("{kind}-{index}"),
                    spec,
                )?;
            }
        }
        for (name, site) in &session.target.docs {
            locks.checkpoint()?;
            evidence::verify(&directory, &manifest.files)?;
            azure::site_policy(site, session.root, &directory)?;
            session.event(
                provider,
                id,
                "deploying",
                "docs-publish",
                json!({"site":name}),
            )?;
            provider.publish_site(site, &directory.join(&site.artifact))?;
            run_hook(
                session,
                provider,
                id,
                &runner,
                &format!("docs-verify-{name}"),
                &site.verify,
            )?;
        }
        locks.checkpoint()?;
        session.assert_source(&manifest.commit)?;
        evidence::verify(&directory, &manifest.files)?;
        session.retain_evidence(provider, id, "accepted")?;
        // The accepted marker is the single source of truth. Write it only after every gate.
        session.save(provider,&format!("accepted/{}/{}/{id}.json",session.cfg.app,session.target_name),&json!({"status":"accepted","at":evidence::now(),"id":id,"target":session.target_name,"commit":manifest.commit,"version":manifest.version,"manifest_sha256":digest,"images":images,"database_changed":database_started}))?;
        Ok(())
    })();
    match operation {
        Ok(()) => {
            locks.release()?;
            println!("accepted {id}: application, browser and documentation checks passed");
            Ok(())
        }
        Err(error) => {
            if mutated {
                // No old image or database undo is ever attempted. Keep locks for recovery.
                let interrupted = interrupt::count() > 0;
                let held =
                    writers_stopping && locks.renew().is_ok() && provider.hold_writers().is_ok();
                let writers = match (writers_stopping, held) {
                    (false, _) => "untouched",
                    (true, true) => "held",
                    (true, false) => "hold-failed",
                };
                let evidence_ok = session.retain_evidence(provider, id, "failure").is_ok();
                let retained=session.event(provider,id,"failed","deployment",json!({"manifest_sha256":digest,"database_started":database_started,"writers":writers,"writers_held":held,"interrupted":interrupted,"evidence_retained":evidence_ok,"locks_retained":true,"automatic_rollback":false})).is_ok();
                if writers_stopping {
                    eprintln!("Deployment failed. Writers held: {held}. Failure receipt saved: {retained}. Locks retained. Recovery files: {}. An operator must inspect jobs/schema compatibility before releasing leases or starting code.",session.work.display());
                } else {
                    eprintln!("Deployment failed before writers were stopped. writers: untouched — the running release is unchanged. Failure receipt saved: {retained}. Locks retained; this release ID is spent. Recovery files: {}. Inspect pushed images and any infrastructure hook that ran before releasing leases.",session.work.display());
                }
            } else {
                let _ = session.event(
                    provider,
                    id,
                    "failed",
                    "deployment-preflight",
                    json!({"manifest_sha256":digest}),
                );
                locks.release()?;
            }
            Err(error)
        }
    }
}
fn run_hook(
    session: &Session<'_>,
    provider: &dyn Provider,
    id: &str,
    runner: &Runner,
    name: &str,
    spec: &config::Command,
) -> Result<()> {
    let manifest: Manifest = evidence::read(&session.work.join("manifest.json"))?;
    session.assert_source(&manifest.commit)?;
    evidence::verify(&session.work.join("artifacts"), &manifest.files)?;
    ensure!(
        !runner
            .secrets
            .iter()
            .any(|secret| spec.argv.iter().any(|arg| arg.contains(secret))),
        "command contains a secret value"
    );
    session.event(provider, id, "running", name, json!({"argv":spec.argv}))?;
    let check = runner.run(name, spec, session.root)?;
    session.event(
        provider,
        id,
        &check.status,
        name,
        serde_json::to_value(&check)?,
    )?;
    // Retain safe project recovery evidence even when the command reports failure.
    session.retain_evidence(provider, id, name)?;
    ensure!(
        check.status == "passed",
        "hook {name} failed with exit {:?}",
        check.exit_code
    );
    session.assert_source(&manifest.commit)?;
    evidence::verify(&session.work.join("artifacts"), &manifest.files)?;
    Ok(())
}
fn executable(root: &Path, name: &str) -> bool {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return root.join(path).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|p| {
            p.join(name).is_file() || cfg!(windows) && p.join(format!("{name}.exe")).is_file()
        })
    })
}
pub fn run(path: Option<&Path>, target: Option<&str>, action: &Action) -> Result<i32> {
    let path = path.context("release commands require --config PATH (release_version: 1)")?;
    let target_name = target.context("release commands require explicit --target NAME")?;
    let (cfg, root, config_hash) = Config::load(path)?;
    let target = cfg.target(target_name)?;
    if matches!(action, Action::Plan) {
        let mut value = serde_json::to_string_pretty(
            &json!({"provider":"azure","app":cfg.app,"checkout":root,"target":target_name,"configuration_sha256":config_hash,"build":cfg.build,"required_checks":cfg.checks,"deployment":target,"order":["all-checks","leases","snapshot","publish-exact-images","infrastructure","stop-writers","guard","migration","rollout","application","browser","docs","accepted"],"automatic_database_rollback":false,"network_or_project_commands_executed":false}),
        )?;
        for name in &cfg.secret_env {
            if let Ok(secret) = std::env::var(name) {
                if !secret.is_empty() {
                    value = value.replace(&secret, "[redacted]");
                }
            }
        }
        println!("{value}");
        return Ok(0);
    }
    if let Action::Preflight { online } = action {
        ensure!(executable(&root, "git"), "Git is unavailable");
        ensure!(
            Path::new(git(&root, &["rev-parse", "--show-toplevel"])?.trim()).canonicalize()?
                == root,
            "checkout must be the Git root"
        );
        ensure!(
            git(&root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty(),
            "preflight requires a clean checkout"
        );
        let mut missing = Vec::new();
        for command in cfg
            .build
            .iter()
            .chain(cfg.checks.values())
            .chain(target.infrastructure.iter())
            .chain(&target.rollout)
            .chain(&target.verify)
            .chain(&target.browser)
            .chain(std::iter::once(&target.guard))
            .chain(target.migrate.iter())
            .chain(target.docs.values().flat_map(|s| [&s.validate, &s.verify]))
        {
            if !command.available() {
                eprintln!(
                    "PENDING required check on {:?}; this host is {}",
                    command.platforms,
                    std::env::consts::OS
                );
            } else if !executable(&root, &command.argv[0]) {
                missing.push(command.argv[0].clone());
            }
        }
        for name in ["az", "docker"] {
            if !executable(&root, name) {
                missing.push(name.into());
            }
        }
        if !target.docs.is_empty() && !executable(&root, "swa") {
            missing.push("swa".into());
        }
        for site in target.docs.values() {
            ensure!(
                root.join(&site.policy).is_file(),
                "documentation policy file missing"
            );
        }
        ensure!(missing.is_empty(), "missing tools: {}", missing.join(", "));
        if *online {
            Azure {
                target,
                runner: &Runner::default(),
                root: &root,
            }
            .preflight()?;
        }
        println!("preflight passed; no project commands or infrastructure writes. Required cross-platform checks still gate deployment.");
        return Ok(0);
    }
    let work = evidence::workspace()?;
    fs::create_dir(work.join("evidence"))?;
    let session = Session {
        cfg: &cfg,
        target,
        target_name,
        root: &root,
        config_hash: &config_hash,
        work,
        runner: Runner::new(&cfg.secret_env)?,
    };
    eprintln!("Private local receipts: {}", session.work.display());
    let provider = session.azure();
    let result = match action {
        Action::Prepare {
            id,
            version,
            ticket,
        } => {
            config::component(id)?;
            prepare(&session, &provider, id, version.as_deref(), ticket)
        }
        Action::Check {
            id,
            receipt_sha256,
            name,
        } => {
            config::component(id)?;
            config::component(name)?;
            check(&session, &provider, id, receipt_sha256, name)
        }
        Action::Deploy { id, receipt_sha256 } => {
            config::component(id)?;
            deploy(&session, &provider, id, receipt_sha256)
        }
        Action::History => {
            provider.preflight()?;
            for prefix in [
                format!("releases/{}/{}/", cfg.app, target_name),
                format!("accepted/{}/{}/", cfg.app, target_name),
            ] {
                for name in provider.list(&prefix)? {
                    if name.contains("/events/") || name.starts_with("accepted/") {
                        let file = session
                            .work
                            .join(format!("history-{}.json", evidence::nonce()));
                        provider.get(&name, &file)?;
                        let value: Value = evidence::read(&file)?;
                        println!("{}", serde_json::to_string(&value)?);
                    }
                }
            }
            Ok(())
        }
        _ => unreachable!(),
    };
    match result {
        Ok(()) => Ok(0),
        Err(error) => {
            eprintln!("release failed: {error:#}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests;
