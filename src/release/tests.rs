use super::*;
use std::cell::RefCell;

struct Fake {
    objects: RefCell<BTreeMap<String, Vec<u8>>>,
    calls: RefCell<Vec<String>>,
    fail: RefCell<Option<String>>,
    /// A call during which the operator presses Ctrl-C; the call still completes.
    press: RefCell<Option<String>>,
}
impl Fake {
    fn new() -> Self {
        Self {
            objects: RefCell::new(BTreeMap::new()),
            calls: RefCell::new(Vec::new()),
            fail: RefCell::new(None),
            press: RefCell::new(None),
        }
    }
    fn call(&self, name: &str) -> Result<()> {
        self.calls.borrow_mut().push(name.into());
        if self.press.borrow().as_deref() == Some(name) {
            crate::interrupt::raise();
        }
        ensure!(
            self.fail.borrow().as_deref() != Some(name),
            "injected failure: {name}"
        );
        Ok(())
    }
}
impl Provider for Fake {
    fn preflight(&self) -> Result<()> {
        self.call("preflight")
    }
    fn put(&self, name: &str, path: &Path) -> Result<()> {
        self.call(&format!("put:{name}"))?;
        ensure!(
            !self.objects.borrow().contains_key(name),
            "immutable object exists"
        );
        self.objects
            .borrow_mut()
            .insert(name.into(), fs::read(path)?);
        Ok(())
    }
    fn get(&self, name: &str, path: &Path) -> Result<()> {
        self.call(&format!("get:{name}"))?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(
            path,
            self.objects.borrow().get(name).context("object missing")?,
        )?;
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .objects
            .borrow()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
    fn acquire(&self, name: &str) -> Result<String> {
        self.call(&format!("acquire:{name}"))?;
        Ok(format!("lease-{name}"))
    }
    fn renew(&self, name: &str, _: &str) -> Result<()> {
        self.call(&format!("renew:{name}"))
    }
    fn release(&self, name: &str, _: &str) -> Result<()> {
        self.call(&format!("release:{name}"))
    }
    fn publish(
        &self,
        _: &str,
        _: &Path,
        _: &BTreeMap<String, evidence::Image>,
    ) -> Result<BTreeMap<String, String>> {
        self.call("publish")?;
        Ok(BTreeMap::from([(
            "api".into(),
            format!("test.azurecr.io/api@sha256:{}", "a".repeat(64)),
        )]))
    }
    fn snapshot(&self) -> Result<Value> {
        self.call("snapshot")?;
        Ok(json!({"old_revision":"r1"}))
    }
    fn hold_writers(&self) -> Result<()> {
        self.call("hold")
    }
    fn job(
        &self,
        name: &str,
        _: &BTreeMap<String, String>,
        _: &dyn Fn(&Value) -> Result<()>,
    ) -> Result<()> {
        self.call(&format!("job:{name}"))
    }
    fn verify_apps(&self, _: &BTreeMap<String, String>) -> Result<Value> {
        self.call("verify-apps")?;
        Ok(json!({"active":"r2"}))
    }
    fn publish_site(&self, _: &config::Site, _: &Path) -> Result<()> {
        self.call("site")
    }
}
/// `deploy` arms the process-wide interrupt count, so it takes turns with
/// every other test that arms or raises it.
fn deploy(session: &Session<'_>, provider: &dyn Provider, id: &str, digest: &str) -> Result<()> {
    let _turn = crate::interrupt::SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    super::deploy(session, provider, id, digest)
}
fn failed_event(p: &Fake) -> Value {
    p.objects
        .borrow()
        .iter()
        .filter(|(k, _)| k.contains("/events/"))
        .map(|(_, v)| serde_json::from_slice::<Value>(v).unwrap())
        .find(|e| e["status"] == "failed" && e["phase"] == "deployment")
        .expect("a failed deployment event")["details"]
        .clone()
}
fn command() -> config::Command {
    config::Command {
        argv: vec!["git".into(), "rev-parse".into(), "HEAD".into()],
        platforms: vec![],
    }
}
fn cfg() -> Config {
    Config {
        release_version: 1,
        app: "sample".into(),
        checkout: ".".into(),
        artifacts: "output".into(),
        source: config::Source {
            remote: "origin".into(),
            branch: "main".into(),
        },
        build: vec![command()],
        checks: BTreeMap::from([("linux".into(), command())]),
        secret_env: vec![],
        targets: BTreeMap::from([(
            "dev".into(),
            Target {
                provider: "azure".into(),
                production: false,
                subscription: "sub".into(),
                tenant: "tenant".into(),
                resource_group: "dev".into(),
                store: config::Store {
                    account: "store".into(),
                    resource_group: "evidence".into(),
                    container: "private".into(),
                    locks: vec!["locks/dev".into(), "locks/docs".into()],
                },
                registry: "registry".into(),
                images: BTreeMap::from([("api".into(), "sample/api".into())]),
                writers: vec!["api".into()],
                guard: command(),
                infrastructure: vec![],
                migrate: Some(command()),
                rollout: vec![command()],
                applications: BTreeMap::from([("api".into(), "api".into())]),
                migration_jobs: vec!["migrate".into()],
                verification_jobs: vec![],
                verify: vec![command()],
                browser: vec![command()],
                docs: BTreeMap::new(),
            },
        )]),
    }
}
fn repo() -> PathBuf {
    let root = evidence::workspace().unwrap();
    git(&root, &["init"]).unwrap();
    git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
    // A relative self-remote also works with Windows verbatim workspace paths.
    git(&root, &["remote", "add", "origin", "."]).unwrap();
    git(&root, &["config", "user.name", "Test"]).unwrap();
    git(&root, &["config", "user.email", "test@example.invalid"]).unwrap();
    fs::write(root.join(".gitignore"), "output/\n").unwrap();
    git(&root, &["add", "."]).unwrap();
    git(&root, &["commit", "-m", "fixture"]).unwrap();
    root
}
fn session<'a>(cfg: &'a Config, root: &'a Path) -> Session<'a> {
    let work = evidence::workspace().unwrap();
    fs::create_dir(work.join("evidence")).unwrap();
    Session {
        cfg,
        target: &cfg.targets["dev"],
        target_name: "dev",
        root,
        config_hash: "config-hash",
        work,
        runner: Runner::default(),
    }
}
fn payload(root: &Path) {
    fs::create_dir(root.join("output")).unwrap();
    fs::write(root.join("output/image.tar"), "tested bytes").unwrap();
    evidence::write(
        &root.join("output/images.json"),
        &BTreeMap::from([(
            "api",
            evidence::Image {
                archive: "image.tar".into(),
                image_id: format!("sha256:{}", "a".repeat(64)),
                local_ref: None,
                config_digest: None,
            },
        )]),
    )
    .unwrap();
}
fn prepared(cfg: &Config, root: &Path, provider: &Fake) -> String {
    payload(root);
    let s = session(cfg, root);
    prepare(&s, provider, "r1", None, &[]).unwrap();
    evidence::hash(
        provider
            .objects
            .borrow()
            .get("releases/sample/dev/r1/manifest.json")
            .unwrap(),
    )
}
fn checked(cfg: &Config, root: &Path, provider: &Fake) -> String {
    let digest = prepared(cfg, root, provider);
    check(&session(cfg, root), provider, "r1", &digest, "linux").unwrap();
    digest
}
#[test]
fn prepare_check_deploy_accepts_only_after_all_gates() {
    let cfg = cfg();
    cfg.validate().unwrap();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    deploy(&session(&cfg, &root), &p, "r1", &hash).unwrap();
    let calls = p.calls.borrow();
    let at = |s: &str| calls.iter().position(|c| c == s).unwrap();
    assert!(at("put:releases/sample/dev/r1/recovery-before.json") < at("publish"));
    assert!(at("hold") < at("job:migrate"));
    assert!(at("job:migrate") < at("verify-apps"));
    assert!(at("verify-apps") < at("put:accepted/sample/dev/r1.json"));
    assert!(at("put:accepted/sample/dev/r1.json") < at("release:locks/dev"));
    let events: Vec<Value> = p
        .objects
        .borrow()
        .iter()
        .filter(|(k, _)| k.contains("/events/"))
        .map(|(_, v)| serde_json::from_slice(v).unwrap())
        .collect();
    assert!(events
        .iter()
        .any(|e| e["phase"] == "browser-0" && e["status"] == "passed"));
}
#[test]
fn missing_check_stops_before_lock_or_deployment() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = prepared(&cfg, &root, &p);
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    assert!(!p.calls.borrow().iter().any(|c| c.starts_with("acquire:")));
}
#[test]
fn mutation_and_wrong_receipt_fail_before_lock() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    assert!(deploy(&session(&cfg, &root), &p, "r1", &"b".repeat(64)).is_err());
    p.objects.borrow_mut().insert(
        "releases/sample/dev/r1/files/image.tar".into(),
        b"changed".to_vec(),
    );
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    assert!(!p.calls.borrow().iter().any(|c| c.starts_with("acquire:")));
}
#[test]
fn migration_failure_holds_writers_retains_lock_and_never_accepts() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.fail.borrow_mut() = Some("job:migrate".into());
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    let calls = p.calls.borrow();
    assert_eq!(calls.iter().filter(|c| *c == "hold").count(), 2);
    assert!(!calls.iter().any(|c| c.starts_with("release:")));
    assert!(!calls.iter().any(|c| c == "verify-apps"));
    assert!(!p
        .objects
        .borrow()
        .contains_key("accepted/sample/dev/r1.json"));
}
#[test]
fn publish_failure_leaves_writers_untouched_and_keeps_leases() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.fail.borrow_mut() = Some("publish".into());
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    let calls = p.calls.borrow().clone();
    assert!(!calls.iter().any(|c| c == "hold"), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("release:")));
    let event = failed_event(&p);
    assert_eq!(event["writers"], "untouched");
    assert_eq!(event["writers_held"], false);
    assert_eq!(event["locks_retained"], true);
}
#[test]
fn infrastructure_failure_leaves_writers_untouched() {
    let mut cfg = cfg();
    cfg.targets.get_mut("dev").unwrap().infrastructure = vec![config::Command {
        argv: vec!["git".into(), "invalid-command".into()],
        platforms: vec![],
    }];
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    let calls = p.calls.borrow().clone();
    assert!(calls.iter().any(|c| c == "publish"));
    assert!(!calls.iter().any(|c| c == "hold"), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("release:")));
    assert_eq!(failed_event(&p)["writers"], "untouched");
}
#[test]
fn ctrl_c_before_writers_stop_fails_without_holding_them() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.press.borrow_mut() = Some("publish".into());
    let error = deploy(&session(&cfg, &root), &p, "r1", &hash).unwrap_err();
    assert!(error.to_string().contains("interrupted"), "{error:#}");
    let calls = p.calls.borrow().clone();
    assert!(!calls.iter().any(|c| c == "hold"), "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("job:")));
    let event = failed_event(&p);
    assert_eq!(event["writers"], "untouched");
    assert_eq!(event["interrupted"], true);
}
#[test]
fn ctrl_c_after_writers_stop_keeps_them_held() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.press.borrow_mut() = Some("hold".into());
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    let calls = p.calls.borrow().clone();
    assert_eq!(calls.iter().filter(|c| *c == "hold").count(), 2);
    assert!(!calls.iter().any(|c| c == "verify-apps"));
    assert!(!calls.iter().any(|c| c.starts_with("release:")));
    let event = failed_event(&p);
    assert_eq!(event["writers"], "held");
    assert_eq!(event["interrupted"], true);
}
#[test]
fn verify_failure_does_not_restore_old_code() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.fail.borrow_mut() = Some("verify-apps".into());
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    assert_eq!(p.calls.borrow().iter().filter(|c| *c == "hold").count(), 2);
    assert!(!p
        .objects
        .borrow()
        .contains_key("accepted/sample/dev/r1.json"));
}
#[test]
fn changed_source_and_wrong_configuration_fail() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    let mut s = session(&cfg, &root);
    s.config_hash = "wrong-config";
    assert!(deploy(&s, &p, "r1", &hash).is_err());
    fs::write(root.join("new"), "change").unwrap();
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
}
#[test]
fn windows_check_is_required_and_cannot_be_relabelled() {
    let mut cfg = cfg();
    cfg.checks.get_mut("linux").unwrap().platforms = vec!["imaginary".into()];
    assert!(cfg.validate().is_err());
    cfg.checks.get_mut("linux").unwrap().platforms =
        vec![if cfg!(windows) { "linux" } else { "windows" }.into()];
    let root = repo();
    let p = Fake::new();
    let hash = prepared(&cfg, &root, &p);
    assert!(check(&session(&cfg, &root), &p, "r1", &hash, "linux").is_err());
    assert!(!p
        .objects
        .borrow()
        .contains_key("releases/sample/dev/r1/checks/linux.json"));
}
#[test]
fn lock_contention_releases_only_already_acquired_locks() {
    let p = Fake::new();
    *p.fail.borrow_mut() = Some("acquire:b".into());
    assert!(Locks::acquire(&p, &["b".into(), "a".into()]).is_err());
    assert_eq!(*p.calls.borrow(), ["acquire:a", "acquire:b", "release:a"]);
}
#[test]
fn release_ids_versions_and_file_paths_are_strict() {
    for v in ["v1.0", "1.2.3", "v01.2.3", "v1.2.3-rc1"] {
        assert!(config::stable_version(v).is_err());
    }
    assert!(config::stable_version("v1.2.3").is_ok());
    for p in ["../x", "/tmp/x", "x/../y", "x\\y"] {
        assert!(config::relative(Path::new(p)).is_err());
    }
}
#[test]
fn hashes_cover_extra_files_and_symlinks() {
    let root = evidence::workspace().unwrap();
    fs::write(root.join("file"), "ok").unwrap();
    let files = evidence::inventory(&root).unwrap();
    fs::write(root.join("extra"), "bad").unwrap();
    assert!(evidence::verify(&root, &files).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("file", root.join("link")).unwrap();
        assert!(evidence::inventory(&root).is_err());
    }
}
#[test]
fn secret_scan_detects_values_across_blocks() {
    let root = evidence::workspace().unwrap();
    let mut bytes = vec![b'x'; 65534];
    bytes.extend_from_slice(b"private-token");
    fs::write(root.join("file"), bytes).unwrap();
    let files = evidence::inventory(&root).unwrap();
    assert!(scan_secrets(&root, &files, &["private-token".into()]).is_err());
}
#[test]
fn production_reserves_versions_and_requires_existing_tag() {
    let mut cfg = cfg();
    cfg.targets.get_mut("dev").unwrap().production = true;
    let root = repo();
    payload(&root);
    let p = Fake::new();
    assert!(prepare(&session(&cfg, &root), &p, "r1", Some("v1.0.0"), &[]).is_err());
    git(&root, &["tag", "v1.0.0"]).unwrap();
    prepare(&session(&cfg, &root), &p, "r1", Some("v1.0.0"), &[]).unwrap();
    assert!(prepare(&session(&cfg, &root), &p, "r2", Some("v1.0.0"), &[]).is_err());
}
#[test]
fn failed_build_keeps_exit_status_and_no_manifest() {
    let mut cfg = cfg();
    cfg.build[0].argv = vec!["git".into(), "invalid-deliveryboy-command".into()];
    let root = repo();
    let p = Fake::new();
    assert!(prepare(&session(&cfg, &root), &p, "r1", None, &[]).is_err());
    assert!(!p
        .objects
        .borrow()
        .contains_key("releases/sample/dev/r1/manifest.json"));
    assert!(p
        .objects
        .borrow()
        .values()
        .any(|b| serde_json::from_slice::<Value>(b)
            .ok()
            .is_some_and(|v| v["details"]["exit_code"] == 1)));
}

