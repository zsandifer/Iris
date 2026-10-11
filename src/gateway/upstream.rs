//! MCP client side of the gateway: talks to the real connector servers,
//! which run on Iris's side (stdio programs Iris starts with their secrets
//! in the environment, or remote Streamable HTTP servers Iris calls with
//! their headers or OAuth token). The agent never sees those secrets.
//!
//! This is plumbing only: Iris forwards `tools/list` and `tools/call`; it
//! never decides what to call.

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio as PStdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::oneshot;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Largest message accepted from a connector (tool results can be big).
const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOOL_PAGES: usize = 20;

/// Environment variables never handed to connector programs.
const STRIPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "IRIS_GATEWAY_TOKEN",
];

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamError {
    /// The server refused our credentials (HTTP 401/403).
    Auth(String),
    /// A JSON-RPC error from the server.
    Rpc {
        code: i64,
        message: String,
    },
    /// The program died, the connection failed, or the reply was garbage.
    Transport(String),
    Timeout,
    /// The HTTP session is gone (404 with a session id); start a new one.
    SessionExpired,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Auth(m) => write!(f, "sign-in refused: {m}"),
            UpstreamError::Rpc { code, message } => write!(f, "error {code}: {message}"),
            UpstreamError::Transport(m) => write!(f, "{m}"),
            UpstreamError::Timeout => write!(f, "no answer in time"),
            UpstreamError::SessionExpired => write!(f, "session expired"),
        }
    }
}

/// How to reach one connector.
#[derive(Debug, Clone)]
pub enum Launch {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: PathBuf,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    /// A program Iris starts that serves MCP over HTTP on a loopback port.
    /// `{port}` in the arguments, environment values, and URL is replaced
    /// with a free port.
    Serve {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: PathBuf,
        url: String,
        headers: BTreeMap<String, String>,
    },
}

pub struct Upstream {
    inner: Inner,
    /// The program behind a `Serve` connection (stopped on close/drop).
    server: Option<ServerProc>,
}

/// A program serving HTTP for one connector.
struct ServerProc {
    pid: Option<u32>,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
}

impl ServerProc {
    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(pid) = self.pid.and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: plain syscall on the process group we created.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        self.kill();
    }
}

fn free_port() -> Result<u16, UpstreamError> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| UpstreamError::Transport(format!("no free port: {e}")))
}

fn base_command(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &std::path::Path,
) -> Result<Command, UpstreamError> {
    let program = crate::harness::resolve_command(command)
        .ok_or_else(|| UpstreamError::Transport(format!("{command} is not installed")))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .process_group(0);
    for k in STRIPPED_ENV {
        cmd.env_remove(k);
    }
    cmd.envs(env);
    Ok(cmd)
}

/// Keep the last ~2 KB a program wrote to stderr.
fn keep_tail(pipe: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::new(pipe);
        let mut line = String::new();
        while r.read_line(&mut line).await.unwrap_or(0) > 0 {
            let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
            t.push_str(&line);
            if t.len() > 2000 {
                let cut = t.len() - 2000;
                let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                t.drain(..cut);
            }
            line.clear();
        }
    });
}

