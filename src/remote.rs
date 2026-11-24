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

#[cfg(test)]
mod tests {
    use super::*;

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
