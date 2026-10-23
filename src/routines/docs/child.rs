//! Reading an upload in a child process (`iris extract-doc <type>`), so a
//! file that makes a parser run away can't take the daemon with it: the
//! child gets a memory and CPU limit, no core dumps, and is killed at the
//! deadline. The bytes go in on stdin; one JSON object comes back on stdout.

use super::{Budget, DocError, DocType, EXTRACT_TIMEOUT, Extracted, doc_type, extract_with};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The subcommand the daemon runs.
pub const ARG: &str = "extract-doc";
/// Address space the child may use.
const MAX_MEMORY: u64 = 1024 * 1024 * 1024;
/// CPU seconds the child may use (it is killed at the deadline first).
const MAX_CPU_SECS: u64 = 60;
/// Most bytes read from the child's answer.
const MAX_ANSWER: u64 = 16 * 1024 * 1024;
/// Most bytes the child reads from stdin.
const MAX_INPUT: u64 = 1024 * 1024 * 1024;

/// `iris extract-doc <type>`: read stdin, write the result. Returns the
/// exit code.
pub fn main(ext: &str) -> i32 {
    let Some(kind) = doc_type(&format!("upload.{ext}")) else {
        eprintln!("extract-doc: unknown type");
        return 2;
    };
    let mut bytes = Vec::new();
    if std::io::stdin()
        .take(MAX_INPUT)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return 1;
    }
    let answer = match extract_with(
        &format!("upload.{}", kind.ext()),
        &bytes,
        Budget::new(EXTRACT_TIMEOUT),
    ) {
        Ok(e) => json!({"text": e.text, "summary": e.summary}),
        Err(e) => json!({"error": e.code()}),
    };
    let mut out = std::io::stdout().lock();
    if serde_json::to_writer(&mut out, &answer).is_err() || out.flush().is_err() {
        return 1;
    }
    0
}

/// The executable to run the child from: this very binary, even if it was
/// replaced on disk since the daemon started.
fn self_exe() -> std::path::PathBuf {
    let proc = std::path::Path::new("/proc/self/exe");
    if proc.exists() {
        proc.to_path_buf()
    } else {
        std::env::current_exe().unwrap_or_else(|_| "iris".into())
    }
}

/// Run the child for one upload. The bytes come back when it couldn't be
/// started (the caller reads the file in-process instead).
pub(crate) async fn run(
    kind: DocType,
    bytes: Vec<u8>,
) -> Result<Result<Extracted, DocError>, Vec<u8>> {
    let mut cmd = tokio::process::Command::new(self_exe());
    cmd.arg(ARG).arg(kind.ext());
    limit(&mut cmd);
    run_command(cmd, kind, bytes, EXTRACT_TIMEOUT).await
}

