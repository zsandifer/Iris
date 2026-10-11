//! Google Gemini CLI adapter (`gemini -o stream-json`, prompt on stdin).
//!
//! Checked against gemini-cli 0.63 (flags, event shapes, and the signed-out
//! and bad-key failures were run for real; a signed-in turn was not, because
//! no Google account was available for verification):
//! - System prompt: `GEMINI_SYSTEM_MD=<private file>`, the CLI's supported
//!   override. It replaces Gemini's coding-agent prompt, so the file also
//!   lists the available tools (`${AvailableTools}`) and skills.
//! - MCP: Gemini reads MCP servers only from settings files. A per-run
//!   system settings path is ignored unless root owns its directory, so
//!   each conversation gets its own working directory under Iris's
//!   workspace with a private `.gemini/settings.json` (servers marked
//!   `trust`, i.e. pre-approved, like `--allowedTools` for Claude). The file
//!   is removed after the turn. Sessions are scoped by directory, which also
//!   keeps `--resume <session id>` per conversation.
//! - `--skip-trust` trusts Iris's own workspace for headless runs.
//! - Model: `-m` (aliases `flash`, `pro`, `flash-lite` resolve to the
//!   newest version). No effort flag.
//! - Images: `@<path>` references (the image directory is added with
//!   `--include-directories`). User text has `@` escaped and a leading `/`
//!   neutralized so it is never read as a file reference or slash command.

use super::{
    Activity, AuthState, Capabilities, Harness, HarnessEvent, Invocation, McpTransport,
    StreamParser, TurnSpec, looks_like_auth_error, mcp_tool_name, summarize,
};
use crate::config::{HarnessConfig, ensure_private_dir, write_private_file};
use crate::util::Result;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Gemini {
    cfg: HarnessConfig,
}

impl Gemini {
    pub fn new(cfg: HarnessConfig) -> Self {
        Self { cfg }
    }
}

/// Appended to the Iris prompt because GEMINI_SYSTEM_MD replaces the CLI's
/// own prompt. The `${...}` names are substituted by the CLI.
/// Gemini CLI's built-in tools (excluded for tool-less helper calls).
const GEMINI_BUILTIN_TOOLS: &[&str] = &[
    "run_shell_command",
    "write_file",
    "replace",
    "read_file",
    "read_many_files",
    "list_directory",
    "glob",
    "search_file_content",
    "web_fetch",
    "google_web_search",
    "save_memory",
    "write_todos",
];

const TOOLS_SUFFIX: &str = "
## Tools

Use these tools when they genuinely help; do not narrate tool use to the
user. Prefer reading over guessing, and never run destructive commands.

${AvailableTools}

${AgentSkills}
";

/// Directory name for one conversation (ids are hex; anything else is
/// reduced to a safe name).
fn scope_dir(cwd: &Path, scope: &str) -> PathBuf {
    let safe: String = scope
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    cwd.join("gemini")
        .join(if safe.is_empty() { "default" } else { &safe })
}

/// Gemini's `mcpServers` settings section.
pub fn mcp_settings(servers: &[super::McpServer]) -> Value {
    let mut map = Map::new();
    for s in servers {
        let v = match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                json!({"command": command, "args": args, "env": env, "trust": true})
            }
            McpTransport::Http { url, headers } => {
                json!({"httpUrl": url, "headers": headers, "trust": true})
            }
        };
        map.insert(s.name.clone(), v);
    }
    Value::Object(map)
}

/// Keep user text literal: no `@file` expansion, no slash commands.
pub fn escape_prompt(text: &str) -> String {
    let escaped = text.replace('@', "\\@");
    if escaped.trim_start().starts_with('/') {
        format!("\u{200B}{escaped}")
    } else {
        escaped
    }
}