/// Start a `Serve` program and wait until its port accepts connections.
async fn serve(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &std::path::Path,
    url: &str,
    timeout: Duration,
) -> Result<(ServerProc, String), UpstreamError> {
    let port = free_port()?.to_string();
    let sub = |s: &str| s.replace("{port}", &port);
    let args: Vec<String> = args.iter().map(|a| sub(a)).collect();
    let env: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.clone(), sub(v))).collect();
    let mut cmd = base_command(command, &args, &env, cwd)?;
    cmd.stdin(PStdio::null())
        .stdout(PStdio::null())
        .stderr(PStdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| UpstreamError::Transport(format!("could not start {command}: {e}")))?;
    let stderr = Arc::new(Mutex::new(String::new()));
    keep_tail(child.stderr.take().expect("piped"), stderr.clone());
    let alive = Arc::new(AtomicBool::new(true));
    let proc = ServerProc {
        pid: child.id(),
        alive: alive.clone(),
        stderr: stderr.clone(),
    };
    tokio::spawn(async move {
        let _ = child.wait().await;
        alive.store(false, Ordering::SeqCst);
    });
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            break;
        }
        if !proc.alive.load(Ordering::SeqCst) {
            let tail = stderr.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let last = tail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            return Err(UpstreamError::Transport(format!(
                "{command} stopped while starting{}",
                if last.is_empty() {
                    String::new()
                } else {
                    format!(": {}", crate::util::truncate_chars(last.trim(), 200))
                }
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(UpstreamError::Timeout);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok((proc, sub(url)))
}

enum Inner {
    Stdio(StdioConn),
    Http(HttpConn),
}

impl Upstream {
    /// Connect and complete the MCP handshake.
    pub async fn start(launch: Launch, timeout: Duration) -> Result<Self, UpstreamError> {
        let (inner, server) = match launch {
            Launch::Stdio {
                command,
                args,
                env,
                cwd,
            } => (
                Inner::Stdio(StdioConn::spawn(&command, &args, &env, &cwd)?),
                None,
            ),
            Launch::Http { url, headers } => (Inner::Http(HttpConn::new(url, headers)), None),
            Launch::Serve {
                command,
                args,
                env,
                cwd,
                url,
                headers,
            } => {
                let (proc, url) = serve(&command, &args, &env, &cwd, &url, timeout).await?;
                (Inner::Http(HttpConn::new(url, headers)), Some(proc))
            }
        };
        let up = Self { inner, server };
        let init = up
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "iris", "version": env!("CARGO_PKG_VERSION")},
                }),
                timeout,
            )
            .await?;
        if let Inner::Http(h) = &up.inner {
            let v = init["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            *h.protocol.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
        }
        up.notify("notifications/initialized").await;
        Ok(up)
    }

    pub fn alive(&self) -> bool {
        let server_ok = self
            .server
            .as_ref()
            .is_none_or(|p| p.alive.load(Ordering::SeqCst));
        server_ok
            && match &self.inner {
                Inner::Stdio(s) => s.alive.load(Ordering::SeqCst),
                Inner::Http(_) => true,
            }
    }

    /// Replace one HTTP header (a refreshed OAuth token).
    pub fn set_header(&self, name: &str, value: &str) {
        if let Inner::Http(h) = &self.inner {
            h.headers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(name.to_string(), value.to_string());
        }
    }

    /// The server said its tool list changed since we last asked.
    pub fn tools_changed(&self) -> bool {
        match &self.inner {
            Inner::Stdio(s) => s.tools_changed.swap(false, Ordering::SeqCst),
            Inner::Http(_) => false,
        }
    }

    /// Last lines the program wrote to stderr (owner diagnostics only).
    pub fn stderr_tail(&self) -> String {
        match (&self.inner, &self.server) {
            (Inner::Stdio(s), _) => s.stderr.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            (Inner::Http(_), Some(p)) => p.stderr.lock().unwrap_or_else(|p| p.into_inner()).clone(),
            (Inner::Http(_), None) => String::new(),
        }
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        match &self.inner {
            Inner::Stdio(s) => s.request(method, params, timeout).await,
            Inner::Http(h) => h.request(method, params, timeout).await,
        }
    }

    async fn notify(&self, method: &str) {
        match &self.inner {
            Inner::Stdio(s) => {
                let _ = s.write(&json!({"jsonrpc": "2.0", "method": method})).await;
            }
            Inner::Http(h) => {
                let _ = h.notify(method).await;
            }
        }
    }

    /// All tools, following pagination.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<Value>, UpstreamError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params, timeout).await?;
            if let Some(list) = r["tools"].as_array() {
                tools.extend(list.iter().cloned());
            }
            cursor = r["nextCursor"].as_str().map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: &Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            timeout,
        )
        .await
    }

    /// Stop the program or end the HTTP session.
    pub async fn close(&self) {
        match &self.inner {
            Inner::Stdio(s) => s.kill(),
            Inner::Http(h) => h.close().await,
        }
        if let Some(p) = &self.server {
            p.kill();
        }
    }
}

