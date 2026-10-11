//! Harness adapters. Iris wraps the official, already-installed and
//! signed-in CLIs; it never implements an agent loop or tools of its own.
//!
//! An adapter only knows how to (1) turn a [`TurnSpec`] into a command line
//! and stdin payload, (2) normalize the CLI's JSONL output into
//! [`HarnessEvent`]s, and (3) ask the CLI for its version and sign-in state.
//! Process management (concurrency cap, timeout, kill) lives in `runner`.
//!
//! Compliance: adapters never read, store, or forward OAuth tokens. The CLIs
//! handle their own credentials; Iris only looks at the CLI's own
//! "logged in?" answer and at error text in the output stream.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod runner;

use crate::util::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// An image attached to the user's message.
#[derive(Debug, Clone)]
pub struct ImageInput {
    pub path: PathBuf,
    pub mime: String,
}

/// An MCP server passed to the harness for one turn.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServer {
    /// `[A-Za-z0-9_-]+`; also the tool prefix (`mcp__<name>__<tool>`).
    pub name: String,
    pub transport: McpTransport,
    /// How long one tool call may take (connector calls can wait for the
    /// owner's approval). None: the CLI's default.
    pub timeout_secs: Option<u64>,
}

/// How the CLI starts an MCP server. Everything Iris passes is a local
/// stdio program: its own bridge (`iris mcp`) and one connector gateway
/// proxy per connector (`iris mcp-gateway`); remote connectors are reached
/// by the gateway, never by the CLI.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

/// Everything an adapter needs to build one CLI invocation.
#[derive(Debug, Clone)]
pub struct TurnSpec {
    /// Unique id used to name per-turn private files.
    pub turn_id: String,
    /// The user message (including any replayed context).
    pub prompt: String,
    pub images: Vec<ImageInput>,
    /// Iris system prompt, injected before the user message.
    pub system_prompt: String,
    /// CLI model value, e.g. "haiku" or "gpt-6-luna".
    pub model: String,
    pub effort: Option<String>,
    /// Harness session/thread id to resume.
    pub resume: Option<String>,
    pub mcp_servers: Vec<McpServer>,
    /// Harness working directory (stable, so sessions can be resumed).
    pub cwd: PathBuf,
    /// Extra directories the agent may write (memory).
    pub writable_dirs: Vec<PathBuf>,
    /// Private directory for per-turn files (0700).
    pub run_dir: PathBuf,
    /// Stable key for per-conversation state some CLIs keep on disk
    /// (the conversation id).
    pub scope: String,
    /// Names of API-key variables Iris passes to this run (keys the owner
    /// added in the app; values are added to the environment by the turn).
    pub key_env: Vec<String>,
    /// Not the owner's own chat: the CLI may use only `mcp_servers`, none
    /// of the MCP servers, memory, or account integrations configured in
    /// the CLI itself (those are the owner's).
    pub isolated: bool,
    /// MCP servers configured in the CLI itself, for CLIs that can only
    /// switch them off by name (see `Harness::own_mcp_servers_args`).
    pub own_mcp_servers: Vec<String>,
}

/// A fully built command line.
#[derive(Debug, Clone, Default)]
pub struct Invocation {
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    /// Private files to delete when the turn ends.
    pub temp_files: Vec<PathBuf>,
    /// Extra environment variables for the CLI process: paths to private
    /// per-turn files and settings; the turn adds the owner's engine API
    /// keys, if any. Unlike argv, a process's environment is readable only
    /// by its own user. Never logged.
    pub env: Vec<(String, String)>,
    /// Working directory override (defaults to `TurnSpec::cwd`).
    pub cwd: Option<PathBuf>,
}

/// What a harness can do. Features degrade per flag: images are dropped
/// with a conversational note, effort is omitted, and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// Accepts image attachments (individual models may still refuse).
    pub images: bool,
    /// Accepts a reasoning-effort setting.
    pub effort: bool,
    /// Accepts per-turn MCP servers (the Iris bridge and connectors).
    pub mcp: bool,
    /// Can resume a previous session.
    pub resume: bool,
}

/// How the CLI's own login command works headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginFlow {
    /// Prints an authorize URL, then reads the code the user copies after
    /// approving (Claude Code).
    CodePaste,
    /// Prints a URL and a one-time code, then waits until the user
    /// approves on another device (Codex device auth).
    DeviceCode,
}