#[test]
fn browser_failure_and_artifact_edits_cannot_accept() {
    for change_artifact in [false, true] {
        let mut cfg = cfg();
        cfg.targets.get_mut("dev").unwrap().browser[0].argv = if change_artifact {
            vec!["git".into(), "rev-parse".into(), "HEAD".into()]
        } else {
            vec!["git".into(), "invalid-deliveryboy-command".into()]
        };
        let root = repo();
        let p = Fake::new();
        let hash = checked(&cfg, &root, &p);
        if change_artifact {
            let key = "releases/sample/dev/r1/checks/linux.json";
            let mut receipt: CheckReceipt =
                serde_json::from_slice(&p.objects.borrow()[key]).unwrap();
            receipt.check.platform = "wrong".into();
            receipt.check.status = "failed".into();
            p.objects
                .borrow_mut()
                .insert(key.into(), serde_json::to_vec(&receipt).unwrap());
        }
        assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
        assert!(!p
            .objects
            .borrow()
            .contains_key("accepted/sample/dev/r1.json"));
    }
}
#[test]
fn no_acceptance_when_durable_acceptance_write_fails() {
    let cfg = cfg();
    let root = repo();
    let p = Fake::new();
    let hash = checked(&cfg, &root, &p);
    *p.fail.borrow_mut() = Some("put:accepted/sample/dev/r1.json".into());
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    assert!(!p.calls.borrow().iter().any(|c| c.starts_with("release:")));
    assert_eq!(p.calls.borrow().iter().filter(|c| *c == "hold").count(), 2);
}
#[test]
fn internal_docs_require_exact_reader_policy() {
    let root = evidence::workspace().unwrap();
    fs::create_dir_all(root.join("docs/internal")).unwrap();
    let site = config::Site {
        name: "internal".into(),
        resource_group: "docs".into(),
        artifact: "docs/internal".into(),
        policy: "policy.json".into(),
        access: config::Access::Internal,
        validate: command(),
        verify: command(),
    };
    let policy = json!({"routes":[{"route":"/*","allowedRoles":["internal-reader"]}]});
    evidence::write(&root.join("policy.json"), &policy).unwrap();
    evidence::write(
        &root.join("docs/internal/staticwebapp.config.json"),
        &policy,
    )
    .unwrap();
    azure::site_policy(&site, &root, &root).unwrap();
    let public = json!({"routes":[{"route":"/*","allowedRoles":["anonymous"]}]});
    evidence::write(
        &root.join("docs/internal/staticwebapp.config.json"),
        &public,
    )
    .unwrap();
    assert!(azure::site_policy(&site, &root, &root).is_err());
    evidence::write(&root.join("policy.json"), &public).unwrap();
    assert!(azure::site_policy(&site, &root, &root).is_err());
}
#[test]
fn documentation_failure_retains_release_as_failed() {
    let mut cfg = cfg();
    let root = repo();
    let policy = json!({"routes":[{"route":"/*","allowedRoles":["internal-reader"]}]});
    evidence::write(&root.join("policy.json"), &policy).unwrap();
    git(&root, &["add", "policy.json"]).unwrap();
    git(&root, &["commit", "-m", "policy"]).unwrap();
    cfg.targets.get_mut("dev").unwrap().docs.insert(
        "internal".into(),
        config::Site {
            name: "internal".into(),
            resource_group: "docs".into(),
            artifact: "docs".into(),
            policy: "policy.json".into(),
            access: config::Access::Internal,
            validate: command(),
            verify: config::Command {
                argv: vec!["git".into(), "invalid-command".into()],
                platforms: vec![],
            },
        },
    );
    payload(&root);
    fs::create_dir(root.join("output/docs")).unwrap();
    evidence::write(&root.join("output/docs/staticwebapp.config.json"), &policy).unwrap();
    let p = Fake::new();
    prepare(&session(&cfg, &root), &p, "r1", None, &[]).unwrap();
    let hash = evidence::hash(&p.objects.borrow()["releases/sample/dev/r1/manifest.json"]);
    check(&session(&cfg, &root), &p, "r1", &hash, "linux").unwrap();
    assert!(deploy(&session(&cfg, &root), &p, "r1", &hash).is_err());
    assert!(p.calls.borrow().iter().any(|c| c == "site"));
    assert!(!p
        .objects
        .borrow()
        .contains_key("accepted/sample/dev/r1.json"));
}
#[cfg(unix)]
fn fake_tools(root: &Path, scenario: &str) -> Runner {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for tool in ["az", "docker", "swa"] {
        let path = bin.join(tool);
        fs::write(
            &path,
            include_str!("../../tests/fixtures/azure_release_tool.py"),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut runner = Runner::default();
    runner.env.insert(
        "PATH".into(),
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    runner.env.insert(
        "TOOL_LOG".into(),
        root.join("calls.jsonl").to_string_lossy().into_owned(),
    );
    runner.env.insert("FAKE_SCENARIO".into(), scenario.into());
    runner
}
#[test]
#[cfg(unix)]
fn azure_adapter_protocol_is_offline_and_secrets_never_enter_arguments() {
    let cfg = cfg();
    let root = evidence::workspace().unwrap();
    let runner = fake_tools(&root, "");
    let provider = Azure {
        target: &cfg.targets["dev"],
        runner: &runner,
        root: &root,
    };
    provider.preflight().unwrap();
    let file = root.join("file");
    fs::write(&file, "evidence").unwrap();
    provider.put("test", &file).unwrap();
    provider.get("test", &root.join("download")).unwrap();
    provider.list("releases/").unwrap();
    let lease = provider.acquire("locks/dev").unwrap();
    provider.renew("locks/dev", &lease).unwrap();
    provider.release("locks/dev", &lease).unwrap();
    let images = BTreeMap::from([(
        "api".into(),
        evidence::Image {
            archive: "image.tar".into(),
            image_id: format!("sha256:{}", "a".repeat(64)),
            local_ref: None,
            config_digest: None,
        },
    )]);
    let published = provider.publish("r1", &root, &images).unwrap();
    let records = RefCell::new(Vec::new());
    provider
        .job("migrate", &published, &|r| {
            records.borrow_mut().push(r.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(records.borrow()[0]["execution"], "execution-123");
    provider.hold_writers().unwrap();
    let snapshot = provider.snapshot().unwrap();
    assert!(!snapshot.to_string().contains("do-not-retain"));
    let site = config::Site {
        name: "docs".into(),
        resource_group: "docs".into(),
        artifact: "docs".into(),
        policy: "policy.json".into(),
        access: config::Access::Public,
        validate: command(),
        verify: command(),
    };
    provider.publish_site(&site, &root).unwrap();
    assert!(!fs::read_to_string(root.join("calls.jsonl"))
        .unwrap()
        .contains("private-swa-token"));
}
#[test]
#[cfg(unix)]
fn azure_adapter_rejects_wrong_identity_public_storage_and_registry_mismatch() {
    let cfg = cfg();
    for scenario in [
        "wrong-account",
        "public-storage",
        "wrong-registry-image",
        "existing-tag",
        "wrong-job-image",
        "failed-job",
    ] {
        let root = evidence::workspace().unwrap();
        let runner = fake_tools(&root, scenario);
        let provider = Azure {
            target: &cfg.targets["dev"],
            runner: &runner,
            root: &root,
        };
        let result = match scenario {
            "wrong-account" | "public-storage" => provider.preflight(),
            "wrong-job-image" | "failed-job" => provider.job(
                "migrate",
                &BTreeMap::from([(
                    "api".into(),
                    format!("registry.azurecr.io/sample/api@sha256:{}", "b".repeat(64)),
                )]),
                &|_| Ok(()),
            ),
            _ => provider
                .publish(
                    "r1",
                    &root,
                    &BTreeMap::from([(
                        "api".into(),
                        evidence::Image {
                            archive: "image.tar".into(),
                            image_id: format!("sha256:{}", "a".repeat(64)),
                            local_ref: None,
                            config_digest: None,
                        },
                    )]),
                )
                .map(|_| ()),
        };
        assert!(result.is_err(), "{scenario} should fail");
    }
}

#[test]
#[cfg(unix)]
fn containerd_tested_id_stays_distinct_from_registry_config_digest() {
    let cfg = cfg();
    let root = evidence::workspace().unwrap();
    let runner = fake_tools(&root, "containerd");
    let provider = Azure {
        target: &cfg.targets["dev"],
        runner: &runner,
        root: &root,
    };
    let images = BTreeMap::from([(
        "api".into(),
        evidence::Image {
            archive: "image.tar".into(),
            image_id: format!("sha256:{}", "c".repeat(64)),
            local_ref: Some("source-api:commit".into()),
            config_digest: Some(format!("sha256:{}", "a".repeat(64))),
        },
    )]);
    provider.publish("r1", &root, &images).unwrap();
    let log = fs::read_to_string(root.join("calls.jsonl")).unwrap();
    assert!(log.contains("source-api:commit"));
    assert!(log.contains(&"c".repeat(64)));
}

#[test]
fn windows_line_endings_preserve_configuration_identity() {
    let root = evidence::workspace().unwrap();
    let text = serde_yaml::to_string(&cfg()).unwrap();
    fs::write(root.join("unix.yml"), &text).unwrap();
    fs::write(root.join("windows.yml"), text.replace('\n', "\r\n")).unwrap();
    assert_eq!(
        Config::load(&root.join("unix.yml")).unwrap().2,
        Config::load(&root.join("windows.yml")).unwrap().2
    );
}
