//! Tail what is running on the target — `deliver logs`.
//!
//! The operational loop the CLI already covers is deploy → read back what is
//! live ([`crate::readback`]). The step after that one — *watch it run* — still
//! meant an ssh session and remembering which Compose project this app uses,
//! for each of ~10 apps on the box.
//!
//! Where the log is comes from one of two places, and the precedence is the
//! point: a `logs:` block on the service is the operator's own answer and wins,
//! and otherwise the deployer that started the thing declares it
//! ([`LogSource`], attached to a step exactly as [`crate::deployers::ReleaseState`]
//! is). Only `docker-compose` can honestly declare one — it knows the `-f`
//! files and the `-p` project because it wrote them — so every other service
//! either carries a `logs:` block or is reported as having nowhere to look.
//!
//! Read-only: nothing here mutates the target, and the command runs with this
//! process's stdio attached so `--follow` streams.

use crate::config::{Config, LogsConfig};
use crate::deployers::LogSource;
use crate::plan::ServicePlan;
use crate::remote::stream;

/// One service whose logs can be read, and where from.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub service: String,
    pub target: String,
    pub host: String,
    pub source: LogSource,
    /// True when the source came from the service's `logs:` block rather than
    /// from the deployer, so the command can say which answer it is using.
    pub declared: bool,
}

/// A service in the plan that has no log source at all.
#[derive(Debug, Clone, PartialEq)]
pub struct Unknown {
    pub service: String,
    pub deployer: String,
}

impl From<&LogsConfig> for LogSource {
    /// Total because [`LogsConfig::validate`] has already rejected a block that
    /// sets none of the three; the fallthrough keeps that invariant local
    /// rather than panicking on a config that reached here unvalidated.
    fn from(cfg: &LogsConfig) -> Self {
        if let Some(name) = &cfg.unit {
            return LogSource::Unit { name: name.clone() };
        }
        if let Some(command) = &cfg.command {
            return LogSource::Command {
                command: command.clone(),
            };
        }
        LogSource::Files {
            paths: cfg.files.clone(),
        }
    }
}

/// Split the plan into services that can be tailed and services that cannot.
///
/// Pure: it reads the config and the plan the deployers already produced, so
/// the command tailed is by construction the command the deploy ran.
pub fn collect(config: &Config, plan: &[ServicePlan]) -> (Vec<Request>, Vec<Unknown>) {
    let mut found: Vec<Request> = Vec::new();
    let mut unknown: Vec<Unknown> = Vec::new();
    for sp in plan {
        let service = config.services.get(&sp.service);
        // The operator's own answer beats the deployer's, so a Compose project
        // fronted by nginx can be pointed at the access log instead.
        let declared = service.and_then(|s| s.logs.as_ref()).map(LogSource::from);
        let source = match declared.clone() {
            Some(source) => Some(source),
            None => sp.steps.iter().find_map(|s| s.log_source.clone()),
        };
        match source {
            Some(source) => {
                // A service planned once per host is tailed once per host; a
                // deployer that ever grows two log-bearing steps is not.
                if !found
                    .iter()
                    .any(|r| r.service == sp.service && r.host == sp.host)
                {
                    found.push(Request {
                        service: sp.service.clone(),
                        target: sp.target.clone(),
                        host: sp.host.clone(),
                        source,
                        declared: declared.is_some(),
                    });
                }
            }
            None => {
                if !unknown.iter().any(|u| u.service == sp.service) {
                    unknown.push(Unknown {
                        service: sp.service.clone(),
                        deployer: service
                            .map(|s| s.deployer.clone())
                            .unwrap_or_else(|| "?".to_string()),
                    });
                }
            }
        }
    }
    (found, unknown)
}

