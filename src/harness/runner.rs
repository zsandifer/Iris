//! Subprocess management for harness CLIs: concurrency cap, stdin payload,
//! line-by-line stdout parsing, stderr tail capture, timeout and
//! cancellation. Each CLI runs in its own process group so that a kill also
//! reaches anything it spawned (MCP servers, shell commands).

use super::{HarnessEvent, Invocation, StreamParser};
use crate::util::Result;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub exit_ok: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Last few KB of stderr; may contain paths, so it is only used for
    /// error classification and never logged verbatim.
    pub stderr_tail: String,
}

/// Global cap on concurrently running CLI subprocesses.
#[derive(Clone)]
pub struct Limiter {
    sem: Arc<Semaphore>,
}

impl Limiter {
    pub fn new(max: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(max.max(1))),
        }
    }

    /// True if a turn started now would have to wait.
    pub fn saturated(&self) -> bool {
        self.sem.available_permits() == 0
    }

    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed")
    }
}

pub struct RunRequest<'a> {
    pub program: &'a Path,
    pub invocation: &'a Invocation,
    pub cwd: &'a Path,
    pub env_remove: &'a [&'a str],
    pub timeout: Duration,
}

fn base_command(program: &Path, cwd: &Path, env_remove: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    for k in env_remove {
        cmd.env_remove(k);
    }
    cmd
}

fn signal_group(pid: Option<u32>, sig: libc::c_int) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok())
        && pid > 0
    {
        // SAFETY: plain syscall; a negative pid targets the process group
        // created for this child by `process_group(0)`.
        unsafe {
            libc::kill(-pid, sig);
        }
    }
}

async fn read_tail(mut r: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > STDERR_TAIL_BYTES * 2 {
                    buf.drain(..buf.len() - STDERR_TAIL_BYTES);
                }
            }
        }
    }
    if buf.len() > STDERR_TAIL_BYTES {
        buf.drain(..buf.len() - STDERR_TAIL_BYTES);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run one turn. Parsed events are sent on `events`; the parser's terminal
/// event is emitted unless the run was cancelled or timed out.
pub async fn run(
    req: RunRequest<'_>,
    mut parser: Box<dyn StreamParser>,
    mut cancel: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) -> Result<RunOutcome> {
    let cwd = req.invocation.cwd.as_deref().unwrap_or(req.cwd);
    let mut cmd = base_command(req.program, cwd, req.env_remove);
    cmd.args(&req.invocation.args);
    for (k, v) in &req.invocation.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let mut stdin = child.stdin.take().expect("piped stdin");
    let payload = req.invocation.stdin.clone();
    tokio::spawn(async move {
        let _ = stdin.write_all(&payload).await;
        let _ = stdin.shutdown().await;
    });
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(read_tail(stderr));
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));

    let mut outcome = RunOutcome::default();
    let deadline = tokio::time::sleep(req.timeout);
    tokio::pin!(deadline);
    let mut line = Vec::with_capacity(4096);
    loop {
        // `read_until` keeps partial data in `line` if another branch wins,
        // so the buffer is only cleared after a complete line.
        let mut limited = (&mut stdout).take(MAX_LINE_BYTES as u64);
        tokio::select! {
            read = limited.read_until(b'\n', &mut line) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim();
                        if !text.is_empty() {
                            for ev in parser.line(text) {
                                let _ = events.send(ev);
                            }
                        }
                        line.clear();
                    }
                }
            }
            _ = &mut deadline => {
                outcome.timed_out = true;
                break;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    outcome.cancelled = true;
                    break;
                }
            }
        }
    }

    if outcome.timed_out || outcome.cancelled {
        signal_group(pid, libc::SIGTERM);
    }
    let status = match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            signal_group(pid, libc::SIGKILL);
            child.wait().await.ok()
        }
    };
    // Reap anything the CLI left behind in its group (e.g. MCP servers).
    signal_group(pid, libc::SIGTERM);
    outcome.exit_ok = status.is_some_and(|s| s.success());
    outcome.stderr_tail = tokio::time::timeout(Duration::from_secs(2), stderr_task)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    if !outcome.timed_out && !outcome.cancelled {
        for ev in parser.finish(outcome.exit_ok, &outcome.stderr_tail) {
            let _ = events.send(ev);
        }
    }
    Ok(outcome)
}