impl Harness for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            images: true,
            effort: false,
            mcp: true,
            resume: true,
            tool_less: false,
        }
    }

    fn build(&self, spec: &TurnSpec) -> Result<Invocation> {
        let mut inv = Invocation::default();
        let dir = scope_dir(&spec.cwd, &spec.scope);
        ensure_private_dir(&dir.join(".gemini"))?;
        let settings = dir.join(".gemini").join("settings.json");
        // The Iris bridge resolves Library paths against this folder.
        let servers: Vec<super::McpServer> = spec
            .mcp_servers
            .iter()
            .cloned()
            .map(|mut s| {
                if let (true, McpTransport::Stdio { args, .. }) =
                    (s.name == "iris", &mut s.transport)
                {
                    args.push("--workdir".into());
                    args.push(dir.to_string_lossy().into_owned());
                }
                s
            })
            .collect();
        let mut body = json!({"mcpServers": mcp_settings(&servers)});
        if !spec.tools {
            // Best effort only: user-level MCP servers and discovered tools
            // can survive workspace settings, so the capability says
            // `tool_less: false` and helper calls skip Gemini. These
            // settings still narrow anything built this way.
            body["tools"] = json!({"exclude": GEMINI_BUILTIN_TOOLS});
            body["excludeTools"] = json!(GEMINI_BUILTIN_TOOLS);
            body["mcp"] = json!({"allowed": []});
            body["allowMCPServers"] = json!([]);
        }
        write_private_file(&settings, &serde_json::to_vec_pretty(&body)?)?;
        inv.temp_files.push(settings);

        let prompt_file = spec.run_dir.join(format!("{}.system.md", spec.turn_id));
        let system = if spec.tools {
            format!("{}\n{TOOLS_SUFFIX}", spec.system_prompt)
        } else {
            spec.system_prompt.clone()
        };
        write_private_file(&prompt_file, system.as_bytes())?;
        inv.temp_files.push(prompt_file.clone());
        inv.env.push((
            "GEMINI_SYSTEM_MD".into(),
            prompt_file.to_string_lossy().into_owned(),
        ));
        inv.cwd = Some(dir);

        let a = &mut inv.args;
        a.extend(["--skip-trust", "-o", "stream-json", "-m"].map(String::from));
        a.push(spec.model.clone());
        if let Some(id) = &spec.resume {
            a.push("--resume".into());
            a.push(id.clone());
        }
        if !spec.tools {
            a.extend(["--approval-mode", "default"].map(String::from));
        } else if !self.cfg.permission_mode.is_empty() {
            a.push("--approval-mode".into());
            a.push(self.cfg.permission_mode.clone());
        }
        let mut include: Vec<String> = spec
            .writable_dirs
            .iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        for img in &spec.images {
            if let Some(parent) = img.path.parent() {
                let p = parent.to_string_lossy().into_owned();
                if !include.contains(&p) {
                    include.push(p);
                }
            }
        }
        if !include.is_empty() {
            a.push("--include-directories".into());
            a.push(include.join(","));
        }
        a.extend(self.cfg.extra_args.iter().cloned());

        let mut prompt = escape_prompt(&spec.prompt);
        for img in &spec.images {
            prompt.push_str(&format!(
                "\n@{}",
                img.path.to_string_lossy().replace(' ', "\\ ")
            ));
        }
        inv.stdin = prompt.into_bytes();
        Ok(inv)
    }

    fn parser(&self, mcp_servers: &[String]) -> Box<dyn StreamParser> {
        Box::new(GeminiParser {
            servers: mcp_servers.to_vec(),
            ..GeminiParser::default()
        })
    }

    /// The system prompt file is read on every run, resumed or not.
    fn resume_updates_system_prompt(&self) -> bool {
        true
    }

    /// Gemini CLI has no sign-in status command; sign-in problems surface
    /// from turns ("Please set an Auth method", invalid key, expired login).
    fn auth_probe_args(&self) -> Option<Vec<String>> {
        None
    }

    fn parse_auth_probe(&self, _exit_ok: bool, _stdout: &str, _stderr: &str) -> AuthState {
        AuthState::Unknown
    }

    fn manual_login_hint(&self) -> String {
        "Run `gemini` on the Iris server once and choose Sign in with Google (Gemini CLI can only sign in from an interactive terminal).".into()
    }

    fn api_key_env(&self) -> &'static [&'static str] {
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"]
    }
}

