//! Best-effort release notices for standalone CLI deploys.
//!
//! These are deliberately outside the step executor. A webhook outage must not
//! roll back a healthy release, while a failed deploy still needs a chance to
//! report the step that stopped it.

use crate::{config::Config, secrets, version::DeployVersion};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn expand(
    template: &str,
    config: &Config,
    version: &DeployVersion,
    event: &str,
    failed: Option<&str>,
) -> String {
    template
        .replace("{app}", &config.app)
        .replace("{version}", &version.marketing_version())
        .replace("{release}", &version.release_display())
        .replace("{deploy}", &version.id)
        .replace("{sha}", &version.git.short_sha)
        .replace("{event}", event)
        .replace("{failed_step}", failed.unwrap_or("unknown step"))
}

fn generic_payload(
    config: &Config,
    version: &DeployVersion,
    event: &str,
    failed: Option<&str>,
) -> String {
    let text = match event {
        "started" => format!(
            "{} {} deploy started",
            config.app,
            version.marketing_version()
        ),
        "succeeded" => format!("{} {} is live", config.app, version.marketing_version()),
        "failed" => format!(
            "{} {} deploy failed at {}",
            config.app,
            version.marketing_version(),
            failed.unwrap_or("an unknown step")
        ),
        _ => format!(
            "{} {} deploy: {event}",
            config.app,
            version.marketing_version()
        ),
    };
    serde_json::json!({"text": text}).to_string()
}

fn command_payload(command: &str, root: &Path) -> Result<String, String> {
    let output = Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run payload command: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "payload command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let payload = String::from_utf8(output.stdout)
        .map_err(|_| "payload command did not return UTF-8".to_string())?;
    serde_json::from_str::<serde_json::Value>(&payload)
        .map_err(|e| format!("payload command did not return JSON: {e}"))?;
    Ok(payload)
}

fn post(webhook: &str, payload: &str) -> Result<(), String> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    // Keep the URL in a short-lived 0600 curl config so the secret never appears
    // in argv, process listings, plan output, or shell history.
    let config = format!(
        "url = {}\nrequest = POST\nheader = \"Content-Type: application/json\"\nsilent\nshow-error\nfail-with-body\n",
        serde_json::to_string(webhook).map_err(|e| e.to_string())?
    );
    let config_path = std::env::temp_dir().join(format!(
        "deliver-slack-{}-{}.conf",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    if let Err(error) = options
        .open(&config_path)
        .and_then(|mut file| file.write_all(config.as_bytes()))
    {
        let _ = std::fs::remove_file(&config_path);
        return Err(format!("could not prepare Slack request: {error}"));
    }
    let result = (|| {
        let mut child = Command::new("curl")
            .arg("--config")
            .arg(&config_path)
            .args(["--data-binary", "@-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start curl: {e}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| "could not open curl stdin".to_string())?
            .write_all(payload.as_bytes())
            .map_err(|e| format!("could not send Slack payload: {e}"))?;
        let output = child
            .wait_with_output()
            .map_err(|e| format!("could not wait for curl: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    })();
    let _ = std::fs::remove_file(&config_path);
    result
}

pub fn send(
    config: &Config,
    root: &Path,
    version: &DeployVersion,
    event: &str,
    failed: Option<&str>,
) {
    if config.notifications.is_empty() {
        return;
    }
    let resolver = match secrets::resolver(config, root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ! notifications unavailable: {e}");
            return;
        }
    };
    for notice in &config.notifications {
        if !notice.events.iter().any(|configured| configured == event) {
            continue;
        }
        if notice.channel != "slack" {
            eprintln!("  ! unsupported notification channel: {}", notice.channel);
            continue;
        }
        let Some(webhook) = resolver.get(&notice.webhook_secret) else {
            eprintln!(
                "  ! Slack notice skipped: {} is missing",
                notice.webhook_secret
            );
            continue;
        };
        let payload = if event == "succeeded" {
            match &notice.success_payload_command {
                Some(command) => {
                    match command_payload(&expand(command, config, version, event, failed), root) {
                        Ok(payload) => payload,
                        Err(e) => {
                            eprintln!("  ! Slack notice skipped: {e}");
                            continue;
                        }
                    }
                }
                None => generic_payload(config, version, event, failed),
            }
        } else {
            generic_payload(config, version, event, failed)
        };
        match post(&webhook.value, &payload) {
            Ok(()) => println!("  ✓ Slack notice: {event}"),
            Err(e) => eprintln!("  ! Slack notice failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::expand;
    use crate::{config::Config, version};

    #[test]
    fn notification_templates_use_marketing_version() {
        let cfg: Config = serde_yaml::from_str(
            "version: 1\napp: demo\ntargets: {local: {host: localhost, dir: /tmp/demo}}\nservices: {}\n",
        )
        .unwrap();
        let v = version::resolve(std::path::Path::new("."), Some("commit"))
            .with_release("v1.2.3".into(), "tag");
        assert_eq!(
            expand(
                "{app} {version} {release} {event}",
                &cfg,
                &v,
                "succeeded",
                None
            ),
            "demo 1.2.3 v1.2.3 succeeded"
        );
    }
}
