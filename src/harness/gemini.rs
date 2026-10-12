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
//! - API key: when the owner added one in the app, `GEMINI_API_KEY` is set
//!   and the workspace settings select `gemini-api-key` auth.
//! - Connector servers get a per-server `timeout` (ms) so a call waiting
//!   for the owner's approval is not cut off.
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
use base64::Engine;
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

/// Appended to the Iris prompt because GEMINI_SYSTEM_MD replaces the CLI's
/// own prompt. The `${...}` names are substituted by the CLI.
const TOOLS_SUFFIX: &str = "
## Tools

Use these tools when they genuinely help; do not narrate tool use to the
user. Prefer reading over guessing, and never run destructive commands.

${AvailableTools}

${AgentSkills}
";

/// Paths read by Gemini CLI 0.63. System overrides workspace; defaults
/// live beside the (possibly overridden) system settings file.
/// See packages/cli/src/config/settings.ts and core/src/utils/paths.ts at
/// https://github.com/google-gemini/gemini-cli/tree/v0.63.0/packages
fn settings_paths(workspace: &Path, env: impl Fn(&str) -> Option<PathBuf>) -> Vec<PathBuf> {
    let system = env("GEMINI_CLI_SYSTEM_SETTINGS_PATH").unwrap_or_else(|| {
        #[cfg(target_os = "macos")]
        let path = "/Library/Application Support/GeminiCli/settings.json";
        #[cfg(target_os = "windows")]
        let path = r"C:\ProgramData\gemini-cli\settings.json";
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let path = "/etc/gemini-cli/settings.json";
        PathBuf::from(path)
    });
    let defaults = env("GEMINI_CLI_SYSTEM_DEFAULTS_PATH")
        .unwrap_or_else(|| system.with_file_name("system-defaults.json"));
    // If HOME is unavailable, Node can use the OS account's home. An
    // unknown path triggers random aliases rather than assuming no servers.
    let home = env("GEMINI_CLI_HOME").or_else(|| env("HOME"));
    let mut paths = vec![
        defaults,
        home.map(|h| h.join(".gemini/settings.json"))
            .unwrap_or_default(),
        system,
    ];
    for path in &mut paths {
        if !path.as_os_str().is_empty() && path.is_relative() {
            *path = workspace.join(&*path);
        }
    }
    paths.push(workspace.join(".gemini/settings.json"));
    paths
}

