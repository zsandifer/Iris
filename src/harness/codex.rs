//! Codex CLI adapter (`codex exec --json`).
//!
//! - System prompt: `-c developer_instructions=<toml string>` (a developer
//!   message placed before the user's message; Codex's base instructions
//!   stay intact).
//! - Model/effort: `-m <model>` and `-c model_reasoning_effort=<effort>`.
//! - Resume: `codex exec resume <thread_id>`. `resume` does not accept
//!   `-s`/`--add-dir`, so sandbox settings are always passed as `-c`
//!   overrides, identically for new and resumed threads.
//! - MCP: `-c mcp_servers.<name>.<key>=<toml value>` per server.
//! - Prompt on stdin (`-`); images via `-i <file>`.

use super::{
    Activity, AuthState, Capabilities, Harness, HarnessEvent, Invocation, McpServer, McpTransport,
    StreamParser, TurnSpec, looks_like_auth_error, summarize,
};
use crate::config::HarnessConfig;
use crate::util::Result;
use serde_json::{Value, json};

pub struct Codex {
    cfg: HarnessConfig,
}

impl Codex {
    pub fn new(cfg: HarnessConfig) -> Self {
        Self { cfg }
    }
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn toml_str_array(items: &[String]) -> String {
    toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect()).to_string()
}

/// Bare TOML keys only, so values can be addressed with dotted `-c` paths.
fn bare_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `-c` overrides that register the given MCP servers.
pub fn mcp_overrides(servers: &[McpServer]) -> Vec<String> {
    let mut out = Vec::new();
    for s in servers {
        let p = format!("mcp_servers.{}", s.name);
        // Headless turns cannot answer approval prompts. Servers passed by
        // Iris (its own bridge and connectors the owner enabled) are
        // pre-approved, matching `--allowedTools mcp__<name>` for Claude.
        out.push(format!("{p}.default_tools_approval_mode=\"approve\""));
        match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                out.push(format!("{p}.command={}", toml_str(command)));
                out.push(format!("{p}.args={}", toml_str_array(args)));
                for (k, v) in env.iter().filter(|(k, _)| bare_key(k)) {
                    out.push(format!("{p}.env.{k}={}", toml_str(v)));
                }
            }
            McpTransport::Http { url, headers } => {
                out.push(format!("{p}.url={}", toml_str(url)));
                for (k, v) in headers.iter().filter(|(k, _)| bare_key(k)) {
                    out.push(format!("{p}.http_headers.{k}={}", toml_str(v)));
                }
            }
        }
    }
    out
}

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
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
        let mut a: Vec<String> = vec!["exec".into()];
        if spec.resume.is_some() {
            a.push("resume".into());
        }
        // `-i` is variadic on `exec`; keep it first and follow it with a flag
        // so it can never consume the `-` (stdin) or the thread id.
        for img in &spec.images {
            a.push("-i".into());
            a.push(img.path.to_string_lossy().into_owned());
        }
        a.extend(["--json", "--skip-git-repo-check", "-m"].map(String::from));
        a.push(spec.model.clone());
        let mut overrides = Vec::new();
        if let Some(e) = &spec.effort {
            overrides.push(format!("model_reasoning_effort={}", toml_str(e)));
        }
        overrides.push(format!(
            "developer_instructions={}",
            toml_str(&spec.system_prompt)
        ));
        if !self.cfg.sandbox.is_empty() {
            overrides.push(format!("sandbox_mode={}", toml_str(&self.cfg.sandbox)));
            if self.cfg.sandbox == "workspace-write" && !spec.writable_dirs.is_empty() {
                let roots: Vec<String> = spec
                    .writable_dirs
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned())
                    .collect();
                overrides.push(format!(
                    "sandbox_workspace_write.writable_roots={}",
                    toml_str_array(&roots)
                ));
            }
        }
        overrides.extend(mcp_overrides(&spec.mcp_servers));
        for o in overrides {
            a.push("-c".into());
            a.push(o);
        }
        a.extend(self.cfg.extra_args.iter().cloned());
        if let Some(id) = &spec.resume {
            a.push(id.clone());
        }
        a.push("-".into());
        Ok(Invocation {
            args: a,
            stdin: spec.prompt.as_bytes().to_vec(),
            ..Invocation::default()
        })
    }

    fn parser(&self, _mcp_servers: &[String]) -> Box<dyn StreamParser> {
        Box::<CodexParser>::default()
    }

    /// `codex exec resume` keeps the developer instructions the thread was
    /// created with (verified with codex-cli 0.162).
    fn resume_updates_system_prompt(&self) -> bool {
        false
    }

    fn auth_probe_args(&self) -> Option<Vec<String>> {
        Some(vec!["login".into(), "status".into()])
    }

    fn parse_auth_probe(&self, exit_ok: bool, stdout: &str, stderr: &str) -> AuthState {
        let text = format!("{stdout}\n{stderr}").to_lowercase();
        if text.contains("not logged in") {
            AuthState::AuthRequired
        } else if exit_ok && text.contains("logged in") {
            AuthState::Ok
        } else if looks_like_auth_error(&text) {
            AuthState::AuthRequired
        } else {
            AuthState::Unknown
        }
    }

    /// Verified: works with piped stdin/stdout (no terminal needed).
    fn login(&self) -> Option<super::LoginSpec> {
        Some(super::LoginSpec {
            args: vec!["login".into(), "--device-auth".into()],
            flow: super::LoginFlow::DeviceCode,
        })
    }

    fn manual_login_hint(&self) -> String {
        "Run `codex login --device-auth` on the Iris server.".into()
    }

    fn api_key_env(&self) -> &'static [&'static str] {
        &["OPENAI_API_KEY", "CODEX_API_KEY"]
    }
}

