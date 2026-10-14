//! `iris mcp`: a small stdio MCP server the harness CLI launches on every
//! turn. It is a thin CRUD bridge into Iris's database so the agent can
//! update the app (goals, ideas, feed, library, cards). It is not a tool
//! layer: no shell, web, or file access beyond saving into the Library.
//!
//! Protocol: JSON-RPC 2.0, one message per line on stdin/stdout (MCP stdio
//! transport). Diagnostics go to stderr.

use crate::config::{Paths, ensure_private_dir, write_private_file};
use crate::db::Db;
use crate::entities::{self, MAX_BODY, MAX_TITLE, NewFeedPost, check_text};
use crate::util::{Result, new_id};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct Bridge {
    pub db: Db,
    pub paths: Paths,
    pub user_id: String,
    pub conversation_id: Option<String>,
    /// The assistant message this turn is writing (memory provenance).
    pub message_id: Option<String>,
    /// When this bridge started (ms): about when the turn began.
    pub started_at: i64,
    /// Conversation with more than one person: only cards are offered, so
    /// nothing reads or changes the owner's personal data.
    pub shared: bool,
    /// The harness's working directory when it is not the workspace itself
    /// (Gemini runs in a per-conversation folder); relative library paths
    /// resolve against it.
    pub workdir: Option<PathBuf>,
    pub agent: crate::config::AgentConfig,
    pub status_token: Option<String>,
}

/// Tools available in shared (group) conversations.
const SHARED_TOOLS: &[&str] = &["create_card"];

/// Largest `write_memory` content for an export file (the whole file).
const MAX_MEMORY_FILE: usize = 1_000_000;

/// Run the stdio loop until stdin closes.
pub fn run(bridge: Bridge) -> Result<()> {
    serve_stdio(|req| bridge.handle(req))
}

/// `iris mcp --browser`: the `browser` tools. Calls are forwarded to the
/// server that owns Iris's browser (see `browser::socket`); this process
/// never touches the browser, the vault, or the database itself.
pub struct BrowserBridge {
    pub socket: PathBuf,
    /// Identifies the turn (from `IRIS_BROWSER_TOKEN`).
    pub token: String,
}

impl BrowserBridge {
    pub fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned()?;
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => initialize_result(
                req,
                "browser",
                "Iris's own web browser (Playwright), which the user can watch and take over \
                 from their phone. Saved logins and cards are filled in by Iris; you never see \
                 them.",
            ),
            "ping" => json!({}),
            "tools/list" => {
                let r = crate::browser::socket::forward(
                    &self.socket,
                    &self.token,
                    "tools/list",
                    &json!({}),
                );
                if r["tools"].is_array() {
                    r
                } else {
                    json!({"tools": []})
                }
            }
            "tools/call" => crate::browser::socket::forward(
                &self.socket,
                &self.token,
                "tools/call",
                &json!({
                    "name": req["params"]["name"].as_str().unwrap_or(""),
                    "arguments": req["params"].get("arguments").cloned().unwrap_or_else(|| json!({})),
                }),
            ),
            "resources/list" => json!({"resources": []}),
            "prompts/list" => json!({"prompts": []}),
            _ => return Some(error_response(id, -32601, "method not found")),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

fn initialize_result(req: &Value, name: &str, instructions: &str) -> Value {
    let requested = req["params"]["protocolVersion"].as_str().unwrap_or("");
    let version = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        SUPPORTED_VERSIONS[1]
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": name, "version": env!("CARGO_PKG_VERSION")},
        "instructions": instructions,
    })
}

/// JSON-RPC over stdio, one message per line.
pub fn serve_stdio(handle: impl Fn(&Value) -> Option<Value>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&req),
            Err(_) => Some(error_response(Value::Null, -32700, "parse error")),
        };
        if let Some(resp) = response {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

pub fn tool_definitions() -> Vec<Value> {
    let s = |d: &str| json!({"type": "string", "description": d});
    vec![
        tool(
            "system_status",
            "Read fresh, non-sensitive Iris system status.",
            json!({}),
            &[],
        ),
        tool(
            "request_engine_signin",
            "Request an owner sign-in card; never signs in directly.",
            json!({"engine": s("Engine name")}),
            &["engine"],
        ),
        tool(
            "request_engine_install",
            "Request an owner install card; never installs directly.",
            json!({"engine": s("Engine name")}),
            &["engine"],
        ),
        tool(
            "request_connector_setup",
            "Request a catalog connect or reconnect card; never connects directly.",
            json!({"name": {"type": "string", "enum": crate::connectors::CATALOG.iter().map(|e| e.id).collect::<Vec<_>>()}}),
            &["name"],
        ),
        tool(
            "search_memory",
            "Recall compact memory hits. Use get_memory for full notes and history.",
            json!({"query": s("Search words"), "topic": s("you|people|projects|routines"), "since": {"type":"integer"}}),
            &["query"],
        ),
        tool(
            "get_memory",
            "Read up to 20 current notes with sources and history.",
            json!({"ids": {"type":"array", "items":{"type":"integer"}, "maxItems":20}}),
            &["ids"],
        ),
        tool(
            "remember",
            "Save a note, excluding secrets and private spans. Similar notes are hints only; update instead of merging automatically.",
            json!({"text":s("Note"), "kind":s("profile|preference|fact|event|person|project|routine"), "importance":{"type":"integer","minimum":1,"maximum":5}, "observed_at":{"type":["integer","null"]}}),
            &["text", "kind"],
        ),
        tool(
            "update_memory",
            "Correct a note by ID; changed text supersedes its prior version with undo history.",
            json!({"id":{"type":"integer"}, "text":s("Corrected note"), "kind":s("Memory kind"), "importance":{"type":"integer","minimum":1,"maximum":5}, "observed_at":{"type":["integer","null"]}, "valid_to":{"type":["integer","null"]}}),
            &["id"],
        ),
        tool(
            "forget",
            "Forget a note by ID, preserving an undo tombstone.",
            json!({"id":{"type":"integer"}}),
            &["id"],
        ),
        tool(
            "search_conversations",
            "Search the owner's past private chats for compact source excerpts.",
            json!({"query":s("Search words")}),
            &["query"],
        ),
        tool(
            "read_memory",
            "Read one of the owner's memory files: MEMORY.md (always in mind) or a topic \
             file (you.md, people.md, projects.md, routines.md).",
            json!({"path": s("MEMORY.md or a topic file such as people.md")}),
            &["path"],
        ),
        tool(
            "write_memory",
            "Save the owner's memory. Write MEMORY.md (short, always-relevant facts) or a \
             topic file (you.md, people.md, projects.md, routines.md) with its full new \
             content, one note per bullet: new bullets are saved and bullets you leave out \
             are removed everywhere they appear. Any other path only adds notes.",
            json!({"path": s("MEMORY.md or a topic file such as people.md"), "content": s("Markdown, one note per bullet")}),
            &["path", "content"],
        ),
        tool(
            "list_goals",
            "List the user's goals (open ones first).",
            json!({"include_done": {"type": "boolean", "description": "Include completed goals"}}),
            &[],
        ),
        tool(
            "create_goal",
            "Create a goal the user wants to track. Set `suggested` when you are proposing a \
             goal the user did not ask for; they confirm it in the app.",
            json!({
                "title": s("Short goal title"),
                "status_text": s("Current progress note"),
                "suggested": {"type": "boolean", "description": "A proposal the user still has to confirm"},
            }),
            &["title"],
        ),
        tool(
            "update_goal",
            "Update a goal's title, progress note, or completion.",
            json!({
                "id": s("Goal id"),
                "title": s("New title"),
                "status_text": s("New progress note"),
                "done": {"type": "boolean", "description": "Mark complete (true) or reopen (false)"},
            }),
            &["id"],
        ),
        tool(
            "list_ideas",
            "List ideas (tasks you proposed to the user).",
            json!({"status": {"type": "string", "enum": entities::IDEA_STATUSES}}),
            &[],
        ),
        tool(
            "create_idea",
            "Propose a task you could do for the user. Tapping it starts a chat.",
            json!({
                "title": s("Short, actionable title"),
                "description": s("One or two sentences on why it helps"),
                "prompt": s("Message sent to you when the user taps the idea"),
            }),
            &["title"],
        ),
        tool(
            "list_feed_posts",
            "List recent posts in the user's feed.",
            json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50}}),
            &[],
        ),
        tool(
            "create_feed_post",
            "Publish a post to the user's feed about something they care about.",
            json!({
                "title": s("Headline"),
                "body": s("The post (Markdown, a few short paragraphs at most)"),
                "topic": s("Short topic label"),
                "source_url": s("Link to the source, if any"),
                "source_name": s("Name of the source, e.g. the site or creator"),
                "image_url": s("Cover image URL (an original photo from the source page)"),
                "images": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": entities::MAX_FEED_IMAGES,
                    "description": "The source page's original photo URLs, in order (never generated images)",
                },
                "why": s("One short line on why the user would like this"),
            }),
            &["title", "body"],
        ),
        tool(
            "save_to_library",
            "Save an artifact (document, note, draft) or media file to the user's Library. \
             Provide either `content` (text) or `path` (a file in your working folder).",
            json!({
                "title": s("Title shown in the Library"),
                "content": s("Text content to save"),
                "path": s("Path of a file in the working folder to save instead of content"),
                "mime": s("MIME type, e.g. text/markdown (default) or image/png"),
                "kind": {"type": "string", "enum": ["artifact", "media"]},
                "description": s("Short description"),
            }),
            &["title"],
        ),
        tool(
            "list_library",
            "List items in the user's Library.",
            json!({"kind": {"type": "string", "enum": ["artifact", "media"]}}),
            &[],
        ),
        tool(
            "create_card",
            "Show a card in the current chat. Use type `connect` (with url and service) to help \
             the user connect an account, `link` for a link, `info` for a highlighted note, \
             `result` for one concrete result (a place, product, or booking) with an image, \
             details, and one action.",
            json!({
                "type": {"type": "string", "enum": entities::CARD_TYPES},
                "title": s("Card title"),
                "body": s("Short supporting text"),
                "url": s("https URL the card opens"),
                "image_url": s("https URL of an image for the card (result cards)"),
                "action_label": s("Button label, e.g. 'Connect'"),
                "service": s("Service name for connect cards, e.g. 'google'"),
                "ref_id": s("Id of a related goal, idea, or feed post, if any"),
            }),
            &["type", "title"],
        ),
        tool(
            "list_connectors",
            "List connected services (MCP connectors), whether they are on and signed in, \
             and the services the user can still connect (`available_to_connect`).",
            json!({}),
            &[],
        ),
    ]
}

