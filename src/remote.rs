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

/// Run a read-only command on the target with this process's stdio attached.
///
/// The streaming counterpart to [`capture`], for `deliver logs`: the output is
/// the point, it can be unbounded (`--follow`), and unlike the batched probes
/// its stderr is kept — `journalctl`'s "no such unit" and `tail`'s "no such
/// file" are the answer when the log is not where the config says.
pub fn stream(target: &Target, host: &str, script: &str) -> std::io::Result<bool> {
    let status = if target.is_local() {
        Command::new("sh").arg("-c").arg(script).status()?
    } else {
        Command::new("ssh")
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(script)
            .status()?
    };
    Ok(status.success())
}

/// Read both output pipes concurrently, forwarding complete lines as they
/// arrive. A bounded queue keeps a slow consumer from buffering a whole follow
/// session. Each pipe preserves its own order; there is no order across pipes.
pub fn stream_lines(
    target: &Target,
    host: &str,
    script: &str,
    mut emit: impl FnMut(&str, &str) -> std::io::Result<()>,
) -> std::io::Result<bool> {
    use std::io::{BufRead, BufReader, Read};
    use std::sync::mpsc::sync_channel;

    let mut command = if target.is_local() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    } else {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(script);
        cmd
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = sync_channel(32);
    let result = std::thread::scope(|scope| {
        for (stream, pipe) in [
            ("stdout", Box::new(stdout) as Box<dyn Read + Send>),
            ("stderr", Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            scope.spawn(move || {
                let mut reader = BufReader::new(pipe);
                loop {
                    let mut bytes = Vec::new();
                    match reader.read_until(b'\n', &mut bytes) {
                        Ok(0) => break,
                        Ok(_) => {
                            if bytes.last() == Some(&b'\n') {
                                bytes.pop();
                            }
                            if tx.send(Ok((stream, bytes))).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                            break;
                        }
                    }
                }
            });
        }
        drop(tx);
        for line in &rx {
            let result =
                line.and_then(|(stream, bytes)| emit(stream, &String::from_utf8_lossy(&bytes)));
            if let Err(err) = result {
                // Reap the process even when the consumer closes its pipe.
                let _ = child.kill();
                drop(rx);
                return Err(err);
            }
        }
        Ok(())
    });
    let status = child.wait()?;
    result?;
    Ok(status.success())
}
