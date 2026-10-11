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
const MAX_LIBRARY_BYTES: u64 = 20 * 1024 * 1024;

pub struct Bridge {
    pub db: Db,
    pub paths: Paths,
    pub user_id: String,
    pub conversation_id: Option<String>,
    /// Conversation with more than one person: only cards are offered, so
    /// nothing reads or changes the owner's personal data.
    pub shared: bool,
    /// The harness's working directory when it is not the workspace itself
    /// (Gemini runs in a per-conversation folder); relative library paths
    /// resolve against it.
    pub workdir: Option<PathBuf>,
}

/// Tools available in shared (group) conversations.
const SHARED_TOOLS: &[&str] = &["create_card"];

/// Run the stdio loop until stdin closes.
pub fn run(bridge: Bridge) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(req) => bridge.handle(&req),
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
            "List connected services (MCP connectors) and whether they are enabled.",
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
                    "serverInfo": {"name": "iris", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Iris app bridge: update the user's goals, ideas, feed, \
                        library, and show cards in the chat.",
                })
            }
            "ping" => json!({}),
            "tools/list" => {
                let tools: Vec<Value> = tool_definitions()
                    .into_iter()
                    .filter(|t| {
                        !self.shared || SHARED_TOOLS.contains(&t["name"].as_str().unwrap_or(""))
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
            && self
                .db
                .with(crate::db::owner_user_id)
                .ok()
                .flatten()
                .is_some_and(|o| o == self.user_id)
    }

    fn call(&self, name: &str, a: &Value) -> std::result::Result<Value, String> {
        if self.shared && !SHARED_TOOLS.contains(&name) {
            return Err(format!("{name} is not available in shared conversations"));
        }
        let s = |k: &str| a.get(k).and_then(Value::as_str);
        let uid = self.user_id.as_str();
        let db_err = |e: crate::util::BoxError| e.to_string();
        match name {
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
                        json!({"name": c.name, "description": c.description,
                               "transport": c.transport, "enabled": c.enabled})
                    })
                    .collect();
                Ok(json!({"connectors": items}))
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
                let src = self.resolve_agent_file(path)?;
                let meta = std::fs::metadata(&src).map_err(|_| "file not found")?;
                if meta.len() > MAX_LIBRARY_BYTES {
                    return Err("file is larger than 20 MB".into());
                }
                let ext = src
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("bin")
                    .to_ascii_lowercase();
                let mime = s("mime")
                    .map(String::from)
                    .unwrap_or_else(|| mime_for_ext(&ext).to_string());
                let file_name = format!("{id}.{}", sanitize_ext(&ext));
                let bytes = std::fs::read(&src).map_err(|e| e.to_string())?;
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

    /// Only files inside the agent's workspace (or, in the owner's own
    /// chats, memory) folder can be saved, and never through symlinks.
    /// Relative paths resolve against the harness's working directory.
    fn resolve_agent_file(&self, path: &str) -> std::result::Result<PathBuf, String> {
        let workspace = self.paths.workspace_dir();
        let base = self
            .workdir
            .as_ref()
            .filter(|w| match (w.canonicalize(), workspace.canonicalize()) {
                (Ok(w), Ok(ws)) => w.starts_with(ws),
                _ => false,
            })
            .cloned()
            .unwrap_or_else(|| workspace.clone());
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            base.join(path)
        };
        let real = candidate
            .canonicalize()
            .map_err(|_| "file not found".to_string())?;
        let mut roots = vec![workspace];
        if self.owner_context() {
            roots.push(self.paths.memory_dir());
        }
        let allowed = roots
            .iter()
            .filter_map(|d| d.canonicalize().ok())
            .any(|d| real.starts_with(d));
        let is_link = std::fs::symlink_metadata(&candidate)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true);
        if !allowed || is_link || !real.is_file() {
            return Err(
                "only regular files in the working folder or memory folder can be saved".into(),
            );
        }
        Ok(real)
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
        Bridge {
            db,
            paths,
            user_id,
            conversation_id: Some(conv),
            shared: false,
            workdir: None,
        }
    }

    #[test]
    fn shared_mode_only_offers_cards() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = bridge(tmp.path());
        b.shared = true;
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
            b.paths.workspace_dir().join("chart.png"),
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
        std::os::unix::fs::symlink("/etc/hostname", b.paths.workspace_dir().join("link.txt"))
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
        let gem = ws.join("gemini/c1");
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
        // A working folder outside the workspace is ignored.
        b.workdir = Some(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("report.md"), "# Outside").unwrap();
        let r = call(
            &b,
            "save_to_library",
            json!({"title": "Report", "path": "report.md"}),
        );
        assert_eq!(r["structuredContent"]["item"]["size"], "# Unrelated".len());
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
        assert_eq!(r["isError"], false, "{r}");
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