impl Bridge {
    /// Handle one JSON-RPC message. Returns None for notifications.
    pub fn handle(&self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req["method"].as_str().unwrap_or("");
        let Some(id) = id else {
            // Notifications (e.g. notifications/initialized) need no reply.
            return None;
        };
        let result = match method {
            "initialize" => initialize_result(
                req,
                "iris",
                "Iris app bridge: update the user's goals, ideas, feed, library, and show \
                 cards in the chat.",
            ),
            "ping" => json!({}),
            "tools/list" => {
                let tools: Vec<Value> = tool_definitions()
                    .into_iter()
                    .filter(|t| {
                        let name = t["name"].as_str().unwrap_or("");
                        (!crate::awareness::TOOLS.contains(&name)
                            || (self.status_token.is_some() && self.private_status_context()))
                            && (!self.shared || SHARED_TOOLS.contains(&name))
                            && (!(matches!(name, "read_memory" | "write_memory")
                                || crate::memory::recall::TOOLS.contains(&name))
                                || self.private_status_context())
                    })
                    .collect();
                json!({"tools": tools})
            }
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.call(name, &args) {
                    Ok(v) => json!({
                        "content": [{"type": "text", "text": v.to_string()}],
                        "structuredContent": v,
                        "isError": false,
                    }),
                    Err(e) => json!({
                        "content": [{"type": "text", "text": format!("Error: {e}")}],
                        "isError": true,
                    }),
                }
            }
            "resources/list" => json!({"resources": []}),
            "prompts/list" => json!({"prompts": []}),
            _ => return Some(error_response(id, -32601, "method not found")),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// The owner's own chat: the only place the owner's connectors and
    /// memory folder are reachable.
    fn owner_context(&self) -> bool {
        !self.shared
            && self.conversation_id.as_deref().is_some_and(|conv| {
                self.db
                    .with(|c| {
                        Ok(
                            crate::db::owner_user_id(c)?.as_deref() == Some(self.user_id.as_str())
                                && crate::db::conversation_member_ids(c, conv)?
                                    == [self.user_id.clone()],
                        )
                    })
                    .unwrap_or(false)
            })
    }

    /// When this turn began (ms): its message was created then; a bridge
    /// started later (or restarted mid-turn) still dates changes from it.
    fn turn_started(&self) -> i64 {
        self.message_id
            .as_deref()
            .and_then(|id| {
                self.db
                    .with(|c| crate::db::get_message(c, id))
                    .ok()
                    .flatten()
            })
            .map_or(self.started_at, |m| m.created_at.min(self.started_at))
    }

    /// One memory batch per turn, so the turn's changes undo together and
    /// later writes in the turn can change them again.
    fn turn_batch(&self) -> String {
        match &self.message_id {
            Some(id) => format!("turn-{id}"),
            None => format!("bridge-{}-{}", std::process::id(), self.started_at),
        }
    }

    fn private_status_context(&self) -> bool {
        self.owner_context()
    }

    fn call(&self, name: &str, a: &Value) -> std::result::Result<Value, String> {
        if self.shared && !SHARED_TOOLS.contains(&name) {
            return Err(format!("{name} is not available in shared conversations"));
        }
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let uid = self.user_id.as_str();
        let db_err = |e: crate::util::BoxError| e.to_string();
        if crate::awareness::TOOLS.contains(&name) {
            if !self.private_status_context() {
                return Ok(json!({"outcome": "not allowed"}));
            }
            let Some(token) = &self.status_token else {
                return Ok(json!({"outcome": "not allowed"}));
            };
            let proxy = crate::gateway::proxy::Proxy {
                socket: crate::gateway::socket_path(&self.paths),
                connector: String::new(),
                token: token.clone(),
            };
            let reply = proxy.forward_envelope(
                &json!({"id": 1, "method": "tools/call", "params": {"name": name, "arguments": a}}),
                true,
            );
            if reply.get("error").is_some() {
                return Err("this private turn is no longer available".into());
            }
            return Ok(reply["result"].clone());
        }
        if crate::memory::recall::TOOLS.contains(&name) {
            if !self.private_status_context() {
                return Err("I can only use memory in the owner's private chats.".into());
            }
            let source = json!({"type":"conversation", "conversation_id": self.conversation_id, "message_ids": self.message_id.iter().collect::<Vec<_>>()});
            let batch = self.turn_batch();
            let turn_started = self.turn_started();
            let out = self.db.with(|c| {
                crate::db::write_tx(c, |c| {
                    // Recheck membership while holding the write lock.
                    if crate::db::conversation_member_ids(
                        c,
                        self.conversation_id.as_deref().unwrap(),
                    )? != [self.user_id.clone()]
                    {
                        return Err(crate::memory::recall::invalid(
                            "I can only use memory in the owner's private chats.",
                        ));
                    }
                    if matches!(name, "remember" | "update_memory" | "forget") {
                        crate::memory::dream::learning_allowed(c)?;
                        crate::memory::lifecycle::write_allowed(c, turn_started)?;
                    }
                    use crate::memory::recall as r;
                    let id = || {
                        a["id"]
                            .as_i64()
                            .filter(|i| *i > 0)
                            .ok_or_else(|| r::invalid("Please give me a memory ID."))
                    };
                    match name {
                        "search_memory" => {
                            if s("query").is_none() {
                                return Err(r::invalid("Please give me words to search."));
                            }
                            let f: r::Filter = serde_json::from_value(a.clone())
                                .map_err(|_| r::invalid("I couldn't read those search options."))?;
                            let out = r::list(c, &f, true)?;
                            crate::memory::files::render_export(c, &self.paths)?;
                            Ok(out)
                        }
                        "search_conversations" => r::conversations(
                            c,
                            s("query")
                                .ok_or_else(|| r::invalid("Please give me words to search."))?,
                        ),
                        "get_memory" => {
                            let ids = a["ids"]
                                .as_array()
                                .filter(|ids| !ids.is_empty() && ids.len() <= 20)
                                .ok_or_else(|| {
                                    r::invalid("Please choose between 1 and 20 memory IDs.")
                                })?;
                            let mut seen = std::collections::HashSet::new();
                            let mut items = Vec::new();
                            for id in ids {
                                let id = id
                                    .as_i64()
                                    .filter(|i| *i > 0)
                                    .ok_or_else(|| r::invalid("Please give me memory IDs."))?;
                                if seen.insert(id) {
                                    items.push(r::detail(c, id, true)?);
                                }
                            }
                            crate::memory::files::render_export(c, &self.paths)?;
                            Ok(json!({"items":items}))
                        }
                        "forget" => r::mutate(c, &self.paths, |c| {
                            r::forget(c, id()?, crate::memory::Actor::Agent, &batch)
                        }),
                        _ => {
                            let mut fields = a.clone();
                            fields
                                .as_object_mut()
                                .ok_or_else(|| r::invalid("Please give me a note."))?
                                .remove("id");
                            let edit: r::Edit = serde_json::from_value(fields)
                                .map_err(|_| r::invalid("I couldn't read those note fields."))?;
                            if edit.pinned.is_some() {
                                return Err(r::invalid(
                                    "Pinning is available to the owner in Memory.",
                                ));
                            }
                            if name == "remember" && (edit.text.is_none() || edit.kind.is_none()) {
                                return Err(r::invalid("Please give me the note and its kind."));
                            }
                            r::mutate(c, &self.paths, |c| {
                                r::save(
                                    c,
                                    &edit,
                                    if name == "update_memory" {
                                        Some(id()?)
                                    } else {
                                        None
                                    },
                                    crate::memory::Actor::Agent,
                                    &source,
                                    &batch,
                                )
                            })
                        }
                    }
                })
            });
            if out.is_err() {
                // The outer write transaction has rolled back; repair an
                // export that failed partway through writing.
                let _ = self.db.with(|c| {
                    crate::db::write_tx(c, |c| crate::memory::files::render_export(c, &self.paths))
                });
            }
            return out.map_err(db_err);
        }
        match name {
            "read_memory" | "write_memory" => {
                if !self.owner_context() {
                    return Err(
                        "memory is only available in the owner's private conversations".into(),
                    );
                }
                let path = s("path").ok_or("path is required")?;
                let relative = Path::new(path);
                if relative.is_absolute()
                    || relative.extension().and_then(|e| e.to_str()) != Some("md")
                    || relative
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err("memory path must be a relative Markdown path".into());
                }
                if name == "read_memory" {
                    let bytes = crate::agent::read_scoped_file(&self.paths.memory_dir(), path)
                        .map_err(|_| {
                            "there is no such memory file; memory is in MEMORY.md and the topic \
                             files listed in your instructions"
                        })?;
                    let text = String::from_utf8(bytes).map_err(|_| "memory must be UTF-8")?;
                    Ok(json!({"content": text}))
                } else {
                    let content = s("content").ok_or("content is required")?;
                    let export = crate::memory::files::is_export_name(path);
                    // A topic file holds every note of its topic and is
                    // written back whole.
                    check_text(
                        "content",
                        content,
                        if export { MAX_MEMORY_FILE } else { MAX_BODY },
                        false,
                    )?;
                    let source = json!({
                        "type": "conversation",
                        "conversation_id": self.conversation_id,
                        "message_ids": self.message_id.iter().collect::<Vec<_>>(),
                    });
                    let w = crate::memory::files::write(
                        &self.db,
                        &self.paths,
                        path,
                        content,
                        &source,
                        self.turn_started(),
                        &self.turn_batch(),
                    )
                    .map_err(db_err)?;
                    let mut out = json!({
                        "written": true,
                        "added": w.added,
                        "unchanged": w.unchanged,
                        "removed": w.removed,
                        "kept_pinned": w.kept_pinned,
                        "kept_recent": w.kept_recent,
                        "not_restored": w.not_restored,
                        "outside_core": w.outside_core,
                    });
                    if !export {
                        out["note"] = json!(
                            "Saved as notes in the topic files. Next time write MEMORY.md, \
                             you.md, people.md, projects.md or routines.md."
                        );
                    }
                    if !w.outside_core.is_empty() {
                        out["note"] = json!(
                            "MEMORY.md is full, so the outside_core notes are saved in their \
                             topic files only. Read them there when they come up."
                        );
                    }
                    if !w.kept_recent.is_empty() {
                        out["note"] = json!(
                            "The kept_recent notes were saved or changed in another chat, \
                             or entered this core after the turn began without being \
                             explicitly written here by this turn, so they were kept. \
                             They can be removed in a later turn if they are wrong."
                        );
                    }
                    if !w.not_restored.is_empty() {
                        out["note"] = json!(
                            "The not_restored notes were removed or corrected from another \
                             chat after you started, so they were not saved again."
                        );
                    }
                    if !w.kept_pinned.is_empty() {
                        out["note"] = json!(
                            "The owner pinned the kept_pinned notes; only they can remove them."
                        );
                    }
                    Ok(out)
                }
            }
            "list_goals" => {
                let include_done = a["include_done"].as_bool().unwrap_or(false);
                let goals = self
                    .db
                    .with(|c| entities::list_goals(c, uid, include_done))
                    .map_err(db_err)?;
                Ok(json!({"goals": goals}))
            }
            "create_goal" => {
                let title = s("title").unwrap_or("");
                let status = s("status_text").unwrap_or("");
                let suggested = a["suggested"].as_bool().unwrap_or(false);
                check_text("title", title, MAX_TITLE, true)?;
                check_text("status_text", status, 2000, false)?;
                let g = self
                    .db
                    .with(|c| entities::create_goal(c, uid, title, status, suggested))
                    .map_err(db_err)?;
                Ok(json!({"goal": g}))
            }
            "update_goal" => {
                let id = s("id").ok_or("id is required")?;
                if let Some(t) = s("title") {
                    check_text("title", t, MAX_TITLE, true)?;
                }
                if let Some(t) = s("status_text") {
                    check_text("status_text", t, 2000, false)?;
                }
                let g = self
                    .db
                    .with(|c| {
                        entities::update_goal(
                            c,
                            uid,
                            id,
                            &entities::GoalChanges {
                                title: s("title"),
                                status_text: s("status_text"),
                                done: a["done"].as_bool(),
                                suggested: None,
                            },
                        )
                    })
                    .map_err(db_err)?
                    .ok_or("no goal with that id")?;
                Ok(json!({"goal": g}))
            }
            "list_ideas" => {
                let ideas = self
                    .db
                    .with(|c| entities::list_ideas(c, uid, s("status")))
                    .map_err(db_err)?;
                Ok(json!({"ideas": ideas}))
            }
            "create_idea" => {
                let title = s("title").unwrap_or("");
                check_text("title", title, MAX_TITLE, true)?;
                check_text("description", s("description").unwrap_or(""), 2000, false)?;
                check_text("prompt", s("prompt").unwrap_or(""), 4000, false)?;
                let i = self
                    .db
                    .with(|c| {
                        entities::create_idea(
                            c,
                            uid,
                            title,
                            s("description").unwrap_or(""),
                            s("prompt"),
                        )
                    })
                    .map_err(db_err)?;
                Ok(json!({"idea": i}))
            }
            "list_feed_posts" => {
                let limit = a["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
                let posts = self
                    .db
                    .with(|c| entities::list_feed(c, uid, None, limit))
                    .map_err(db_err)?;
                Ok(json!({"posts": posts}))
            }
            "create_feed_post" => {
                let p = NewFeedPost {
                    title: s("title").unwrap_or("").to_string(),
                    body: s("body").unwrap_or("").to_string(),
                    topic: s("topic").map(String::from),
                    source_url: s("source_url").map(String::from),
                    image_url: s("image_url").map(String::from),
                    images: match a.get("images") {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(v)) => v
                            .iter()
                            .map(|x| x.as_str().map(String::from).ok_or("images must be strings"))
                            .collect::<std::result::Result<_, _>>()?,
                        Some(_) => return Err("images must be an array of URLs".into()),
                    },
                    source_name: s("source_name").map(String::from),
                    why: s("why").map(String::from),
                };
                p.validate()?;
                let post = self
                    .db
                    .with(|c| entities::create_feed_post(c, uid, &p))
                    .map_err(db_err)?;
                Ok(json!({"post": post}))
            }
            "save_to_library" => self.save_to_library(a),
            "list_library" => {
                let items = self
                    .db
                    .with(|c| entities::list_library(c, uid, s("kind")))
                    .map_err(db_err)?;
                Ok(json!({"items": items}))
            }
            "create_card" => {
                let conv = self
                    .conversation_id
                    .as_deref()
                    .ok_or("no current conversation")?;
                let card = entities::validate_card(a)?;
                // In the owner's own chat the card may carry private context:
                // it stays hidden until the turn is saved, which publishes it
                // only if nobody else has joined meanwhile.
                let status = if self.owner_context() {
                    "pending"
                } else {
                    "complete"
                };
                let m = self
                    .db
                    .with(|c| {
                        entities::insert_card_with_status(c, conv, &card, "assistant", status)
                    })
                    .map_err(db_err)?;
                Ok(json!({"card_message_id": m.id, "shown": true}))
            }
            "list_connectors" => {
                if !self.owner_context() {
                    // Connectors are the owner's accounts.
                    return Ok(json!({"connectors": []}));
                }
                let list = self
                    .db
                    .with(|c| entities::list_connectors(c, false))
                    .map_err(db_err)?;
                let items: Vec<Value> = list
                    .iter()
                    .map(|c| {
                        json!({"name": c.name, "service": c.service,
                               "description": c.description, "enabled": c.enabled,
                               "signed_in": c.auth_state == "ok",
                               "paused": c.paused_reason.is_some(),
                               "rules": c.policy.mode.describe()})
                    })
                    .collect();
                let available: Vec<Value> = crate::connectors::CATALOG
                    .iter()
                    .filter(|e| e.id == "custom" || !list.iter().any(|c| c.service == e.id))
                    .map(
                        |e| json!({"service": e.id, "name": e.label, "description": e.description}),
                    )
                    .collect();
                Ok(json!({"connectors": items, "available_to_connect": available}))
            }
            other => Err(format!("unknown tool {other}")),
        }
    }