/// Normalizes `codex exec --json` JSONL. Agent text arrives as whole
/// `agent_message` items (no token deltas), tool use as items with
/// started/completed lifecycle.
#[derive(Default)]
pub struct CodexParser {
    any_text: bool,
    last_error: Option<String>,
    finished: bool,
}

impl CodexParser {
    fn item_activity(item: &Value, phase: &str) -> Option<Activity> {
        let id = item["id"].as_str()?.to_string();
        let failed = item["status"].as_str() == Some("failed")
            || item["exit_code"].as_i64().is_some_and(|c| c != 0)
            || item.get("error").is_some_and(|e| !e.is_null());
        let status = match phase {
            "started" => "started",
            _ if failed => "failed",
            _ => "completed",
        };
        let (kind, name, summary) = match item["type"].as_str()? {
            "command_execution" => (
                "command",
                "shell".to_string(),
                summarize(item["command"].as_str().unwrap_or(""), 120),
            ),
            "mcp_tool_call" => (
                "mcp",
                format!(
                    "{}/{}",
                    item["server"].as_str().unwrap_or("?"),
                    item["tool"].as_str().unwrap_or("?")
                ),
                String::new(),
            ),
            "web_search" => (
                "web_search",
                "web_search".to_string(),
                summarize(item["query"].as_str().unwrap_or(""), 120),
            ),
            "file_change" => (
                "file_change",
                "edit".to_string(),
                summarize(
                    &item["changes"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .filter_map(|x| x["path"].as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    120,
                ),
            ),
            _ => return None,
        };
        let auth_error = status == "failed"
            && kind == "mcp"
            && super::tool_auth_error(&format!(
                "{} {}",
                super::tool_error_text(&item["error"]),
                super::tool_error_text(&item["result"]["content"])
            ));
        Some(Activity {
            id,
            kind: kind.into(),
            name,
            status: status.into(),
            summary,
            auth_error,
        })
    }
}

impl StreamParser for CodexParser {
    fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return out;
        };
        match v["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = v["thread_id"].as_str() {
                    out.push(HarnessEvent::Session { id: id.into() });
                }
            }
            Some(phase @ ("item.started" | "item.updated" | "item.completed")) => {
                let item = &v["item"];
                let phase = phase.trim_start_matches("item.");
                match item["type"].as_str() {
                    Some("agent_message") if phase == "completed" => {
                        let text = item["text"].as_str().unwrap_or("");
                        if !text.is_empty() {
                            let sep = if self.any_text { "\n\n" } else { "" };
                            self.any_text = true;
                            out.push(HarnessEvent::TextDelta {
                                text: format!("{sep}{text}"),
                            });
                        }
                    }
                    Some("error") => {
                        // Non-fatal notices (e.g. transport fallback).
                        self.last_error = item["message"].as_str().map(String::from);
                    }
                    _ if phase != "updated" => {
                        out.extend(Self::item_activity(item, phase).map(HarnessEvent::Activity));
                    }
                    _ => {}
                }
            }
            Some("error") => {
                // Usually transient ("Reconnecting... 2/5"); remembered in
                // case the turn then fails.
                self.last_error = v["message"].as_str().map(String::from);
            }
            Some("turn.completed") => {
                self.finished = true;
                let u = &v["usage"];
                out.push(HarnessEvent::Usage {
                    usage: json!({
                        "input_tokens": u["input_tokens"],
                        "cached_input_tokens": u["cached_input_tokens"],
                        "output_tokens": u["output_tokens"],
                        "reasoning_output_tokens": u["reasoning_output_tokens"],
                    }),
                });
                out.push(HarnessEvent::Done);
            }
            Some("turn.failed") => {
                self.finished = true;
                let msg = v["error"]["message"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| self.last_error.clone())
                    .unwrap_or_else(|| "Codex turn failed".into());
                out.push(if looks_like_auth_error(&msg) {
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
        let msg = self
            .last_error
            .clone()
            .unwrap_or_else(|| stderr_tail.trim().to_string());
        if looks_like_auth_error(&msg) || looks_like_auth_error(stderr_tail) {
            return vec![HarnessEvent::AuthRequired {
                message: summarize(&msg, 300),
            }];
        }
        if success && self.any_text {
            return vec![HarnessEvent::Done];
        }
        vec![HarnessEvent::Error {
            message: if msg.is_empty() {
                "Codex exited without completing the turn".into()
            } else {
                summarize(&msg, 500)
            },
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::ImageInput;
    use std::collections::BTreeMap;

    fn run(lines: &[Value], success: bool) -> Vec<HarnessEvent> {
        let mut p = CodexParser::default();
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

    #[test]
    fn parses_messages_commands_and_usage() {
        // Synthetic sample modeled on codex-cli 0.162 `exec --json` output.
        let lines = vec![
            json!({"type": "thread.started", "thread_id": "th-1"}),
            json!({"type": "turn.started"}),
            json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message", "text": "Checking."}}),
            json!({"type": "item.started", "item": {"id": "item_1", "type": "command_execution",
                   "command": "/bin/bash -lc 'cat notes.txt'", "aggregated_output": "", "exit_code": null, "status": "in_progress"}}),
            json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution",
                   "command": "/bin/bash -lc 'cat notes.txt'", "aggregated_output": "hi\n", "exit_code": 0, "status": "completed"}}),
            json!({"type": "item.started", "item": {"id": "item_2", "type": "mcp_tool_call", "server": "iris", "tool": "create_goal", "status": "in_progress"}}),
            json!({"type": "item.completed", "item": {"id": "item_2", "type": "mcp_tool_call", "server": "iris", "tool": "create_goal", "status": "completed"}}),
            json!({"type": "item.completed", "item": {"id": "item_3", "type": "agent_message", "text": "Done."}}),
            json!({"type": "turn.completed", "usage": {"input_tokens": 10, "cached_input_tokens": 5, "output_tokens": 3}}),
        ];
        let ev = run(&lines, true);
        assert_eq!(ev[0], HarnessEvent::Session { id: "th-1".into() });
        assert_eq!(text_of(&ev), "Checking.\n\nDone.");
        let acts: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Activity(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 4);
        assert_eq!(acts[0].summary, "/bin/bash -lc 'cat notes.txt'");
        assert_eq!(acts[1].status, "completed");
        assert_eq!(
            (acts[2].kind.as_str(), acts[2].name.as_str()),
            ("mcp", "iris/create_goal")
        );
        assert!(matches!(ev.last(), Some(HarnessEvent::Done)));
    }

    #[test]
    fn failed_command_and_turn_failure() {
        let lines = vec![
            json!({"type": "item.completed", "item": {"id": "i", "type": "command_execution", "command": "false", "exit_code": 1, "status": "failed"}}),
            json!({"type": "turn.failed", "error": {"message": "model overloaded"}}),
        ];
        let ev = run(&lines, false);
        assert!(matches!(&ev[0], HarnessEvent::Activity(a) if a.status == "failed"));
        assert!(
            matches!(ev.last(), Some(HarnessEvent::Error { message }) if message == "model overloaded")
        );
    }

    #[test]
    fn detects_auth_failure() {
        let lines = vec![
            json!({"type": "thread.started", "thread_id": "t"}),
            json!({"type": "error", "message": "Reconnecting... 1/5 (unexpected status 401 Unauthorized: Missing bearer)"}),
            json!({"type": "turn.failed", "error": {"message": "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header"}}),
        ];
        let ev = run(&lines, false);
        assert!(matches!(ev.last(), Some(HarnessEvent::AuthRequired { .. })));
    }

    #[test]
    fn exit_without_turn_completed() {
        assert!(matches!(
            run(&[], false).last(),
            Some(HarnessEvent::Error { .. })
        ));
        let mut p = CodexParser::default();
        assert!(matches!(
            &p.finish(false, "Error: Not logged in")[..],
            [HarnessEvent::AuthRequired { .. }]
        ));
    }

    #[test]
    fn auth_probe() {
        let h = Codex::new(HarnessConfig::codex_default());
        assert_eq!(
            h.parse_auth_probe(true, "Logged in using ChatGPT\n", ""),
            AuthState::Ok
        );
        assert_eq!(
            h.parse_auth_probe(false, "", "Not logged in\n"),
            AuthState::AuthRequired
        );
        assert_eq!(
            h.parse_auth_probe(false, "", "segfault"),
            AuthState::Unknown
        );
    }

    #[test]
    fn builds_new_and_resume_invocations() {
        let mut env = BTreeMap::new();
        env.insert("IRIS_DATA_DIR".to_string(), "/d".to_string());
        env.insert("bad key".to_string(), "x".to_string());
        let mut spec = TurnSpec {
            turn_id: "t".into(),
            prompt: "hello".into(),
            images: vec![ImageInput {
                path: "/u/a.png".into(),
                mime: "image/png".into(),
            }],
            system_prompt: "You are \"Iris\"\nline2".into(),
            model: "gpt-6-luna".into(),
            effort: Some("low".into()),
            resume: None,
            mcp_servers: vec![McpServer {
                name: "iris".into(),
                transport: McpTransport::Stdio {
                    command: "/bin/iris".into(),
                    args: vec!["mcp".into(), "--conversation".into(), "c1".into()],
                    env,
                },
            }],
            cwd: "/w".into(),
            writable_dirs: vec!["/m".into()],
            run_dir: "/r".into(),
            scope: "c1".into(),
        };
        let mut cfg = HarnessConfig::codex_default();
        cfg.sandbox = "workspace-write".into();
        let h = Codex::new(cfg);
        let inv = h.build(&spec).unwrap();
        assert_eq!(
            &inv.args[..5],
            ["exec", "-i", "/u/a.png", "--json", "--skip-git-repo-check"]
        );
        let di = inv
            .args
            .iter()
            .find_map(|a| a.strip_prefix("developer_instructions="))
            .unwrap();
        let parsed: toml::Table = toml::from_str(&format!("x = {di}")).unwrap();
        assert_eq!(parsed["x"].as_str(), Some("You are \"Iris\"\nline2"));
        assert!(
            inv.args
                .contains(&r#"model_reasoning_effort="low""#.to_string())
        );
        assert!(
            inv.args
                .contains(&r#"sandbox_mode="workspace-write""#.to_string())
        );
        assert!(
            inv.args
                .contains(&r#"sandbox_workspace_write.writable_roots=["/m"]"#.to_string())
        );
        assert!(
            inv.args
                .contains(&r#"mcp_servers.iris.args=["mcp", "--conversation", "c1"]"#.to_string())
        );
        assert!(
            inv.args
                .contains(&r#"mcp_servers.iris.env.IRIS_DATA_DIR="/d""#.to_string())
        );
        assert!(!inv.args.iter().any(|a| a.contains("bad key")));
        assert!(
            inv.args
                .contains(&r#"mcp_servers.iris.default_tools_approval_mode="approve""#.to_string())
        );
        assert_eq!(inv.args.last().unwrap(), "-");
        assert_eq!(inv.stdin, b"hello");

        spec.resume = Some("th-1".into());
        spec.images.clear();
        let inv = h.build(&spec).unwrap();
        assert_eq!(&inv.args[..3], ["exec", "resume", "--json"]);
        let n = inv.args.len();
        assert_eq!(&inv.args[n - 2..], ["th-1", "-"]);
    }

    #[test]
    fn developer_instructions_round_trip_as_toml() {
        let tricky = "quotes \" and \\ backslashes\nand 'single' and unicode é";
        let arg = format!("developer_instructions={}", toml_str(tricky));
        let (_, value) = arg.split_once('=').unwrap();
        let parsed: toml::Table = toml::from_str(&format!("x = {value}")).unwrap();
        assert_eq!(parsed["x"].as_str(), Some(tricky));
    }
}