/// Memory, CPU, core dump and file-size limits for the child.
fn limit(cmd: &mut tokio::process::Command) {
    // SAFETY: setrlimit is async-signal-safe and touches only the child.
    unsafe {
        cmd.pre_exec(|| {
            let set = |cur: u64| libc::rlimit {
                rlim_cur: cur as libc::rlim_t,
                rlim_max: cur as libc::rlim_t,
            };
            for (res, v) in [
                (libc::RLIMIT_AS, MAX_MEMORY),
                (libc::RLIMIT_CPU, MAX_CPU_SECS),
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_FSIZE, 0),
            ] {
                if libc::setrlimit(res, &set(v)) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Feed `bytes` to the command and read its answer within `timeout`.
pub(crate) async fn run_command(
    mut cmd: tokio::process::Command,
    kind: DocType,
    bytes: Vec<u8>,
    timeout: Duration,
) -> Result<Result<Extracted, DocError>, Vec<u8>> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            crate::warn!("could not start the document reader: {e}");
            return Err(bytes);
        }
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(bytes);
    };
    // The child may stop reading early (a bad file); that's not an error here.
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
    });
    let talk = async {
        let mut answer = Vec::new();
        let _ = stdout.take(MAX_ANSWER).read_to_end(&mut answer).await;
        (answer, child.wait().await)
    };
    let outcome = tokio::time::timeout(timeout, talk).await;
    feed.abort();
    let Ok((answer, status)) = outcome else {
        // Out of time: kill_on_drop ends it.
        return Ok(Err(DocError::TooComplex));
    };
    let noun = kind.noun();
    let status = match status {
        Ok(s) => s,
        Err(_) => return Ok(Err(DocError::Unreadable(noun))),
    };
    if !status.success() {
        // A panic (exit 101) is a file the parser choked on; a signal
        // (out of memory, out of CPU) is a file too big to read.
        return Ok(Err(if status.code() == Some(101) {
            DocError::Unreadable(noun)
        } else {
            DocError::TooComplex
        }));
    }
    let Ok(v) = serde_json::from_slice::<Value>(&answer) else {
        return Ok(Err(DocError::Unreadable(noun)));
    };
    Ok(
        match (
            v["text"].as_str(),
            v["summary"].as_str(),
            v["error"].as_str(),
        ) {
            (Some(text), Some(summary), None) => Ok(Extracted {
                kind,
                text: text.to_string(),
                summary: summary.to_string(),
            }),
            (_, _, Some(code)) => Err(match code {
                "encrypted" => DocError::Encrypted(noun),
                "empty" => DocError::Empty,
                "no_text" => DocError::NoText,
                "too_complex" => DocError::TooComplex,
                _ => DocError::Unreadable(noun),
            }),
            _ => Err(DocError::Unreadable(noun)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }

    async fn run_sh(script: &str, timeout_ms: u64) -> Result<Extracted, DocError> {
        let mut cmd = sh(script);
        limit(&mut cmd);
        run_command(
            cmd,
            DocType::Xlsx,
            b"bytes".to_vec(),
            Duration::from_millis(timeout_ms),
        )
        .await
        .expect("started")
    }

    #[tokio::test]
    async fn answers_come_back_as_results() {
        let ok = run_sh(
            r#"cat >/dev/null; printf '{"text":"| a |","summary":"1 sheet · 1 row"}'"#,
            5_000,
        )
        .await
        .unwrap();
        assert_eq!((ok.kind, ok.text.as_str()), (DocType::Xlsx, "| a |"));
        assert_eq!(ok.summary, "1 sheet · 1 row");
        for (code, want) in [
            ("encrypted", DocError::Encrypted("spreadsheet")),
            ("empty", DocError::Empty),
            ("no_text", DocError::NoText),
            ("too_complex", DocError::TooComplex),
            ("unreadable", DocError::Unreadable("spreadsheet")),
        ] {
            let script = format!(r#"cat >/dev/null; printf '{{"error":"{code}"}}'"#);
            assert_eq!(run_sh(&script, 5_000).await.unwrap_err(), want);
        }
        // Nonsense, a panic, and a crash.
        assert_eq!(
            run_sh("echo nonsense", 5_000).await.unwrap_err(),
            DocError::Unreadable("spreadsheet")
        );
        assert_eq!(
            run_sh("exit 101", 5_000).await.unwrap_err(),
            DocError::Unreadable("spreadsheet")
        );
        assert_eq!(
            run_sh("kill -9 $$", 5_000).await.unwrap_err(),
            DocError::TooComplex
        );
    }

    #[tokio::test]
    async fn a_stuck_reader_is_killed_at_the_deadline() {
        let started = std::time::Instant::now();
        let e = run_sh("exec sleep 30", 300).await.unwrap_err();
        assert_eq!(e, DocError::TooComplex);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_reader_runs_with_limits() {
        // ulimit reports KiB of address space, then blocks of file size.
        let got = run_sh(
            r#"cat >/dev/null; printf '{"text":"x","summary":"%s %s %s"}' "$(ulimit -v)" "$(ulimit -c)" "$(ulimit -f)""#,
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(got.summary, format!("{} 0 0", MAX_MEMORY / 1024));
    }

    #[tokio::test]
    async fn a_missing_reader_falls_back() {
        let cmd = tokio::process::Command::new("/nonexistent/iris");
        assert!(
            run_command(cmd, DocType::Csv, vec![1], Duration::from_secs(1))
                .await
                .is_err_and(|bytes| bytes == [1])
        );
    }
}
