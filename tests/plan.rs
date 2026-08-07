//! Compile-path tests: config loading, ordering, and deployer output.
//! No execution, so these run anywhere.

use std::process::Command;

fn deliver() -> Command {
    Command::new(env!("CARGO_BIN_EXE_deliver"))
}

const SAMPLE: &str = r#"
version: 1
app: sample
defaults: {target: production}
targets:
  production: {host: sample.md, user: root, dir: /var/universal/sample}
services:
  web:
    deployer: hugo
    before: [{command: "python3 scripts/release-notes.py --check-all"}]
    config: {source: apps/web, minify: true, remote_subdir: web, owner: www-data:www-data}
    verify: [{http: {url: "https://sample.md/", expect_status: 200}}]
  nginx:
    deployer: nginx-vhost
    needs: [web]
    config: {strategy: script, conf: nginx/sample.conf, script: "nginx/bootstrap-tls.sh activate"}
"#;

fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("cfg.yml");
    std::fs::write(&path, body).unwrap();
    path
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("deliver-test-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn validate_accepts_sample_config() {
    let dir = tmpdir("validate");
    let cfg = write_config(&dir, SAMPLE);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sample"), "{stdout}");
    assert!(stdout.contains("2 service(s)"), "{stdout}");
}

#[test]
fn plan_compiles_the_real_sample_recipe() {
    let dir = tmpdir("plan");
    let cfg = write_config(&dir, SAMPLE);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("plan")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);

    for expected in [
        "release-notes.py --check-all",
        "hugo --minify",
        "sample-web.tar.gz -C apps/web/public .",
        "install -d -m 755 -o www-data -g www-data",
        "chown -R www-data:www-data",
        "bootstrap-tls.sh activate",
        "verify http https://sample.md/",
    ] {
        assert!(
            text.contains(expected),
            "plan missing {expected:?}:\n{text}"
        );
    }
    // `needs` ordering: web before nginx.
    assert!(
        text.find("▸ web").unwrap() < text.find("▸ nginx").unwrap(),
        "{text}"
    );
}

#[test]
fn plan_json_is_machine_readable() {
    let dir = tmpdir("json");
    let cfg = write_config(&dir, SAMPLE);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .args(["plan", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(parsed.as_array().unwrap().len(), 2);
    assert_eq!(parsed[0]["service"], "web");
}

#[test]
fn unsupported_version_is_a_config_error() {
    let dir = tmpdir("version");
    let cfg = write_config(&dir, &SAMPLE.replace("version: 1", "version: 9"));
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported config version"));
}

#[test]
fn notification_channel_and_events_are_validated() {
    let dir = tmpdir("notification-validation");
    let bad_channel =
        format!("{SAMPLE}\nnotifications:\n  - channel: email\n    webhook_secret: NOTICE_URL\n");
    let cfg = write_config(&dir, &bad_channel);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("notification channel 'email'"));

    let bad_event = format!(
        "{SAMPLE}\nnotifications:\n  - channel: slack\n    webhook_secret: NOTICE_URL\n    events: [maybe]\n"
    );
    let cfg = write_config(&dir, &bad_event);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("validate")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("notification event 'maybe'"));
}

#[test]
fn unknown_deployer_is_reported() {
    let dir = tmpdir("deployer");
    let cfg = write_config(&dir, &SAMPLE.replace("deployer: hugo", "deployer: nope"));
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("plan")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown deployer"));
}

#[test]
fn dependency_cycle_is_detected() {
    let dir = tmpdir("cycle");
    let body = r#"
version: 1
app: x
defaults: {target: t}
targets: {t: {host: h, dir: /d}}
services:
  a: {deployer: hugo, needs: [b], config: {}}
  b: {deployer: hugo, needs: [a], config: {}}
"#;
    let cfg = write_config(&dir, body);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .arg("plan")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("dependency cycle"));
}

#[test]
fn service_filter_limits_the_plan() {
    let dir = tmpdir("filter");
    let cfg = write_config(&dir, SAMPLE);
    let out = deliver()
        .arg("--config")
        .arg(&cfg)
        .args(["plan", "--service", "nginx"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("▸ nginx"));
    assert!(!text.contains("▸ web"), "{text}");
}

#[test]
fn compose_remote_health_can_set_the_host_header() {
    let dir = tmpdir("compose-health-host");
    std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(dir.join("docker-compose.yml"), "services: {}\n").unwrap();
    let body = r#"
version: 1
app: example
defaults: {target: production}
targets: {production: {host: example.com, dir: /srv/example}}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      image: {tag: example/app:latest, context: ., transport: tarball}
      health:
        remote_url: http://127.0.0.1:8000/healthz
        host: example.com
"#;
    std::fs::write(dir.join(".deliver.yml"), body).unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("-H 'Host: example.com'"), "{text}");
    assert!(text.contains("|| STATUS=000"), "{text}");
    assert!(!text.contains("HTTP 400000"), "{text}");
}

#[test]
fn compose_deploy_history_is_recorded_on_the_target() {
    let dir = compose_repo("compose-history", "");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("record deploy"), "{text}");
    assert!(text.contains(".deliver/history.tsv"), "{text}");
}

