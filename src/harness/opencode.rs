//! OpenCode adapter (`opencode run --format json`). OpenCode is open source
//! and reaches many providers, including local models served by Ollama or
//! LM Studio, so it is how Iris runs open-weight and local models.
//!
//! Verified against opencode 1.19:
//! - Config: `OPENCODE_CONFIG=<private json file>` layers a config file over
//!   the user's own (never edited). It carries `instructions` (the Iris
//!   system prompt file, appended to OpenCode's prompt and re-read on every
//!   run, so resumed sessions see persona changes), `mcp` servers, and
//!   `permission` rules.
//! - Permissions: headless runs auto-reject anything set to `ask`, which is
//!   how shell access is withheld (`deny` would remove the tool entirely,
//!   which some hosted providers reject).
//! - Model/effort: `-m provider/model` and `--variant <effort>`.
//! - Resume: `-s <session id>`. Images: `-f <file>`. Prompt on stdin.
//! - Models: `opencode models --verbose` (provider/model header lines, each
//!   followed by a pretty-printed JSON object with capabilities).

use super::{
    Activity, AuthState, Capabilities, DiscoveredModel, Harness, HarnessEvent, Invocation,
    McpTransport, StreamParser, TurnSpec, looks_like_auth_error, mcp_tool_name, summarize,
};
use crate::config::{HarnessConfig, write_private_file};
use crate::util::Result;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub struct OpenCode {
    cfg: HarnessConfig,
}

impl OpenCode {
    pub fn new(cfg: HarnessConfig) -> Self {
        Self { cfg }
    }

    /// Permission rules from the harness `sandbox` setting. There is no OS
    /// sandbox here, so shell commands always stay behind `ask` (rejected
    /// headless) unless the operator chooses `danger-full-access`.
    fn permissions(&self, writable: &[std::path::PathBuf]) -> Value {
        let (edit, bash) = match self.cfg.sandbox.as_str() {
            "read-only" => ("ask", "ask"),
            "danger-full-access" => ("allow", "allow"),
            _ => ("allow", "ask"),
        };
        let mut external = Map::new();
        if edit == "allow" {
            for d in writable {
                external.insert(format!("{}/**", d.to_string_lossy()), json!("allow"));
            }
        }
        json!({
            "edit": edit,
            "bash": bash,
            "webfetch": "allow",
            "external_directory": external,
        })
    }
}

/// OpenCode's `mcp` config section.
pub fn mcp_config(servers: &[super::McpServer]) -> Value {
    let mut map = Map::new();
    for s in servers {
        let v = match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                let mut cmd = vec![command.clone()];
                cmd.extend(args.iter().cloned());
                json!({"type": "local", "command": cmd, "environment": env, "enabled": true})
            }
            McpTransport::Http { url, headers } => {
                json!({"type": "remote", "url": url, "headers": headers, "enabled": true})
            }
        };
        let mut v = v;
        // Iris's browser tools may wait for the owner (take-over, card
        // approval) longer than the usual request timeout.
        if [
            crate::browser::tools::SERVER,
            crate::browser::tools::SERVER_ALT,
        ]
        .contains(&s.name.as_str())
        {
            v["timeout"] = 180_000.into();
        }
        map.insert(s.name.clone(), v);
    }
    Value::Object(map)
}