/// Deserialize only server keys. All settings and server values (which
/// may contain credentials) are skipped, never retained or reported.
fn proxy_servers(servers: &[super::McpServer], paths: &[PathBuf]) -> Vec<super::McpServer> {
    #[derive(serde::Deserialize)]
    struct Names {
        #[serde(default, rename = "mcpServers")]
        servers: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
    }
    let mut taken = Vec::new();
    let mut uncertain = false;
    for path in paths {
        if path.as_os_str().is_empty() {
            uncertain = true;
            continue;
        }
        match std::fs::File::open(path) {
            Ok(file) => match serde_json::from_reader::<_, Names>(std::io::BufReader::new(file)) {
                Ok(names) => taken.extend(names.servers.into_keys()),
                // JSON with comments is also valid in Gemini. If we cannot
                // enumerate it, use unguessable per-turn names.
                Err(_) => uncertain = true,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => uncertain = true,
        }
    }
    let servers = if uncertain {
        servers
            .iter()
            .map(|s| super::McpServer {
                // Keep mcp_<server>_ within Gemini's first 30 characters
                // so its tool-name shortening preserves attribution.
                name: format!(
                    "iris_{}_{}",
                    s.name.chars().take(3).collect::<String>(),
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(crate::util::random_bytes::<12>())
                ),
                ..s.clone()
            })
            .collect::<Vec<_>>()
    } else {
        servers.to_vec()
    };
    super::unclaimed_servers(&servers, &taken)
}

/// Gemini's `mcpServers` settings section.
pub fn mcp_settings(servers: &[super::McpServer]) -> Value {
    let mut map = Map::new();
    for s in servers {
        let mut v = match &s.transport {
            McpTransport::Stdio { command, args, env } => {
                json!({"command": command, "args": args, "env": env, "trust": true})
            }
        };
        if let Some(t) = s.timeout_secs {
            v["timeout"] = (t * 1000).into();
        }
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
            // Restricted mode disables project memory and MCP connections in Gemini 0.63.
            mcp: false,
            resume: true,
            tool_less: false,
        }
    }

    fn build(&self, spec: &TurnSpec) -> Result<Invocation> {
        let mut inv = Invocation::default();
        inv.env
            .push(("GEMINI_RESTRICTED_MODE".into(), "true".into()));
        // 0.63 adds context.fileName to GEMINI.md; [] does not disable it.
        // Restricted mode is the supported switch for both initial and JIT project context.
        let policy_file = spec.run_dir.join(format!("{}.policy.toml", spec.turn_id));
        let mut rules = String::new();
        for tool in [
            "run_shell_command",
            "read_file",
            "read_many_files",
            "list_directory",
            "glob",
            "grep",
            "search_file_content",
            "write_file",
            "replace",
            "save_memory",
        ] {
            rules.push_str(&format!(
                "[[rule]]\ntoolName = \"{tool}\"\ndecision = \"deny\"\npriority = 999\n\n"
            ));
        }
        write_private_file(&policy_file, rules.as_bytes())?;
        inv.temp_files.push(policy_file.clone());
        let dir = spec.cwd.clone();
        if !spec.split {
            ensure_private_dir(&dir.join(".gemini"))?;
        }
        let settings = if spec.split {
            spec.run_dir.join(format!("{}.gemini.json", spec.turn_id))
        } else {
            dir.join(".gemini").join("settings.json")
        };
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
        let paths = settings_paths(&dir, |key| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        });
        let servers = proxy_servers(&servers, &paths);
        let names: Vec<String> = servers.iter().map(|s| s.name.clone()).collect();
        inv.mcp_server_names = Some(names.clone());
        let mut body = json!({"mcpServers": mcp_settings(&servers),
            "context": {"fileName": [], "includeDirectoryTree": false, "loadMemoryFromIncludeDirectories": false},
            "tools": {"exclude": ["run_shell_command"]}
        });
        if spec.key_env.iter().any(|k| k == "GEMINI_API_KEY") {
            // An API key the owner added in the app: use it even if a
            // Google sign-in was saved before (a saved auth type would
            // otherwise win over the environment).
            body["security"] = json!({"auth": {"selectedType": "gemini-api-key"}});
        }
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
        // Only Iris's MCP servers in every turn (not ones configured in
        // Gemini CLI itself or its extensions, which would bypass Iris's
        // gateway). Someone else's chat also gets no extensions (they may
        // carry the owner's accounts).
        a.push("--allowed-mcp-server-names".into());
        if names.is_empty() {
            // A non-existent, unguessable name keeps an empty turn closed.
            a.push(format!("iris_disabled_{}", crate::util::new_id()));
        }
        a.extend(names);
        a.extend(["-e", "none"].map(String::from));
        a.extend(["-o", "stream-json", "-m"].map(String::from));
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
        if self.cfg.extra_args.iter().any(|a| a == "--skip-trust") {
            return Err("Gemini project trust cannot be enabled for Iris turns".into());
        }
        a.extend([
            "--policy".into(),
            policy_file.to_string_lossy().into_owned(),
        ]);

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
    fn proxies_avoid_names_in_every_settings_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = settings_paths(tmp.path(), |key| {
            Some(tmp.path().join(match key {
                "GEMINI_CLI_SYSTEM_SETTINGS_PATH" => "system/settings.json",
                "GEMINI_CLI_SYSTEM_DEFAULTS_PATH" => "defaults.json",
                "GEMINI_CLI_HOME" => "home",
                _ => return None,
            }))
        });
        let server = McpServer {
            name: "github".into(),
            transport: McpTransport::Stdio {
                command: "iris".into(),
                args: vec!["mcp-gateway".into()],
                env: BTreeMap::new(),
            },
            timeout_secs: None,
        };
        for (path, name) in paths.iter().zip([
            "github",
            "iris_github",
            "iris_iris_github",
            "iris_iris_iris_github",
        ]) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, json!({"mcpServers": {name: {"command": "native", "env": {"SECRET": "do-not-retain"}}}, "unrelated": {"secret": "also-ignored"}}).to_string()).unwrap();
        }
        let proxies = proxy_servers(std::slice::from_ref(&server), &paths);
        assert_eq!(proxies[0].name, "iris_iris_iris_iris_github");
        assert_eq!(mcp_settings(&proxies)[&proxies[0].name]["command"], "iris");
        // Simulate an unreadable file without depending on uid/permissions.
        std::fs::remove_file(&paths[2]).unwrap();
        std::fs::create_dir(&paths[2]).unwrap();
        let a = proxy_servers(std::slice::from_ref(&server), &paths);
        let b = proxy_servers(&[server], &paths);
        assert!(a[0].name.starts_with("iris_git_"));
        assert_ne!(a[0].name, b[0].name);
        assert_eq!(a[0].name.len(), 25);
        let parser = GeminiParser {
            servers: vec![a[0].name.clone()],
            ..Default::default()
        };
        assert_eq!(
            parser
                .classify(&format!("mcp_{}_search_messages", a[0].name))
                .0,
            "mcp"
        );
    }

    #[test]
    fn settings_overrides_and_default_paths_match_gemini() {
        let workspace = Path::new("/workspace");
        let paths = settings_paths(workspace, |key| match key {
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH" => Some("/system/custom.json".into()),
            "HOME" => Some("/user".into()),
            _ => None,
        });
        assert_eq!(
            paths,
            [
                PathBuf::from("/system/system-defaults.json"),
                "/user/.gemini/settings.json".into(),
                "/system/custom.json".into(),
                "/workspace/.gemini/settings.json".into()
            ]
        );
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
                timeout_secs: Some(420),
            }],
            cwd: tmp.path().into(),
            writable_dirs: vec!["/data/memory".into()],
            run_dir: tmp.path().into(),
            scope: "conv/../1".into(),
            tools: true,
            key_env: vec!["GEMINI_API_KEY".into()],
            isolated: false,
            own_mcp_servers: Vec::new(),
            protected_paths: vec![
                "/private/iris".into(),
                "/private/config".into(),
                "/home/agent/.codex/auth.json".into(),
            ],
            split: false,
        };
        let inv = Gemini::new(HarnessConfig::gemini_default())
            .build(&spec)
            .unwrap();
        let args = inv.args.join(" ");
        let name = &inv.mcp_server_names.as_ref().unwrap()[0];
        assert!(
            args.starts_with(&format!(
                "--allowed-mcp-server-names {name} -e none -o stream-json -m flash --resume sess-1"
            )),
            "{args}"
        );
        assert!(args.contains("--approval-mode auto_edit"));
        assert!(args.contains(&format!(
            "--include-directories /data/memory,{}",
            tmp.path().join("uploads").display()
        )));
        assert!(!inv.args.iter().any(|a| a == "low" || a.contains("effort")));
        let cwd = inv.cwd.clone().unwrap();
        assert_eq!(cwd, tmp.path());
        assert!(
            inv.env
                .contains(&("GEMINI_RESTRICTED_MODE".into(), "true".into()))
        );
        assert!(!inv.args.iter().any(|a| a == "--skip-trust"));
        let settings: Value =
            serde_json::from_slice(&std::fs::read(cwd.join(".gemini/settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["mcpServers"][name]["trust"], true);
        assert_eq!(settings["mcpServers"][name]["timeout"], 420_000);
        assert_eq!(
            settings["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec![name]
        );
        assert_eq!(
            settings["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
        assert_eq!(
            settings["mcpServers"][name]["args"],
            json!(["mcp", "--workdir", cwd.to_string_lossy()])
        );
        let (_, prompt_file) = inv
            .env
            .iter()
            .find(|(k, _)| k == "GEMINI_SYSTEM_MD")
            .unwrap();
        let system = std::fs::read_to_string(prompt_file).unwrap();
        assert!(system.starts_with("You are Iris") && system.contains("${AvailableTools}"));
        let stdin = String::from_utf8(inv.stdin).unwrap();
        assert!(stdin.starts_with("what is in \\@this\n@"));
        assert!(stdin.ends_with("a\\ b.png"));
        assert_eq!(inv.temp_files.len(), 3);
        // Someone else's chat: only Iris's MCP servers, no extensions.
        let mut iso = spec.clone();
        iso.isolated = true;
        let inv = Gemini::new(HarnessConfig::gemini_default())
            .build(&iso)
            .unwrap();
        let args = inv.args.join(" ");
        let name = &inv.mcp_server_names.as_ref().unwrap()[0];
        assert!(
            args.starts_with(&format!("--allowed-mcp-server-names {name} -e none -o")),
            "{args}"
        );
        let plain = Gemini::new(HarnessConfig::gemini_default())
            .build(&spec)
            .unwrap();
        assert!(plain.args.iter().any(|a| a == "-e"));
        let mut empty = spec;
        empty.mcp_servers.clear();
        let inv = Gemini::new(HarnessConfig::gemini_default())
            .build(&empty)
            .unwrap();
        assert!(inv.args[1].starts_with("iris_disabled_"));
        assert_ne!(inv.args[1], "iris");
    }
}
