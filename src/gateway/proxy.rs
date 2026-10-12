//! `iris mcp-gateway`: the stdio MCP server a CLI starts for one connector
//! in one turn. It holds no secrets and makes no decisions: it answers the
//! handshake itself and forwards `tools/list` and `tools/call` to the Iris
//! daemon over its private Unix socket, with the turn's token (from the
//! `IRIS_GATEWAY_TOKEN` environment variable). Calls run concurrently, so
//! one call waiting for the owner's approval does not block the others.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct Proxy {
    pub socket: PathBuf,
    pub connector: String,
    pub token: String,
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

impl Proxy {
    /// Answer locally what needs no daemon; None means "forward it".
    pub fn local(&self, req: &Value) -> Option<Option<Value>> {
        let method = req["method"].as_str().unwrap_or("");
        let Some(id) = req.get("id").cloned() else {
            // Notifications need no reply.
            return Some(None);
        };
        let result = match method {
            "initialize" => {
                let requested = req["params"]["protocolVersion"].as_str().unwrap_or("");
                let version = if SUPPORTED_VERSIONS.contains(&requested) {
                    requested
                } else {
                    SUPPORTED_VERSIONS[1]
                };
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": self.connector, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "A service the user connected, reached through Iris. Actions \
                        that send, buy, delete, or reach someone new wait for the user's approval.",
                })
            }
            "ping" => json!({}),
            "resources/list" => json!({"resources": []}),
            "resources/templates/list" => json!({"resourceTemplates": []}),
            "prompts/list" => json!({"prompts": []}),
            "tools/list" | "tools/call" => return None,
            _ => return Some(Some(error_response(id, -32601, "method not found"))),
        };
        Some(Some(json!({"jsonrpc": "2.0", "id": id, "result": result})))
    }

    /// Send one request to the daemon and wait for its answer.
    pub fn forward(&self, req: &Value) -> Value {
        self.forward_envelope(req, false)
    }

    pub fn forward_envelope(&self, req: &Value, awareness: bool) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let attempt = || -> std::io::Result<Value> {
            let mut s = UnixStream::connect(&self.socket)?;
            let msg = json!({"token": self.token, "connector": self.connector, "request": req, "awareness": awareness});
            let mut line = msg.to_string().into_bytes();
            line.push(b'\n');
            s.write_all(&line)?;
            let mut reply = String::new();
            BufReader::new(s).read_line(&mut reply)?;
            serde_json::from_str(&reply)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        };
        match attempt() {
            Ok(v) => v,
            Err(_) => error_response(id, -32003, "Iris is not reachable right now"),
        }
    }
}

/// Run until stdin closes.
pub fn run(proxy: Proxy) -> crate::util::Result<()> {
    let proxy = Arc::new(proxy);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let write = |out: &Arc<Mutex<std::io::Stdout>>, v: &Value| {
        let mut o = out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    };
    let mut workers = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                write(&out, &error_response(Value::Null, -32700, "parse error"));
                continue;
            }
        };
        match proxy.local(&req) {
            Some(Some(reply)) => write(&out, &reply),
            Some(None) => {}
            None => {
                let (p, o) = (proxy.clone(), out.clone());
                workers.push(std::thread::spawn(move || {
                    let reply = p.forward(&req);
                    write(&o, &reply);
                }));
                workers.retain(|w| !w.is_finished());
            }
        }
    }
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_handshake_locally_and_forwards_tools() {
        let p = Proxy {
            socket: PathBuf::from("/nonexistent/gateway.sock"),
            connector: "mail".into(),
            token: "t".into(),
        };
        let init = p
            .local(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                           "params": {"protocolVersion": "2025-06-18"}}))
            .unwrap()
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "mail");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(
            p.local(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .unwrap()
                .is_none()
        );
        let req = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        assert!(p.local(&req).is_none());
        // Daemon not running: a clean JSON-RPC error, not a crash.
        assert_eq!(p.forward(&req)["error"]["code"], -32003);
    }
}