impl Harness for OpenCode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            images: true,
            effort: true,
            mcp: true,
            resume: true,
        }
    }

    fn build(&self, spec: &TurnSpec) -> Result<Invocation> {
        let mut inv = Invocation::default();
        let prompt_file = spec.run_dir.join(format!("{}.prompt.md", spec.turn_id));
        write_private_file(&prompt_file, spec.system_prompt.as_bytes())?;
        inv.temp_files.push(prompt_file.clone());
        let config = json!({
            "$schema": "https://opencode.ai/config.json",
            "instructions": [prompt_file.to_string_lossy()],
            "permission": self.permissions(&spec.writable_dirs),
            "mcp": mcp_config(&spec.mcp_servers),
        });
        // A private file rather than OPENCODE_CONFIG_CONTENT: connector
        // headers must not sit in the process environment.
        let config_file = spec.run_dir.join(format!("{}.opencode.json", spec.turn_id));
        write_private_file(&config_file, &serde_json::to_vec(&config)?)?;
        inv.temp_files.push(config_file.clone());
        inv.env.push((
            "OPENCODE_CONFIG".into(),
            config_file.to_string_lossy().into_owned(),
        ));

        let a = &mut inv.args;
        a.push("run".into());
        // `-f` is an array option: keep it first and follow it with flags.
        for img in &spec.images {
            a.push("-f".into());
            a.push(img.path.to_string_lossy().into_owned());
        }
        a.extend(["--format", "json", "-m"].map(String::from));
        a.push(spec.model.clone());
        if let Some(e) = &spec.effort {
            a.push("--variant".into());
            a.push(e.clone());
        }
        if let Some(id) = &spec.resume {
            a.push("-s".into());
            a.push(id.clone());
        }
        a.extend(self.cfg.extra_args.iter().cloned());
        inv.stdin = spec.prompt.as_bytes().to_vec();
        Ok(inv)
    }

    fn parser(&self, mcp_servers: &[String]) -> Box<dyn StreamParser> {
        Box::new(OpenCodeParser {
            servers: mcp_servers.to_vec(),
            ..OpenCodeParser::default()
        })
    }

    /// Verified: `instructions` files are re-read on every run.
    fn resume_updates_system_prompt(&self) -> bool {
        true
    }

    /// OpenCode has no single sign-in; it is usable when it lists at least
    /// one model (hosted provider credentials, free models, or local).
    fn auth_probe_args(&self) -> Option<Vec<String>> {
        Some(vec!["models".into()])
    }

    fn parse_auth_probe(&self, exit_ok: bool, stdout: &str, stderr: &str) -> AuthState {
        if exit_ok && stdout.lines().any(|l| l.trim().contains('/')) {
            AuthState::Ok
        } else if exit_ok || looks_like_auth_error(stderr) {
            AuthState::AuthRequired
        } else {
            AuthState::Unknown
        }
    }

    /// Provider keys are OpenCode's normal way to sign in, so nothing is
    /// stripped.
    fn manual_login_hint(&self) -> String {
        "Run `opencode auth login` on the Iris server to add a provider, or configure a local provider such as Ollama.".into()
    }

    fn api_key_env(&self) -> &'static [&'static str] {
        &[]
    }

    fn models_probe_args(&self) -> Option<Vec<String>> {
        Some(vec!["models".into(), "--verbose".into()])
    }

    fn parse_models(&self, stdout: &str) -> Vec<DiscoveredModel> {
        parse_models_verbose(stdout)
    }
}

/// Providers that serve models from this machine or the local network.
const LOCAL_PROVIDERS: &[&str] = &[
    "ollama",
    "lmstudio",
    "lm-studio",
    "llamacpp",
    "llama.cpp",
    "llama-cpp",
    "local",
    "vllm",
    "localai",
    "jan",
];

fn is_local_url(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    };
    host == "localhost"
        || host == "::1"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.ends_with(".local")
        || host.ends_with(".lan")
}

fn header_line(line: &str) -> Option<&str> {
    let t = line.trim_end();
    (!t.is_empty()
        && !t.starts_with([' ', '{', '}', '"', '\t'])
        && t.contains('/')
        && !t.contains(char::is_whitespace))
    .then_some(t)
}