/// The CLI's official login command, as Iris runs it on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSpec {
    pub args: Vec<String>,
    pub flow: LoginFlow,
}

/// A model the CLI itself reports as available (models cache or a
/// `models` command). Metadata only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscoveredModel {
    /// Value passed to the CLI's model flag.
    pub model: String,
    pub label: Option<String>,
    pub description: Option<String>,
    /// Supported efforts; empty means "use the harness defaults".
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    /// Accepts image input, if the CLI says.
    pub images: Option<bool>,
    /// Runs on this machine (or the local network), e.g. Ollama.
    pub local: bool,
    /// Upstream provider id, e.g. "ollama", "anthropic".
    pub provider: Option<String>,
    /// The CLI hides it from its own picker.
    pub hidden: bool,
}

/// Normalized tool-use activity (shown as "Searching the web..." etc.).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Activity {
    pub id: String,
    /// tool | command | mcp | web_search | file_change
    pub kind: String,
    /// Tool name, e.g. "Read", "shell", "iris/create_goal".
    pub name: String,
    /// started | completed | failed
    pub status: String,
    /// Short human-readable summary (file path, command, query).
    pub summary: String,
    /// A failed tool call whose error looks like an expired or rejected
    /// sign-in (used to offer reconnecting a connector).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_error: bool,
}

/// Error text of a failed tool result (string or content blocks).
pub fn tool_error_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i["text"].as_str().or_else(|| i.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Error text that means a connector's credentials were rejected.
pub fn tool_auth_error(text: &str) -> bool {
    let l = text.to_lowercase();
    looks_like_auth_error(&l)
        || [
            "invalid_grant",
            "unauthorized",
            "401",
            "403 forbidden",
            "token expired",
            "expired token",
            "invalid_token",
            "reauthenticate",
            "re-authenticate",
        ]
        .iter()
        .any(|p| l.contains(p))
}

/// Normalized stream event emitted by a parser.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// The CLI's session/thread id, for `--resume`.
    Session {
        id: String,
    },
    TextDelta {
        text: String,
    },
    Activity(Activity),
    Usage {
        usage: Value,
    },
    /// The CLI is not signed in or its login expired.
    AuthRequired {
        message: String,
    },
    Error {
        message: String,
    },
    /// The turn finished successfully.
    Done,
}

/// Sign-in state reported by the CLI itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    Ok,
    AuthRequired,
    Unknown,
}

pub trait StreamParser: Send {
    /// Feed one stdout line (one JSON object).
    fn line(&mut self, line: &str) -> Vec<HarnessEvent>;
    /// The process exited; emit a terminal event if none was seen.
    fn finish(&mut self, success: bool, stderr_tail: &str) -> Vec<HarnessEvent>;
}

pub trait Harness: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Build the CLI arguments and stdin for one turn.
    fn build(&self, spec: &TurnSpec) -> Result<Invocation>;
    /// A parser for one run. `mcp_servers` are the names passed in the
    /// turn, so tool names can be attributed to their server.
    fn parser(&self, mcp_servers: &[String]) -> Box<dyn StreamParser>;
    /// Whether a resumed session picks up a changed system prompt. If not,
    /// Iris starts a fresh session (with context replay) when the persona
    /// changes.
    fn resume_updates_system_prompt(&self) -> bool;
    /// Arguments for a cheap command that reports sign-in state, or None
    /// if the CLI has none (sign-in state is then learned from turns).
    fn auth_probe_args(&self) -> Option<Vec<String>>;
    fn parse_auth_probe(&self, exit_ok: bool, stdout: &str, stderr: &str) -> AuthState;
    /// Environment variables removed from the subprocess so the CLI uses
    /// its signed-in subscription rather than API billing.
    fn api_key_env(&self) -> &'static [&'static str];
    /// Arguments for a command that lists the models the CLI can use right
    /// now (cheap, local, no model request).
    fn models_probe_args(&self) -> Option<Vec<String>> {
        None
    }
    fn parse_models(&self, _stdout: &str) -> Vec<DiscoveredModel> {
        Vec::new()
    }
    /// For CLIs that can only switch off their own MCP servers by name: a
    /// cheap local command listing them (run before isolated turns).
    fn own_mcp_servers_args(&self) -> Option<Vec<String>> {
        None
    }
    /// Names from that command's output (nothing else of it is kept), or
    /// None if it cannot be read, so the CLI is not used rather than
    /// leaving the owner's servers on.
    fn parse_own_mcp_servers(&self, _stdout: &str) -> Option<Vec<String>> {
        Some(Vec::new())
    }
    /// The CLI's own login command when it can run without a terminal, so
    /// Iris can relay it to the app. None: sign-in needs the server.
    fn login(&self) -> Option<LoginSpec> {
        None
    }
    /// What the owner runs on the server when `login` is None (or fails).
    fn manual_login_hint(&self) -> String;
}

