//! Claude Code adapter (`claude -p --output-format stream-json`).
//!
//! - System prompt: `--append-system-prompt-file` (Claude Code's own system
//!   prompt stays intact) plus `--system-prompt-snapshot off` so persona
//!   edits and renames apply to resumed sessions immediately.
//! - Input: `--input-format stream-json` on stdin, which also carries images
//!   as base64 content blocks.
//! - Resume: `--resume <session_id>` (sessions are scoped by cwd).
//! - MCP: `--mcp-config <private json file>`.

use super::{
    Activity, AuthState, Capabilities, Harness, HarnessEvent, Invocation, McpTransport,
    StreamParser, TurnSpec, looks_like_auth_error, summarize,
};
use crate::config::{HarnessConfig, write_private_file};
use crate::util::Result;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;

pub struct Claude {
    cfg: HarnessConfig,
}

impl Claude {
    pub fn new(cfg: HarnessConfig) -> Self {
        Self { cfg }
    }
}

/// Claude Code's MCP config file format.
pub fn mcp_config_json(servers: &[super::McpServer]) -> Value {
    let mut map = serde_json::Map::new();
    for s in servers {
        let v = match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                json!({"type": "stdio", "command": command, "args": args, "env": env})
            }
            McpTransport::Http { url, headers } => {
                json!({"type": "http", "url": url, "headers": headers})
            }
        };
        map.insert(s.name.clone(), v);
    }
    json!({ "mcpServers": map })
}

/// Permission deny rules for paths the agent must not touch. Claude Code
/// writes absolute paths as `//abs/path`; folders get `/**`. Paths with
/// characters the rule syntax cannot carry (commas, parentheses) are
/// skipped rather than mangled.
pub fn deny_rules(paths: &[std::path::PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        let s = p.to_string_lossy();
        if !s.starts_with('/') || s.contains([',', '(', ')', '\n']) {
            continue;
        }
        let s = s.trim_end_matches('/');
        // Folders get `/**`; a path that does not exist yet gets both forms.
        let mut targets = Vec::new();
        if !p.is_dir() {
            targets.push(format!("/{s}"));
        }
        if !p.is_file() {
            targets.push(format!("/{s}/**"));
        }
        for t in targets {
            out.push(format!("Read({t})"));
            out.push(format!("Edit({t})"));
        }
    }
    out
}

