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

pub(crate) fn base_command(program: &Path, cwd: &Path, env_remove: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    // Some CLIs take their project root from PWD rather than the real
    // working directory; never let them inherit the daemon's.
    cmd.current_dir(cwd)
        .env("PWD", cwd)
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

/// Whether any process is left in the group (zombies awaiting their
/// reaper still count, so callers bound how long they wait).
fn group_alive(pid: Option<u32>) -> bool {
    match pid.and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: signal 0 only checks for existence and permission.
        Some(pid) if pid > 0 => unsafe { libc::kill(-pid, 0) == 0 },
        _ => false,
    }
}

/// Stop everything left in the CLI's process group (MCP servers, shell
/// commands): SIGTERM, a short grace period, then SIGKILL for anything
/// that ignored it.
async fn reap_group(pid: Option<u32>) {
    if !group_alive(pid) {
        return;
    }
    signal_group(pid, libc::SIGTERM);
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !group_alive(pid) {
            return;
        }
    }
    signal_group(pid, libc::SIGKILL);
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
    let mut cmd = base_command(
        req.invocation.program.as_deref().unwrap_or(req.program),
        cwd,
        req.env_remove,
    );
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
    let mut stderr_task = tokio::spawn(read_tail(stderr));
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
    // Stop anything the CLI left behind in its group (e.g. MCP servers),
    // even if it ignores SIGTERM, before this run's slot is released.
    reap_group(pid).await;
    outcome.exit_ok = status.is_some_and(|s| s.success());
    outcome.stderr_tail = match tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await
    {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => {
            // Something outside the group still holds stderr open.
            stderr_task.abort();
            String::new()
        }
    };
    if !outcome.timed_out && !outcome.cancelled {
        for ev in parser.finish(outcome.exit_ok, &outcome.stderr_tail) {
            let _ = events.send(ev);
        }
    }
    Ok(outcome)
}

/// Run a short command (`--version`, `auth status`) and capture its output.
/// `env_add` carries the owner's engine API keys, if any.
pub async fn probe(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env_remove: &[&str],
    env_add: &[(String, String)],
) -> Result<(bool, String, String)> {
    let inv = Invocation {
        args: args.to_vec(),
        env: env_add.to_vec(),
        ..Default::default()
    };
    probe_invocation(program, &inv, cwd, env_remove).await
}

pub async fn probe_invocation(
    program: &Path,
    inv: &Invocation,
    cwd: &Path,
    env_remove: &[&str],
) -> Result<(bool, String, String)> {
    let mut cmd = base_command(
        inv.program.as_deref().unwrap_or(program),
        inv.cwd.as_deref().unwrap_or(cwd),
        env_remove,
    );
    cmd.args(&inv.args).envs(inv.env.iter().cloned());
    let mut child = cmd.spawn()?;
    let mut input = child.stdin.take().expect("piped stdin");
    let payload = inv.stdin.clone();
    tokio::spawn(async move {
        let _ = input.write_all(&payload).await;
    });
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
        let (out, evs) = sh("cat; echo \"done pwd=$PWD\"", b"a\nb\n", 5000).await;
        assert!(out.exit_ok && !out.timed_out);
        assert_eq!(
            evs,
            vec![
                HarnessEvent::TextDelta { text: "a".into() },
                HarnessEvent::TextDelta { text: "b".into() },
                // PWD is the run's directory, not the daemon's.
                HarnessEvent::TextDelta {
                    text: "done pwd=/".into()
                },
                HarnessEvent::Done
            ]
        );
    }

    #[tokio::test]
    async fn strips_inherited_variables() {
        // A variable this process really has (stand-in for an inherited API
        // key such as ANTHROPIC_API_KEY); setting one here would race other
        // tests.
        assert!(std::env::var_os("HOME").is_some(), "test needs HOME set");
        let inv = Invocation {
            args: vec!["-c".into(), "echo \"k=${HOME:-unset}\"".into()],
            ..Default::default()
        };
        for (strip, want_unset) in [(&["HOME"][..], true), (&[][..], false)] {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (_ctx, crx) = watch::channel(false);
            run(
                RunRequest {
                    program: Path::new("/bin/sh"),
                    invocation: &inv,
                    cwd: Path::new("/"),
                    env_remove: strip,
                    timeout: Duration::from_secs(10),
                },
                Box::new(Echo),
                crx,
                tx,
            )
            .await
            .unwrap();
            let first = rx.recv().await.unwrap();
            assert_eq!(
                first
                    == (HarnessEvent::TextDelta {
                        text: "k=unset".into()
                    }),
                want_unset,
                "{first:?}"
            );
        }
    }

    /// Whether `pid` is a live (non-zombie) process.
    fn running(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z')),
            Err(_) => false,
        }
    }

    async fn wait_gone(pid: i32) -> bool {
        for _ in 0..50 {
            if !running(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// A child that ignores SIGTERM and writes its pid only once it does.
    fn stubborn_child(pidfile: &Path) -> String {
        format!(
            "sh -c 'trap \"\" TERM; echo $$ > {}.tmp; mv {0}.tmp {0}; exec sleep 30'",
            pidfile.display()
        )
    }

    fn wait_for(pidfile: &Path) -> String {
        format!("while [ ! -s {} ]; do sleep 0.01; done", pidfile.display())
    }

    fn read_pid(path: &Path) -> i32 {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn leftover_processes_that_ignore_sigterm_are_killed() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        // The CLI exits right away; its child ignores SIGTERM.
        let script = format!(
            "{} >/dev/null 2>&1 </dev/null & {}; echo hi",
            stubborn_child(&pidfile),
            wait_for(&pidfile)
        );
        let start = std::time::Instant::now();
        let (out, evs) = sh(&script, b"", 10_000).await;
        assert!(out.exit_ok && !out.timed_out);
        assert_eq!(evs.last(), Some(&HarnessEvent::Done));
        let pid = read_pid(&pidfile);
        assert!(wait_gone(pid).await, "descendant {pid} survived the run");
        assert!(start.elapsed() < Duration::from_secs(8));
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
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let start = std::time::Instant::now();
        let script = format!(
            "{} & {}; sleep 30; echo never",
            stubborn_child(&pidfile),
            wait_for(&pidfile)
        );
        let (out, evs) = sh(&script, b"", 300).await;
        assert!(out.timed_out);
        assert!(evs.is_empty());
        assert!(start.elapsed() < Duration::from_secs(10));
        let pid = read_pid(&pidfile);
        assert!(
            wait_gone(pid).await,
            "descendant {pid} survived the timeout"
        );
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