/// Phrases CLIs print when the user must sign in again. Matched
/// case-insensitively against error text only.
const AUTH_PHRASES: &[&str] = &[
    "not logged in",
    "please run /login",
    "run `claude login`",
    "run claude login",
    "codex login",
    "authentication_failed",
    "oauth token has expired",
    "token has expired",
    "invalid api key",
    "401 unauthorized",
    "missing bearer",
    "please log in",
    "login required",
    "sign in again",
    "refresh token",
    "set an auth method",
    "api key not valid",
    "manual authorization is required",
    "failed to authenticate",
    "providerautherror",
];

pub fn looks_like_auth_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    AUTH_PHRASES.iter().any(|p| lower.contains(p))
}

/// Classify an MCP-ish tool name (`<server><sep><tool>`) against the
/// servers passed to the turn. Returns `server/tool`.
pub fn mcp_tool_name(raw: &str, servers: &[String], seps: &[&str]) -> Option<String> {
    let raw = raw
        .strip_prefix("mcp__")
        .or_else(|| raw.strip_prefix("mcp_"))
        .unwrap_or(raw);
    servers.iter().find_map(|s| {
        seps.iter().find_map(|sep| {
            raw.strip_prefix(s.as_str())
                .and_then(|rest| rest.strip_prefix(sep))
                .filter(|t| !t.is_empty())
                .map(|t| format!("{s}/{t}"))
        })
    })
}

/// Keep summaries short and single-line.
pub fn summarize(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::util::truncate_chars(&one_line, max)
}

/// Engines directory `bin/` (set once at startup); searched first.
static ENGINES_BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_engines_bin(dir: PathBuf) {
    let _ = ENGINES_BIN.set(dir);
}

/// Whether a resolved program is one Iris installed (and so manages).
pub fn is_managed(program: &Path) -> bool {
    ENGINES_BIN.get().is_some_and(|b| program.starts_with(b))
}

/// Resolve a configured command to an executable path. Engines Iris
/// installed come first; services started by systemd often have a minimal
/// PATH, so common per-user install locations are searched too.
pub fn resolve_command(command: &str) -> Option<PathBuf> {
    let p = Path::new(command);
    if command.contains('/') {
        return is_executable(p).then(|| p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = ENGINES_BIN.get().cloned().into_iter().collect();
    dirs.extend(
        std::env::var_os("PATH")
            .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for rel in [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".cargo/bin",
            "bin",
        ] {
            dirs.push(home.join(rel));
        }
    }
    dirs.extend(["/usr/local/bin", "/usr/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    dirs.into_iter()
        .map(|d| d.join(command))
        .find(|c| is_executable(c))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Valid MCP server name (also safe as a TOML key and tool prefix).
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_phrases() {
        assert!(looks_like_auth_error("Not logged in · Please run /login"));
        assert!(looks_like_auth_error(
            "unexpected status 401 Unauthorized: Missing bearer or basic authentication"
        ));
        assert!(!looks_like_auth_error("rate limited, try again"));
    }

    #[test]
    fn summaries_are_single_line() {
        assert_eq!(summarize("ls\n  -la   /tmp", 50), "ls -la /tmp");
        assert_eq!(summarize("abcdef", 3), "abc…");
    }

    #[test]
    fn server_names() {
        assert!(valid_server_name("iris"));
        assert!(valid_server_name("google-calendar_2"));
        assert!(!valid_server_name("a.b"));
        assert!(!valid_server_name(""));
        assert!(!valid_server_name("x y"));
    }

    #[test]
    fn resolve_finds_absolute_and_path_commands() {
        assert!(resolve_command("sh").is_some());
        assert!(resolve_command("/bin/sh").is_some());
        assert!(resolve_command("definitely-not-a-real-binary-xyz").is_none());
    }
}