/// Normalizes `-o stream-json` JSONL: `init`, `message` (assistant text
/// deltas), `tool_use`, `tool_result`, `error` (warnings and errors), and a
/// final `result` with `status` and `stats`.
#[derive(Default)]
pub struct GeminiParser {
    servers: Vec<String>,
    tools: HashMap<String, (String, String, String)>,
    last_error: Option<String>,
    finished: bool,
}

impl GeminiParser {
    fn classify(&self, raw: &str) -> (&'static str, String) {
        if let Some(n) = mcp_tool_name(raw, &self.servers, &["__", "_"]) {
            return ("mcp", n);
        }
        match raw {
            "run_shell_command" => ("command", "shell".into()),
            "write_file" | "replace" | "edit" => ("file_change", raw.into()),
            "google_web_search" | "web_fetch" => ("web_search", raw.into()),
            _ => ("tool", raw.into()),
        }
    }

    fn fail(&mut self, msg: String) -> HarnessEvent {
        self.finished = true;
        if looks_like_auth_error(&msg) {
            HarnessEvent::AuthRequired {
                message: summarize(&msg, 300),
            }
        } else {
            HarnessEvent::Error {
                message: summarize(&msg, 500),
            }
        }
    }
}

impl StreamParser for GeminiParser {
    fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return out;
        };
        if self.finished {
            return out;
        }
        match v["type"].as_str() {
            Some("init") => {
                if let Some(id) = v["session_id"].as_str() {
                    out.push(HarnessEvent::Session { id: id.into() });
                }
            }
            Some("message") if v["role"] == "assistant" => {
                if let Some(t) = v["content"].as_str().filter(|t| !t.is_empty()) {
                    out.push(HarnessEvent::TextDelta { text: t.into() });
                }
            }
            Some("tool_use") => {
                let id = v["tool_id"].as_str().unwrap_or("").to_string();
                let raw = v["tool_name"].as_str().unwrap_or("tool");
                let (kind, name) = self.classify(raw);
                let p = &v["parameters"];
                let summary = [
                    "file_path",
                    "absolute_path",
                    "command",
                    "query",
                    "url",
                    "pattern",
                    "path",
                ]
                .iter()
                .find_map(|k| p.get(*k).and_then(Value::as_str))
                .map(|s| summarize(s, 120))
                .unwrap_or_default();
                self.tools
                    .insert(id.clone(), (kind.into(), name.clone(), summary.clone()));
                out.push(HarnessEvent::Activity(Activity {
                    id,
                    kind: kind.into(),
                    name,
                    status: "started".into(),
                    summary,
                    auth_error: false,
                }));
            }
            Some("tool_result") => {
                let id = v["tool_id"].as_str().unwrap_or("").to_string();
                if let Some((kind, name, summary)) = self.tools.get(&id).cloned() {
                    let auth_error = v["status"] == "error"
                        && kind == "mcp"
                        && super::tool_auth_error(&super::tool_error_text(&v["error"]));
                    out.push(HarnessEvent::Activity(Activity {
                        id,
                        kind,
                        name,
                        status: if v["status"] == "error" {
                            "failed"
                        } else {
                            "completed"
                        }
                        .into(),
                        auth_error,
                        summary,
                    }));
                }
            }
            Some("error") => {
                if v["severity"] == "error" {
                    self.last_error = v["message"].as_str().map(String::from);
                }
            }
            Some("result") => {
                let s = &v["stats"];
                if !s.is_null() {
                    out.push(HarnessEvent::Usage {
                        usage: json!({
                            "input_tokens": s["input_tokens"],
                            "output_tokens": s["output_tokens"],
                            "cached_tokens": s["cached"],
                            "tool_calls": s["tool_calls"],
                            "duration_ms": s["duration_ms"],
                        }),
                    });
                }
                if v["status"] == "success" {
                    self.finished = true;
                    out.push(HarnessEvent::Done);
                } else {
                    let msg = v["error"]["message"]
                        .as_str()
                        .map(String::from)
                        .or_else(|| self.last_error.clone())
                        .unwrap_or_else(|| "Gemini CLI reported an error".into());
                    out.push(self.fail(msg));
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
        let tail = stderr_tail.trim();
        // Startup failures (no sign-in, bad resume) are plain stderr text.
        let msg = self
            .last_error
            .clone()
            .filter(|_| !looks_like_auth_error(tail))
            .unwrap_or_else(|| {
                tail.lines()
                    .find(|l| looks_like_auth_error(l))
                    .unwrap_or(tail.lines().next().unwrap_or(""))
                    .to_string()
            });
        if success && msg.is_empty() {
            self.finished = true;
            return vec![HarnessEvent::Done];
        }
        vec![self.fail(if msg.is_empty() {
            "Gemini CLI exited without a result".into()
        } else {
            msg
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{ImageInput, McpServer};
    use std::collections::BTreeMap;

    fn run(lines: &[Value], success: bool, stderr: &str) -> Vec<HarnessEvent> {
        let mut p = GeminiParser {
            servers: vec!["iris".into()],
            ..Default::default()
        };
        let mut out = Vec::new();
        for l in lines {
            out.extend(p.line(&l.to_string()));
        }
        out.extend(p.finish(success, stderr));
        out
    }

    /// Synthetic sample modeled on gemini-cli 0.63 stream-json output.
    #[test]
    fn parses_stream() {
        let lines = vec![
            json!({"type": "init", "timestamp": "t", "session_id": "0f0e-uuid", "model": "gemini-3-flash"}),
            json!({"type": "message", "role": "user", "content": "hi\n"}),
            json!({"type": "message", "role": "assistant", "content": "Checking ", "delta": true}),
            json!({"type": "tool_use", "tool_name": "read_file", "tool_id": "t1", "parameters": {"absolute_path": "/d/notes.md"}}),
            json!({"type": "tool_result", "tool_id": "t1", "status": "success", "output": "x"}),
            json!({"type": "tool_use", "tool_name": "iris__create_goal", "tool_id": "t2", "parameters": {}}),
            json!({"type": "tool_result", "tool_id": "t2", "status": "error", "error": {"type": "x", "message": "bad"}}),
            json!({"type": "error", "severity": "warning", "message": "Loop detected"}),
            json!({"type": "message", "role": "assistant", "content": "done.", "delta": true}),
            json!({"type": "result", "status": "success", "stats": {"input_tokens": 9, "output_tokens": 3}}),
        ];
        let out = run(&lines, true, "");
        assert_eq!(
            out[0],
            HarnessEvent::Session {
                id: "0f0e-uuid".into()
            }
        );
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Checking done.");
        let acts: Vec<&Activity> = out
            .iter()
            .filter_map(|e| match e {
                HarnessEvent::Activity(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 4);
        assert_eq!(acts[1].summary, "/d/notes.md");
        assert_eq!(acts[1].status, "completed");
        assert_eq!(
            (acts[3].name.as_str(), acts[3].status.as_str()),
            ("iris/create_goal", "failed")
        );
        assert!(matches!(out.last(), Some(HarnessEvent::Done)));
    }

    #[test]
    fn auth_failures_and_crash() {
        // Not signed in: plain stderr, exit 41.
        let out = run(
            &[],
            false,
            "Please set an Auth method in your /home/u/.gemini/settings.json or specify one of the following environment variables",
        );
        assert!(matches!(&out[..], [HarnessEvent::AuthRequired { .. }]));
        // Bad key: error result.
        let lines = vec![
            json!({"type": "init", "session_id": "s"}),
            json!({"type": "result", "status": "error", "error": {"type": "unknown",
                "message": "[API Error: API key not valid. Please pass a valid API key.]"}}),
        ];
        assert!(matches!(
            run(&lines, false, "").last(),
            Some(HarnessEvent::AuthRequired { .. })
        ));
        let lines = vec![
            json!({"type": "result", "status": "error", "error": {"message": "quota exceeded"}}),
        ];
        let out = run(&lines, false, "");
        assert!(
            matches!(out.last(), Some(HarnessEvent::Error { message }) if message == "quota exceeded")
        );
        assert_eq!(
            out.iter()
                .filter(|e| matches!(
                    e,
                    HarnessEvent::Error { .. }
                        | HarnessEvent::AuthRequired { .. }
                        | HarnessEvent::Done
                ))
                .count(),
            1
        );
        // Crash without a result.
        assert!(matches!(
            &run(&[], false, "Segmentation fault")[..],
            [HarnessEvent::Error { .. }]
        ));
        let resume = run(
            &[],
            false,
            "Error resuming session: No previous sessions found for this project.",
        );
        assert!(
            matches!(&resume[..], [HarnessEvent::Error { message }] if message.contains("resuming"))
        );
    }

    #[test]
    fn escapes_user_text() {
        assert_eq!(
            escape_prompt("mail me @ 5, @notes.md"),
            "mail me \\@ 5, \\@notes.md"
        );
        assert!(escape_prompt("/quit now").starts_with('\u{200B}'));
        assert_eq!(escape_prompt("a // b"), "a // b");
    }

    #[test]
    fn builds_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = TurnSpec {
            turn_id: "t1".into(),
            prompt: "what is in @this".into(),
            images: vec![ImageInput {
                path: tmp.path().join("uploads/a b.png"),
                mime: "image/png".into(),
            }],
            system_prompt: "You are Iris".into(),
            model: "flash".into(),
            effort: Some("low".into()),
            resume: Some("sess-1".into()),
            mcp_servers: vec![McpServer {
                name: "iris".into(),
                transport: McpTransport::Stdio {
                    command: "/bin/iris".into(),
                    args: vec!["mcp".into()],
                    env: BTreeMap::new(),
                },
            }],
            cwd: tmp.path().into(),
            writable_dirs: vec!["/data/memory".into()],
            run_dir: tmp.path().into(),
            scope: "conv/../1".into(),
            tools: true,
        };
        let inv = Gemini::new(HarnessConfig::gemini_default())
            .build(&spec)
            .unwrap();
        let args = inv.args.join(" ");
        assert!(
            args.starts_with("--skip-trust -o stream-json -m flash --resume sess-1"),
            "{args}"
        );
        assert!(args.contains("--approval-mode auto_edit"));
        assert!(args.contains(&format!(
            "--include-directories /data/memory,{}",
            tmp.path().join("uploads").display()
        )));
        assert!(!args.contains("effort") && !args.contains("low"));
        let cwd = inv.cwd.clone().unwrap();
        assert_eq!(cwd, tmp.path().join("gemini/conv1"));
        let settings: Value =
            serde_json::from_slice(&std::fs::read(cwd.join(".gemini/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["mcpServers"]["iris"]["trust"], true);
        assert_eq!(
            settings["mcpServers"]["iris"]["args"],
            json!(["mcp", "--workdir", cwd.to_string_lossy()])
        );
        assert_eq!(inv.env[0].0, "GEMINI_SYSTEM_MD");
        let system = std::fs::read_to_string(&inv.env[0].1).unwrap();
        assert!(system.starts_with("You are Iris") && system.contains("${AvailableTools}"));
        let stdin = String::from_utf8(inv.stdin).unwrap();
        assert!(stdin.starts_with("what is in \\@this\n@"));
        assert!(stdin.ends_with("a\\ b.png"));
        assert_eq!(inv.temp_files.len(), 2);
    }
}
