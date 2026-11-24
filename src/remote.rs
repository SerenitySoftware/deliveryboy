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
use std::io::{self, BufRead, BufReader, Read, Write};
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

/// Run a read-only command, scrubbing each output line before printing it.
///
/// The streaming counterpart to [`capture`], for `deliver logs`: the output is
/// the point, it can be unbounded (`--follow`), and unlike the batched probes
/// its stderr is kept — `journalctl`'s "no such unit" and `tail`'s "no such
/// file" are the answer when the log is not where the config says.
pub fn stream(target: &Target, host: &str, script: &str) -> std::io::Result<bool> {
    let mut command = if target.is_local() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    } else {
        let mut command = Command::new("ssh");
        command
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .args(target.ssh_args())
            .arg(format!("{}@{host}", target.ssh.user))
            .arg(script);
        command
    };
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain both pipes at once: a full stderr pipe must not block stdout (or
    // vice versa). Preserve the channels, and stop the child on a reader or
    // terminal error rather than leaving a follow command running unseen.
    let copied = std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let err_tx = tx.clone();
        scope.spawn(move || {
            let _ = tx.send(scrub_stream(stdout, io::stdout()));
        });
        scope.spawn(move || {
            let _ = err_tx.send(scrub_stream(stderr, io::stderr()));
        });
        let mut result = Ok(());
        for copied in rx {
            if copied.is_err() {
                let _ = child.kill();
                if result.is_ok() {
                    result = copied;
                }
            }
        }
        result
    });
    let status = child.wait()?;
    copied?;
    Ok(status.success())
}

fn scrub_stream(input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    // A pipe read can split a secret in two. Buffer through the newline (or
    // EOF), then scrub; lossy decoding also accepts non-UTF-8 application logs.
    while reader.read_until(b'\n', &mut line)? != 0 {
        let text = crate::secrets::redact::scrub(&String::from_utf8_lossy(&line));
        output.write_all(text.as_bytes())?;
        output.flush()?;
        line.clear();
    }
    Ok(())
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
    // Keep a local shell's children in the same cancellable group. Killing
    // only sh can leave tail holding a silent stderr pipe open forever.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
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
                #[cfg(unix)]
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                #[cfg(windows)]
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_closed_consumer_cancels_children_holding_silent_pipes() {
        let target = serde_yaml::from_str("method: local\nhost: localhost\ndir: .\n").unwrap();
        let start = std::time::Instant::now();
        let result = super::stream_lines(
            &target,
            "localhost",
            "printf 'first\\n'; sleep 5",
            |_, _| Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        );
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert!(start.elapsed() < std::time::Duration::from_secs(4));
    }

    #[test]
    fn scrubs_secrets_split_across_reads_and_keeps_line_endings() {
        crate::secrets::redact::record("STREAM_TOKEN", "synthetic-stream-token-5829");
        // Force one-byte reads, including through the middle of the value.
        struct Bytes(std::io::Cursor<Vec<u8>>);
        impl Read for Bytes {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(&mut buf[..1])
            }
        }
        let input = b"unchanged\r\nsynthetic-stream-token-5829\n\xffsynthetic-stream-token-5829";
        let mut output = Vec::new();
        scrub_stream(Bytes(io::Cursor::new(input.to_vec())), &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "unchanged\r\n[redacted:STREAM_TOKEN]\n\u{fffd}[redacted:STREAM_TOKEN]"
        );
    }
}