/// One `--input-format stream-json` user message.
pub fn user_message_line(spec: &TurnSpec) -> Result<Vec<u8>> {
    let mut content = vec![json!({"type": "text", "text": spec.prompt})];
    for img in &spec.images {
        let data = std::fs::read(&img.path)?;
        content.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": img.mime,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }
        }));
    }
    let mut line = serde_json::to_vec(&json!({
        "type": "user",
        "message": {"role": "user", "content": content},
    }))?;
    line.push(b'\n');
    Ok(line)
}

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
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

        let a = &mut inv.args;
        a.extend(
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--input-format",
                "stream-json",
                "--model",
            ]
            .map(String::from),
        );
        a.push(spec.model.clone());
        if let Some(e) = &spec.effort {
            a.push("--effort".into());
            a.push(e.clone());
        }
        if let Some(id) = &spec.resume {
            a.push("--resume".into());
            a.push(id.clone());
        }
        a.push("--append-system-prompt-file".into());
        a.push(prompt_file.to_string_lossy().into_owned());
        a.extend(["--system-prompt-snapshot", "off"].map(String::from));
        if !self.cfg.permission_mode.is_empty() {
            a.push("--permission-mode".into());
            a.push(self.cfg.permission_mode.clone());
        }
        // Nobody can answer an interactive approval in a headless turn.
        a.extend(["--permission-prompts", "none"].map(String::from));

        let mut allowed = self.cfg.allowed_tools.clone();
        if !spec.mcp_servers.is_empty() {
            let file = spec.run_dir.join(format!("{}.mcp.json", spec.turn_id));
            let body = serde_json::to_vec(&mcp_config_json(&spec.mcp_servers))?;
            write_private_file(&file, &body)?;
            inv.temp_files.push(file.clone());
            a.push("--mcp-config".into());
            a.push(file.to_string_lossy().into_owned());
            allowed.extend(spec.mcp_servers.iter().map(|s| format!("mcp__{}", s.name)));
        }
        if !allowed.is_empty() {
            // Variadic flag: always follow it with another flag (prompt is on
            // stdin, so no positional argument can be swallowed).
            a.push("--allowedTools".into());
            a.push(allowed.join(","));
        }
        let denied = deny_rules(&spec.private_paths);
        if !denied.is_empty() {
            // Variadic like --allowedTools; only flags follow.
            a.push("--disallowedTools".into());
            a.push(denied.join(","));
        }
        for dir in &spec.writable_dirs {
            a.push("--add-dir".into());
            a.push(dir.to_string_lossy().into_owned());
        }
        a.extend(self.cfg.extra_args.iter().cloned());
        inv.stdin = user_message_line(spec)?;
        Ok(inv)
    }

    fn parser(&self, _mcp_servers: &[String]) -> Box<dyn StreamParser> {
        Box::<ClaudeParser>::default()
    }

    /// True thanks to `--system-prompt-snapshot off`.
    fn resume_updates_system_prompt(&self) -> bool {
        true
    }

    fn auth_probe_args(&self) -> Option<Vec<String>> {
        Some(vec!["auth".into(), "status".into()])
    }

    fn parse_auth_probe(&self, _exit_ok: bool, stdout: &str, stderr: &str) -> AuthState {
        // Only `loggedIn` is read; the rest of the output (account details)
        // is discarded and never stored or logged.
        match serde_json::from_str::<Value>(stdout.trim())
            .ok()
            .and_then(|v| v.get("loggedIn").and_then(Value::as_bool))
        {
            Some(true) => AuthState::Ok,
            Some(false) => AuthState::AuthRequired,
            None if looks_like_auth_error(stdout) || looks_like_auth_error(stderr) => {
                AuthState::AuthRequired
            }
            None => AuthState::Unknown,
        }
    }

    /// Verified: works with piped stdin/stdout (no terminal needed).
    fn login(&self) -> Option<super::LoginSpec> {
        Some(super::LoginSpec {
            args: vec!["auth".into(), "login".into(), "--claudeai".into()],
            flow: super::LoginFlow::CodePaste,
        })
    }

    fn manual_login_hint(&self) -> String {
        "Run `claude` on the Iris server and sign in with /login.".into()
    }

    fn api_key_env(&self) -> &'static [&'static str] {
        &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
    }
}

/// Normalizes `stream-json` output. Text arrives as `text_delta` stream
/// events and again as complete `assistant` messages; the parser emits each
/// character once and fills gaps when partial events are missing.
#[derive(Default)]
pub struct ClaudeParser {
    current_msg: Option<String>,
    /// Text emitted so far per assistant message id.
    emitted: HashMap<String, String>,
    /// Order in which messages produced text (for separators).
    any_text: bool,
    last_text_msg: Option<String>,
    tools: HashMap<String, (String, String, String)>,
    finished: bool,
}

impl ClaudeParser {
    fn emit_text(&mut self, msg_id: &str, text: &str, out: &mut Vec<HarnessEvent>) {
        if text.is_empty() {
            return;
        }
        let mut chunk = String::new();
        if self.any_text && self.last_text_msg.as_deref() != Some(msg_id) {
            // New assistant message after earlier text (e.g. text, tool, text).
            chunk.push_str("\n\n");
        }
        chunk.push_str(text);
        self.any_text = true;
        self.last_text_msg = Some(msg_id.to_string());
        self.emitted
            .entry(msg_id.to_string())
            .or_default()
            .push_str(text);
        out.push(HarnessEvent::TextDelta { text: chunk });
    }

    fn tool_activity(&mut self, block: &Value, status: &str) -> Option<HarnessEvent> {
        let id = block.get("id")?.as_str()?.to_string();
        let raw_name = block.get("name")?.as_str()?.to_string();
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        let (kind, name) = match raw_name.strip_prefix("mcp__") {
            Some(rest) => match rest.split_once("__") {
                Some((server, tool)) => ("mcp", format!("{server}/{tool}")),
                None => ("mcp", rest.to_string()),
            },
            None if raw_name == "WebSearch" || raw_name == "WebFetch" => {
                ("web_search", raw_name.clone())
            }
            None if raw_name == "Bash" => ("command", raw_name.clone()),
            None if matches!(raw_name.as_str(), "Write" | "Edit" | "NotebookEdit") => {
                ("file_change", raw_name.clone())
            }
            None => ("tool", raw_name.clone()),
        };
        let summary = [
            "file_path",
            "command",
            "query",
            "url",
            "pattern",
            "path",
            "description",
        ]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(|s| summarize(s, 120))
        .unwrap_or_default();
        let known = self.tools.get(&id);
        if status == "started" && known.is_some_and(|(_, _, s)| !s.is_empty() || summary.is_empty())
        {
            return None;
        }
        self.tools.insert(
            id.clone(),
            (kind.to_string(), name.clone(), summary.clone()),
        );
        Some(HarnessEvent::Activity(Activity {
            id,
            kind: kind.into(),
            name,
            status: status.into(),
            summary,
            auth_error: false,
        }))
    }
}