fn rpc_result(msg: Value) -> Result<Value, UpstreamError> {
    if let Some(err) = msg.get("error") {
        return Err(UpstreamError::Rpc {
            code: err["code"].as_i64().unwrap_or(-32000),
            message: crate::util::truncate_chars(err["message"].as_str().unwrap_or("error"), 500),
        });
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

// -------------------------------------------------------------------- stdio

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

struct StdioConn {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
    tools_changed: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
    pid: Option<u32>,
}

impl StdioConn {
    fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self, UpstreamError> {
        let mut cmd = base_command(command, args, env, cwd)?;
        cmd.stdin(PStdio::piped())
            .stdout(PStdio::piped())
            .stderr(PStdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| UpstreamError::Transport(format!("could not start {command}: {e}")))?;
        let pid = child.id();
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        keep_tail(child.stderr.take().expect("piped"), stderr.clone());
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let tools_changed = Arc::new(AtomicBool::new(false));
        let (p, a, tc, w) = (
            pending.clone(),
            alive.clone(),
            tools_changed.clone(),
            stdin.clone(),
        );
        tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                let mut limited = (&mut r).take(MAX_MESSAGE_BYTES as u64);
                match limited.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                let id = msg.get("id").cloned();
                match (msg.get("method").and_then(Value::as_str), id) {
                    (Some(method), Some(id)) => {
                        // A request from the server: answer the harmless
                        // ones, refuse the rest (no sampling, no prompts).
                        let reply = match method {
                            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                            "roots/list" => {
                                json!({"jsonrpc": "2.0", "id": id, "result": {"roots": []}})
                            }
                            _ => json!({"jsonrpc": "2.0", "id": id,
                                        "error": {"code": -32601, "message": "not supported"}}),
                        };
                        let mut line = reply.to_string().into_bytes();
                        line.push(b'\n');
                        let _ = w.lock().await.write_all(&line).await;
                    }
                    (Some(method), None) => {
                        if method == "notifications/tools/list_changed" {
                            tc.store(true, Ordering::SeqCst);
                        }
                    }
                    (None, Some(id)) => {
                        if let Some(id) = id.as_i64()
                            && let Some(tx) =
                                p.lock().unwrap_or_else(|x| x.into_inner()).remove(&id)
                        {
                            let _ = tx.send(msg);
                        }
                    }
                    (None, None) => {}
                }
            }
            a.store(false, Ordering::SeqCst);
            p.lock().unwrap_or_else(|x| x.into_inner()).clear();
            let _ = child.wait().await;
        });
        Ok(Self {
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            alive,
            tools_changed,
            stderr,
            pid,
        })
    }

    async fn write(&self, msg: &Value) -> Result<(), UpstreamError> {
        let mut line = msg.to_string().into_bytes();
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line)
            .await
            .map_err(|_| UpstreamError::Transport("the connector program stopped".into()))?;
        w.flush()
            .await
            .map_err(|_| UpstreamError::Transport("the connector program stopped".into()))
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(UpstreamError::Transport(
                "the connector program stopped".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(msg)) => rpc_result(msg),
            Ok(Err(_)) => Err(UpstreamError::Transport(
                "the connector program stopped".into(),
            )),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(UpstreamError::Timeout)
            }
        }
    }

    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(pid) = self.pid.and_then(|p| i32::try_from(p).ok()) {
            // SAFETY: plain syscall on the process group we created.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}

impl Drop for StdioConn {
    fn drop(&mut self) {
        self.kill();
    }
}

// --------------------------------------------------------------------- http

struct HttpConn {
    url: String,
    headers: Mutex<BTreeMap<String, String>>,
    session: Arc<Mutex<Option<String>>>,
    protocol: Arc<Mutex<Option<String>>>,
    next_id: AtomicI64,
}

/// One HTTP exchange, run on the blocking pool.
struct Exchange {
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    /// Wait for the response with this id (None: notification).
    want_id: Option<i64>,
}

enum HttpReply {
    Message(Value),
    Accepted,
    Status(u16, String),
}

fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .user_agent(format!("iris/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn exchange(x: Exchange) -> Result<(HttpReply, Option<String>), UpstreamError> {
    let agent = http_agent(x.timeout);
    let mut req = agent.post(&x.url);
    for (k, v) in &x.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send(&x.body[..]).map_err(|e| match e {
        ureq::Error::Timeout(_) => UpstreamError::Timeout,
        e => UpstreamError::Transport(format!("could not reach the connector: {e}")),
    })?;
    let status = resp.status().as_u16();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let www_auth = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if status == 401 || status == 403 {
        return Err(UpstreamError::Auth(if www_auth.is_empty() {
            format!("HTTP {status}")
        } else {
            crate::util::truncate_chars(&www_auth, 300)
        }));
    }
    if status == 202 || (x.want_id.is_none() && (200..300).contains(&status)) {
        return Ok((HttpReply::Accepted, session));
    }
    let mut body = resp.into_body();
    let reader = body.as_reader().take(MAX_MESSAGE_BYTES as u64);
    if !(200..300).contains(&status) {
        let mut text = String::new();
        let _ = reader.take(2000).read_to_string(&mut text);
        return Ok((
            HttpReply::Status(status, crate::util::truncate_chars(text.trim(), 300)),
            session,
        ));
    }
    if ctype.starts_with("text/event-stream") {
        let msg = read_sse(BufReader::new(reader), x.want_id)?;
        return Ok((HttpReply::Message(msg), session));
    }
    let mut buf = Vec::new();
    let mut reader = reader;
    reader
        .read_to_end(&mut buf)
        .map_err(|e| UpstreamError::Transport(format!("bad reply: {e}")))?;
    let v: Value = serde_json::from_slice(&buf)
        .map_err(|_| UpstreamError::Transport("the connector sent an unreadable reply".into()))?;
    // A batch reply: pick ours.
    let msg = match v {
        Value::Array(items) => items
            .into_iter()
            .find(|m| m["id"].as_i64() == x.want_id)
            .unwrap_or(Value::Null),
        v => v,
    };
    Ok((HttpReply::Message(msg), session))
}

/// Read Server-Sent Events until the JSON-RPC response with `want_id`.
pub fn read_sse(r: impl BufRead, want_id: Option<i64>) -> Result<Value, UpstreamError> {
    let mut data = String::new();
    for line in r.lines() {
        let line = line.map_err(|e| UpstreamError::Transport(format!("stream ended: {e}")))?;
        if line.is_empty() {
            if !data.is_empty() {
                if let Ok(msg) = serde_json::from_str::<Value>(&data)
                    && msg.get("method").is_none()
                    && msg["id"].as_i64() == want_id
                {
                    return Ok(msg);
                }
                data.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !data.is_empty()
        && let Ok(msg) = serde_json::from_str::<Value>(&data)
        && msg["id"].as_i64() == want_id
    {
        return Ok(msg);
    }
    Err(UpstreamError::Transport(
        "the connector closed the stream without answering".into(),
    ))
}

impl HttpConn {
    fn new(url: String, headers: BTreeMap<String, String>) -> Self {
        Self {
            url,
            headers: Mutex::new(headers),
            session: Arc::default(),
            protocol: Arc::default(),
            next_id: AtomicI64::new(1),
        }
    }

    fn header_list(&self) -> Vec<(String, String)> {
        let mut h: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/json".into()),
            (
                "Accept".into(),
                "application/json, text/event-stream".into(),
            ),
        ];
        if let Some(s) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            h.push(("Mcp-Session-Id".into(), s));
        }
        if let Some(v) = self
            .protocol
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            h.push(("MCP-Protocol-Version".into(), v));
        }
        for (k, v) in self
            .headers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
        {
            h.push((k.clone(), v.clone()));
        }
        h
    }

    async fn send(
        &self,
        body: Value,
        want_id: Option<i64>,
        timeout: Duration,
    ) -> Result<HttpReply, UpstreamError> {
        let had_session = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some();
        let x = Exchange {
            url: self.url.clone(),
            headers: self.header_list(),
            body: body.to_string().into_bytes(),
            timeout,
            want_id,
        };
        let (reply, session) = tokio::task::spawn_blocking(move || exchange(x))
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string()))??;
        if let Some(s) = session {
            *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
        }
        if let HttpReply::Status(404, _) = reply
            && had_session
        {
            return Err(UpstreamError::SessionExpired);
        }
        Ok(reply)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.send(body, Some(id), timeout).await? {
            HttpReply::Message(m) => rpc_result(m),
            HttpReply::Accepted => Err(UpstreamError::Transport(
                "the connector accepted the request but sent no answer".into(),
            )),
            HttpReply::Status(code, text) => Err(UpstreamError::Transport(format!(
                "the connector answered HTTP {code}{}",
                if text.is_empty() {
                    String::new()
                } else {
                    format!(": {text}")
                }
            ))),
        }
    }

    async fn notify(&self, method: &str) -> Result<(), UpstreamError> {
        self.send(
            json!({"jsonrpc": "2.0", "method": method}),
            None,
            Duration::from_secs(30),
        )
        .await
        .map(|_| ())
    }

    /// End the session politely (DELETE with the session id).
    async fn close(&self) {
        let Some(sid) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return;
        };
        let url = self.url.clone();
        let mut headers = self.header_list();
        headers.push(("Mcp-Session-Id".into(), sid));
        let _ = tokio::task::spawn_blocking(move || {
            let agent = http_agent(Duration::from_secs(5));
            let mut req = agent.delete(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let _ = req.call();
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_picks_our_response() {
        let stream = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                      data: {\"jsonrpc\":\"2.0\",\"id\":3,\n\
                      data: \"result\":{\"ok\":true}}\n\n";
        let msg = read_sse(std::io::Cursor::new(stream), Some(3)).unwrap();
        assert_eq!(msg["result"]["ok"], true);
        let other = "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\n";
        assert!(read_sse(std::io::Cursor::new(other), Some(3)).is_err());
    }

    #[test]
    fn rpc_errors_are_reported() {
        let e = rpc_result(
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "bad"}}),
        );
        assert_eq!(
            e,
            Err(UpstreamError::Rpc {
                code: -32602,
                message: "bad".into()
            })
        );
    }

    /// A tiny MCP server written in shell (one tool), started over stdio.
    #[tokio::test]
    async fn stdio_handshake_list_and_call() {
        let tmp = tempfile::tempdir().unwrap();
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"t","version":"1"}}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$SECRET_VALUE" ;;
  esac
done"#;
        let up = Upstream::start(
            Launch::Stdio {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), script.into()],
                env: BTreeMap::from([("SECRET_VALUE".into(), "from-the-vault".into())]),
                cwd: tmp.path().to_path_buf(),
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let tools = up.list_tools(Duration::from_secs(10)).await.unwrap();
        assert_eq!(tools[0]["name"], "echo");
        let r = up
            .call_tool("echo", &json!({}), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(r["content"][0]["text"], "from-the-vault");
        up.close().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            up.call_tool("echo", &json!({}), Duration::from_secs(2))
                .await
                .is_err()
        );
    }
}