/// Run a short command (`--version`, `auth status`) and capture its output.
pub async fn probe(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env_remove: &[&str],
) -> Result<(bool, String, String)> {
    let mut cmd = base_command(program, cwd, env_remove);
    cmd.args(args).stdin(Stdio::null());
    let child = cmd.spawn()?;
    let pid = child.id();
    match tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await {
        Ok(out) => {
            let out = out?;
            Ok((
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ))
        }
        Err(_) => {
            signal_group(pid, libc::SIGKILL);
            Err("probe timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl StreamParser for Echo {
        fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
            vec![HarnessEvent::TextDelta { text: line.into() }]
        }
        fn finish(&mut self, success: bool, stderr: &str) -> Vec<HarnessEvent> {
            if success {
                vec![HarnessEvent::Done]
            } else {
                vec![HarnessEvent::Error {
                    message: stderr.trim().into(),
                }]
            }
        }
    }

    async fn sh(script: &str, stdin: &[u8], timeout_ms: u64) -> (RunOutcome, Vec<HarnessEvent>) {
        let inv = Invocation {
            args: vec!["-c".into(), script.into()],
            stdin: stdin.to_vec(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ctx, crx) = watch::channel(false);
        let out = run(
            RunRequest {
                program: Path::new("/bin/sh"),
                invocation: &inv,
                cwd: Path::new("/"),
                env_remove: &["IRIS_TEST_SECRET"],
                timeout: Duration::from_millis(timeout_ms),
            },
            Box::new(Echo),
            crx,
            tx,
        )
        .await
        .unwrap();
        let mut evs = Vec::new();
        while let Ok(e) = rx.try_recv() {
            evs.push(e);
        }
        (out, evs)
    }

    #[tokio::test]
    async fn streams_stdin_through_and_finishes() {
        let (out, evs) = sh(
            "cat; echo \"k=${IRIS_TEST_SECRET:-unset}\"",
            b"a\nb\n",
            5000,
        )
        .await;
        assert!(out.exit_ok && !out.timed_out);
        assert_eq!(
            evs,
            vec![
                HarnessEvent::TextDelta { text: "a".into() },
                HarnessEvent::TextDelta { text: "b".into() },
                HarnessEvent::TextDelta {
                    text: "k=unset".into()
                },
                HarnessEvent::Done
            ]
        );
    }

    #[tokio::test]
    async fn captures_stderr_on_failure() {
        let (out, evs) = sh("echo oops >&2; exit 3", b"", 5000).await;
        assert!(!out.exit_ok);
        assert_eq!(
            evs,
            vec![HarnessEvent::Error {
                message: "oops".into()
            }]
        );
    }

    #[tokio::test]
    async fn invocation_env_and_cwd_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let inv = Invocation {
            args: vec!["-c".into(), "echo \"$IRIS_TEST_VAR $(pwd)\"".into()],
            env: vec![("IRIS_TEST_VAR".into(), "hello".into())],
            cwd: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ctx, crx) = watch::channel(false);
        run(
            RunRequest {
                program: Path::new("/bin/sh"),
                invocation: &inv,
                cwd: Path::new("/"),
                env_remove: &[],
                timeout: Duration::from_secs(10),
            },
            Box::new(Echo),
            crx,
            tx,
        )
        .await
        .unwrap();
        let first = rx.recv().await.unwrap();
        let want = format!("hello {}", tmp.path().canonicalize().unwrap().display());
        assert_eq!(first, HarnessEvent::TextDelta { text: want });
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let start = std::time::Instant::now();
        let (out, evs) = sh("sleep 30 & sleep 30; echo never", b"", 300).await;
        assert!(out.timed_out);
        assert!(evs.is_empty());
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn cancel_stops_run() {
        let inv = Invocation {
            args: vec!["-c".into(), "sleep 30".into()],
            ..Default::default()
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let (ctx, crx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            run(
                RunRequest {
                    program: Path::new("/bin/sh"),
                    invocation: &inv,
                    cwd: Path::new("/"),
                    env_remove: &[],
                    timeout: Duration::from_secs(60),
                },
                Box::new(Echo),
                crx,
                tx,
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        ctx.send(true).unwrap();
        let out = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(out.cancelled);
    }

    #[tokio::test]
    async fn limiter_caps_concurrency() {
        let l = Limiter::new(1);
        let p = l.acquire().await;
        assert!(l.saturated());
        drop(p);
        assert!(!l.saturated());
    }
}