    fn save_to_library(&self, a: &Value) -> std::result::Result<Value, String> {
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let title = s("title").unwrap_or("");
        check_text("title", title, MAX_TITLE, true)?;
        let description = s("description").unwrap_or("");
        check_text("description", description, 2000, false)?;
        let id = new_id();
        let lib = self.paths.library_dir();
        ensure_private_dir(&lib).map_err(|e| e.to_string())?;
        let (mime, size, file_name) = match (s("content"), s("path")) {
            (Some(content), None) => {
                check_text("content", content, MAX_BODY * 20, false)?;
                let mime = s("mime").unwrap_or("text/markdown");
                if !mime.starts_with("text/") && mime != "application/json" {
                    return Err("text content needs a text/* or application/json mime".into());
                }
                let file_name = format!("{id}.{}", ext_for_mime(mime));
                write_private_file(&lib.join(&file_name), content.as_bytes())
                    .map_err(|e| e.to_string())?;
                (mime.to_string(), content.len() as i64, file_name)
            }
            (None, Some(path)) => {
                let bytes = if self.agent.user.is_empty() {
                    let root = self.conversation_workdir()?;
                    crate::agent::read_scoped_file(&root, path).map_err(|e| e.to_string())?
                } else {
                    crate::agent::read_file(
                        &self.agent,
                        self.conversation_id
                            .as_deref()
                            .ok_or("no current conversation")?,
                        path,
                    )
                    .map_err(|e| e.to_string())?
                };
                let ext = Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("bin")
                    .to_ascii_lowercase();
                let mime = s("mime")
                    .map(String::from)
                    .unwrap_or_else(|| mime_for_ext(&ext).to_string());
                let file_name = format!("{id}.{}", sanitize_ext(&ext));
                write_private_file(&lib.join(&file_name), &bytes).map_err(|e| e.to_string())?;
                (mime, bytes.len() as i64, file_name)
            }
            _ => return Err("provide exactly one of `content` or `path`".into()),
        };
        let kind = s("kind").unwrap_or(
            if mime.starts_with("image/")
                || mime.starts_with("audio/")
                || mime.starts_with("video/")
            {
                "media"
            } else {
                "artifact"
            },
        );
        if !matches!(kind, "artifact" | "media") {
            return Err("kind must be artifact or media".into());
        }
        let item = self
            .db
            .with(|c| {
                entities::insert_library_item(
                    c,
                    &self.user_id,
                    &entities::NewLibraryItem {
                        id: &id,
                        kind,
                        title,
                        description,
                        mime: &mime,
                        file_name: &file_name,
                        size,
                        conversation_id: self.conversation_id.as_deref(),
                    },
                )
            })
            .map_err(|e| e.to_string())?;
        Ok(json!({"item": item}))
    }