/// Parse `opencode models --verbose` (or plain `opencode models`).
pub fn parse_models_verbose(text: &str) -> Vec<DiscoveredModel> {
    let mut out: Vec<DiscoveredModel> = Vec::new();
    let mut current: Option<String> = None;
    let mut buf = String::new();
    let flush = |id: Option<String>, buf: &str, out: &mut Vec<DiscoveredModel>| {
        let Some(id) = id else { return };
        let meta: Value = serde_json::from_str(buf.trim()).unwrap_or(Value::Null);
        out.push(model_from_meta(&id, &meta));
    };
    for line in text.lines() {
        if let Some(h) = header_line(line) {
            flush(current.take(), &buf, &mut out);
            buf.clear();
            current = Some(h.to_string());
        } else if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush(current, &buf, &mut out);
    let mut seen = HashSet::new();
    out.retain(|m| seen.insert(m.model.clone()));
    out
}

fn model_from_meta(id: &str, meta: &Value) -> DiscoveredModel {
    let provider = id.split('/').next().unwrap_or("").to_string();
    let url = meta["api"]["url"].as_str().unwrap_or("");
    let local = LOCAL_PROVIDERS.contains(&provider.to_lowercase().as_str()) || is_local_url(url);
    let mut efforts: Vec<String> = meta["variants"]
        .as_object()
        .map(|v| v.keys().cloned().collect())
        .unwrap_or_default();
    const ORDER: &[&str] = &[
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ];
    efforts.sort_by_key(|e| ORDER.iter().position(|o| o == e).unwrap_or(usize::MAX));
    DiscoveredModel {
        model: id.to_string(),
        label: meta["name"].as_str().map(String::from),
        description: None,
        efforts,
        default_effort: None,
        images: meta["capabilities"]["input"]["image"].as_bool(),
        local,
        provider: (!provider.is_empty()).then_some(provider),
        hidden: meta["status"].as_str().is_some_and(|s| s == "deprecated"),
    }
}

/// Normalizes `opencode run --format json` output. Text arrives as whole
/// parts (no token deltas); tool calls arrive once, already finished.
#[derive(Default)]
pub struct OpenCodeParser {
    servers: Vec<String>,
    session_sent: bool,
    any_text: bool,
    steps: u32,
    tokens: [i64; 5],
    cost: f64,
    finished: bool,
}

impl OpenCodeParser {
    fn tool_activity(&self, part: &Value) -> Option<Activity> {
        let raw = part["tool"].as_str()?;
        let id = part["callID"]
            .as_str()
            .or_else(|| part["id"].as_str())?
            .to_string();
        let st = &part["state"];
        let status = match st["status"].as_str() {
            Some("completed") => "completed",
            Some("error") => "failed",
            Some("running") | Some("pending") => "started",
            _ => "completed",
        };
        let input = &st["input"];
        let (kind, name) = match raw {
            "bash" => ("command", "shell".to_string()),
            "edit" | "write" | "patch" | "multiedit" => ("file_change", raw.to_string()),
            "webfetch" | "websearch" => ("web_search", raw.to_string()),
            _ => match mcp_tool_name(raw, &self.servers, &["_", "__"]) {
                Some(n) => ("mcp", n),
                None => ("tool", raw.to_string()),
            },
        };
        let summary = ["filePath", "command", "query", "url", "pattern", "path"]
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .or_else(|| st["title"].as_str())
            .map(|s| summarize(s, 120))
            .unwrap_or_default();
        let auth_error = status == "failed"
            && kind == "mcp"
            && super::tool_auth_error(&super::tool_error_text(&st["error"]));
        Some(Activity {
            id,
            kind: kind.into(),
            name,
            status: status.into(),
            summary,
            auth_error,
        })
    }

    fn usage(&self) -> HarnessEvent {
        HarnessEvent::Usage {
            usage: json!({
                "input_tokens": self.tokens[0],
                "output_tokens": self.tokens[1],
                "reasoning_tokens": self.tokens[2],
                "cache_read_tokens": self.tokens[3],
                "cache_write_tokens": self.tokens[4],
                "cost": self.cost,
                "steps": self.steps,
            }),
        }
    }
}

impl StreamParser for OpenCodeParser {
    fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return out;
        };
        if self.finished {
            return out;
        }
        if !self.session_sent
            && let Some(id) = v["sessionID"].as_str()
        {
            self.session_sent = true;
            out.push(HarnessEvent::Session { id: id.into() });
        }
        let part = &v["part"];
        match v["type"].as_str() {
            Some("text") => {
                let text = part["text"].as_str().unwrap_or("");
                if !text.trim().is_empty() {
                    let sep = if self.any_text { "\n\n" } else { "" };
                    self.any_text = true;
                    out.push(HarnessEvent::TextDelta {
                        text: format!("{sep}{text}"),
                    });
                }
            }
            Some("tool_use") => {
                out.extend(self.tool_activity(part).map(HarnessEvent::Activity));
            }
            Some("step_finish") => {
                self.steps += 1;
                let t = &part["tokens"];
                for (i, v) in [
                    &t["input"],
                    &t["output"],
                    &t["reasoning"],
                    &t["cache"]["read"],
                    &t["cache"]["write"],
                ]
                .iter()
                .enumerate()
                {
                    self.tokens[i] += v.as_i64().unwrap_or(0);
                }
                self.cost += part["cost"].as_f64().unwrap_or(0.0);
            }
            Some("error") => {
                self.finished = true;
                let e = &v["error"];
                let name = e["name"].as_str().unwrap_or("");
                let msg = e["data"]["message"]
                    .as_str()
                    .or_else(|| e["message"].as_str())
                    .unwrap_or("OpenCode reported an error")
                    .to_string();
                let status = e["data"]["statusCode"].as_i64();
                let auth = name == "ProviderAuthError"
                    || status == Some(401)
                    || looks_like_auth_error(&msg);
                out.push(if auth {
                    HarnessEvent::AuthRequired {
                        message: summarize(&msg, 300),
                    }
                } else {
                    HarnessEvent::Error {
                        message: summarize(&msg, 500),
                    }
                });
            }
            _ => {}
        }
        out
    }

    fn finish(&mut self, success: bool, stderr_tail: &str) -> Vec<HarnessEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let msg = stderr_tail.trim();
        if looks_like_auth_error(msg) {
            return vec![HarnessEvent::AuthRequired {
                message: summarize(msg, 300),
            }];
        }
        if success && self.steps > 0 {
            return vec![self.usage(), HarnessEvent::Done];
        }
        vec![HarnessEvent::Error {
            message: if msg.is_empty() {
                "OpenCode exited without a result".into()
            } else {
                summarize(msg, 500)
            },
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{ImageInput, McpServer};
    use std::collections::BTreeMap;

    fn run(lines: &[Value], success: bool) -> Vec<HarnessEvent> {
        let mut p = OpenCodeParser {
            servers: vec!["iris".into()],
            ..Default::default()
        };
        let mut out = Vec::new();
        for l in lines {
            out.extend(p.line(&l.to_string()));
        }
        out.extend(p.finish(success, ""));
        out
    }

    fn text_of(events: &[HarnessEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn ev(ty: &str, part: Value) -> Value {
        json!({"type": ty, "timestamp": 1, "sessionID": "ses_1", "part": part})
    }

    /// Synthetic sample modeled on opencode 1.19 `run --format json`.
    #[test]
    fn parses_text_tools_and_usage() {
        let lines = vec![
            ev("step_start", json!({"type": "step-start"})),
            ev(
                "text",
                json!({"type": "text", "text": "Let me look.", "messageID": "m1"}),
            ),
            ev(
                "tool_use",
                json!({"type": "tool", "tool": "read", "callID": "c1",
                    "state": {"status": "completed", "input": {"filePath": "/data/notes.md"}, "output": "x"}}),
            ),
            ev(
                "tool_use",
                json!({"type": "tool", "tool": "iris_create_goal", "callID": "c2",
                    "state": {"status": "completed", "input": {"title": "Run"}}}),
            ),
            ev(
                "tool_use",
                json!({"type": "tool", "tool": "bash", "callID": "c3",
                    "state": {"status": "error", "input": {"command": "ls"}, "error": "The user rejected permission"}}),
            ),
            ev(
                "step_finish",
                json!({"type": "step-finish", "reason": "tool-calls",
                    "tokens": {"input": 10, "output": 2, "reasoning": 1, "cache": {"read": 5, "write": 0}}, "cost": 0}),
            ),
            ev(
                "text",
                json!({"type": "text", "text": "It says hi.", "messageID": "m2"}),
            ),
            ev(
                "step_finish",
                json!({"type": "step-finish", "reason": "stop",
                    "tokens": {"input": 3, "output": 4, "reasoning": 0, "cache": {"read": 0, "write": 0}}, "cost": 0.5}),
            ),
        ];
        let out = run(&lines, true);
        assert_eq!(out[0], HarnessEvent::Session { id: "ses_1".into() });
        assert_eq!(text_of(&out), "Let me look.\n\nIt says hi.");
        let acts: Vec<&Activity> = out
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Activity(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(acts[0].summary, "/data/notes.md");
        assert_eq!(
            (acts[1].kind.as_str(), acts[1].name.as_str()),
            ("mcp", "iris/create_goal")
        );
        assert_eq!(
            (acts[2].kind.as_str(), acts[2].status.as_str()),
            ("command", "failed")
        );
        let usage = out.iter().find_map(|e| match e {
            HarnessEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        });
        assert_eq!(usage.unwrap()["input_tokens"], 13);
        assert!(matches!(out.last(), Some(HarnessEvent::Done)));
        assert_eq!(
            out.iter()
                .filter(|e| matches!(e, HarnessEvent::Session { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn errors_auth_and_crash() {
        let err = vec![
            json!({"type": "error", "sessionID": "s", "error": {"name": "APIError",
            "data": {"message": "model overloaded", "statusCode": 529}}}),
        ];
        let out = run(&err, false);
        assert!(
            matches!(out.last(), Some(HarnessEvent::Error { message }) if message == "model overloaded")
        );
        assert_eq!(
            out.iter()
                .filter(|e| matches!(e, HarnessEvent::Error { .. } | HarnessEvent::Done))
                .count(),
            1
        );
        let auth = vec![
            json!({"type": "error", "sessionID": "s", "error": {"name": "APIError",
            "data": {"message": "Invalid API key", "statusCode": 401}}}),
        ];
        assert!(matches!(
            run(&auth, false).last(),
            Some(HarnessEvent::AuthRequired { .. })
        ));
        let provider = vec![
            json!({"type": "error", "error": {"name": "ProviderAuthError",
            "data": {"providerID": "anthropic", "message": "expired"}}}),
        ];
        assert!(matches!(
            run(&provider, false).last(),
            Some(HarnessEvent::AuthRequired { .. })
        ));
        // Crash before any step finished.
        assert!(matches!(
            run(&[], false).last(),
            Some(HarnessEvent::Error { .. })
        ));
        assert!(matches!(
            run(&[], true).last(),
            Some(HarnessEvent::Error { .. })
        ));
    }

    #[test]
    fn parses_verbose_model_list() {
        let text = r#"opencode/free-model
{
  "id": "free-model",
  "providerID": "opencode",
  "name": "Free Model",
  "api": {"url": "https://opencode.ai/zen/v1"},
  "capabilities": {"input": {"text": true, "image": false}},
  "variants": {"high": {}, "low": {}, "medium": {}}
}
ollama/qwen3:32b
{
  "id": "qwen3:32b",
  "name": "Qwen3 32B",
  "api": {"url": "http://127.0.0.1:11434/v1"},
  "capabilities": {"input": {"image": true}},
  "variants": {}
}
lab/mystery
{
  "api": {"url": "http://192.168.1.20:8080/v1"}
}
anthropic/claude-haiku-4-5
"#;
        let models = parse_models_verbose(text);
        let ids: Vec<&str> = models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(
            ids,
            [
                "opencode/free-model",
                "ollama/qwen3:32b",
                "lab/mystery",
                "anthropic/claude-haiku-4-5"
            ]
        );
        assert_eq!(models[0].efforts, ["low", "medium", "high"]);
        assert_eq!(models[0].images, Some(false));
        assert!(!models[0].local);
        assert!(models[1].local && models[2].local);
        assert_eq!(models[1].label.as_deref(), Some("Qwen3 32B"));
        assert_eq!(models[3].provider.as_deref(), Some("anthropic"));
        // The plain (non-verbose) listing parses too.
        assert_eq!(parse_models_verbose("a/b\nc/d:1b\n").len(), 2);
    }

    #[test]
    fn auth_probe() {
        let h = OpenCode::new(HarnessConfig::opencode_default());
        assert_eq!(h.parse_auth_probe(true, "opencode/x\n", ""), AuthState::Ok);
        assert_eq!(h.parse_auth_probe(true, "", ""), AuthState::AuthRequired);
        assert_eq!(h.parse_auth_probe(false, "", "boom"), AuthState::Unknown);
    }

    #[test]
    fn builds_invocation_with_private_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer s3cret".to_string());
        let spec = TurnSpec {
            turn_id: "t1".into(),
            prompt: "--help me".into(),
            images: vec![ImageInput {
                path: "/u/a.png".into(),
                mime: "image/png".into(),
            }],
            system_prompt: "You are Iris".into(),
            model: "ollama/qwen3:8b".into(),
            effort: Some("low".into()),
            resume: Some("ses_9".into()),
            mcp_servers: vec![
                McpServer {
                    name: "iris".into(),
                    transport: McpTransport::Stdio {
                        command: "/bin/iris".into(),
                        args: vec!["mcp".into()],
                        env: BTreeMap::new(),
                    },
                },
                McpServer {
                    name: "web".into(),
                    transport: McpTransport::Http {
                        url: "https://mcp.example.com".into(),
                        headers,
                    },
                },
            ],
            cwd: tmp.path().into(),
            writable_dirs: vec!["/data/memory".into()],
            run_dir: tmp.path().into(),
            scope: "c1".into(),
        };
        let inv = OpenCode::new(HarnessConfig::opencode_default())
            .build(&spec)
            .unwrap();
        assert_eq!(
            inv.args,
            [
                "run",
                "-f",
                "/u/a.png",
                "--format",
                "json",
                "-m",
                "ollama/qwen3:8b",
                "--variant",
                "low",
                "-s",
                "ses_9"
            ]
        );
        assert_eq!(inv.stdin, b"--help me");
        let (k, path) = &inv.env[0];
        assert_eq!(k, "OPENCODE_CONFIG");
        // Secrets stay out of argv and the environment.
        assert!(!inv.args.iter().any(|a| a.contains("s3cret")));
        assert!(!inv.env.iter().any(|(_, v)| v.contains("s3cret")));
        let cfg: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(cfg["mcp"]["iris"]["command"], json!(["/bin/iris", "mcp"]));
        assert_eq!(cfg["mcp"]["web"]["type"], "remote");
        assert_eq!(cfg["permission"]["bash"], "ask");
        assert_eq!(cfg["permission"]["edit"], "allow");
        assert_eq!(
            cfg["permission"]["external_directory"]["/data/memory/**"],
            "allow"
        );
        let prompt = cfg["instructions"][0].as_str().unwrap();
        assert_eq!(std::fs::read_to_string(prompt).unwrap(), "You are Iris");
        assert_eq!(inv.temp_files.len(), 2);
    }

    #[test]
    fn local_urls() {
        assert!(is_local_url("http://localhost:11434/v1"));
        assert!(is_local_url("http://[::1]:1234"));
        assert!(is_local_url("http://user@127.0.0.1"));
        assert!(!is_local_url("https://api.example.com/v1"));
        assert!(!is_local_url(""));
    }
}
