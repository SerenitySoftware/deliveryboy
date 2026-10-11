//! Direct commands with streaming redaction. Azure output stays private by default.
use super::{
    config::{check_platform, Command as Spec},
    evidence::Check,
};
use anyhow::{ensure, Context, Result};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

#[derive(Clone, Default)]
pub struct Runner {
    pub secrets: Vec<String>,
    pub env: BTreeMap<String, String>,
}
impl Runner {
    pub fn new(names: &[String]) -> Result<Self> {
        let mut runner = Self::default();
        for name in names {
            let value = std::env::var(name).with_context(|| {
                format!("required secret environment variable {name} is missing")
            })?;
            ensure!(!value.is_empty(), "secret environment variable is empty");
            runner.secrets.push(value);
        }
        runner.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Ok(runner)
    }
    pub fn command(&self, argv: &[String], root: &Path) -> Command {
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(root)
            .envs(&self.env)
            .stdin(Stdio::null());
        // Never let ambient pipeline variables forge a hosted producer.
        for key in [
            "TF_BUILD",
            "BUILD_BUILDID",
            "SOURCE_CI_RUN_ID",
            "SYSTEM_JOBID",
            "SYSTEM_ACCESSTOKEN",
            "AZURE_STORAGE_KEY",
            "AZURE_STORAGE_SAS_TOKEN",
            "AZURE_STORAGE_CONNECTION_STRING",
        ] {
            command.env_remove(key);
        }
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("DELIVER_") {
                command.env_remove(key);
            }
        }
        command.envs(&self.env);
        command
    }
    pub fn capture(&self, argv: &[String], root: &Path) -> Result<Vec<u8>> {
        let output = self
            .command(argv, root)
            .output()
            .with_context(|| format!("could not start {}", argv[0]))?;
        ensure!(
            output.status.success(),
            "{} failed (exit {:?}); raw output withheld because it may contain secrets",
            argv[0],
            output.status.code()
        );
        Ok(output.stdout)
    }
    pub fn run(&self, name: &str, spec: &Spec, root: &Path) -> Result<Check> {
        check_platform(spec)?;
        // Values are allowed only in the environment, never the command line or receipt.
        ensure!(
            !self
                .secrets
                .iter()
                .any(|s| spec.argv.iter().any(|a| a.contains(s))),
            "command contains a secret value; use the environment"
        );
        eprintln!("running {name}");
        let mut child = self
            .command(&spec.argv, root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not start check {name}"))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let left = self.secrets.clone();
        let right = left.clone();
        let a = std::thread::spawn(move || stream(stdout, &left));
        let b = std::thread::spawn(move || stream(stderr, &right));
        let status = child.wait()?;
        a.join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
        b.join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
        Ok(Check {
            name: name.into(),
            command: spec.clone(),
            platform: std::env::consts::OS.into(),
            status: if status.success() { "passed" } else { "failed" }.into(),
            exit_code: status.code(),
            at: super::evidence::now(),
        })
    }
}
fn stream(input: impl std::io::Read, secrets: &[String]) -> Result<()> {
    // Buffer by line so a value split across pipe reads cannot escape the scrubber.
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        let mut text = String::from_utf8_lossy(&line).into_owned();
        for secret in secrets {
            text = text.replace(secret, "[redacted]");
        }
        std::io::stderr().lock().write_all(text.as_bytes())?;
        line.clear();
    }
    Ok(())
}