impl StreamParser for ClaudeParser {
    fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return out;
        };
        // Ignore subagent traffic; only the top-level conversation is shown.
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return out;
        }
        match v.get("type").and_then(Value::as_str) {
            Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                    out.push(HarnessEvent::Session { id: id.into() });
                }
            }
            Some("stream_event") => {
                let e = &v["event"];
                match e.get("type").and_then(Value::as_str) {
                    Some("message_start") => {
                        self.current_msg = e["message"]["id"].as_str().map(String::from);
                    }
                    Some("content_block_delta") => {
                        if e["delta"]["type"] == "text_delta" {
                            let text = e["delta"]["text"].as_str().unwrap_or("").to_string();
                            let msg = self.current_msg.clone().unwrap_or_default();
                            self.emit_text(&msg, &text, &mut out);
                        }
                    }
                    Some("content_block_start") => {
                        let block = &e["content_block"];
                        if block["type"] == "tool_use" {
                            out.extend(self.tool_activity(block, "started"));
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let msg = &v["message"];
                let msg_id = msg["id"].as_str().unwrap_or("").to_string();
                if v.get("error").and_then(Value::as_str) == Some("authentication_failed") {
                    let text = msg["content"][0]["text"]
                        .as_str()
                        .unwrap_or("Not logged in");
                    out.push(HarnessEvent::AuthRequired {
                        message: summarize(text, 300),
                    });
                    self.finished = true;
                    return out;
                }
                for block in msg["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let full = block["text"].as_str().unwrap_or("");
                            let seen = self.emitted.get(&msg_id).cloned().unwrap_or_default();
                            // Each assistant event carries one block; only
                            // emit what streaming did not already deliver.
                            if let Some(rest) = full.strip_prefix(seen.as_str()) {
                                self.emit_text(&msg_id, rest, &mut out);
                            } else if !seen.contains(full) {
                                self.emit_text(&msg_id, full, &mut out);
                            }
                        }
                        Some("tool_use") => out.extend(self.tool_activity(block, "started")),
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let Some(id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    let Some((kind, name, summary)) = self.tools.get(id).cloned() else {
                        continue;
                    };
                    let failed = block["is_error"].as_bool().unwrap_or(false);
                    let auth_error = failed
                        && kind == "mcp"
                        && super::tool_auth_error(&super::tool_error_text(&block["content"]));
                    out.push(HarnessEvent::Activity(Activity {
                        id: id.into(),
                        kind,
                        name,
                        status: if failed { "failed" } else { "completed" }.into(),
                        summary,
                        auth_error,
                    }));
                }
            }
            Some("result") => {
                self.finished = true;
                if let Some(u) = v.get("usage") {
                    out.push(HarnessEvent::Usage {
                        usage: json!({
                            "input_tokens": u["input_tokens"],
                            "output_tokens": u["output_tokens"],
                            "cache_read_input_tokens": u["cache_read_input_tokens"],
                            "cache_creation_input_tokens": u["cache_creation_input_tokens"],
                            "duration_ms": v["duration_ms"],
                        }),
                    });
                }
                let is_error = v["is_error"].as_bool().unwrap_or(false)
                    || v["subtype"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("error"));
                if is_error {
                    let msg = v["result"]
                        .as_str()
                        .map(String::from)
                        .or_else(|| v["errors"].as_array().map(|e| format!("{e:?}")))
                        .unwrap_or_else(|| "Claude Code reported an error".into());
                    if looks_like_auth_error(&msg) {
                        out.push(HarnessEvent::AuthRequired {
                            message: summarize(&msg, 300),
                        });
                    } else {
                        out.push(HarnessEvent::Error {
                            message: summarize(&msg, 500),
                        });
                    }
                } else {
                    if !self.any_text
                        && let Some(text) = v["result"].as_str()
                    {
                        self.emit_text("result", text, &mut out);
                    }
                    out.push(HarnessEvent::Done);
                }
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
        if success {
            return vec![HarnessEvent::Done];
        }
        vec![HarnessEvent::Error {
            message: if msg.is_empty() {
                "Claude Code exited without a result".into()
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

    fn run(lines: &[Value]) -> Vec<HarnessEvent> {
        let mut p = ClaudeParser::default();
        let mut out = Vec::new();
        for l in lines {
            out.extend(p.line(&l.to_string()));
        }
        out.extend(p.finish(true, ""));
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

    fn se(event: Value) -> Value {
        json!({"type": "stream_event", "event": event, "parent_tool_use_id": null})
    }

    /// Synthetic sample modeled on Claude Code 2.1 output: text, tool use,
    /// tool result, more text, result.
    fn sample_turn() -> Vec<Value> {
        vec![
            json!({"type": "system", "subtype": "init", "session_id": "sess-1", "tools": []}),
            se(json!({"type": "message_start", "message": {"id": "m1"}})),
            se(
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
            ),
            se(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": ""}}),
            ),
            se(
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            ),
            se(
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me "}}),
            ),
            se(
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "check."}}),
            ),
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "Let me check."}]}}),
            se(
                json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}}),
            ),
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/data/notes.md"}}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "hello"}]}}),
            se(json!({"type": "message_start", "message": {"id": "m2"}})),
            se(
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "It says hello."}}),
            ),
            json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "It says hello."}]}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "It says hello.",
                   "session_id": "sess-1", "duration_ms": 1200,
                   "usage": {"input_tokens": 3, "output_tokens": 20, "cache_read_input_tokens": 100}}),
        ]
    }

    #[test]
    fn parses_text_tools_and_result() {
        let ev = run(&sample_turn());
        assert_eq!(
            ev[0],
            HarnessEvent::Session {
                id: "sess-1".into()
            }
        );
        assert_eq!(text_of(&ev), "Let me check.\n\nIt says hello.");
        let acts: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Activity(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 3, "{acts:?}");
        assert_eq!(
            (acts[0].name.as_str(), acts[0].status.as_str()),
            ("Read", "started")
        );
        assert_eq!(acts[1].summary, "/data/notes.md");
        assert_eq!(acts[2].status, "completed");
        assert!(matches!(ev.last(), Some(HarnessEvent::Done)));
        assert!(ev.iter().any(|e| matches!(e, HarnessEvent::Usage { .. })));
    }

    #[test]
    fn fills_text_when_partial_events_missing() {
        let lines = vec![
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "Whole reply"}]}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "Whole reply"}),
        ];
        assert_eq!(text_of(&run(&lines)), "Whole reply");
        let only_result =
            vec![json!({"type": "result", "subtype": "success", "is_error": false, "result": "R"})];
        assert_eq!(text_of(&run(&only_result)), "R");
    }

    #[test]
    fn mcp_tools_and_subagents() {
        let lines = vec![
            se(json!({"type": "message_start", "message": {"id": "m1"}})),
            se(json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t9", "name": "mcp__iris__create_goal", "input": {}}})),
            json!({"type": "stream_event", "parent_tool_use_id": "t0",
                   "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "SUBAGENT"}}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": ""}),
        ];
        let ev = run(&lines);
        assert!(!text_of(&ev).contains("SUBAGENT"));
        assert!(ev.iter().any(|e| matches!(e,
            HarnessEvent::Activity(a) if a.kind == "mcp" && a.name == "iris/create_goal")));
    }

    #[test]
    fn detects_auth_failure() {
        let lines = vec![
            json!({"type": "system", "subtype": "init", "session_id": "s"}),
            json!({"type": "assistant", "error": "authentication_failed",
                   "message": {"id": "x", "content": [{"type": "text", "text": "Not logged in · Please run /login"}]}}),
            json!({"type": "result", "subtype": "success", "is_error": true, "result": "Not logged in · Please run /login"}),
        ];
        let ev = run(&lines);
        assert!(
            ev.iter()
                .any(|e| matches!(e, HarnessEvent::AuthRequired { .. }))
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, HarnessEvent::Done | HarnessEvent::Error { .. }))
        );
    }

    #[test]
    fn error_result_and_crash() {
        let lines = vec![
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "result": "boom"}),
        ];
        assert!(
            matches!(run(&lines).last(), Some(HarnessEvent::Error { message }) if message == "boom")
        );
        let mut p = ClaudeParser::default();
        assert!(matches!(
            &p.finish(false, "segfault")[..],
            [HarnessEvent::Error { .. }]
        ));
        let mut p = ClaudeParser::default();
        assert!(matches!(
            &p.finish(false, "Not logged in")[..],
            [HarnessEvent::AuthRequired { .. }]
        ));
    }

    #[test]
    fn auth_probe_reads_only_logged_in() {
        let h = Claude::new(HarnessConfig::claude_default());
        assert_eq!(
            h.parse_auth_probe(true, r#"{"loggedIn": true, "authMethod": "claude.ai"}"#, ""),
            AuthState::Ok
        );
        assert_eq!(
            h.parse_auth_probe(true, r#"{"loggedIn": false}"#, ""),
            AuthState::AuthRequired
        );
        assert_eq!(h.parse_auth_probe(false, "garbage", ""), AuthState::Unknown);
    }

    #[test]
    fn builds_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("a.png");
        std::fs::write(&img, b"PNG").unwrap();
        let spec = TurnSpec {
            turn_id: "t1".into(),
            prompt: "hi".into(),
            images: vec![ImageInput {
                path: img,
                mime: "image/png".into(),
            }],
            system_prompt: "You are Iris".into(),
            model: "haiku".into(),
            effort: Some("low".into()),
            resume: Some("sess-1".into()),
            mcp_servers: vec![McpServer {
                name: "iris".into(),
                transport: McpTransport::Stdio {
                    command: "/usr/bin/iris".into(),
                    args: vec!["mcp".into()],
                    env: BTreeMap::new(),
                },
            }],
            cwd: tmp.path().into(),
            writable_dirs: vec![tmp.path().join("memory")],
            run_dir: tmp.path().into(),
            scope: "c1".into(),
            private_paths: vec![tmp.path().join("other"), tmp.path().join("vault.key")],
        };
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("vault.key"), "k").unwrap();
        let inv = Claude::new(HarnessConfig::claude_default())
            .build(&spec)
            .unwrap();
        let args = inv.args.join(" ");
        let i = inv
            .args
            .iter()
            .position(|a| a == "--disallowedTools")
            .unwrap();
        let other = tmp.path().join("other").display().to_string();
        let key = tmp.path().join("vault.key").display().to_string();
        assert_eq!(
            inv.args[i + 1],
            format!("Read(/{other}/**),Edit(/{other}/**),Read(/{key}),Edit(/{key})")
        );
        assert!(inv.args[i + 2].starts_with("--"), "followed by a flag");
        assert!(deny_rules(&["/a,b".into(), "rel".into()]).is_empty());
        assert_eq!(
            deny_rules(&["/no/such/dir".into()]),
            [
                "Read(//no/such/dir)",
                "Edit(//no/such/dir)",
                "Read(//no/such/dir/**)",
                "Edit(//no/such/dir/**)"
            ]
        );
        assert!(args.starts_with("-p --output-format stream-json --verbose"));
        assert!(args.contains("--model haiku --effort low --resume sess-1"));
        assert!(args.contains("--system-prompt-snapshot off"));
        assert!(args.contains("--permission-prompts none"));
        assert!(args.contains("--allowedTools Read,Glob,Grep,WebSearch,WebFetch,mcp__iris"));
        assert_eq!(inv.temp_files.len(), 2);
        let mcp: Value =
            serde_json::from_slice(&std::fs::read(&inv.temp_files[1]).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["iris"]["command"], "/usr/bin/iris");
        assert_eq!(
            std::fs::read_to_string(&inv.temp_files[0]).unwrap(),
            "You are Iris"
        );
        let stdin: Value = serde_json::from_slice(&inv.stdin).unwrap();
        assert_eq!(stdin["message"]["content"][0]["text"], "hi");
        assert_eq!(stdin["message"]["content"][1]["source"]["data"], "UE5H");
        // The prompt never appears on the command line.
        assert!(!inv.args.iter().any(|a| a == "hi"));
    }
}
