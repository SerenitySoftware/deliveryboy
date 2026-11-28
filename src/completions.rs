//! Shell registration and config names, evaluated afresh on each Tab press.

use clap::ValueEnum;
use clap_complete::env::{Bash, EnvCompleter, Fish, Zsh};
use clap_complete::CompletionCandidate;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;

pub const ENV: &str = "DELIVER_COMPLETE";

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

pub fn generate(shell: Shell, output: &mut dyn Write) -> io::Result<()> {
    let shell: &dyn EnvCompleter = match shell {
        Shell::Bash => &Bash,
        Shell::Zsh => &Zsh,
        Shell::Fish => &Fish,
    };
    shell.write_registration(ENV, "deliver", "deliver", "deliver", output)
}

pub fn services(current: &OsStr) -> Vec<CompletionCandidate> {
    names(current, false)
}

pub fn targets(current: &OsStr) -> Vec<CompletionCandidate> {
    names(current, true)
}

fn names(current: &OsStr, targets: bool) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    // CompleteEnv passes the command line after `--`. Honor a config flag
    // already typed, without asking clap to parse the unfinished command.
    let mut args = std::env::args_os().skip_while(|arg| arg != "--");
    let mut explicit = None;
    while let Some(arg) = args.next() {
        if arg == "--config" || arg == "-c" {
            explicit = args.next().map(PathBuf::from);
        } else if let Some(arg) = arg.to_str() {
            if let Some(path) = arg.strip_prefix("--config=") {
                explicit = Some(PathBuf::from(path));
            } else if let Some(path) = arg.strip_prefix("-c").filter(|p| !p.is_empty()) {
                explicit = Some(PathBuf::from(path));
            }
        }
    }
    let Some(config) = crate::config::resolve(explicit.as_deref())
        .and_then(|path| crate::config::load(&path))
        .ok()
    else {
        return Vec::new();
    };
    let names: Vec<_> = if targets {
        config.targets.into_keys().collect()
    } else {
        config.services.into_keys().collect()
    };
    names
        .into_iter()
        .filter(|name| name.starts_with(prefix))
        .map(CompletionCandidate::new)
        .collect()
}
