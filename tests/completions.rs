use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

const CONFIG: &str = "\
version: 1
app: completion-demo
defaults: {target: preview}
targets:
  preview: {method: local, dir: /tmp/completion-preview}
  test: {method: local, dir: /tmp/completion-test}
services:
  api: {deployer: commands, config: {steps: []}}
  web: {deployer: commands, config: {steps: []}}
";

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "deliver-completions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join(".git")).unwrap();
        Self(path)
    }

    fn config(&self, content: &str) {
        std::fs::write(self.0.join(".deliver.yml"), content).unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn deliver() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deliver"));
    cmd.env_remove("DELIVER_COMPLETE");
    cmd
}

// Exercise each generated script's binary protocol on every CI platform.
fn complete(dir: &Path, words: &[&str]) -> Vec<String> {
    let bash = complete_for("bash", dir, words);
    for shell in ["zsh", "fish"] {
        assert_eq!(complete_for(shell, dir, words), bash, "{shell}");
    }
    bash
}

fn complete_for(shell: &str, dir: &Path, words: &[&str]) -> Vec<String> {
    let output = deliver()
        .current_dir(dir)
        .env("DELIVER_COMPLETE", shell)
        .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
        .env("_CLAP_IFS", "\n")
        .arg("--")
        .args(words)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split([':', '\t']).next().unwrap().to_owned())
        .collect()
}

#[test]
fn prints_registration_scripts_without_a_config() {
    let project = Project::new();
    for (shell, registration) in [
        ("bash", "complete -o"),
        ("zsh", "compdef"),
        ("fish", "--command deliver"),
    ] {
        let output = deliver()
            .current_dir(&project.0)
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let script = String::from_utf8(output.stdout).unwrap();
        assert!(script.contains(registration), "{shell}: {script}");
        assert!(script.contains("DELIVER_COMPLETE"), "{script}");
    }
}

#[cfg(unix)]
#[test]
fn sourced_bash_script_completes_config_names() {
    let project = Project::new();
    project.config(CONFIG);
    let output = deliver().args(["completions", "bash"]).output().unwrap();
    assert!(output.status.success());
    std::fs::write(project.0.join("deliver.bash"), output.stdout).unwrap();
    let binary_dir = Path::new(env!("CARGO_BIN_EXE_deliver")).parent().unwrap();
    let path = std::env::join_paths(
        std::iter::once(binary_dir.to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new("bash")
        .current_dir(&project.0)
        .env("PATH", path)
        .args([
            "-c",
            "source deliver.bash; COMP_WORDS=(deliver plan --service w); COMP_CWORD=3; COMP_TYPE=9; _clap_complete_deliver deliver w --service; printf '%s\\n' \"${COMPREPLY[@]}\"",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "web\n");
}

#[test]
fn service_and_target_names_follow_the_current_project_and_prefix() {
    let project = Project::new();
    project.config(CONFIG);
    for command in [
        "plan",
        "preflight",
        "deploy",
        "verify",
        "status",
        "history",
        "logs",
        "rollback",
    ] {
        assert_eq!(
            complete(&project.0, &["deliver", command, "--service", ""]),
            ["api", "web"]
        );
    }
    assert_eq!(
        complete(&project.0, &["deliver", "plan", "--service", "w"]),
        ["web"]
    );
    assert_eq!(
        complete(&project.0, &["deliver", "--target", ""]),
        ["preview", "test"]
    );
    assert_eq!(
        complete(&project.0, &["deliver", "plan", "--target", "p"]),
        ["preview"]
    );
    assert_eq!(
        complete(&project.0, &["deliver", "plan", "--target=te"]),
        ["--target=test"]
    );
    assert_eq!(
        complete(
            &project.0,
            &["deliver", "plan", "--service", "api", "--service", "w"]
        ),
        ["web"]
    );
    project.config(&CONFIG.replace("web:", "worker:"));
    assert_eq!(
        complete(&project.0, &["deliver", "plan", "--service", "w"]),
        ["worker"]
    );
    let other = Project::new();
    other.config(&CONFIG.replace("web:", "website:"));
    assert_eq!(
        complete(&other.0, &["deliver", "plan", "--service", "w"]),
        ["website"]
    );
}

#[test]
fn config_discovery_and_explicit_paths_match_other_commands() {
    let project = Project::new();
    project.config(CONFIG);
    let subdir = project.0.join("src");
    std::fs::create_dir(&subdir).unwrap();
    assert_eq!(
        complete(&subdir, &["deliver", "plan", "--service", "w"]),
        ["web"]
    );
    std::fs::write(
        project.0.join("other.yml"),
        CONFIG.replace("web:", "worker:"),
    )
    .unwrap();
    for flag in [
        vec!["--config", "other.yml"],
        vec!["-c", "other.yml"],
        vec!["--config=other.yml"],
        vec!["-cother.yml"],
    ] {
        let mut words = vec!["deliver", "plan"];
        words.extend(flag);
        words.extend(["--service", "w"]);
        assert_eq!(complete(&project.0, &words), ["worker"]);
    }
}

#[test]
fn absent_invalid_or_unreadable_config_has_no_names_or_errors() {
    let project = Project::new();
    for config in [
        None,
        Some("services: ["),
        Some("version: 999"),
        Some("{}"),
        Some("version: 1\napp: demo\ntargets: {}\nservices: {}\n"),
    ] {
        if let Some(config) = config {
            project.config(config);
        }
        for flag in ["--service", "--target"] {
            assert!(complete(&project.0, &["deliver", "plan", flag, ""]).is_empty());
        }
    }
    assert!(complete(
        &project.0,
        &[
            "deliver",
            "--config",
            "missing.yml",
            "plan",
            "--service",
            ""
        ]
    )
    .is_empty());
    let commands = complete(&project.0, &["deliver", "pl"]);
    assert_eq!(commands, ["plan"]);
}
