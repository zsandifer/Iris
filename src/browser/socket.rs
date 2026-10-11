//! Private Unix socket between `iris mcp --browser` (started by the harness
//! CLI for one turn) and the server that owns the browser. It lives in the
//! run directory (0700), and each request carries the turn's token, so only
//! a tool process of a running turn can drive the browser.
//!
//! Protocol: one JSON line per connection,
//! `{"token": "...", "name": "<tool>", "arguments": {...}}`, answered with
//! one JSON line holding the MCP `tools/call` result.

use crate::config::Paths;
use crate::state::AppState;
use crate::util::Result;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const MAX_REQUEST: usize = 256 * 1024;
/// Upper bound for one call as seen by the tool process (waits for the
/// owner's approval or for take-over to end are shorter).
const CLIENT_TIMEOUT: Duration = Duration::from_secs(180);

/// Socket path: in the run directory, or (when that path is too long for a
/// Unix socket) in a private per-user directory under the runtime dir.
pub fn path(paths: &Paths) -> PathBuf {
    let p = paths.run_dir().join("browser.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    use sha2::Digest;
    let hash: String = sha2::Sha256::digest(paths.data_dir.as_os_str().as_encoded_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute() && d.as_os_str().len() < 60)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    base.join(format!("iris-{uid}-{hash}")).join("browser.sock")
}

/// Accept tool calls until the server stops.
pub async fn serve(state: AppState) -> Result<()> {
    let p = path(&state.paths);
    if let Some(dir) = p.parent() {
        crate::config::ensure_private_dir(dir)?;
        if !owned_by_us(dir) {
            return Err(format!("{} belongs to another user", dir.display()).into());
        }
    }
    let _ = std::fs::remove_file(&p);
    let listener = tokio::net::UnixListener::bind(&p)
        .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                crate::warn!("browser socket: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let st = state.clone();
        tokio::spawn(async move {
            let _ = handle(st, stream).await;
        });
    }
}

async fn handle(state: AppState, stream: tokio::net::UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(r).take(MAX_REQUEST as u64);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let out = super::tools::call(
        &state,
        req["token"].as_str().unwrap_or(""),
        req["name"].as_str().unwrap_or(""),
        &req.get("arguments").cloned().unwrap_or_else(|| json!({})),
    )
    .await;
    let mut body = out.to_mcp().to_string();
    body.push('\n');
    w.write_all(body.as_bytes()).await?;
    w.shutdown().await?;
    Ok(())
}

/// The socket's directory must be ours and private (it may sit under /tmp).
fn owned_by_us(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    std::fs::symlink_metadata(dir)
        .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
}

/// Client side (inside `iris mcp --browser`): forward one call.
pub fn forward(socket: &Path, token: &str, name: &str, arguments: &Value) -> Value {
    let fail = |msg: &str| json!({"content": [{"type": "text", "text": msg}], "isError": true});
    if !socket.parent().is_some_and(owned_by_us) {
        return fail("The browser is not available right now (Iris is not running).");
    }
    let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(s) => s,
        Err(_) => return fail("The browser is not available right now (Iris is not running)."),
    };
    let _ = stream.set_read_timeout(Some(CLIENT_TIMEOUT));
    let mut req = json!({"token": token, "name": name, "arguments": arguments}).to_string();
    req.push('\n');
    if stream.write_all(req.as_bytes()).is_err() {
        return fail("The browser is not available right now.");
    }
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(n) if n > 0 => serde_json::from_str(line.trim())
            .unwrap_or_else(|_| fail("The browser sent an unreadable answer.")),
        _ => fail("The browser did not answer in time."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_data_dirs_get_a_short_socket_path() {
        let short = Paths {
            config_file: "/c".into(),
            data_dir: "/d".into(),
        };
        assert_eq!(path(&short), PathBuf::from("/d/run/browser.sock"));
        let long = Paths {
            config_file: "/c".into(),
            data_dir: PathBuf::from(format!("/{}", "x".repeat(120))),
        };
        let p = path(&long);
        assert!(p.as_os_str().len() < 100 && p.ends_with("browser.sock"));
        assert_eq!(p, path(&long));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_through_the_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::browser::tools::tests::test_state(tmp.path());
        let sock = path(&state.paths);
        tokio::spawn(serve(state.clone()));
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let g = state.browser.begin_turn("u", "c", "m", &[]);
        let token = g.token().to_string();
        let s2 = sock.clone();
        // The tool process side is blocking I/O.
        let ask = move |token: String, name: &'static str| {
            let s = s2.clone();
            tokio::task::spawn_blocking(move || forward(&s, &token, name, &json!({"x": 1})))
        };
        // A live token reaches the tools (an unknown tool name is refused
        // by them, proving the call arrived with its name and token).
        let r = ask(token.clone(), "no_such_tool").await.unwrap();
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "unknown tool no_such_tool");
        // A wrong or expired token is refused before anything runs.
        let r = ask("bogus".into(), "snapshot").await.unwrap();
        assert_eq!(r["content"][0]["text"], crate::browser::TURN_ENDED);
        drop(g);
        let r = ask(token, "snapshot").await.unwrap();
        assert_eq!(r["content"][0]["text"], crate::browser::TURN_ENDED);
    }

    #[test]
    fn forward_reports_a_missing_server() {
        let tmp = tempfile::tempdir().unwrap();
        let v = forward(&tmp.path().join("none.sock"), "t", "snapshot", &json!({}));
        assert_eq!(v["isError"], true);
    }
}
