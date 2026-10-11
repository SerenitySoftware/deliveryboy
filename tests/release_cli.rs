use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
fn temp() -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "deliver-release-cli-{}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    dir.canonicalize().unwrap()
}
fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
        .status
        .success());
}
fn config(root: &Path, tool: &str) -> PathBuf {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(include_str!("../examples/thanktank.release.yml")).unwrap();
    value["checkout"] = root.to_string_lossy().as_ref().into();
    // Point every project command at a sentinel. Plans and preflight must not run it.
    fn change(value: &mut serde_yaml::Value, tool: &str) {
        match value {
            serde_yaml::Value::Mapping(m) => {
                for (key, v) in m {
                    if key.as_str() == Some("argv") {
                        *v = serde_yaml::Value::Sequence(vec![tool.into()]);
                    } else {
                        change(v, tool);
                    }
                }
            }
            serde_yaml::Value::Sequence(seq) => {
                for v in seq {
                    change(v, tool);
                }
            }
            _ => {}
        }
    }
    change(&mut value, tool);
    let path = temp().join("release.yml");
    fs::write(&path, serde_yaml::to_string(&value).unwrap()).unwrap();
    path
}
fn deliver(path: &Path, action: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_deliver"))
        .arg("--config")
        .arg(path)
        .args(["--target", "dev", "release"])
        .args(action)
        .output()
        .unwrap()
}
#[test]
fn plan_compiles_the_sample_without_running_commands_or_azure() {
    let root = temp();
    let marker = root.join("must-not-run");
    let cfg = config(&root, marker.to_str().unwrap());
    let result = deliver(&cfg, &["plan"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(plan["network_or_project_commands_executed"], false);
    assert!(plan["required_checks"]["windows"].is_object());
    assert!(!marker.exists());
}
#[test]
fn offline_preflight_fails_missing_tools_without_executing_hooks() {
    let root = temp();
    git(&root, &["init"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@example.invalid"]);
    for audience in ["internal", "external"] {
        let dir = root.join(format!("docs/sites/{audience}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("staticwebapp.config.json"), "{}").unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "fixture"]);
    let marker = root.join("must-not-run");
    let cfg = config(&root, marker.to_str().unwrap());
    let result = deliver(&cfg, &["preflight"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("missing tools"));
    assert!(!marker.exists());
}
#[test]
fn release_requires_explicit_target_and_rejects_unknown_fields() {
    let root = temp();
    let cfg = config(&root, "git");
    let result = Command::new(env!("CARGO_BIN_EXE_deliver"))
        .arg("--config")
        .arg(&cfg)
        .args(["release", "plan"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let mut text = fs::read_to_string(&cfg).unwrap();
    text.push_str("silently_skip_windows: true\n");
    fs::write(&cfg, text).unwrap();
    assert!(!deliver(&cfg, &["plan"]).status.success());
}

#[test]
fn offline_preflight_passes_with_tools_without_invoking_any_of_them() {
    let root = temp();
    git(&root, &["init"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@example.invalid"]);
    for audience in ["internal", "external"] {
        let dir = root.join(format!("docs/sites/{audience}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("staticwebapp.config.json"), "{}").unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "fixture"]);
    let bin = temp();
    for tool in ["task", "az", "docker", "swa"] {
        let name = if cfg!(windows) {
            format!("{tool}.exe")
        } else {
            tool.into()
        };
        fs::write(
            bin.join(name),
            "NOT AN EXECUTABLE; preflight must not run it",
        )
        .unwrap();
    }
    let task = bin.join(if cfg!(windows) { "task.exe" } else { "task" });
    let cfg = config(&root, task.to_str().unwrap());
    let paths = std::iter::once(bin)
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))
        .collect::<Vec<_>>();
    let result = Command::new(env!("CARGO_BIN_EXE_deliver"))
        .arg("--config")
        .arg(&cfg)
        .args(["--target", "dev", "release", "preflight"])
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout)
        .contains("no project commands or infrastructure writes"));
}