#[test]
fn init_detects_a_hugo_repo() {
    let dir = tmpdir("init");
    let web = dir.join("apps/web");
    std::fs::create_dir_all(web.join("content")).unwrap();
    std::fs::write(web.join("hugo.toml"), "baseURL = 'https://x/'\n").unwrap();
    std::fs::create_dir_all(dir.join("nginx")).unwrap();
    std::fs::write(dir.join("nginx/site.conf"), "server {}\n").unwrap();

    let out = deliver()
        .args(["init", "--path"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Hugo site at apps/web"), "{text}");
    assert!(text.contains("deployer: hugo"), "{text}");
    assert!(text.contains("nginx vhost nginx/site.conf"), "{text}");
    // No install script present → managed strategy.
    assert!(text.contains("strategy: managed"), "{text}");
}

#[test]
fn deploy_dry_run_executes_nothing() {
    let dir = tmpdir("dryrun");
    let marker = dir.join("should-not-exist.txt");
    let body = format!(
        r#"
version: 1
app: t
defaults: {{target: local}}
targets: {{local: {{host: localhost, dir: /tmp/nope}}}}
services:
  a:
    deployer: files
    before: [{{command: "touch {}"}}]
    config: {{src: x.txt}}
"#,
        marker.display()
    );
    std::fs::write(dir.join("x.txt"), "payload\n").unwrap(); // preflight checks inputs exist
    let cfg = write_config(&dir, &body);
    let out = deliver()
        .current_dir(&dir)
        .arg("--config")
        .arg(&cfg)
        .args(["deploy", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!marker.exists(), "dry run must not execute steps");
}

#[test]
fn dry_run_with_version_compiles_the_release_without_tagging() {
    let dir = local_deploy_repo("dryrun-version");
    git_init_tagged(&dir, "v1.0.0");
    std::fs::write(dir.join("payload.txt"), "next\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "next"]] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .unwrap();
    }
    let out = run_in(&dir, &["deploy", "--dry-run", "--version", "1.1.0"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("release:        v1.1.0"), "{text}");
    assert!(!tag_exists(&dir, "v1.1.0"), "dry run must not tag");
}

// --- config discovery -------------------------------------------------------

fn run_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    deliver()
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn discovers_deliver_yml_in_cwd() {
    let dir = tmpdir("discover-cwd");
    std::fs::write(dir.join(".deliver.yml"), SAMPLE).unwrap();
    let out = run_in(&dir, &["validate"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(".deliver.yml"), "{text}");
}

#[test]
fn discovers_deliveryboy_config_dir() {
    let dir = tmpdir("discover-dir");
    std::fs::create_dir_all(dir.join(".deliveryboy")).unwrap();
    std::fs::write(dir.join(".deliveryboy/config.yml"), SAMPLE).unwrap();
    let out = run_in(&dir, &["validate"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(".deliveryboy/config.yml"));
}

#[test]
fn discovers_config_from_a_subdirectory() {
    let dir = tmpdir("discover-up");
    std::fs::write(dir.join(".deliver.yml"), SAMPLE).unwrap();
    let nested = dir.join("apps/web/deep");
    std::fs::create_dir_all(&nested).unwrap();
    let out = run_in(&nested, &["validate"]);
    assert!(
        out.status.success(),
        "walking up failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn plain_file_wins_over_config_dir() {
    let dir = tmpdir("discover-order");
    std::fs::write(dir.join(".deliver.yml"), SAMPLE).unwrap();
    std::fs::create_dir_all(dir.join(".deliveryboy")).unwrap();
    std::fs::write(dir.join(".deliveryboy/config.yml"), SAMPLE).unwrap();
    let out = run_in(&dir, &["validate"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(".deliver.yml") && !text.contains(".deliveryboy/config.yml"),
        "{text}"
    );
}

#[test]
fn missing_config_explains_where_it_looked() {
    let dir = tmpdir("discover-none");
    // .git stops the upward walk so we don't find a real config above tmp.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let out = run_in(&dir, &["validate"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no config file found"), "{err}");
    assert!(err.contains(".deliveryboy/config.yml"), "{err}");
    assert!(err.contains("deliver init"), "{err}");
}

#[test]
fn explicit_config_flag_overrides_discovery() {
    let dir = tmpdir("discover-explicit");
    std::fs::write(dir.join(".deliver.yml"), SAMPLE).unwrap();
    let other = dir.join("other.yml");
    std::fs::write(&other, SAMPLE.replace("app: sample", "app: elsewhere")).unwrap();
    let out = deliver()
        .current_dir(&dir)
        .arg("--config")
        .arg(&other)
        .arg("validate")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("elsewhere"));
}

// --- init write prompt ------------------------------------------------------

fn hugo_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    let web = dir.join("apps/web");
    std::fs::create_dir_all(web.join("content")).unwrap();
    std::fs::write(web.join("hugo.toml"), "baseURL = 'https://x/'\n").unwrap();
    dir
}

#[test]
fn init_without_write_and_no_input_does_not_write() {
    let dir = hugo_repo("init-eof");
    let out = deliver()
        .args(["init", "--path"])
        .arg(&dir)
        .stdin(std::process::Stdio::null()) // EOF
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Write "), "should have prompted: {text}");
    assert!(text.contains("no input"), "{text}");
    assert!(!dir.join(".deliver.yml").exists(), "must not write on EOF");
}

#[test]
fn init_prompt_accepts_yes_from_stdin() {
    use std::io::Write;
    let dir = hugo_repo("init-yes");
    let mut child = deliver()
        .args(["init", "--path"])
        .arg(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(
        dir.join(".deliver.yml").exists(),
        "expected the config to be written"
    );
    let body = std::fs::read_to_string(dir.join(".deliver.yml")).unwrap();
    assert!(body.contains("deployer: hugo"), "{body}");
}

#[test]
fn init_prompt_declines_on_n() {
    use std::io::Write;
    let dir = hugo_repo("init-no");
    let mut child = deliver()
        .args(["init", "--path"])
        .arg(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(!dir.join(".deliver.yml").exists());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Not written"));
}

#[test]
fn init_write_flag_skips_the_prompt() {
    let dir = hugo_repo("init-write");
    let out = deliver()
        .args(["init", "--write", "--path"])
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(dir.join(".deliver.yml").exists());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("[y/N]"));
}

#[test]
fn init_refuses_to_clobber_without_force() {
    let dir = hugo_repo("init-exists");
    std::fs::write(dir.join(".deliver.yml"), "# mine\n").unwrap();
    let out = deliver()
        .args(["init", "--write", "--path"])
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--force"));
    assert_eq!(
        std::fs::read_to_string(dir.join(".deliver.yml")).unwrap(),
        "# mine\n"
    );
}

// --- nginx_vhost managed strategy -------------------------------------------

/// Mirrors sample's real vhost: an HTTP redirect block, an apex HTTPS block
/// with its own cert, and a www block with a *different* cert.
const TWO_CERT_VHOST: &str = r#"
server {
    listen 80;
    server_name example.md www.example.md;
    location /.well-known/acme-challenge/ { root /var/www/le; }
    location / { return 301 https://example.md$request_uri; }
}
server {
    listen 443 ssl;
    server_name example.md;
    ssl_certificate /etc/letsencrypt/live/example.md/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/example.md/privkey.pem;
    root /var/www/example/web;
}
server {
    listen 443 ssl;
    server_name www.example.md;
    ssl_certificate /etc/letsencrypt/live/www.example.md/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/www.example.md/privkey.pem;
    return 301 https://example.md$request_uri;
}
"#;

fn nginx_repo(name: &str, vhost: &str, service_cfg: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::create_dir_all(dir.join("nginx")).unwrap();
    std::fs::write(dir.join("nginx/site.conf"), vhost).unwrap();
    let cfg = format!(
        r#"
version: 1
app: example
defaults: {{target: production}}
targets:
  production: {{host: example.md, user: root, dir: /var/universal/example}}
services:
  nginx:
    deployer: nginx-vhost
    config:
{service_cfg}
"#
    );
    std::fs::write(dir.join(".deliver.yml"), cfg).unwrap();
    dir
}

#[test]
fn managed_pairs_each_cert_with_only_its_own_domains() {
    let dir = nginx_repo(
        "nginx-two-certs",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);

    // Two separate cert steps, each scoped to its own cert name.
    assert!(
        text.contains("certbot: ensure example.md covers ["),
        "{text}"
    );
    assert!(
        text.contains("certbot: ensure www.example.md covers ["),
        "{text}"
    );
    assert!(text.contains("--cert-name example.md"), "{text}");
    assert!(text.contains("--cert-name www.example.md"), "{text}");
    // The apex cert must NOT claim the www domain (that was the bug).
    let apex = text.split("certbot: ensure www.example.md").next().unwrap();
    let apex_step = apex.split("certbot: ensure example.md").nth(1).unwrap();
    assert!(
        !apex_step.contains("-d www.example.md"),
        "apex cert grabbed www: {apex_step}"
    );
}

#[test]
fn managed_is_idempotent_and_installs_with_rollback() {
    let dir = nginx_repo(
        "nginx-managed",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // Skips issuance when the cert is already there.
    assert!(text.contains("already covers"), "idempotent skip: {text}");
    // Stages, validates, and restores the previous vhost on failure.
    assert!(
        text.contains("install vhost site.conf (validate + rollback on failure)"),
        "{text}"
    );
    assert!(text.contains("restoring previous vhost"), "{text}");
    assert!(text.contains("nginx -t"), "{text}");
}

#[test]
fn managed_needs_no_bootstrap_script() {
    let dir = nginx_repo(
        "nginx-no-script",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        !text.contains("bootstrap-tls"),
        "managed should not need a repo script: {text}"
    );
}

#[test]
fn managed_without_certbot_email_fails_loudly_at_runtime() {
    let dir = nginx_repo(
        "nginx-no-email",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // The guard is inside the step (we can't know remotely whether a cert exists).
    assert!(text.contains("certbot.email is not set"), "{text}");
}

#[test]
fn domains_override_is_rejected_when_ambiguous() {
    let dir = nginx_repo(
        "nginx-ambiguous",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      domains: [only.example.md]\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
}

#[test]
fn http_only_vhost_needs_no_cert_step() {
    let vhost = "server {\n    listen 80;\n    server_name plain.example.md;\n    root /srv;\n}\n";
    let dir = nginx_repo("nginx-plain", vhost, "      conf: nginx/site.conf\n");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(!text.contains("certbot: ensure"), "{text}");
    assert!(text.contains("install vhost site.conf"), "{text}");
}

#[test]
fn script_strategy_still_supported() {
    let dir = nginx_repo(
        "nginx-script",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      strategy: script\n      script: nginx/mine.sh activate\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("run nginx/mine.sh activate"), "{text}");
    assert!(
        !text.contains("certbot: ensure"),
        "script strategy owns certs: {text}"
    );
}

// --- multiple vhosts, ssl default, cert provider ----------------------------

fn multi_vhost_repo(name: &str, service_cfg: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::create_dir_all(dir.join("nginx")).unwrap();
    std::fs::write(dir.join("nginx/site.conf"), TWO_CERT_VHOST).unwrap();
    std::fs::write(
        dir.join("nginx/api.conf"),
        "server {\n    listen 443 ssl;\n    server_name api.example.md;\n          ssl_certificate /etc/letsencrypt/live/api.example.md/fullchain.pem;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("nginx/internal.conf"),
        "server {\n    listen 8080;\n    server_name internal.example.md;\n}\n",
    )
    .unwrap();
    let cfg = format!(
        r#"
version: 1
app: example
defaults: {{target: production}}
targets:
  production: {{host: example.md, user: root, dir: /var/universal/example}}
services:
  nginx:
    deployer: nginx-vhost
    config:
{service_cfg}
"#
    );
    std::fs::write(dir.join(".deliver.yml"), cfg).unwrap();
    dir
}

#[test]
fn multiple_vhosts_each_get_certs_and_an_install() {
    let dir = multi_vhost_repo(
        "nginx-multi",
        "      certbot:\n        email: ops@example.md\n      vhosts:\n        - conf: nginx/site.conf\n        - conf: nginx/api.conf\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);

    // Three distinct certs across the two vhosts.
    for cert in ["example.md", "www.example.md", "api.example.md"] {
        assert!(
            text.contains(&format!("certbot: ensure {cert} covers [")),
            "missing cert {cert}: {text}"
        );
    }
    // Both vhosts installed.
    assert!(text.contains("install vhost site.conf"), "{text}");
    assert!(text.contains("install vhost api.conf"), "{text}");
    // Certs are provisioned before any vhost is installed.
    let first_install = text.find("install vhost").unwrap();
    let last_cert = text.rfind("certbot: ensure").unwrap();
    assert!(
        last_cert < first_install,
        "certs must precede installs: {text}"
    );
}

#[test]
fn ssl_defaults_to_true_and_uses_certbot() {
    // No `ssl:` and no `provider:` in config — certs should still be provisioned.
    let dir = multi_vhost_repo(
        "nginx-ssl-default",
        "      certbot:\n        email: ops@example.md\n      vhosts:\n        - conf: nginx/api.conf\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("certbot: ensure api.example.md covers"),
        "{text}"
    );
    assert!(text.contains("certbot certonly --webroot"), "{text}");
}

#[test]
fn ssl_false_skips_certs_per_vhost() {
    let dir = multi_vhost_repo(
        "nginx-ssl-off",
        "      certbot:\n        email: ops@example.md\n      vhosts:\n        - conf: nginx/api.conf\n          ssl: false\n        - conf: nginx/site.conf\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        !text.contains("api.example.md/fullchain"),
        "api opted out of ssl: {text}"
    );
    assert!(
        text.contains("certbot: ensure example.md covers ["),
        "site.conf still gets certs: {text}"
    );
    assert!(
        text.contains("install vhost api.conf"),
        "still installed: {text}"
    );
}

#[test]
fn ssl_false_at_top_level_disables_all_certs() {
    let dir = multi_vhost_repo(
        "nginx-ssl-off-all",
        "      ssl: false\n      vhosts:\n        - conf: nginx/site.conf\n        - conf: nginx/api.conf\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(!text.contains("certbot: ensure"), "{text}");
    assert!(text.contains("install vhost site.conf") && text.contains("install vhost api.conf"));
}

#[test]
fn provider_none_skips_provisioning_but_still_installs() {
    let dir = multi_vhost_repo(
        "nginx-provider-none",
        "      provider: none\n      vhosts:\n        - conf: nginx/api.conf\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(!text.contains("certbot"), "{text}");
    assert!(text.contains("install vhost api.conf"), "{text}");
}

#[test]
fn unknown_provider_is_rejected() {
    let dir = multi_vhost_repo(
        "nginx-provider-bad",
        "      provider: acme-sh\n      vhosts:\n        - conf: nginx/api.conf\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown cert provider"));
}

#[test]
fn shared_cert_across_vhosts_is_provisioned_once() {
    let dir = tmpdir("nginx-shared-cert");
    std::fs::create_dir_all(dir.join("nginx")).unwrap();
    for (file, host) in [("a.conf", "a.example.md"), ("b.conf", "b.example.md")] {
        std::fs::write(
            dir.join("nginx").join(file),
            format!(
                "server {{\n    listen 443 ssl;\n    server_name {host};\n                  ssl_certificate /etc/letsencrypt/live/shared.example.md/fullchain.pem;\n}}\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: example
defaults: {target: production}
targets:
  production: {host: example.md, user: root, dir: /var/universal/example}
services:
  nginx:
    deployer: nginx-vhost
    config:
      certbot:
        email: ops@example.md
      vhosts:
        - conf: nginx/a.conf
        - conf: nginx/b.conf
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert_eq!(
        text.matches("certbot: ensure shared.example.md").count(),
        1,
        "{text}"
    );
    // Both hosts land on the one cert.
    assert!(
        text.contains("-d a.example.md") && text.contains("-d b.example.md"),
        "{text}"
    );
}

#[test]
fn conf_and_vhosts_together_is_an_error() {
    let dir = multi_vhost_repo(
        "nginx-both",
        "      conf: nginx/site.conf\n      vhosts:\n        - conf: nginx/api.conf\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not both"));
}

// --- atomic releases, rollback, preflight, commands -------------------------

fn hugo_site_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    let web = dir.join("apps/web");
    std::fs::create_dir_all(web.join("content")).unwrap();
    std::fs::write(web.join("hugo.toml"), "baseURL = 'https://x/'\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /var/universal/demo}
services:
  web:
    deployer: hugo
    config: {source: apps/web, remote_subdir: web, owner: www-data:www-data}
"#,
    )
    .unwrap();
    dir
}

#[test]
fn deploy_uses_release_dirs_and_never_wipes_the_live_path() {
    let dir = hugo_site_repo("rel-atomic");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    // Release dirs are named after the deploy version (UTC stamp + sha).
    assert!(text.contains("stage release 20"), "{text}");
    assert!(text.contains("/releases/20"), "{text}");
    // The atomic swap, not an in-place overwrite.
    assert!(text.contains("activate release"), "{text}");
    assert!(
        text.contains("mv -Tf"),
        "expected an atomic symlink rename: {text}"
    );
    // The old destructive pattern must be gone.
    assert!(
        !text.contains("-mindepth 1 -maxdepth 1 -exec rm -rf"),
        "live path is still being emptied: {text}"
    );
}

#[test]
fn release_is_sanity_checked_before_activation() {
    let dir = hugo_site_repo("rel-guard");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let check = text
        .find("is not empty")
        .expect("expected an empty-release guard");
    let activate = text
        .find("activate release")
        .expect("expected an activate step");
    assert!(
        check < activate,
        "the guard must run before activation: {text}"
    );
}

#[test]
fn activation_carries_a_rollback_and_releases_are_pruned() {
    let dir = hugo_site_repo("rel-rollback");
    let out = run_in(&dir, &["plan", "--json"]);
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let steps = plan[0]["steps"].as_array().unwrap();

    let activate = steps
        .iter()
        .find(|s| {
            s["label"]
                .as_str()
                .unwrap_or("")
                .starts_with("activate release")
        })
        .expect("no activate step");
    let undo = activate["rollback"]
        .as_str()
        .expect("activate step has no rollback");
    assert!(
        undo.contains(".deliver-previous"),
        "rollback should use the recorded previous: {undo}"
    );
    assert!(
        undo.contains("mv -Tf"),
        "rollback should swap atomically too: {undo}"
    );

    assert!(
        steps.iter().any(|s| s["label"]
            .as_str()
            .unwrap_or("")
            .starts_with("prune old releases")),
        "expected a prune step"
    );
}

#[test]
fn rollback_command_reports_when_nothing_is_reversible() {
    // A commands-only service mutates nothing reversible.
    let dir = tmpdir("rollback-none");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /var/universal/demo}
services:
  fix:
    deployer: commands
    config:
      steps:
        - ssh: echo hi
"#,
    )
    .unwrap();
    let out = run_in(&dir, &["rollback"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing in this config is rollback-able")
    );
}

#[test]
fn dry_run_announces_available_undo() {
    let dir = hugo_site_repo("rel-dryrun");
    let out = run_in(&dir, &["deploy", "--dry-run"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("undo available if a later step fails"),
        "{text}"
    );
}

#[test]
fn commands_deployer_compiles_declared_steps() {
    let dir = tmpdir("cmds");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /srv/demo}
services:
  perms:
    deployer: commands
    config:
      steps:
        - ssh: sudo install -d -m 755 /srv/demo/downloads
        - command: echo local
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("sudo install -d -m 755 /srv/demo/downloads"),
        "{text}"
    );
    assert!(text.contains("echo local"), "{text}");
}

#[test]
fn commands_deployer_expands_release_placeholders() {
    let dir = tmpdir("cmd-placeholders");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /srv/demo}
services:
  release:
    deployer: commands
    config:
      steps:
        - command: echo {version} {release} {sha} {deploy} {work}
"#,
    )
    .unwrap();
    git_init_tagged(&dir, "v1.2.3");

    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("echo 1.2.3 v1.2.3"), "{text}");
    for placeholder in ["{version}", "{release}", "{sha}", "{deploy}", "{work}"] {
        assert!(
            !text.contains(placeholder),
            "unexpanded {placeholder}: {text}"
        );
    }
}

#[test]
fn commands_deployer_requires_steps() {
    let dir = tmpdir("cmds-empty");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /srv/demo}
services:
  perms:
    deployer: commands
    config: {}
"#,
    )
    .unwrap();
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("`steps:` is required"));
}

#[test]
fn preflight_reports_missing_input_files() {
    let dir = tmpdir("preflight-missing");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: demo.example.md, user: root, dir: /srv/demo}
services:
  nginx:
    deployer: nginx-vhost
    config:
      ssl: false
      conf: nginx/missing.conf
"#,
    )
    .unwrap();
    // With ssl: false the conf is never parsed, so preflight is what catches it.
    let out = run_in(&dir, &["preflight"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("missing file referenced by config"), "{err}");
    assert!(err.contains("nginx/missing.conf"), "{err}");
}

#[test]
fn deploy_runs_preflight_before_touching_anything() {
    let dir = hugo_site_repo("preflight-order");
    // Force a preflight failure: a before-hook naming a binary that can't exist.
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg).unwrap().replace(
        "    deployer: hugo",
        "    deployer: hugo\n    before: [{command: definitely-not-a-real-binary-xyz --go}]",
    );
    std::fs::write(&cfg, body).unwrap();

    let out = run_in(&dir, &["deploy", "--dry-run"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("preflight failed"), "{err}");
    assert!(err.contains("definitely-not-a-real-binary-xyz"), "{err}");
}

// --- staged output ----------------------------------------------------------

#[test]
fn announces_version_and_phases_in_order() {
    let dir = hugo_site_repo("ui-phases");
    let out = run_in(&dir, &["deploy", "--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Phase narration goes to stderr; step content to stdout.
    let err = String::from_utf8_lossy(&out.stderr);

    assert!(
        err.starts_with(&format!("Delivery Boy CLI v{}", env!("CARGO_PKG_VERSION"))),
        "should open with the version banner: {err}"
    );
    let order = [
        "Loading configuration",
        "Compiling plan",
        "Preflight",
        "Dry run",
        "Done",
    ];
    let mut last = 0;
    for phase in order {
        let at = err
            .find(phase)
            .unwrap_or_else(|| panic!("missing phase {phase:?}: {err}"));
        assert!(at >= last, "phase {phase:?} out of order: {err}");
        last = at;
    }
    // Loading reports what it picked up.
    assert!(err.contains("config: "), "{err}");
    assert!(err.contains("app: demo"), "{err}");
}

#[test]
fn json_output_stays_clean_on_stdout() {
    let dir = hugo_site_repo("ui-json");
    let out = run_in(&dir, &["plan", "--json"]);
    // Banner/phases must not corrupt machine-readable output.
    serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("stdout must be pure JSON");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Delivery Boy CLI v"));
}

// --- target login method ----------------------------------------------------

fn login_repo(name: &str, target_yaml: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::write(dir.join("payload.txt"), "hi\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        format!(
            r#"
version: 1
app: demo
defaults: {{target: production}}
targets:
  production:
{target_yaml}
services:
  ship:
    deployer: files
    config: {{src: payload.txt}}
"#
        ),
    )
    .unwrap();
    dir
}

#[test]
fn ssh_key_is_used_for_the_scp_transport() {
    let dir = login_repo(
        "login-key",
        "    host: box.example.md\n    user: deploy\n    port: 2222\n    dir: /srv/demo\n    ssh:\n      key: /keys/id_ed25519\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("scp -C -p -o Port=2222 -i /keys/id_ed25519"),
        "{text}"
    );
    assert!(text.contains("deploy@box.example.md:/srv/demo/"), "{text}");
}

#[test]
fn ssh_options_jump_and_strict_host_checking_are_passed() {
    let dir = login_repo(
        "login-opts",
        "    host: box.example.md\n    user: root\n    dir: /srv/demo\n    ssh:\n      key: /keys/k\n      agent: false\n      strict_host_key_checking: accept-new\n      jump: bastion.example.md\n      options: [\"-o\", \"ServerAliveInterval=30\"]\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("IdentitiesOnly=yes"),
        "agent: false should pin the identity: {text}"
    );
    assert!(text.contains("StrictHostKeyChecking=accept-new"), "{text}");
    assert!(text.contains("-J bastion.example.md"), "{text}");
    assert!(text.contains("ServerAliveInterval=30"), "{text}");
}

#[test]
fn agent_only_target_needs_no_key() {
    let dir = login_repo(
        "login-agent",
        "    host: box.example.md\n    user: root\n    dir: /srv/demo\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("scp -C -p -o Port=22 "), "{text}");
    assert!(
        !text.contains(" -i "),
        "no identity file should be passed: {text}"
    );
}

#[test]
fn local_method_skips_ssh_entirely() {
    let dir = login_repo(
        "login-local",
        "    host: localhost\n    user: root\n    dir: /srv/demo\n    method: local\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // No ssh transport, no user@host: prefix — it's this machine.
    assert!(!text.contains("scp"), "{text}");
    assert!(!text.contains("root@localhost:"), "{text}");
    assert!(text.contains("cp -p payload.txt /srv/demo/"), "{text}");

    // Preflight has no remote to reach.
    let pf = String::from_utf8_lossy(&run_in(&dir, &["preflight"]).stderr).to_string();
    assert!(pf.contains("target is local (no ssh needed)"), "{pf}");
}

#[test]
fn unknown_method_is_rejected() {
    let dir = login_repo(
        "login-bad",
        "    host: box.example.md\n    user: root\n    dir: /srv/demo\n    method: telnet\n",
    );
    let out = run_in(&dir, &["validate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("method must be 'ssh' or 'local'"));
}

#[test]
fn tilde_in_key_path_is_expanded() {
    let dir = login_repo(
        "login-tilde",
        "    host: box.example.md\n    user: root\n    dir: /srv/demo\n    ssh:\n      key: ~/.ssh/demo.pem\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let home = std::env::var("HOME").unwrap();
    assert!(text.contains(&format!("-i {home}/.ssh/demo.pem")), "{text}");
}

// --- deploy version vs app release -----------------------------------------

#[test]
fn reports_deploy_version_and_release_separately() {
    let dir = hugo_site_repo("ver-outputs");
    let err = String::from_utf8_lossy(&run_in(&dir, &["deploy", "--dry-run"]).stderr).to_string();

    assert!(err.contains("▸ Versioning"), "{err}");
    assert!(err.contains("deploy version: "), "{err}");
    assert!(err.contains("release "), "{err}");
    // Both are reported again as outputs when it finishes.
    let done = err.split("▸ Done").nth(1).expect("no Done phase");
    assert!(done.contains("deploy version:"), "{done}");
    assert!(done.contains("release:"), "{done}");
}

#[test]
fn deploy_version_is_a_sortable_utc_stamp_plus_sha() {
    let dir = hugo_site_repo("ver-format");
    let out = run_in(&dir, &["plan", "--json"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // e.g. 20260730T064040Z-<sha> used as the release directory name.
    // Take it from the stage step, which names the release directly.
    let at = text.find("stage release ").expect("no stage step");
    let tail = &text[at + "stage release ".len()..];
    let id: String = tail
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"')
        .collect();
    assert_eq!(
        id.len(),
        16 + 1 + id.split('-').nth(1).unwrap_or("").len(),
        "unexpected id: {id}"
    );
    assert!(
        id.contains('T') && id.contains("Z-"),
        "not a UTC stamp: {id}"
    );
    assert!(id.starts_with("20"), "{id}");
}

#[test]
fn release_from_commit_count_is_supported() {
    // What Sparkle needs: a monotonic integer, independent of any CI counter.
    let dir = hugo_site_repo("ver-count");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg).unwrap().replace(
        "app: demo",
        "app: demo
release:
  version_from: commit-count",
    );
    std::fs::write(&cfg, body).unwrap();

    let err = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stderr).to_string();
    assert!(err.contains("(commit-count)"), "{err}");
}

#[test]
fn deploy_history_is_recorded_on_the_target() {
    let dir = hugo_site_repo("ver-history");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("record deploy"), "{text}");
    assert!(text.contains(".deliver/history.tsv"), "{text}");
}

// --- macos_app --------------------------------------------------------------

/// Make `dir` a git repo with `tag` on HEAD — a release number comes from a tag.
fn git_init_tagged(dir: &std::path::Path, tag: &str) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        vec!["add", "-A"],
        vec!["commit", "-qm", "init"],
        vec!["tag", tag],
    ] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&args)
            .output()
            .unwrap();
    }
}

fn macos_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes/1.0.0.html"), "<p>notes</p>").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, user: root, dir: /var/universal/demo}
services:
  macos:
    deployer: macos-app
    config:
      lane: release
      dmg: build/Demo-{version}.dmg
      appcast:
        url: https://demo.example.md/download/mac/appcast.xml
        download_url_prefix: https://demo.example.md/download/mac/
        notes: notes/{version}.html
        ed_key_keychain: demo-sparkle-private
      publish:
        remote_subdir: downloads/mac
        aliases:
          - Demo.dmg
          - latest/Demo.dmg
"#,
    )
    .unwrap();
    git_init_tagged(&dir, "1.0.0");
    dir
}

#[cfg(target_os = "macos")]
#[test]
fn macos_app_compiles_the_release_lifecycle() {
    let dir = macos_repo("macos-plan");
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);

    // Guards run before anything is built.
    let build_at = text.find("fastlane release").expect("no fastlane step");
    for guard in [
        "is well-formed and the tree is clean",
        "is newer than the published appcast",
    ] {
        let at = text
            .find(guard)
            .unwrap_or_else(|| panic!("missing guard {guard}: {text}"));
        assert!(
            at < build_at,
            "guard {guard:?} must precede the build: {text}"
        );
    }
    // Version + build are passed to the lane, not invented by it.
    assert!(text.contains("VERSION=1.0.0 BUILD="), "{text}");
    // Appcast is seeded from the live feed so old items survive.
    assert!(
        text.contains("curl -fsS https://demo.example.md/download/mac/appcast.xml -o"),
        "{text}"
    );
    assert!(text.contains("--ed-key-file -"), "{text}");
    assert!(
        text.contains("SourcePackages/artifacts/sparkle/Sparkle/bin/generate_appcast"),
        "{text}"
    );
    assert!(text.contains("command -v generate_appcast"), "{text}");
    assert!(!text.contains("$HOME/dev/sparkle"), "{text}");
    // Aliases are explicit and can use nested output paths.
    assert!(
        text.contains("Demo-1.0.0.dmg") && text.contains("Demo.dmg"),
        "{text}"
    );
    assert!(text.contains("latest/Demo.dmg"), "{text}");
    assert!(
        text.contains(
            "publish Demo-1.0.0.dmg + Demo.dmg + latest/Demo.dmg + Demo-1.0.0.html + appcast.xml"
        ),
        "{text}"
    );
    assert!(
        text.contains("verify Sparkle release notes are downloadable"),
        "{text}"
    );
    assert!(
        text.contains("verify alias Demo.dmg is downloadable"),
        "{text}"
    );
    assert!(
        text.contains("verify alias Demo.dmg matches Demo-1.0.0.dmg"),
        "{text}"
    );
    assert!(
        text.contains("verify alias latest/Demo.dmg is downloadable"),
        "{text}"
    );
    assert!(
        text.contains("published alias Demo.dmg does not match Demo-1.0.0.dmg"),
        "{text}"
    );
    // Publish + verify.
    assert!(text.contains("verify appcast advertises 1.0.0"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_app_does_not_invent_publish_aliases() {
    let dir = macos_repo("macos-no-alias");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg).unwrap().replace(
        "      publish:\n        remote_subdir: downloads/mac\n        aliases:\n          - Demo.dmg\n          - latest/Demo.dmg\n",
        "",
    );
    std::fs::write(&cfg, body).unwrap();

    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("publish Demo-1.0.0.dmg + Demo-1.0.0.html + appcast.xml"),
        "{text}"
    );
    assert!(!text.contains("verify alias"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_app_rejects_publish_aliases_outside_the_publish_dir() {
    let dir = macos_repo("macos-bad-alias");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("          - Demo.dmg\n", "          - ../Demo.dmg\n");
    std::fs::write(&cfg, body).unwrap();

    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("must be a relative path"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_app_guards_sparkle_build_number_against_live_appcast() {
    let dir = macos_repo("macos-guard");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("sparkle:version"),
        "must read published versions: {text}"
    );
    assert!(
        text.contains("clients would never see the update"),
        "{text}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_app_requires_an_ed_key_source() {
    let dir = macos_repo("macos-nokey");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("        ed_key_keychain: demo-sparkle-private\n", "");
    std::fs::write(&cfg, body).unwrap();
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("EdDSA private key"));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn macos_app_fails_fast_off_macos() {
    let dir = macos_repo("macos-wrong-os");
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("requires macOS"));
}

#[test]
fn named_service_overrides_enabled_false() {
    let dir = tmpdir("enabled-override");
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, user: root, dir: /srv/demo}
services:
  ship:
    deployer: files
    enabled: false
    config: {src: payload.txt}
"#,
    )
    .unwrap();
    // Off by default…
    assert!(!String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).contains("scp"));
    // …but naming it explicitly is how you run it.
    assert!(
        String::from_utf8_lossy(&run_in(&dir, &["plan", "--service", "ship"]).stdout)
            .contains("scp")
    );
}

// --- macos_app: xcodebuild strategy -----------------------------------------

#[cfg(target_os = "macos")]
fn xcodebuild_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes/1.0.0.html"), "<p>n</p>").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, user: root, dir: /var/universal/demo}
services:
  macos:
    deployer: macos-app
    config:
      strategy: xcodebuild
      xcodebuild:
        project: apps/macos/Demo.xcodeproj
        scheme: Demo
        team: TEAMID123
        identity: Developer ID Application
        export_options: .github/macos/ExportOptions.plist
        sign_embedded: [Contents/Resources/helper]
        notary_profile: demo-notary
        build_settings:
          EXTRA_SETTING: hello
      dmg: build/Demo-{version}.dmg
      appcast:
        url: https://demo.example.md/download/mac/appcast.xml
        notes: notes/{version}.html
        ed_key_keychain: demo-sparkle-private
"#,
    )
    .unwrap();
    git_init_tagged(&dir, "1.0.0");
    dir
}

#[cfg(target_os = "macos")]
#[test]
fn xcodebuild_strategy_compiles_apple_toolchain_without_fastlane() {
    let dir = xcodebuild_repo("xcb-plan");
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);

    assert!(
        !text.contains("fastlane"),
        "no fastlane dependency (raw Apple toolchain): {text}"
    );
    for expected in [
        "xcodebuild archive",
        "-exportArchive",
        "codesign",
        "hdiutil create",
        "xcrun notarytool submit",
        "xcrun stapler staple",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    // Versions come from the deploy identity, not from the build.
    assert!(text.contains("MARKETING_VERSION='1.0.0'"), "{text}");
    assert!(text.contains("CURRENT_PROJECT_VERSION="), "{text}");
    assert!(text.contains("EXTRA_SETTING=\"hello\""), "{text}");
    assert!(text.contains("--keychain-profile demo-notary"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn xcodebuild_strategy_compiles_release_helpers_and_extra_artifacts() {
    let dir = xcodebuild_repo("xcb-release-helpers");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace(
            "        export_options: .github/macos/ExportOptions.plist\n",
            "        export_options: .github/macos/ExportOptions.plist\n        prebuild: scripts/prep.sh {version} {work}\n        symbols: '{work}/Demo-{version}.dSYM.tar.gz'\n        smoke_test_seconds: 5\n        resign_app: false\n",
        )
        .replace(
            "        url: https://demo.example.md/download/mac/appcast.xml\n",
            "        url: https://demo.example.md/download/mac/appcast.xml\n        download_url_prefix: https://demo.example.md/download/mac/\n",
        )
        .replace(
            "        ed_key_keychain: demo-sparkle-private\n",
            "        ed_key_keychain: demo-sparkle-private\n      publish:\n        extra: ['{work}/helper-{version}.dmg']\n        archive: ['{work}/Demo-{version}.dSYM.tar.gz']\n        archive_remote_subdir: .deliver/symbols\n",
        );
    std::fs::write(&cfg, body).unwrap();

    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "scripts/prep.sh 1.0.0",
        "archive dSYM",
        "Demo-1.0.0.dSYM.tar.gz",
        "smoke test Demo.app startup (5s)",
        "verify exported signature for Demo.app",
        "helper-1.0.0.dmg",
        "verify extra artifact helper-1.0.0.dmg is downloadable",
        "archive Demo-1.0.0.dSYM.tar.gz",
        ".deliver/symbols/Demo-1.0.0.dSYM.tar.gz",
        "verify private archive Demo-1.0.0.dSYM.tar.gz",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(
        !text.contains("sign + verify Demo.app"),
        "resign_app=false must preserve export signing: {text}"
    );
    assert!(
        !text.contains("verify extra artifact Demo-1.0.0.dSYM"),
        "private archives must not get public URL checks: {text}"
    );
    assert!(!text.contains("{version}"), "{text}");
    assert!(!text.contains("{work}"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn embedded_binaries_are_signed_before_the_outer_bundle() {
    // Notarization rejects the DMG if this order is wrong.
    let dir = xcodebuild_repo("xcb-order");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let inner = text
        .find("sign embedded Contents/Resources/helper")
        .expect("no inner sign");
    let outer = text.find("sign + verify Demo.app").expect("no outer sign");
    let dmg = text.find("package Demo.dmg").expect("no dmg step");
    assert!(
        inner < outer,
        "embedded binary must be signed first: {text}"
    );
    assert!(
        outer < dmg,
        "bundle must be signed before packaging: {text}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn xcodebuild_strategy_is_inferred_from_its_block() {
    let dir = xcodebuild_repo("xcb-infer");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("      strategy: xcodebuild\n", "");
    std::fs::write(&cfg, body).unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("xcodebuild archive"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn unknown_strategy_is_rejected() {
    let dir = xcodebuild_repo("xcb-bad");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("strategy: xcodebuild", "strategy: carrier-pigeon");
    std::fs::write(&cfg, body).unwrap();
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown strategy"));
}

// --- cleanup, multi-host targets, pre/post ----------------------------------

#[test]
fn artifacts_are_cleaned_up_locally_and_on_the_target() {
    let dir = hugo_site_repo("cleanup-steps");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // Intermediates live in the system temp dir, never in the repo.
    let tmp = std::env::temp_dir()
        .join("deliver")
        .to_string_lossy()
        .to_string();
    assert!(
        text.contains(&tmp),
        "artifacts should be under {tmp}: {text}"
    );
    assert!(
        !text.contains(" sample-web.tar.gz "),
        "no bare repo-root artifact: {text}"
    );
    // Both copies of the shipped archive are removed.
    assert!(text.contains("remove shipped"), "{text}");
    assert!(text.contains("remove local "), "{text}");
}

#[test]
fn cleanup_runs_after_the_real_steps_and_is_skipped_on_failure() {
    let dir = hugo_site_repo("cleanup-order");
    let out = run_in(&dir, &["deploy", "--dry-run"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let last_real = text
        .rfind("activate release")
        .or_else(|| text.rfind("prune old"))
        .unwrap();
    let cleanup = text.find("cleanup: remove").expect("no cleanup phase");
    assert!(cleanup > last_real, "cleanup must come last: {text}");
}

#[test]
fn clean_command_removes_the_work_dir() {
    let dir = hugo_site_repo("clean-cmd");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("app: demo", "app: clean-test");
    std::fs::write(&cfg, body).unwrap();
    // `deliver clean` sweeps the app's scratch dir under the system temp dir.
    // Keep this test's app distinct because the suite runs in parallel.
    let scratch = std::env::temp_dir().join("deliver").join("clean-test");
    std::fs::create_dir_all(scratch.join("run-1")).unwrap();
    std::fs::write(scratch.join("run-1/leftover.tar.gz"), "x").unwrap();

    let out = run_in(&dir, &["clean"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!scratch.exists(), "scratch dir should be gone");
}

#[test]
fn a_target_can_have_several_hosts() {
    let dir = tmpdir("multi-host");
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production:
    hosts: [web1.example.md, web2.example.md]
    dir: /srv/demo
    ssh: {user: deploy, port: 2222, key: /keys/id}
services:
  ship:
    deployer: files
    config: {src: payload.txt}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // Planned once per host, each with its own destination.
    assert!(text.contains("deploy@web1.example.md:/srv/demo/"), "{text}");
    assert!(text.contains("deploy@web2.example.md:/srv/demo/"), "{text}");
    assert_eq!(text.matches("scp -C -p").count(), 2, "{text}");
    // ssh settings come from the ssh block.
    assert!(text.contains("-o Port=2222 -i /keys/id"), "{text}");
}

#[test]
fn host_and_hosts_together_is_an_error() {
    let dir = tmpdir("host-conflict");
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: a.example.md, hosts: [b.example.md], dir: /srv/demo}
services:
  ship: {deployer: commands, config: {steps: [{ssh: "true"}]}}
"#,
    )
    .unwrap();
    let out = run_in(&dir, &["validate"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not both"));
}

#[test]
fn legacy_top_level_user_and_port_still_work() {
    let dir = tmpdir("legacy-target");
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, user: root, port: 2200, dir: /srv/demo}
services:
  ship:
    deployer: files
    config: {src: payload.txt}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("root@box.example.md:"), "{text}");
    assert!(text.contains("-o Port=2200"), "{text}");
}

#[test]
fn nginx_verify_and_reload_are_implicit() {
    let dir = nginx_repo(
        "nginx-implicit-verify",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // No verify block in the config, yet nginx is validated and reloaded.
    assert!(text.contains("verify: nginx -t, then reload"), "{text}");
    assert!(text.contains("systemctl reload nginx"), "{text}");
}

#[test]
fn certbot_setup_is_idempotent_and_expands_when_domains_change() {
    let dir = nginx_repo(
        "certbot-idempotent",
        TWO_CERT_VHOST,
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // Setup: install if missing, webroot, renewal hook, timer — all re-runnable.
    assert!(
        text.contains("certbot: ensure installed, webroot, renewal hook and timer"),
        "{text}"
    );
    assert!(text.contains("renewal-hooks/deploy"), "{text}");
    assert!(text.contains("certbot.timer"), "{text}");
    // Issue/expand decided by comparing the cert's SANs to the wanted domains.
    assert!(text.contains("already covers"), "{text}");
    assert!(text.contains("--expand"), "{text}");
    assert!(text.contains("--keep-until-expiring"), "{text}");
}

#[test]
fn pre_and_post_hooks_run_around_the_deployer() {
    let dir = tmpdir("pre-post");
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  ship:
    deployer: files
    pre:
      - command: echo before-it
    post:
      - command: echo after-it
    config: {src: payload.txt}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let pre = text.find("echo before-it").expect("no pre step");
    let ship = text.find("scp -C -p").expect("no deployer step");
    let post = text.find("echo after-it").expect("no post step");
    assert!(pre < ship && ship < post, "{text}");
}

#[test]
fn legacy_before_after_keys_still_work() {
    let dir = tmpdir("before-after");
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  ship:
    deployer: files
    before: [{command: echo legacy-pre}]
    after: [{command: echo legacy-post}]
    config: {src: payload.txt}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("echo legacy-pre") && text.contains("echo legacy-post"),
        "{text}"
    );
}

// --- release tagging --------------------------------------------------------

fn git_repo_with_tagging(name: &str, tag_yaml: &str) -> std::path::PathBuf {
    let dir = hugo_site_repo(name);
    // A real repo, so tagging has something to point at.
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "T"],
        vec!["add", "-A"],
        vec!["commit", "-qm", "init"],
    ] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .unwrap();
    }
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("app: demo", &format!("app: demo\n{tag_yaml}"));
    std::fs::write(&cfg, body).unwrap();
    dir
}

fn tag_exists(dir: &std::path::Path, name: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn tagging_is_off_unless_enabled() {
    let dir = git_repo_with_tagging("tag-off", "versioning:\n  from: commit-count");
    run_in(&dir, &["deploy", "--dry-run"]);
    assert!(
        !tag_exists(&dir, "v1"),
        "nothing should be tagged when disabled"
    );
}

#[test]
fn dry_run_never_tags() {
    let dir = git_repo_with_tagging(
        "tag-dryrun",
        "versioning:\n  from: commit-count\n  tag: {enabled: true}",
    );
    run_in(&dir, &["deploy", "--dry-run"]);
    assert!(!tag_exists(&dir, "v1"), "a dry run must not tag");
}

#[test]
fn tag_name_is_templated() {
    // Verified through config parsing + the plan path; the tag itself is only
    // written after a real (non-dry-run) deploy.
    let dir = git_repo_with_tagging(
        "tag-template",
        "versioning:\n  from: commit-count\n  tag: {enabled: true, name: 'release-{version}'}",
    );
    let out = run_in(&dir, &["validate"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn versioning_block_is_canonical_and_release_is_still_accepted() {
    for (key, name) in [("versioning", "vers-canonical"), ("release", "vers-legacy")] {
        let dir = hugo_site_repo(name);
        let cfg = dir.join(".deliver.yml");
        let body = std::fs::read_to_string(&cfg).unwrap().replace(
            "app: demo",
            &format!("app: demo\n{key}:\n  from: commit-count"),
        );
        std::fs::write(&cfg, body).unwrap();

        let err = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stderr).to_string();
        assert!(err.contains("(commit-count)"), "{key}: {err}");
    }
}

#[test]
fn versioning_from_is_an_alias_for_version_from() {
    let dir = hugo_site_repo("vers-from-alias");
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("app: demo", "app: demo\nversioning:\n  from: commit-count");
    std::fs::write(&cfg, body).unwrap();

    let err = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stderr).to_string();
    assert!(err.contains("(commit-count)"), "{err}");
}

// --- release comes from a git tag -------------------------------------------

#[test]
fn deploy_uses_the_tag_on_head_without_prompting_when_yes() {
    let dir = hugo_site_repo("rel-tag-yes");
    git_init_tagged(&dir, "v3.1.4");
    let err = String::from_utf8_lossy(&run_in(&dir, &["deploy", "--dry-run", "--yes"]).stderr)
        .to_string();
    assert!(err.contains("release v3.1.4 (tag)"), "{err}");
}

#[test]
fn deploy_confirms_the_found_tag_and_cancels_on_no() {
    use std::io::Write;
    let dir = local_deploy_repo("rel-tag-confirm");
    git_init_tagged(&dir, "v3.1.4");

    let mut child = deliver()
        .current_dir(&dir)
        .args(["deploy"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("Deploy release v3.1.4"),
        "should confirm: {text}"
    );
    assert!(text.contains("canceled"), "{text}");
    assert!(out.status.success(), "declining is not an error");
}

/// A repo that deploys to this machine, so a real (non-dry-run) deploy needs no
/// network and no server.
fn local_deploy_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::write(dir.join("payload.txt"), "hi\n").unwrap();
    let dest = dir.join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        format!(
            r#"
version: 1
app: demo
versioning:
  tag: {{enabled: true}}
defaults: {{target: local}}
targets:
  local: {{method: local, dir: {}}}
services:
  ship:
    deployer: files
    config: {{src: payload.txt}}
"#,
            dest.display()
        ),
    )
    .unwrap();
    dir
}

#[test]
fn untagged_head_offers_to_create_a_tag() {
    use std::io::Write;
    let dir = local_deploy_repo("rel-create-tag");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();

    let mut child = deliver()
        .current_dir(&dir)
        .args(["deploy"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"0.4.1\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("HEAD has no tag"),
        "should offer a version: {text}"
    );
    assert!(text.contains("tag:            v0.4.1 (created)"), "{text}");
    assert!(tag_exists(&dir, "v0.4.1"), "tag should exist on HEAD");
}

#[test]
fn untagged_head_with_no_input_explains_instead_of_hanging() {
    let dir = local_deploy_repo("rel-no-input");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();

    let out = run_in(&dir, &["deploy"]); // stdin is /dev/null
    assert!(out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("no release to deploy"), "{text}");
    assert!(
        text.contains("--version"),
        "should say how to proceed: {text}"
    );
}

#[test]
fn version_flag_creates_the_tag_without_prompting() {
    let dir = local_deploy_repo("rel-version-flag");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();

    let out = run_in(&dir, &["deploy", "--version", "2.0.0"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tag_exists(&dir, "v2.0.0"), "expected v2.0.0 to be created");
}

#[test]
fn failed_deploy_does_not_create_a_release_tag() {
    let dir = local_deploy_repo("rel-failed-no-tag");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();
    let cfg = dir.join(".deliver.yml");
    let body = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("deployer: files", "deployer: commands")
        .replace(
            "config: {src: payload.txt}",
            "config: {steps: [{command: 'exit 1'}]}",
        );
    std::fs::write(&cfg, body).unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "make deploy fail"]] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .unwrap();
    }

    let out = run_in(&dir, &["deploy", "--version", "2.0.0"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        !tag_exists(&dir, "v2.0.0"),
        "failed deploy must not leave a release tag"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn untagged_mac_release_is_a_runtime_guard_not_a_plan_failure() {
    // Enabling macos-app by default must not break `plan` on an untagged commit.
    let dir = macos_repo("macos-untagged");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "1.0.0"])
        .output()
        .unwrap();

    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "plan must still work: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("guard: HEAD must be tagged"), "{text}");
    assert!(
        !text.contains("xcodebuild archive"),
        "nothing to build without a release: {text}"
    );
}

// --- version bump menu ------------------------------------------------------

fn deploy_with_input(dir: &std::path::Path, input: &str) -> String {
    use std::io::Write;
    let mut child = deliver()
        .current_dir(dir)
        .args(["deploy"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A local-deploy repo whose history already has `previous` tagged, with an
/// untagged commit on top.
fn repo_with_previous_release(name: &str, previous: &str) -> std::path::PathBuf {
    let dir = local_deploy_repo(name);
    git_init_tagged(&dir, previous);
    std::fs::write(dir.join("payload.txt"), "changed\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "next"]] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(&args)
            .output()
            .unwrap();
    }
    dir
}

#[test]
fn offers_patch_minor_major_off_the_previous_release() {
    let dir = repo_with_previous_release("bump-menu", "v1.4.9");
    let text = deploy_with_input(&dir, "\n"); // blank cancels, but the menu shows

    assert!(text.contains("Previous release: v1.4.9"), "{text}");
    assert!(text.contains("patch") && text.contains("v1.4.10"), "{text}");
    assert!(text.contains("minor") && text.contains("v1.5.0"), "{text}");
    assert!(text.contains("major") && text.contains("v2.0.0"), "{text}");
    assert!(text.contains("custom"), "{text}");
}

#[test]
fn choosing_minor_tags_the_bumped_version() {
    let dir = repo_with_previous_release("bump-minor", "v1.4.9");
    deploy_with_input(&dir, "2\n");
    assert!(tag_exists(&dir, "v1.5.0"), "expected v1.5.0 to be created");
}

#[test]
fn choosing_patch_is_the_first_option() {
    let dir = repo_with_previous_release("bump-patch", "v0.4.0");
    deploy_with_input(&dir, "1\n");
    assert!(tag_exists(&dir, "v0.4.1"), "expected v0.4.1");
}

#[test]
fn a_typed_version_is_accepted_directly() {
    let dir = repo_with_previous_release("bump-typed", "v1.0.0");
    deploy_with_input(&dir, "3.2.1\n");
    assert!(tag_exists(&dir, "v3.2.1"), "typing a version should work");
}

#[test]
fn tag_prefix_convention_is_preserved() {
    // Repo tags without a leading v — new tags should match.
    let dir = repo_with_previous_release("bump-noprefix", "2.0.0");
    deploy_with_input(&dir, "1\n");
    assert!(tag_exists(&dir, "2.0.1"), "expected unprefixed 2.0.1");
    assert!(!tag_exists(&dir, "v2.0.1"), "should not invent a v prefix");
}

#[test]
fn first_release_is_offered_when_there_are_no_tags() {
    let dir = local_deploy_repo("bump-first");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();

    let text = deploy_with_input(&dir, "1\n");
    assert!(text.contains("no previous releases"), "{text}");
    assert!(tag_exists(&dir, "v0.1.0"), "{text}");
}

// --- secrets ----------------------------------------------------------------

fn secrets_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::write(dir.join("payload.txt"), "x").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production:
    host: box.example.md
    dir: /srv/demo
    ssh: {key: /definitely/not/here.pem}
services:
  macos:
    deployer: macos-app
    config:
      strategy: xcodebuild
      xcodebuild:
        project: a.xcodeproj
        scheme: Demo
        export_options: e.plist
        notary_profile: demo-notary
        build_settings:
          PUBLIC_KEY: $(security find-generic-password -w -s demo-sparkle-public -a release)
      dmg: "{work}/Demo-{version}.dmg"
      appcast:
        url: https://demo.example.md/appcast.xml
        ed_key_keychain: demo-sparkle-private
"#,
    )
    .unwrap();
    dir
}

#[test]
fn secrets_are_derived_from_the_config() {
    let dir = secrets_repo("secrets-derive");
    let out = run_in(&dir, &["secrets"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Every secret-bearing reference in the config is found, including the one
    // buried in a build setting's shell fragment.
    assert!(text.contains("demo-sparkle-private"), "{text}");
    assert!(text.contains("demo-sparkle-public"), "{text}");
    assert!(text.contains("demo-notary"), "{text}");
    assert!(text.contains("/definitely/not/here.pem"), "{text}");
    // Each says where it's used and how to fix it.
    assert!(
        text.contains("used by services.macos.appcast.ed_key_keychain"),
        "{text}"
    );
    assert!(
        text.contains("deliver secrets set demo-sparkle-private"),
        "{text}"
    );
    assert!(
        text.contains("xcrun notarytool store-credentials demo-notary"),
        "{text}"
    );
    // Missing secrets are an error, not a warning.
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn secrets_never_print_values() {
    let dir = secrets_repo("secrets-quiet");
    let out = run_in(&dir, &["secrets"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // The check must not shell out with -w (which would read the secret).
    assert!(!text.contains("-w "), "{text}");
}

#[test]
fn preflight_fails_when_a_secret_is_missing() {
    // The point: fail in a second, not ten minutes into a notarized build.
    let dir = secrets_repo("secrets-preflight");
    let out = run_in(&dir, &["preflight"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("demo-sparkle-private") || text.contains("not found"),
        "{text}"
    );
}

#[test]
fn a_config_with_no_secrets_says_so() {
    let dir = local_deploy_repo("secrets-none");
    let out = run_in(&dir, &["secrets"]);
    assert!(out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("needs no secrets"), "{text}");
}

#[test]
fn secrets_are_checked_before_anything_is_tagged() {
    // A missing secret must stop the run *before* the version prompt, because
    // resolving a release can create a git tag — and tagging a commit for a
    // deploy that cannot succeed leaves a false record in history.
    let dir = secrets_repo("secrets-first");
    git_init_tagged(&dir, "tmp");
    std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-d", "tmp"])
        .output()
        .unwrap();

    let out = run_in(&dir, &["deploy"]);
    assert_eq!(out.status.code(), Some(2));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let secrets_at = text.find("▸ Secrets").expect("no secrets phase");
    assert!(
        text.contains("nothing was tagged, built, or changed"),
        "{text}"
    );
    // Versioning must not have run at all.
    if let Some(v) = text.find("▸ Versioning") {
        assert!(secrets_at < v, "secrets must come first: {text}");
    }
    // And no tag was created.
    let tags = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["tag", "-l"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&tags.stdout).trim().is_empty(),
        "no tag should exist"
    );
}

// --- docker-compose ---------------------------------------------------------

fn compose_repo(name: &str, extra: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {app: {image: demo:latest}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("docker-compose.prod.yml"),
        "services: {app: {restart: always}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".env.deploy"),
        "DB_PASSWORD=from-file\nAPI_KEY=file-key\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        format!(
            r#"
version: 1
app: demo
defaults: {{target: production}}
targets:
  production: {{host: box.example.md, dir: /var/universal/demo}}
secrets:
  providers:
    - env
    - file: .env.deploy
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml, docker-compose.prod.yml]
      project: demo
{extra}
"#
        ),
    )
    .unwrap();
    dir
}

const COMPOSE_ENV: &str = r#"      env_file:
        path: .env
        literals: {ENVIRONMENT: production}
        from_secrets: [DB_PASSWORD, API_KEY]
"#;

#[test]
fn compose_ships_the_image_as_a_tarball_by_default() {
    let dir = compose_repo("compose-tarball", "");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("docker build --platform linux/amd64"),
        "{text}"
    );
    assert!(text.contains("docker save"), "{text}");
    assert!(text.contains("docker load -i"), "{text}");
    assert!(
        !text.contains("docker push"),
        "tarball is the default: {text}"
    );
    // The shipped archive is removed from the target afterwards.
    assert!(text.contains("remove the shipped image tarball"), "{text}");
}

#[test]
fn compose_can_use_a_registry_instead() {
    let dir = compose_repo(
        "compose-registry",
        "      image: {tag: 'demo:latest', transport: registry, registry: ghcr.io/acme}\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("docker push ghcr.io/acme/demo:latest"),
        "{text}"
    );
    assert!(
        text.contains("docker pull ghcr.io/acme/demo:latest"),
        "{text}"
    );
    assert!(!text.contains("docker save"), "{text}");
}

#[test]
fn compose_files_are_shipped_unmodified_and_used_in_order() {
    let dir = compose_repo("compose-files", "");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("ship docker-compose.yml"), "{text}");
    assert!(text.contains("ship docker-compose.prod.yml"), "{text}");
    // Both files, in order, on every compose invocation.
    assert!(
        text.contains("-f docker-compose.yml -f docker-compose.prod.yml -p demo"),
        "{text}"
    );
}

#[test]
fn env_is_rendered_from_the_provider_chain_without_leaking_values() {
    let dir = compose_repo("compose-env", COMPOSE_ENV);
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();

    // The step exists and reports a line count, never contents.
    assert!(text.contains("render .env"), "{text}");
    assert!(text.contains("contents hidden"), "{text}");
    // Values resolved from the dotenv provider must not appear anywhere.
    assert!(
        !text.contains("from-file"),
        "secret leaked into the plan: {text}"
    );
    assert!(
        !text.contains("file-key"),
        "secret leaked into the plan: {text}"
    );
    // Shipped with restrictive permissions.
    assert!(text.contains("chmod 600"), "{text}");
}

#[test]
fn env_values_never_reach_json_output_either() {
    let dir = compose_repo("compose-env-json", COMPOSE_ENV);
    let out = run_in(&dir, &["plan", "--json"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str::<serde_json::Value>(&text).expect("valid JSON");
    assert!(
        !text.contains("from-file"),
        "secret leaked into JSON: {text}"
    );
}

#[test]
fn a_missing_secret_names_it_and_the_chain_searched() {
    let dir = compose_repo(
        "compose-missing",
        "      env_file:\n        path: .env\n        from_secrets: [DB_PASSWORD, NOT_ANYWHERE]\n",
    );
    let out = run_in(&dir, &["plan"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NOT_ANYWHERE"), "{err}");
    assert!(
        err.contains("env → file .env.deploy"),
        "should name the chain: {err}"
    );
    assert!(
        !err.contains("from-file"),
        "must not echo values it did find: {err}"
    );
}

#[test]
fn backups_run_before_services_are_replaced() {
    let dir = compose_repo(
        "compose-backup",
        "      backup:\n        database: {service: db, user: demo, name: demo}\n        volumes: [demo_media]\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let db = text.find("back up database").expect("no db backup");
    let vol = text
        .find("back up volume demo_media")
        .expect("no volume backup");
    let up = text.find("start services").expect("no up step");
    assert!(
        db < up && vol < up,
        "backups must precede replacement: {text}"
    );
    assert!(text.contains("pg_dump"), "{text}");
    assert!(
        text.contains("compose.yml -f docker-compose.prod.yml -p demo ps -q db"),
        "{text}"
    );
    assert!(
        !text.contains("docker ps -q --filter name="),
        "backup must not guess Compose container names: {text}"
    );
}

#[test]
fn a_failed_start_can_roll_back_to_the_previous_image() {
    let dir = compose_repo("compose-rollback", "");
    let text = String::from_utf8_lossy(&run_in(&dir, &["deploy", "--dry-run"]).stdout).to_string();
    assert!(
        text.contains("mark the current image as rollback"),
        "{text}"
    );
    let mark = text
        .find("mark the current image as rollback")
        .expect("no rollback tag step");
    let load = text
        .find("load image on the target")
        .expect("no image load step");
    let start = text.find("start services").expect("no start step");
    assert!(
        mark < load && load < start,
        "outgoing image must be tagged before the new image replaces it: {text}"
    );
    // The start step advertises an undo.
    let after = &text[start..];
    assert!(after.contains("undo available"), "{after}");
}

// --- secrets.define ---------------------------------------------------------

fn declared_repo(name: &str, define: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {app: {image: demo}}\n",
    )
    .unwrap();
    std::fs::write(dir.join(".env.deploy"), "DB_PASSWORD=from-file\nOTHER=o\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        format!(
            r#"
version: 1
app: demo
defaults: {{target: production}}
targets:
  production: {{host: box.example.md, dir: /srv/demo}}
secrets:
  providers: [env, {{file: .env.deploy}}]
  define:
{define}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      env_file: {{path: .env}}
"#
        ),
    )
    .unwrap();
    dir
}

#[test]
fn declared_secrets_are_used_without_repeating_them_per_service() {
    let dir = declared_repo("declare-list", "    - DB_PASSWORD\n    - OTHER\n");
    let out = run_in(&dir, &["plan"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // Both declared names land in the rendered env, with no `from_secrets` list.
    assert!(text.contains("render .env (2 lines)"), "{text}");
    assert!(!text.contains("from-file"), "no value leakage: {text}");
}

#[test]
fn secrets_command_reports_where_each_declared_secret_came_from() {
    let dir = declared_repo("declare-report", "    - DB_PASSWORD\n    - MISSING_ONE\n");
    let out = run_in(&dir, &["secrets"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("DB_PASSWORD — found in file .env.deploy"),
        "{text}"
    );
    assert!(
        text.contains("MISSING_ONE — not found in any provider"),
        "{text}"
    );
    assert!(
        !text.contains("from-file"),
        "reports the provider, never the value: {text}"
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_secret_can_override_the_chain_with_its_own_source() {
    let dir = declared_repo("declare-override", "    DB_PASSWORD: {file: .env.other}\n");
    std::fs::write(dir.join(".env.other"), "DB_PASSWORD=from-other\n").unwrap();
    let out = run_in(&dir, &["secrets"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Its own source wins over the default chain, which also has DB_PASSWORD.
    assert!(
        text.contains("DB_PASSWORD — found in file .env.other"),
        "{text}"
    );
}

#[test]
fn optional_secrets_may_be_absent() {
    let dir = declared_repo(
        "declare-optional",
        "    DB_PASSWORD: {}\n    NICE_TO_HAVE: {required: false}\n",
    );
    let out = run_in(&dir, &["secrets"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("NICE_TO_HAVE — not set (optional)"), "{text}");
    assert!(
        out.status.success(),
        "an absent optional secret is not a failure: {text}"
    );
    // And it simply doesn't appear in the rendered env.
    let plan = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(plan.contains("render .env (1 lines)"), "{plan}");
}

#[test]
fn declared_secrets_gate_the_startup_phase() {
    let dir = declared_repo("declare-gate", "    - DB_PASSWORD\n    - MISSING_ONE\n");
    let out = run_in(&dir, &["deploy"]);
    assert_eq!(out.status.code(), Some(2));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("1/2 declared secret(s) resolved"), "{text}");
    assert!(
        text.contains("nothing was tagged, built, or changed"),
        "{text}"
    );
}

#[test]
fn infra_and_release_commands_run_in_the_right_order() {
    // Migrations need a live database, and must land before new app containers
    // start serving with the new code.
    let dir = compose_repo(
        "compose-release",
        "      infra:\n        services: [db, redis]\n        wait: {service: db, command: pg_isready}\n\
         \n      release:\n        service: app\n        commands: ['python manage.py migrate --noinput']\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    let infra = text
        .find("start infrastructure (db, redis)")
        .expect("no infra step");
    let wait = text.find("wait for db").expect("no wait step");
    let migrate = text
        .find("release: python manage.py migrate")
        .expect("no migrate step");
    let up = text.find("start services").expect("no up step");
    assert!(infra < wait, "{text}");
    assert!(wait < migrate, "wait for the db before migrating: {text}");
    assert!(migrate < up, "migrate before serving new code: {text}");
    assert!(
        text.contains("run --rm app python manage.py migrate"),
        "{text}"
    );
}

// --- serving from the target directory itself -------------------------------

#[test]
fn a_site_can_be_served_from_the_target_directory_itself() {
    // When nginx's root can't be repointed, the served path becomes the symlink.
    let dir = tmpdir("release-root-served");
    std::fs::create_dir_all(dir.join("content")).unwrap();
    std::fs::write(dir.join("hugo.toml"), "baseURL = 'https://x/'\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: site
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /var/www/site}
services:
  web:
    deployer: hugo
    config:
      source: .
      remote_subdir: "."
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    // Releases go in a sibling, never under the web root — otherwise every old
    // release would be downloadable, and the incoming tarball would land inside
    // the live site.
    assert!(text.contains("/var/www/site.releases"), "{text}");
    assert!(!text.contains("/var/www/site/releases"), "{text}");
    // The served path itself is what gets swapped.
    assert!(
        text.contains("mv -Tf /var/www/site.new /var/www/site"),
        "{text}"
    );
    // A pre-existing directory is migrated, not deleted.
    assert!(text.contains("premigrate-"), "{text}");
    assert!(
        text.contains("PREV=/var/www/site.releases/premigrate-"),
        "the migrated directory must become the rollback target: {text}"
    );
    assert!(
        text.contains("mv /var/www/site \"$PREV\""),
        "the migration and rollback marker must name the same path: {text}"
    );
    assert!(text.contains("tar -xzf"), "extract on the server: {text}");
}

#[test]
fn a_nested_served_path_keeps_releases_under_the_target_dir() {
    let dir = tmpdir("release-nested");
    std::fs::create_dir_all(dir.join("content")).unwrap();
    std::fs::write(dir.join("hugo.toml"), "baseURL = 'https://x/'\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: site
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /var/www/site}
services:
  web:
    deployer: hugo
    config:
      source: .
      remote_subdir: apps/website/public
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("/var/www/site/releases/"), "{text}");
    assert!(text.contains("/var/www/site/apps/website/public"), "{text}");
}

#[test]
fn several_images_build_separately_and_ship_in_one_archive() {
    let dir = tmpdir("compose-multi-image");
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {api: {image: demo-api}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      project: demo
      images:
        - {tag: 'demo-api:latest', context: ., dockerfile: services/api/Dockerfile}
        - {tag: 'demo-web:latest', context: ., dockerfile: services/web/Dockerfile}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    assert!(text.contains("build demo-api:latest"), "{text}");
    assert!(text.contains("build demo-web:latest"), "{text}");
    assert!(
        text.contains("-f services/web/Dockerfile"),
        "per-image dockerfile: {text}"
    );
    // One archive, one transfer, one load — not three of each.
    assert_eq!(text.matches("docker save").count(), 1, "{text}");
    assert!(
        text.contains("docker save demo-api:latest demo-web:latest"),
        "{text}"
    );
    assert_eq!(text.matches("docker load -i").count(), 1, "{text}");
}

#[test]
fn single_image_build_uses_its_dockerfile() {
    let dir = compose_repo(
        "compose-single-dockerfile",
        "      image:\n        tag: demo:latest\n        context: .\n        dockerfile: apps/server/Dockerfile\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("docker build --platform linux/amd64 -f apps/server/Dockerfile"),
        "single-image dockerfile: {text}"
    );
}

#[test]
fn scp_keeps_dash_p_for_mode_and_spells_the_port_differently() {
    let dir = login_repo(
        "scp-port",
        "    host: box.example.md\n    user: deploy\n    port: 2222\n    dir: /srv/demo\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // ssh's `-p <port>` is scp's `-o Port=`; scp's own `-p` preserves mode, which
    // is what keeps a 0600 .env at 0600 in flight. Passing ssh's form through
    // unchanged would silently turn the port into a permission flag.
    assert!(text.contains("scp -C -p -o Port=2222"), "{text}");
    assert!(
        !text.contains("-p 2222"),
        "port must not survive as scp's -p: {text}"
    );
}

#[test]
fn an_nginx_conf_directory_ships_recursively() {
    let dir = nginx_repo(
        "nginx-scp-dir",
        "server {\n    listen 80;\n    server_name demo.example.md;\n}\n",
        "      conf: nginx/site.conf\n      ssl: false\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(
        text.contains("scp -C -p -r "),
        "a directory needs -r: {text}"
    );
}

// --- nginx_vhost: render + snippets ------------------------------------------

fn render_repo(name: &str) -> std::path::PathBuf {
    let dir = tmpdir(name);
    std::fs::create_dir_all(dir.join("nginx")).unwrap();
    std::fs::write(
        dir.join("nginx/site.conf"),
        "server {\n    listen 80;\n    server_name demo.example.md;\n    \
         add_header Content-Security-Policy \"connect-src 'self' __SENTRY_ORIGIN__;\";\n    \
         include /etc/nginx/snippets/demo-access.conf;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: example
defaults: {target: production}
targets:
  production: {host: example.md, user: root, dir: /var/universal/example}
secrets:
  providers: [env]
  define: {SENTRY_DSN: {}, ADMIN_TOKEN: {}}
services:
  nginx:
    deployer: nginx-vhost
    config:
      conf: nginx/site.conf
      ssl: false
      render:
        __SENTRY_ORIGIN__: "{origin:SENTRY_DSN}"
      snippets:
        - path: /etc/nginx/snippets/demo-access.conf
          mode: "0600"
          content: 'proxy_set_header X-Admin "{secret:ADMIN_TOKEN}";'
"#,
    )
    .unwrap();
    dir
}

#[test]
fn a_csp_origin_is_rendered_without_the_dsn_key() {
    let dir = render_repo("nginx-render");
    let mut cmd = deliver();
    cmd.current_dir(&dir)
        .arg("plan")
        .env(
            "SENTRY_DSN",
            "https://abc123publickey@o42.ingest.sentry.io/7",
        )
        .env("ADMIN_TOKEN", "s3cr3t-token-value-that-is-long-enough");
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        text.contains("render nginx/site.conf (1 placeholder(s))"),
        "{text}"
    );
    assert!(text.contains("render snippet demo-access.conf"), "{text}");
    // The snippet installs before the vhost — a conf that includes a missing
    // snippet fails nginx -t, which would roll the vhost back for the wrong reason.
    let snippet = text.find("install snippet").expect("no snippet install");
    let vhost = text.find("install vhost").expect("no vhost install");
    assert!(snippet < vhost, "snippet must install first: {text}");

    // Neither the DSN's public key nor the token may appear anywhere in output.
    assert!(!text.contains("abc123publickey"), "DSN key leaked: {text}");
    assert!(!text.contains("s3cr3t-token"), "token leaked: {text}");
}

#[test]
fn a_renamed_placeholder_fails_before_anything_installs() {
    let dir = render_repo("nginx-render-typo");
    let cfg = dir.join(".deliver.yml");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("__SENTRY_ORIGIN__", "__WRONG__");
    std::fs::write(&cfg, text).unwrap();
    let mut cmd = deliver();
    cmd.current_dir(&dir)
        .arg("plan")
        .env("SENTRY_DSN", "https://k@o42.ingest.sentry.io/7")
        .env("ADMIN_TOKEN", "t");
    let out = cmd.output().unwrap();
    assert!(
        !out.status.success(),
        "a missing placeholder must fail the plan"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("contains no placeholder '__WRONG__'"), "{err}");
}

#[test]
fn an_origin_refuses_a_non_https_secret() {
    let dir = render_repo("nginx-render-http");
    let mut cmd = deliver();
    cmd.current_dir(&dir)
        .arg("plan")
        .env("SENTRY_DSN", "http://k@insecure.example.md/7")
        .env("ADMIN_TOKEN", "t");
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("must be https"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_built_bundle_is_packaged_and_released_atomically() {
    let dir = tmpdir("files-build");
    std::fs::create_dir_all(dir.join("apps/panel/dist")).unwrap();
    std::fs::write(dir.join("apps/panel/dist/index.html"), "<html>").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  panel:
    deployer: files
    config:
      build: npm ci && npm run build
      build_dir: apps/panel
      src: apps/panel/dist
      remote_subdir: panel
      env: {VITE_API_URL: "https://api.example.md", VITE_RELEASE: "{version}"}
"#,
    )
    .unwrap();
    git_init_tagged(&dir, "v1.2.3");
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    assert!(text.contains("build (apps/panel)"), "{text}");
    assert!(
        text.contains("VITE_API_URL='https://api.example.md'"),
        "{text}"
    );
    assert!(text.contains("VITE_RELEASE='1.2.3'"), "{text}");
    // A directory src implies packaging and an atomic release — a bare scp of a
    // directory would leave the site half-written while it copied.
    assert!(text.contains("package apps/panel/dist"), "{text}");
    assert!(
        text.contains("demo-panel.tar.gz"),
        "named per subdir: {text}"
    );
    assert!(text.contains("atomic symlink swap"), "{text}");
}

#[test]
fn included_paths_ship_next_to_the_compose_files() {
    let dir = tmpdir("compose-include");
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {app: {image: demo}}\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".infra/nats")).unwrap();
    std::fs::write(dir.join(".infra/nats/nats.conf"), "port: 4222\n").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      include: [.infra]
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("ship .infra/"), "{text}");
    assert!(
        text.contains("scp -C -p -r "),
        "a directory needs -r: {text}"
    );
    // Config the stack bind-mounts has to land before anything starts.
    let ship = text.find("ship .infra/").unwrap();
    let up = text.find("start services").unwrap();
    assert!(ship < up, "{text}");
}

#[test]
fn a_missing_include_fails_the_plan() {
    let dir = tmpdir("compose-include-missing");
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {app: {image: demo}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      include: [.infra]
"#,
    )
    .unwrap();
    let out = run_in(&dir, &["plan"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("include '.infra' does not exist"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_cert_is_renewed_when_it_expires_even_if_the_names_still_match() {
    let dir = nginx_repo(
        "nginx-expiry",
        "server {\n    listen 443 ssl;\n    server_name demo.example.md;\n    \
         ssl_certificate /etc/letsencrypt/live/demo.example.md/fullchain.pem;\n}\n",
        "      conf: nginx/site.conf\n      certbot:\n        webroot: /var/universal/letsencrypt\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    // Matching SANs are not sufficient: an expired cert covers exactly the right
    // names, and skipping on names alone is how one silently stays expired.
    assert!(text.contains("-checkend 2592000"), "30-day window: {text}");
    assert!(text.contains("expires within 30d"), "{text}");
    // A cert that renews from a webroot nginx does not serve fails forever while
    // looking healthy in config, so the renewal path is checked too.
    assert!(
        text.contains("webroot_path = /var/universal/letsencrypt"),
        "{text}"
    );
    assert!(text.contains("wrong webroot"), "{text}");
}

#[test]
fn the_default_acme_webroot_is_the_current_convention() {
    let dir = nginx_repo(
        "nginx-default-webroot",
        "server {\n    listen 443 ssl;\n    server_name demo.example.md;\n    \
         ssl_certificate /etc/letsencrypt/live/demo.example.md/fullchain.pem;\n}\n",
        "      conf: nginx/site.conf\n      certbot:\n        email: ops@example.md\n",
    );
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    assert!(text.contains("/var/universal/letsencrypt"), "{text}");
    assert!(
        !text.contains("/var/www/letsencrypt"),
        "the old path must not be the default: {text}"
    );
}

#[test]
fn a_release_workdir_precedes_the_service_name() {
    let dir = tmpdir("compose-release-workdir");
    std::fs::write(
        dir.join("docker-compose.yml"),
        "services: {app: {image: demo}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /srv/demo}
services:
  app:
    deployer: docker-compose
    config:
      files: [docker-compose.yml]
      release:
        service: app
        workdir: /app/packages/backend
        commands: ["npx tsx src/db/migrate.ts"]
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // `-w` after the service name is passed to the entrypoint, which fails with
    // an opaque "node: bad option: -w" halfway through a deploy.
    assert!(
        text.contains("run --rm -w /app/packages/backend app npx tsx"),
        "{text}"
    );
}

#[test]
fn a_root_served_site_records_history_outside_the_swapped_symlink() {
    let dir = tmpdir("files-root-state");
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /var/www/demo}
services:
  web:
    deployer: files
    config: {src: site, remote_subdir: "."}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();

    // /var/www/demo is the symlink, so anything written under it lands inside
    // whichever release is current and is orphaned by the next swap — the
    // history would restart at #1 on every deploy.
    assert!(
        text.contains("/var/www/demo.releases/.deliver/history.tsv"),
        "{text}"
    );
    assert!(
        !text.contains("/var/www/demo/.deliver"),
        "state must not sit inside the symlink: {text}"
    );
}

#[test]
fn a_nested_served_path_keeps_history_at_the_target_root() {
    let dir = tmpdir("files-nested-state");
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(
        dir.join(".deliver.yml"),
        r#"
version: 1
app: demo
defaults: {target: production}
targets:
  production: {host: box.example.md, dir: /var/universal/demo}
services:
  web:
    deployer: files
    config: {src: site, remote_subdir: public}
"#,
    )
    .unwrap();
    let text = String::from_utf8_lossy(&run_in(&dir, &["plan"]).stdout).to_string();
    // Here the target dir is a real directory that outlives every swap.
    assert!(
        text.contains("/var/universal/demo/.deliver/history.tsv"),
        "{text}"
    );
    assert!(
        !text.contains("wc -l <"),
        "a missing first-run history file must not cause a shell redirection error: {text}"
    );
}