/// Read one service's log off its target.
///
/// Returns false when the remote command failed, which the caller turns into a
/// non-zero exit: a missing unit or an unreadable file is a real answer to
/// "show me the logs", not an empty one.
pub fn run(request: &Request, config: &Config, follow: bool, tail: usize) -> bool {
    let Some(target) = config.targets.get(&request.target) else {
        crate::ui::fail(format!(
            "{}: target '{}' is not defined",
            request.service, request.target
        ));
        return false;
    };
    // The deploy writes these logs under the target's sudo, so reading them
    // generally needs the same.
    let sudo = if target.uses_sudo() { "sudo " } else { "" };
    let command = request.source.command(sudo, follow, tail);
    crate::ui::detail(format!("$ {command}"));
    match stream(target, &request.host, &command) {
        Ok(ok) => ok,
        Err(err) => {
            crate::ui::fail(format!(
                "{}: could not run the read — {err}",
                request.service
            ));
            false
        }
    }
}

/// How each service in the plan answers "where are your logs?".
pub fn describe(request: &Request) -> String {
    let where_from = if request.declared {
        "logs:"
    } else {
        "deployer"
    };
    let what = match &request.source {
        LogSource::Compose { compose, .. } => format!("compose ({compose})"),
        LogSource::Unit { name } => format!("unit {name}"),
        LogSource::Files { paths } => paths.join(", "),
        LogSource::Command { .. } => "command".to_string(),
    };
    format!("{} — {what} [{where_from}]", request.service)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compose() -> LogSource {
        LogSource::Compose {
            compose: "docker compose -f docker-compose.yml -p amp".to_string(),
            dir: "/var/universal/amp".to_string(),
        }
    }

    #[test]
    fn compose_tails_the_project_the_deploy_started() {
        let cmd = compose().command("sudo ", false, 50);
        assert_eq!(
            cmd,
            "cd '/var/universal/amp' && sudo docker compose -f docker-compose.yml -p amp \
             logs --no-color --tail 50"
        );
    }

    #[test]
    fn follow_adds_the_flag_each_source_actually_uses() {
        assert!(compose().command("", true, 10).ends_with("--follow"));
        assert!(LogSource::Unit {
            name: "api".to_string()
        }
        .command("", true, 10)
        .ends_with(" -f"));
        // -F, not -f: a rotated log must not strand the tail on a dead inode.
        assert!(LogSource::Files {
            paths: vec!["/var/log/a.log".to_string()]
        }
        .command("", true, 10)
        .contains(" -F "));
    }

    #[test]
    fn a_path_with_a_quote_in_it_cannot_break_out_of_the_command() {
        let cmd = LogSource::Files {
            paths: vec!["/var/log/x'; rm -rf /; '.log".to_string()],
        }
        .command("", false, 5);
        assert!(
            cmd.contains(r"'/var/log/x'\''; rm -rf /; '\''.log'"),
            "{cmd}"
        );
    }

    #[test]
    fn files_are_tailed_after_a_double_dash_so_a_leading_dash_is_a_path() {
        let cmd = LogSource::Files {
            paths: vec!["/var/log/a.log".to_string(), "/var/log/b.log".to_string()],
        }
        .command("sudo ", false, 20);
        assert_eq!(cmd, "sudo tail -n 20 -- '/var/log/a.log' '/var/log/b.log'");
    }

    #[test]
    fn a_command_source_is_verbatim_with_its_placeholders_filled() {
        let source = LogSource::Command {
            command: "kubectl logs deploy/api --tail={tail} {follow}".to_string(),
        };
        assert_eq!(
            source.command("sudo ", false, 100),
            "kubectl logs deploy/api --tail=100"
        );
        assert_eq!(
            source.command("sudo ", true, 100),
            "kubectl logs deploy/api --tail=100 -f"
        );
    }

    #[test]
    fn a_logs_block_picks_exactly_the_source_it_names() {
        let unit = LogsConfig {
            unit: Some("api".to_string()),
            ..Default::default()
        };
        assert_eq!(
            LogSource::from(&unit),
            LogSource::Unit {
                name: "api".to_string()
            }
        );
        let files = LogsConfig {
            files: vec!["/var/log/a.log".to_string()],
            ..Default::default()
        };
        assert_eq!(
            LogSource::from(&files),
            LogSource::Files {
                paths: vec!["/var/log/a.log".to_string()]
            }
        );
    }
}
