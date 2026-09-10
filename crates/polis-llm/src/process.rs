// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Driving a JSON-lines child process to completion: every stdout line that
//! parses is handed to the caller's fold, stderr is drained concurrently (a
//! full pipe must never block the child), and the exit status is waited for.
//! Shared by the two CLI backends.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

/// What the drive returns beside the caller's folded state.
#[derive(Debug, Default)]
pub struct Drained {
    /// The stderr text, capped at `STDERR_CAP` bytes — enough to name the
    /// failure, not enough to swamp a log.
    pub stderr: String,
    /// Whether at least one stdout line parsed as JSON. `false` after a run
    /// means the binary answered in some other protocol (or not at all).
    pub saw_json: bool,
    pub exit_ok: bool,
    pub error: Option<String>,
}

pub const STDERR_CAP: usize = 2_000;
pub const STDOUT_CAP: usize = 2_000_000;

/// Spawn `cmd` with stdin closed and both pipes captured. A `NotFound` is
/// reported as such so the caller can turn it into "unavailable" rather than
/// "failed".
pub fn spawn(cmd: &mut Command) -> Result<Child, (bool, String)> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    cmd.spawn().map_err(|e| (e.kind() == std::io::ErrorKind::NotFound, e.to_string()))
}

/// Read the child's stdout line by line, folding each JSON line through
/// `on_line`, until it exits.
pub async fn drive(child: Child, on_line: impl FnMut(&Value)) -> Drained {
    drive_bounded(child, 120_000, on_line).await
}

pub async fn drive_bounded(mut child: Child, timeout_ms: u64, mut on_line: impl FnMut(&Value)) -> Drained {
    let mut out = Drained::default();
    let Some(stdout) = child.stdout.take() else {
        return out;
    };
    let stderr = child.stderr.take();
    let mut stderr_bytes = Vec::new();
    let drain_stderr = async {
        if let Some(mut stderr) = stderr {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 { break; }
                let keep = n.min(STDERR_CAP.saturating_sub(stderr_bytes.len()));
                stderr_bytes.extend_from_slice(&buf[..keep]);
            }
        }
    };
    let drain_stdout = async {
        let mut lines = BufReader::new(stdout).take(STDOUT_CAP as u64 + 1);
        let mut total = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = lines.read_until(b'\n', &mut line).await.map_err(|e| e.to_string())?;
            if n == 0 { break; }
            total += n;
            if total > STDOUT_CAP { return Err("model stdout exceeded wire byte limit".to_string()); }
            if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                out.saw_json = true;
                on_line(&value);
            }
        }
        out.exit_ok = child.wait().await.map(|s| s.success()).unwrap_or(false);
        Ok::<_, String>(())
    };
    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms.clamp(1, 600_000)), async {
        tokio::pin!(drain_stderr);
        tokio::pin!(drain_stdout);
        tokio::select! {
            result = &mut drain_stdout => { if result.is_ok() { drain_stderr.await; } result },
            _ = &mut drain_stderr => drain_stdout.await,
        }
    }).await;
    out.error = match result { Ok(Ok(())) => None, Ok(Err(e)) => Some(e), Err(_) => Some("model request deadline exceeded".into()) };
    if out.error.is_some() { let _ = child.kill().await; out.exit_ok = false; }
    out.stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drives_json_lines_and_caps_stderr() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "printf '{\"a\":1}\\nnot json\\n{\"a\":2}\\n'; printf 'warn\\n' 1>&2; exit 0",
        );
        let child = spawn(&mut cmd).unwrap();
        let mut seen = Vec::new();
        let d = drive(child, |v| seen.push(v["a"].as_i64().unwrap())).await;
        assert_eq!(seen, [1, 2], "the non-JSON line is skipped, not fatal");
        assert!(d.saw_json);
        assert!(d.exit_ok);
        assert_eq!(d.stderr.trim(), "warn");
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_as_not_found() {
        let mut cmd = Command::new("/nonexistent/polis-llm-test-binary");
        let err = spawn(&mut cmd).expect_err("no such binary");
        assert!(err.0, "NotFound is distinguished from other spawn failures");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_child_is_cancelled_at_deadline() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let child = spawn(&mut cmd).unwrap();
        let started = std::time::Instant::now();
        let result = drive_bounded(child, 25, |_| {}).await;
        assert!(result.error.as_deref().unwrap().contains("deadline"));
        assert!(!result.exit_ok);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newline_free_output_cannot_allocate_without_bound() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("head -c 2100000 /dev/zero");
        let result = drive_bounded(spawn(&mut cmd).unwrap(), 2000, |_| {}).await;
        assert!(result.error.as_deref().unwrap().contains("byte limit"));
        assert!(!result.exit_ok);
    }
}