    fn conversation_workdir(&self) -> std::result::Result<PathBuf, String> {
        let id = self
            .conversation_id
            .as_deref()
            .ok_or("no current conversation")?;
        if !crate::agent::valid_scope(id) {
            return Err("invalid conversation id".into());
        }
        let expected = self.paths.workspace_dir().join(id);
        if self.workdir.as_ref().is_some_and(|w| w != &expected) {
            return Err("working directory does not match the conversation".into());
        }
        Ok(expected)
    }
}

fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "text/markdown" => "md",
        "text/html" => "html",
        "text/csv" => "csv",
        "application/json" => "json",
        _ => "txt",
    }
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn sanitize_ext(ext: &str) -> String {
    let e: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect();
    if e.is_empty() { "bin".into() } else { e }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn bridge(tmp: &Path) -> Bridge {
        let paths = Paths {
            config_file: tmp.join("config.toml"),
            data_dir: tmp.join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        let (user_id, conv) = db
            .with(|c| {
                let u = db::owner_user_id(c)?.unwrap();
                let m = db::ensure_main_conversation(c, &u)?;
                Ok((u, m))
            })
            .unwrap();
        crate::agent::workdir(&crate::config::AgentConfig::default(), &paths, &conv).unwrap();
        Bridge {
            db,
            paths,
            user_id,
            conversation_id: Some(conv),
            message_id: Some("m1".into()),
            started_at: 0,
            shared: false,
            workdir: None,
            agent: crate::config::AgentConfig::default(),
            status_token: None,
        }
    }

    #[test]
    fn shared_mode_only_offers_cards() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        b.shared = true;
        b.status_token = Some("turn-token".into());
        let tools = b
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .unwrap();
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["create_card"]);
        for tool in crate::awareness::TOOLS {
            assert_eq!(call(&b, tool, json!({}))["isError"], true);
        }
        assert_eq!(call(&b, "list_goals", json!({}))["isError"], true);
        let r = call(&b, "create_card", json!({"type": "info", "title": "Hello"}));
        assert_eq!(r["isError"], false);
    }

    fn call(b: &Bridge, name: &str, args: Value) -> Value {
        b.handle(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                          "params": {"name": name, "arguments": args}}))
            .unwrap()["result"]
            .clone()
    }

    #[test]
    fn handshake_and_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let init = b
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                            "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "iris");
        let odd = b
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
                            "params": {"protocolVersion": "1999-01-01"}}))
            .unwrap();
        assert_eq!(odd["result"]["protocolVersion"], SUPPORTED_VERSIONS[1]);
        assert!(
            b.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
        let tools = b
            .handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
            .unwrap();
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"create_goal".to_string()));
        assert!(names.contains(&"create_card".to_string()));
        let unknown = b
            .handle(&json!({"jsonrpc": "2.0", "id": 4, "method": "bogus"}))
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn browser_bridge_lists_tools_and_forwards_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let b = BrowserBridge {
            socket: tmp.path().join("missing.sock"),
            token: "t".into(),
        };
        let init = b
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                            "params": {"protocolVersion": "2025-06-18"}}))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "browser");
        let tools = b
            .handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        // No server: no tools, and calls get a friendly tool error, not a
        // protocol error.
        assert_eq!(tools["result"]["tools"], json!([]));
        let r = b
            .handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                            "params": {"name": "browser_snapshot", "arguments": {}}}))
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(
            b.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
    }

    #[test]
    fn goal_crud_through_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let r = call(
            &b,
            "create_goal",
            json!({"title": "Read 12 books", "status_text": "1 of 12"}),
        );
        assert_eq!(r["isError"], false);
        let id = r["structuredContent"]["goal"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let r = call(
            &b,
            "update_goal",
            json!({"id": id, "status_text": "2 of 12", "done": false}),
        );
        assert_eq!(r["structuredContent"]["goal"]["status_text"], "2 of 12");
        let r = call(&b, "list_goals", json!({}));
        assert_eq!(r["structuredContent"]["goals"].as_array().unwrap().len(), 1);
        let r = call(&b, "create_goal", json!({"title": ""}));
        assert_eq!(r["isError"], true);
        let r = call(&b, "update_goal", json!({"id": "missing"}));
        assert_eq!(r["isError"], true);
        // Visible to the API layer.
        let goals =
            b.db.with(|c| entities::list_goals(c, &b.user_id, true))
                .unwrap();
        assert_eq!(goals[0].title, "Read 12 books");
    }

    #[test]
    fn feed_ideas_cards_and_library() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let r = call(
            &b,
            "create_feed_post",
            json!({"title": "T", "body": "B", "source_url": "https://example.com"}),
        );
        assert_eq!(r["isError"], false);
        let r = call(
            &b,
            "create_feed_post",
            json!({"title": "T", "body": "B", "source_url": "ftp://x"}),
        );
        assert_eq!(r["isError"], true);
        let r = call(
            &b,
            "create_idea",
            json!({"title": "Plan dinner", "prompt": "Plan dinner for Friday"}),
        );
        assert_eq!(r["structuredContent"]["idea"]["status"], "open");
        let r = call(
            &b,
            "create_card",
            json!({"type": "connect", "title": "Connect Google",
            "url": "https://accounts.google.com", "service": "google"}),
        );
        assert_eq!(r["structuredContent"]["shown"], true);
        let conv = b.conversation_id.clone().unwrap();
        // The owner's own chat: held back until the turn is saved.
        let card_id = r["structuredContent"]["card_message_id"]
            .as_str()
            .unwrap()
            .to_string();
        let card =
            b.db.with(|c| db::get_message(c, &card_id))
                .unwrap()
                .unwrap();
        assert_eq!(
            (card.kind.as_str(), card.status.as_str()),
            ("card", "pending")
        );
        let msgs =
            b.db.with(|c| db::list_messages(c, &conv, None, 10))
                .unwrap();
        assert!(
            msgs.iter().all(|m| m.id != card_id),
            "hidden until published"
        );

        let r = call(
            &b,
            "save_to_library",
            json!({"title": "Packing list", "content": "- socks"}),
        );
        assert_eq!(r["structuredContent"]["item"]["mime"], "text/markdown");
        std::fs::write(
            b.conversation_workdir().unwrap().join("chart.png"),
            b"\x89PNG\r\n\x1a\n",
        )
        .unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "Chart", "path": "chart.png"}),
        );
        assert_eq!(r["structuredContent"]["item"]["kind"], "media");
        // No escaping the workspace.
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "x", "path": "/etc/hostname"}),
        );
        assert_eq!(r["isError"], true);
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "x", "path": "../iris.db"}),
        );
        assert_eq!(r["isError"], true);
        std::os::unix::fs::symlink(
            "/etc/hostname",
            b.conversation_workdir().unwrap().join("link.txt"),
        )
        .unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "x", "path": "link.txt"}),
        );
        assert_eq!(r["isError"], true);
        let r = call(&b, "list_library", json!({}));
        assert_eq!(r["structuredContent"]["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn library_paths_follow_the_harness_working_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        let ws = b.paths.workspace_dir();
        let gem = ws.join(b.conversation_id.as_deref().unwrap());
        std::fs::create_dir_all(&gem).unwrap();
        std::fs::write(gem.join("report.md"), "# From Gemini").unwrap();
        std::fs::write(ws.join("report.md"), "# Unrelated").unwrap();
        b.workdir = Some(gem.clone());
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "Report", "path": "report.md"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(
            r["structuredContent"]["item"]["size"],
            "# From Gemini".len()
        );
        // A working folder outside this conversation is refused.
        b.workdir = Some(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("report.md"), "# Outside").unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "Report", "path": "report.md"}),
        );
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn library_cannot_save_other_conversations_or_symlinked_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let other = b.paths.workspace_dir().join("another-chat");
        std::fs::create_dir(&other).unwrap();
        let file = other.join("report.md");
        std::fs::write(&file, "another conversation").unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "report", "path": file}),
        );
        assert_eq!(r["isError"], true);
        std::os::unix::fs::symlink(&other, b.conversation_workdir().unwrap().join("link")).unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "report", "path": "link/report.md"}),
        );
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn memory_tools_are_owner_only_and_reject_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        // The export file view: read, add a bullet, write back.
        let r = call(&b, "read_memory", json!({"path": "MEMORY.md"}));
        let core = r["structuredContent"]["content"]
            .as_str()
            .unwrap()
            .to_string();
        let r = call(
            &b,
            "write_memory",
            json!({"path": "MEMORY.md", "content": format!("{core}\n- Prefers mornings.\n")}),
        );
        assert_eq!(r["isError"], false, "{r}");
        assert_eq!(r["structuredContent"]["added"], 1);
        assert_eq!(r["structuredContent"]["outside_core"], json!([]));
        assert!(r["structuredContent"].get("note").is_none());
        let r = call(&b, "read_memory", json!({"path": "MEMORY.md"}));
        assert!(
            r["structuredContent"]["content"]
                .as_str()
                .unwrap()
                .contains("- Prefers mornings.")
        );
        let item = b.db.with(crate::memory::live).unwrap().pop().unwrap();
        assert_eq!(item.source["conversation_id"], json!(b.conversation_id));
        assert_eq!(item.source["message_ids"], json!(["m1"]));
        // Old-style note paths add notes; nothing is written there.
        let r = call(
            &b,
            "write_memory",
            json!({"path": "notes/preferences.md", "content": "Likes tea."}),
        );
        assert_eq!(r["isError"], false, "{r}");
        assert!(r["structuredContent"]["note"].is_string());
        assert!(!b.paths.memory_dir().join("notes").exists());
        let r = call(&b, "read_memory", json!({"path": "you.md"}));
        assert!(
            r["structuredContent"]["content"]
                .as_str()
                .unwrap()
                .contains("- Preferences: Likes tea.")
        );
        let r = call(&b, "read_memory", json!({"path": "notes/preferences.md"}));
        assert_eq!(r["isError"], true);
        for path in [
            "../iris.db",
            "/etc/example.md",
            "../outside.md",
            "file.txt",
            "./MEMORY.md",
        ] {
            let r = call(&b, "write_memory", json!({"path": path, "content": "- x"}));
            assert_eq!(r["isError"], true, "{path}");
        }
        std::os::unix::fs::symlink(tmp.path(), b.paths.memory_dir().join("link")).unwrap();
        std::fs::write(tmp.path().join("note.md"), "outside").unwrap();
        let r = call(&b, "read_memory", json!({"path": "link/note.md"}));
        assert_eq!(r["isError"], true);
        let r = call(
            &b,
            "write_memory",
            json!({"path": "link/outside.md", "content": "- x"}),
        );
        assert_eq!(r["isError"], false, "a note, never a file: {r}");
        assert!(!tmp.path().join("outside.md").exists());
        b.shared = true;
        for name in ["read_memory", "write_memory"] {
            let r = call(&b, name, json!({"path": "MEMORY.md", "content": "- x"}));
            assert_eq!(r["isError"], true);
        }
        let listed = b.handle(&json!({"id": 1, "method": "tools/list"})).unwrap();
        assert!(
            !listed["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "read_memory" || t["name"] == "write_memory")
        );
        // Another person's own chat cannot reach memory either.
        b.shared = false;
        b.db.with(|c| {
            c.execute(
                "INSERT INTO users (id, display_name, is_owner, created_at) VALUES ('g1', 'Guest', 0, 0)",
                [],
            )
        })
        .unwrap();
        b.user_id = "g1".into();
        let before = b.db.with(crate::memory::live).unwrap().len();
        for name in ["read_memory", "write_memory"] {
            let r = call(
                &b,
                name,
                json!({"path": "MEMORY.md", "content": "- guest fact"}),
            );
            assert_eq!(r["isError"], true, "{name}");
        }
        assert_eq!(b.db.with(crate::memory::live).unwrap().len(), before);
    }

    #[test]
    fn reset_blocks_existing_turn_writes_including_legacy_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        assert_eq!(
            call(
                &b,
                "remember",
                json!({"text":"Old garden note","kind":"fact"})
            )["isError"],
            false
        );
        b.db.with(|c| crate::memory::lifecycle::forget_everything(c, &b.paths))
            .unwrap();
        for (tool, args) in [
            ("remember", json!({"text":"Old garden note","kind":"fact"})),
            (
                "write_memory",
                json!({"path":"MEMORY.md","content":"- Old garden note"}),
            ),
        ] {
            let out = call(&b, tool, args);
            assert_eq!(out["isError"], true, "{out}");
            assert!(
                out["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("new message")
            );
        }
        assert_eq!(
            call(&b, "search_memory", json!({"query":"garden"}))["structuredContent"]["hits"],
            json!([])
        );
        b.started_at =
            b.db.with(|c| {
                Ok(
                    crate::db::get_setting(c, crate::memory::lifecycle::RESET_AFTER)?
                        .unwrap()
                        .parse::<i64>()
                        .unwrap()
                        + 1,
                )
            })
            .unwrap();
        b.message_id = None;
        assert_eq!(
            call(
                &b,
                "remember",
                json!({"text":"New garden note","kind":"fact"})
            )["isError"],
            false
        );
    }

    #[test]
    fn pause_learning_blocks_every_agent_memory_write_but_keeps_recall() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let saved = call(&b, "remember", json!({"text":"Likes hiking","kind":"fact"}));
        let id = saved["structuredContent"]["item"]["id"].as_i64().unwrap();
        b.db.with(|c| crate::db::set_setting(c, crate::memory::dream::PAUSED, "true"))
            .unwrap();
        for (tool, args) in [
            ("remember", json!({"text":"Likes biking","kind":"fact"})),
            ("update_memory", json!({"id":id,"text":"Likes biking"})),
            ("forget", json!({"id":id})),
            (
                "write_memory",
                json!({"path":"MEMORY.md","content":"- Likes biking"}),
            ),
        ] {
            let out = call(&b, tool, args);
            assert_eq!(out["isError"], true, "{tool}: {out}");
            assert!(out.to_string().contains("paused learning"), "{out}");
        }
        assert_eq!(
            call(&b, "search_memory", json!({"query":"hiking"}))["isError"],
            false
        );
        assert_eq!(b.db.with(crate::memory::live).unwrap().len(), 1);
    }

    #[test]
    fn recall_tools_write_gate_history_and_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        let saved = call(
            &b,
            "remember",
            json!({"text":"Likes climbing <private>synthetic-canary</private>","kind":"preference", "importance":4, "observed_at":100}),
        );
        assert_eq!(saved["isError"], false, "{saved}");
        let id = saved["structuredContent"]["item"]["id"].as_i64().unwrap();
        assert_eq!(saved["structuredContent"]["item"]["observed_at"], 100);
        let found = call(
            &b,
            "search_memory",
            json!({"query":"climbing", "topic":"you", "since":0}),
        );
        assert_eq!(found["structuredContent"]["hits"][0]["id"], id, "{found}");
        let full = call(&b, "get_memory", json!({"ids":[id,id]}));
        assert_eq!(
            full["structuredContent"]["items"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            full["structuredContent"]["items"][0]["item"]["use_count"],
            2
        );
        assert_eq!(
            full["structuredContent"]["items"][0]["item"]["source"]["message_ids"],
            json!(["m1"])
        );
        assert!(!full.to_string().contains("synthetic-canary"));
        assert_eq!(
            call(
                &b,
                "remember",
                json!({"text":"password: synthetic-canary", "kind":"fact"})
            )["isError"],
            true
        );
        let replaced = call(
            &b,
            "update_memory",
            json!({"id":id,"text":"Likes swimming"}),
        );
        assert_eq!(replaced["isError"], false, "{replaced}");
        let next = replaced["structuredContent"]["item"]["id"]
            .as_i64()
            .unwrap();
        assert_ne!(next, id);
        assert_eq!(call(&b, "forget", json!({"id":next}))["isError"], false);
        assert_eq!(
            call(&b, "get_memory", json!({"ids":[next]}))["isError"],
            true
        );
        let note = call(
            &b,
            "remember",
            json!({"text":"Likes hiking", "kind":"fact"}),
        );
        let live_id = note["structuredContent"]["item"]["id"].as_i64().unwrap();
        let args = [
            ("search_memory", json!({"query":"hiking"})),
            ("get_memory", json!({"ids":[live_id]})),
            (
                "remember",
                json!({"text":"Another safe note", "kind":"fact"}),
            ),
            ("update_memory", json!({"id":live_id,"importance":5})),
            ("forget", json!({"id":live_id})),
            ("search_conversations", json!({"query":"practice"})),
        ];
        let snapshot = |b: &Bridge| {
            b.db.with(|c| Ok(json!({"notes":crate::memory::live(c)?,"history":c.query_row("SELECT COUNT(*) FROM memory_history",[],|r| r.get::<_,i64>(0))?}))).unwrap()
        };
        let before = snapshot(&b);
        let denied = |b: &Bridge| {
            for (name, args) in &args {
                let reply = call(b, name, args.clone());
                assert_eq!(reply["isError"], true, "{name}: {reply}");
                let error = reply["content"][0]["text"].as_str().unwrap();
                assert!(
                    error.contains("shared conversations")
                        || error.contains("owner's private chats"),
                    "{name}: {error}"
                );
            }
            assert_eq!(snapshot(b), before);
            let tools = b.handle(&json!({"id":1,"method":"tools/list"})).unwrap();
            assert!(
                !tools["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| crate::memory::recall::TOOLS.contains(&t["name"].as_str().unwrap()))
            );
        };
        b.shared = true;
        denied(&b);
        b.shared = false;
        b.db.with(|c| {
            c.execute("INSERT INTO users(id,display_name,created_at) VALUES ('guest','Guest',0)", [])?;
            c.execute("INSERT INTO conversation_members(conversation_id,user_id,joined_at) VALUES (?1,'guest',0)", [b.conversation_id.as_deref().unwrap()])?;
            Ok(())
        }).unwrap();
        denied(&b); // A bridge created before the join must not retain access.
        let guest_chat =
            b.db.with(|c| crate::db::create_conversation(c, "guest", "side", "Guest chat", None))
                .unwrap();
        b.user_id = "guest".into();
        b.conversation_id = Some(guest_chat.id);
        denied(&b); // A guest's own sole-member conversation is also denied.
    }

    #[test]
    fn aws_secret_access_keys_never_reach_notes_history_or_exports() {
        for secret in [
            "AWS_SECRET_ACCESS_KEY=synthetic-canary",
            "aws_secret_access_key = synthetic-canary",
            r#"{"SecretAccessKey": "synthetic-canary"}"#,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let b = bridge(tmp.path());
            let saved = call(
                &b,
                "remember",
                json!({"text":format!("Likes tea\n{secret}"),"kind":"fact"}),
            );
            assert_eq!(saved["isError"], false);
            assert_eq!(saved["structuredContent"]["item"]["text"], "Likes tea");
            let id = saved["structuredContent"]["item"]["id"].as_i64().unwrap();
            let updated = call(
                &b,
                "update_memory",
                json!({"id":id,"text":format!("Likes coffee\n{secret}")}),
            );
            assert_eq!(updated["isError"], false);
            assert_eq!(updated["structuredContent"]["item"]["text"], "Likes coffee");
            assert_eq!(
                call(
                    &b,
                    "write_memory",
                    json!({"path":"you.md","content":format!("- Likes walking\n{secret}")}),
                )["isError"],
                false
            );
            let imported = crate::memory::files::import_notes(
                &b.db,
                &b.paths,
                &[("you.md".into(), format!("- Likes climbing\n{secret}"))],
            )
            .unwrap();
            assert_eq!(imported[0].added, 1);
            let stored = b.db.with(|c| {
                let mut rows = c.prepare("SELECT text FROM memory_items UNION ALL SELECT coalesce(before, '') || coalesce(after, '') FROM memory_history")?;
                rows.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
            }).unwrap();
            assert!(stored.iter().all(|s| !s.contains("synthetic-canary")));
            for path in ["MEMORY.md", "you.md"] {
                let export = std::fs::read_to_string(b.paths.memory_dir().join(path)).unwrap();
                assert!(export.contains("Likes climbing"));
                assert!(!export.contains("synthetic-canary"));
            }
        }
    }

    #[test]
    fn credential_formats_are_scrubbed_by_tools_and_legacy_import() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        for secret in [
            r#"{"password":"synthetic-canary"}"#,
            "**password**: synthetic-canary",
            "Uses a password manager; password: synthetic-canary",
            r#"{"value":"sk-synthetic0000000000000000"}"#,
            r#"{"apiKey":"synthetic-canary"}"#,
            r#"{"accessToken":"synthetic-canary"}"#,
            r#"{"refreshToken":"synthetic-canary"}"#,
            r#"{"clientSecret":"synthetic-canary"}"#,
        ] {
            let text = format!("Likes tea\n{secret}");
            assert_eq!(
                call(&b, "remember", json!({"text":text,"kind":"fact"}))["isError"],
                false
            );
            assert_eq!(
                call(
                    &b,
                    "write_memory",
                    json!({"path":"you.md","content":format!("- {text}")})
                )["isError"],
                false
            );
            crate::memory::files::import_notes(&b.db, &b.paths, &[("you.md".into(), text)])
                .unwrap();
        }
        let notes = b.db.with(crate::memory::live).unwrap();
        let history =
            b.db.with(|c| crate::memory::recall::history(c, notes[0].id))
                .unwrap();
        let export = std::fs::read_to_string(b.paths.memory_dir().join("MEMORY.md")).unwrap();
        for content in [json!(notes).to_string(), json!(history).to_string(), export] {
            assert!(!content.contains("synthetic-canary"));
            assert!(!content.contains("sk-synthetic"));
        }
    }

    #[test]
    fn multiline_credentials_reject_writes_and_are_skipped_by_import() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        call(
            &b,
            "remember",
            json!({"text":"Public existing note","kind":"fact"}),
        );
        let before = b.db.with(crate::memory::live).unwrap();
        for text in [
            "{\"password\":\n  \"synthetic-canary\"}",
            "password: |\n  synthetic-canary",
            "password: |- # comment\n  synthetic-canary",
            "password: first\n  synthetic-canary",
            "{\"password\": [\n\"synthetic-canary\"]}",
            "AWS_SECRET_ACCESS_KEY=\n  synthetic-canary",
            "aws_secret_access_key: |\n  synthetic-canary",
            "{\"SecretAccessKey\":\n  \"synthetic-canary\"}",
        ] {
            assert_eq!(
                call(&b, "remember", json!({"text":text,"kind":"fact"}))["isError"],
                true
            );
            assert_eq!(
                call(&b, "write_memory", json!({"path":"you.md","content":text}))["isError"],
                true
            );
            let imported = crate::memory::files::import_notes(
                &b.db,
                &b.paths,
                &[("you.md".into(), text.into())],
            )
            .unwrap();
            assert_eq!(imported[0].added, 0);
        }
        let after = b.db.with(crate::memory::live).unwrap();
        assert_eq!(json!(after), json!(before));
        let export = std::fs::read_to_string(b.paths.memory_dir().join("MEMORY.md")).unwrap();
        assert!(export.contains("Public existing note"));
        assert!(!export.contains("synthetic-canary"));
    }

    #[test]
    fn agent_edits_can_clear_timestamps_in_place_and_on_replacements() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let a = call(
            &b,
            "remember",
            json!({"text":"Likes tea","kind":"fact","observed_at":100}),
        );
        let id = a["structuredContent"]["item"]["id"].as_i64().unwrap();
        let b1 = call(&b, "update_memory", json!({"id":id,"valid_to":i64::MAX}));
        assert_eq!(b1["isError"], false, "{b1}");
        assert_eq!(b1["structuredContent"]["item"]["observed_at"], 100);
        let b2 = call(
            &b,
            "update_memory",
            json!({"id":id,"text":"Likes coffee","valid_to":null,"observed_at":null}),
        );
        assert_eq!(b2["isError"], false, "{b2}");
        assert!(b2["structuredContent"]["item"]["valid_to"].is_null());
        assert!(b2["structuredContent"]["item"]["observed_at"].is_null());
        assert_ne!(b2["structuredContent"]["item"]["id"], id);
        let next = b2["structuredContent"]["item"]["id"].as_i64().unwrap();
        let b3 = call(
            &b,
            "update_memory",
            json!({"id":next,"observed_at":200,"valid_to":i64::MAX}),
        );
        assert_eq!(b3["isError"], false);
        let b4 = call(
            &b,
            "update_memory",
            json!({"id":next,"valid_to":null,"observed_at":null}),
        );
        assert_eq!(b4["isError"], false);
        assert_eq!(b4["structuredContent"]["item"]["id"], next);
        assert!(b4["structuredContent"]["item"]["valid_to"].is_null());
        assert!(b4["structuredContent"]["item"]["observed_at"].is_null());
    }

    #[test]
    fn memory_changes_date_from_the_turn_message() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        let conv = b.conversation_id.clone().unwrap();
        let msg =
            b.db.with(|c| {
                db::insert_message(
                    c,
                    &db::NewMessage {
                        conversation_id: &conv,
                        role: "assistant",
                        author_user_id: None,
                        kind: "text",
                        content: "",
                        attachments: &[],
                        card: None,
                        status: "streaming",
                        harness: None,
                        model: None,
                        effort: None,
                        meta: None,
                    },
                )
            })
            .unwrap();
        // The bridge started (or restarted) well after the turn began.
        b.message_id = Some(msg.id.clone());
        b.started_at = msg.created_at + 60_000;
        assert_eq!(b.turn_started(), msg.created_at);
        assert_eq!(b.turn_batch(), format!("turn-{}", msg.id));
        b.message_id = Some("missing".into());
        assert_eq!(b.turn_started(), b.started_at);
    }

    #[test]
    fn other_people_cannot_reach_owner_memory_or_connectors() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        std::fs::write(b.paths.memory_dir().join("diary.md"), "private").unwrap();
        let diary = b.paths.memory_dir().join("diary.md");
        let diary = diary.to_string_lossy();
        b.db.with(|c| {
            c.execute(
                "INSERT INTO connectors (id, name, description, transport, config, enabled, created_at, updated_at)
                 VALUES ('k1', 'notes', '', 'stdio', '{}', 1, 0, 0)",
                [],
            )
        })
        .unwrap();
        // The owner, alone: both reachable.
        let r = call(&b, "list_connectors", json!({}));
        assert_eq!(
            r["structuredContent"]["connectors"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let r = call(&b, "save_to_library", json!({"title": "d", "path": diary}));
        assert_eq!(r["isError"], true, "{r}");
        // Another person's own chat: neither.
        let guest = b
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO users (id, display_name, is_owner, created_at) VALUES ('g1', 'Guest', 0, 0)",
                    [],
                )
            })
            .map(|_| "g1".to_string())
            .unwrap();
        b.user_id = guest;
        let r = call(&b, "list_connectors", json!({}));
        assert_eq!(r["structuredContent"]["connectors"], json!([]));
        let r = call(&b, "save_to_library", json!({"title": "d", "path": diary}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn feed_posts_carry_photos_and_goals_can_be_suggestions() {
        let tmp = tempfile::tempdir().unwrap();
        let b = bridge(tmp.path());
        let r = call(
            &b,
            "create_feed_post",
            json!({"title": "Slab bench", "body": "A weekend build.", "source_name": "Woodshop Weekly",
                   "images": ["https://img.example/a.jpg", "https://img.example/b.jpg"],
                   "why": "You liked two slab tables"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let post = &r["structuredContent"]["post"];
        assert_eq!(post["image_url"], "https://img.example/a.jpg");
        assert_eq!(post["images"].as_array().unwrap().len(), 2);
        assert_eq!(post["why"], "You liked two slab tables");
        let r = call(
            &b,
            "create_feed_post",
            json!({"title": "x", "body": "y", "images": "nope"}),
        );
        assert_eq!(r["isError"], true);
        let r = call(
            &b,
            "create_feed_post",
            json!({"title": "x", "body": "y", "images": ["ftp://x"]}),
        );
        assert_eq!(r["isError"], true);
        let r = call(
            &b,
            "create_goal",
            json!({"title": "Learn to bake bread", "suggested": true}),
        );
        assert_eq!(r["structuredContent"]["goal"]["suggested"], true);
        let r = call(&b, "create_goal", json!({"title": "Asked for"}));
        assert_eq!(r["structuredContent"]["goal"]["suggested"], false);
        let r = call(
            &b,
            "create_card",
            json!({"type": "result", "title": "Kindred", "image_url": "https://img.example/k.jpg"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let r = call(
            &b,
            "create_card",
            json!({"type": "result", "title": "K", "image_url": "data:x"}),
        );
        assert_eq!(r["isError"], true);
    }
}
