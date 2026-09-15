//! The small shell primitives the read-only target probes share.
//!
//! Two features now read the target without deploying to it — the pre-deploy
//! config diff ([`crate::configdiff`]) and the release read-back
//! ([`crate::readback`]). Both need the same three things: a way to quote a
//! path for `sh`, a marker no file will contain so several answers can travel
//! back over one connection, and one place that decides whether "run this on
//! the target" means `ssh` or a local shell.
//!
//! Keeping them here rather than in either feature is what stops the second
//! reader from growing a second, subtly different idea of how a target is read.

use crate::config::Target;
use std::process::{Command, Stdio};

/// Single-quote a path for `sh`.
pub fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// A marker no config file or release id will contain, used to frame each
/// answer inside the output of one batched read.
pub fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("__DELIVER_CFG_{:016x}_{}__", nanos, std::process::id())
}

/// Run a read-only script on the target and capture its stdout.
///
/// `method: local` reads this machine, matching how [`crate::exec::run_ssh`]
/// treats a local target. stderr is discarded: every caller reports a failed
/// read as its own line rather than letting ssh's noise land in the output.
pub fn capture(target: &Target, host: &str, script: &str) -> std::io::Result<String> {
    let out = if target.is_local() {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stderr(Stdio::null())
            .output()?
    } else {
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(script)
            .stderr(Stdio::null())
            .output()?
    };
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
