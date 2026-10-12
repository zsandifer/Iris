//! Persona files ("system files") and system-prompt assembly.
//!
//! The prompt is rebuilt for every turn from `persona/*.md`, `memory/MEMORY.md`
//! and an index of `memory/`, so edits and agent renames apply immediately.
//! It is injected through the harness's own system-prompt mechanism
//! (`--append-system-prompt-file`, `GEMINI_SYSTEM_MD`, ...; for Codex, on
//! stdin at the start of a session), before the
//! user's message.

use crate::config::{Paths, ensure_private_dir, write_private_file};
use crate::router::Purpose;
use crate::util::{Result, truncate_chars};
use std::path::{Path, PathBuf};

/// Where a system file lives inside the data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Persona,
    Memory,
}

pub struct SystemFile {
    pub name: &'static str,
    pub area: Area,
    pub template: &'static str,
    pub description: &'static str,
    /// Included in the chat system prompt.
    pub in_prompt: bool,
}

/// Editable persona markdown, seeded from generic templates on first run.
pub const SYSTEM_FILES: &[SystemFile] = &[
    SystemFile {
        name: "IDENTITY.md",
        area: Area::Persona,
        template: include_str!("../templates/IDENTITY.md"),
        description: "Who the agent is: role, boundaries, introduction",
        in_prompt: true,
    },
    SystemFile {
        name: "SOUL.md",
        area: Area::Persona,
        template: include_str!("../templates/SOUL.md"),
        description: "How the agent communicates and thinks",
        in_prompt: true,
    },
    SystemFile {
        name: "USER.md",
        area: Area::Persona,
        template: include_str!("../templates/USER.md"),
        description: "Facts about you: preferences, goals, people",
        in_prompt: true,
    },
    SystemFile {
        name: "TOOLS.md",
        area: Area::Persona,
        template: include_str!("../templates/TOOLS.md"),
        description: "Notes about tools, accounts and connectors",
        in_prompt: true,
    },
    SystemFile {
        name: "PROACTIVE_PREFERENCES.md",
        area: Area::Persona,
        template: include_str!("../templates/PROACTIVE_PREFERENCES.md"),
        description: "When and how the agent acts on its own",
        in_prompt: true,
    },
    SystemFile {
        name: "HEARTBEAT.md",
        area: Area::Persona,
        template: include_str!("../templates/HEARTBEAT.md"),
        description: "Checklist for scheduled check-ins",
        in_prompt: false,
    },
    SystemFile {
        name: "MEMORY.md",
        area: Area::Memory,
        template: include_str!("../templates/MEMORY.md"),
        description: "Curated long-term memory (always in context)",
        in_prompt: false,
    },
];

pub fn system_file(name: &str) -> Option<&'static SystemFile> {
    SYSTEM_FILES.iter().find(|f| f.name == name)
}

pub fn system_file_path(paths: &Paths, f: &SystemFile) -> PathBuf {
    match f.area {
        Area::Persona => paths.persona_dir().join(f.name),
        Area::Memory => paths.memory_dir().join(f.name),
    }
}

const MAX_PERSONA_FILE_CHARS: usize = 24_000;
const MAX_MEMORY_INLINE_CHARS: usize = 12_000;
const MAX_MEMORY_INDEX_ENTRIES: usize = 200;
const MAX_MEMORY_DEPTH: usize = 4;
/// Bounds the prompt (counted in UTF-8 bytes) so it stays a modest share
/// of every engine's context.
pub const MAX_PROMPT_BYTES: usize = 100_000;

/// Whose private context a conversation may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Only the owner: their USER.md, memory, and connectors are available.
    Private,
    /// One person who is not the owner: none of the owner's private files.
    Personal,
    /// More than one person: none of the owner's private files, and the
    /// app bridge only offers cards.
    Shared,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Private => "private",
            Scope::Personal => "personal",
            Scope::Shared => "shared",
        }
    }
}

/// Create the data directory layout and seed missing persona files.
/// Returns the files that were created.
pub fn seed(paths: &Paths) -> Result<Vec<String>> {
    ensure_private_dir(&paths.data_dir)?;
    for dir in [
        paths.persona_dir(),
        paths.memory_dir(),
        paths.workspace_dir(),
        paths.uploads_dir(),
        paths.library_dir(),
        paths.run_dir(),
    ] {
        ensure_private_dir(&dir)?;
    }
    let mut created = Vec::new();
    for f in SYSTEM_FILES {
        let path = system_file_path(paths, f);
        if !path.exists() {
            write_private_file(&path, f.template.as_bytes())?;
            created.push(f.name.to_string());
        }
    }
    Ok(created)
}

/// Per-turn context that shapes the system prompt.
#[derive(Debug, Clone)]
pub struct PromptContext<'a> {
    pub agent_name: &'a str,
    /// main | side | group | job
    pub conversation_kind: &'a str,
    pub purpose: Purpose,
    /// Display names of the humans in a group conversation.
    pub participants: &'a [String],
    /// Whether the `iris` MCP bridge is attached to this turn.
    pub app_tools: bool,
    /// Name of Iris's browser MCP server when it is attached.
    pub browser: Option<&'a str>,
    /// Who is in the conversation. The owner's private persona (USER.md)
    /// and memory are only included when it is `Private`.
    pub scope: Scope,
    /// Short notes such as "the Claude engine needs the owner to sign in".
    pub notices: &'a [String],
}

/// Assemble the system prompt appended to the harness's own system prompt.
pub fn assemble(paths: &Paths, ctx: &PromptContext) -> String {
    let name = ctx.agent_name;
    let private = ctx.scope == Scope::Private;
    let mut out = String::new();
    out.push_str(&format!(
        "# You are {name}\n\n\
         You are {name}, the user's personal AI agent. The user talks to you \
         through the Iris app, usually on their phone, so keep replies \
         conversational and use Markdown only where it helps. Your name is \
         {name}; if any file below gives a different name, {name} wins. Do \
         not bring up AI models, tokens, or reasoning settings unless the \
         user asks.\n"
    ));

    match (ctx.purpose, ctx.conversation_kind) {
        (Purpose::Scheduled, _) => out.push_str(
            "\nThis is a scheduled background job, not a live chat. Nobody is \
             reading your reply in real time; follow the job instructions in \
             the message exactly.\n",
        ),
        (_, "group") => out.push_str(&format!(
            "\nThis is a group conversation with: {}. Each user message is \
             prefixed with the sender's name. Only share private details \
             about one person if they are relevant and appropriate for the \
             whole group.\n",
            ctx.participants.join(", ")
        )),
        (_, "side") => out.push_str(
            "\nThis is a side chat focused on one topic, separate from the \
             user's Main chat.\n",
        ),
        _ => {}
    }

    for f in SYSTEM_FILES
        .iter()
        .filter(|f| f.in_prompt && (private || shared_with_others(f.name)))
    {
        let Ok(body) = std::fs::read_to_string(system_file_path(paths, f)) else {
            continue;
        };
        let body = body.replace("{{agent_name}}", name);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n<persona file=\"{}\">\n{}\n</persona>\n",
            f.name,
            truncate_chars(body, MAX_PERSONA_FILE_CHARS)
        ));
    }

    if ctx.app_tools && ctx.scope == Scope::Shared {
        out.push_str(SHARED_TOOLS_GUIDE);
    } else if ctx.app_tools {
        out.push_str(APP_TOOLS_GUIDE);
    }
    if ctx.app_tools && private {
        out.push_str("\nCheck `system_status` for current setup; use `request_engine_signin`, `request_engine_install`, and `request_connector_setup` to request owner cards. You cannot change connections, credentials, or settings directly. System notes report outcomes; resolved cards need no further action.\n");
    }
    if let Some(server) = ctx.browser.filter(|_| ctx.scope == Scope::Private) {
        out.push_str(&crate::browser::tools::guide(server));
    }

    if ctx.scope == Scope::Shared {
        out.push_str(
            "\n## Memory\n\nThe owner's personal memory is not available in shared \
             conversations. Do not look for it or reveal personal details about anyone \
             that were not shared in this conversation.\n",
        );
    } else if !private {
        out.push_str(
            "\n## Memory\n\nLong-term memory is not available in this conversation. Do \
             not look for memory or personal files on the server, and do not reveal \
             details about other people who use this server.\n",
        );
    } else {
        let memory_dir = paths.memory_dir();
        let mut entries = Vec::new();
        collect_memory(&memory_dir, &memory_dir, 0, &mut entries);
        entries.retain(|(rel, _, _)| rel != "MEMORY.md");
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        out.push_str(
            "\n## Memory\n\nLong-term memory is stored by Iris as Markdown files. \
             Use the Iris read_memory tool to read a file when its topic comes up. \
             Use write_memory to update durable facts (MEMORY.md for short, \
             always-relevant facts). These tools are only available in the owner's private chats.\n",
        );
        if entries.is_empty() {
            out.push_str("\n(No other memory files yet.)\n");
        } else {
            out.push('\n');
            let total = entries.len();
            for (rel, title, size) in entries.iter().take(MAX_MEMORY_INDEX_ENTRIES) {
                out.push_str(&format!("- `{rel}` ({}) {title}\n", human_size(*size)));
            }
            if total > MAX_MEMORY_INDEX_ENTRIES {
                out.push_str(&format!(
                    "- ... and {} more files\n",
                    total - MAX_MEMORY_INDEX_ENTRIES
                ));
            }
        }
        if let Ok(body) = std::fs::read_to_string(memory_dir.join("MEMORY.md")) {
            let body = body.trim();
            if !body.is_empty() {
                out.push_str(&format!(
                    "\n<memory file=\"MEMORY.md\">\n{}\n</memory>\n",
                    truncate_chars(body, MAX_MEMORY_INLINE_CHARS)
                ));
            }
        }
    }

    if !ctx.notices.is_empty() {
        out.push_str("\n## Notices\n\n");
        for n in ctx.notices {
            out.push_str(&format!("- {n}\n"));
        }
        out.push_str(
            "\nMention these to the user naturally if they are relevant or the \
             user asks why something is not working; do not open every reply \
             with them.\n",
        );
    }
    truncate_bytes(&out, MAX_PROMPT_BYTES)
}

/// Truncate to at most `max` UTF-8 bytes, on a character boundary.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

const SHARED_TOOLS_GUIDE: &str = "
## The Iris app

In this shared conversation the `iris` MCP server only offers `create_card`,
which shows a structured card (for example a `link` card) in the chat.
";

const APP_TOOLS_GUIDE: &str = "
## The Iris app

The `iris` MCP server lets you update what the user sees in the app. Use it
when it genuinely helps, and never mention tool names to the user.

- Goals: `list_goals`, `create_goal`, `update_goal` (status text, done). Keep
  the status of goals you are helping with current. When you propose a goal
  the user did not ask for, create it with `suggested: true` so they can
  confirm it.
- Ideas: `list_ideas`, `create_idea` for tasks you could do for the user.
- Feed: `list_feed_posts`, `create_feed_post` for posts about the user's
  interests, with the source's name, its original photos (`images`, never
  generated ones), and a short `why` line.
- Library: `save_to_library` for documents, drafts and other artifacts you
  produce; `list_library`.
- Cards: `create_card` shows links, results, and other structured information.
- Connectors: `list_connectors` shows which services are connected and which
  can still be connected.

## Connected services

Tools from other MCP servers are services the user connected. Iris runs
them and enforces the user's rules: anything that sends a message or email,
buys or pays, deletes, exports a lot of data, or reaches someone new waits
until the user approves it on a card Iris shows in the chat (built from the
exact arguments you send). Waiting is expected; say briefly that you asked
for their OK. If a call comes back denied, unanswered, paused, or refused,
tell the user plainly what did not happen and do not retry unless they ask.
Never try to get around an approval (for example by using another tool or
service for the same action).
";

/// Hash of the inputs that define who the agent is (name, conversation
/// kind, privacy scope, persona files, MEMORY.md), prefixed with the scope
/// name (`private-1a2b...`) so a scope change can be told apart from a
/// persona edit (see [`hash_scope`]). The memory index and notices are left
/// out so routine memory writes do not invalidate sessions.
pub fn persona_hash(
    paths: &Paths,
    agent_name: &str,
    conversation_kind: &str,
    scope: Scope,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(agent_name.as_bytes());
    h.update([0]);
    h.update(conversation_kind.as_bytes());
    h.update([0]);
    h.update(scope.name().as_bytes());
    let private = scope == Scope::Private;
    for f in SYSTEM_FILES
        .iter()
        .filter(|f| f.in_prompt || f.name == "MEMORY.md")
        .filter(|f| private || shared_with_others(f.name))
    {
        h.update([0]);
        h.update(std::fs::read(system_file_path(paths, f)).unwrap_or_default());
    }
    format!("{}-{}", scope.name(), crate::util::hex(&h.finalize()[..12]))
}

/// Persona files other people's chats also get: only the agent's own
/// character. USER.md, TOOLS.md (accounts, devices), PROACTIVE_PREFERENCES.md
/// (the owner's interests), and memory stay in the owner's chats.
fn shared_with_others(name: &str) -> bool {
    matches!(name, "IDENTITY.md" | "SOUL.md")
}

/// The scope a stored persona hash was made for (None for hashes stored
/// before scopes existed).
pub fn hash_scope(hash: &str) -> Option<&str> {
    hash.split_once('-').map(|(scope, _)| scope)
}

fn collect_memory(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, String, u64)>) {
    if depth > MAX_MEMORY_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        // file_type() does not follow symlinks; skip them entirely.
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect_memory(root, &path, depth + 1, out);
        } else if ft.is_file() && is_text_note(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel, note_title(&path), size));
        }
    }
}

pub fn is_text_note(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt")
    )
}

/// First Markdown heading or first non-empty line, truncated.
fn note_title(path: &Path) -> String {
    use std::io::{BufRead, BufReader};
    let Ok(f) = std::fs::File::open(path) else {
        return String::new();
    };
    for line in BufReader::new(f).lines().take(20).map_while(|l| l.ok()) {
        let t = line.trim().trim_start_matches('#').trim();
        if !t.is_empty() && !t.starts_with("<!--") && t != "---" {
            return format!("— {}", truncate_chars(t, 80));
        }
    }
    String::new()
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        Paths {
            config_file: dir.join("config.toml"),
            data_dir: dir.join("data"),
        }
    }

    fn ctx<'a>(name: &'a str) -> PromptContext<'a> {
        PromptContext {
            agent_name: name,
            conversation_kind: "main",
            purpose: Purpose::Chat,
            participants: &[],
            app_tools: true,
            browser: None,
            scope: Scope::Private,
            notices: &[],
        }
    }

    #[test]
    fn seed_creates_layout_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let created = seed(&p).unwrap();
        assert_eq!(created.len(), SYSTEM_FILES.len());
        assert!(p.memory_dir().join("MEMORY.md").is_file());
        std::fs::write(p.persona_dir().join("USER.md"), "custom").unwrap();
        assert!(seed(&p).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(p.persona_dir().join("USER.md")).unwrap(),
            "custom"
        );
        assert!(p.workspace_dir().is_dir() && p.library_dir().is_dir());
    }

    #[test]
    fn templates_are_generic() {
        for f in SYSTEM_FILES {
            assert!(!f.template.is_empty(), "{}", f.name);
            assert!(!f.template.contains('@'), "{} looks personal", f.name);
        }
    }

    #[test]
    fn assemble_substitutes_name_and_indexes_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        std::fs::write(p.persona_dir().join("USER.md"), "# User\n\nLikes tea.").unwrap();
        std::fs::create_dir_all(p.memory_dir().join("people")).unwrap();
        std::fs::write(p.memory_dir().join("people/alex.md"), "# Alex\nFriend").unwrap();
        std::fs::write(p.memory_dir().join("MEMORY.md"), "Prefers mornings.").unwrap();
        std::fs::write(p.memory_dir().join(".hidden.md"), "secret").unwrap();
        std::fs::write(p.memory_dir().join("photo.jpg"), [0u8; 4]).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", p.memory_dir().join("link.md")).unwrap();

        let prompt = assemble(&p, &ctx("Nova"));
        assert!(prompt.starts_with("# You are Nova"));
        assert!(prompt.contains("I am Nova, a personal agent"));
        assert!(!prompt.contains("{{agent_name}}"));
        assert!(prompt.contains("Likes tea."));
        assert!(prompt.contains("<persona file=\"TOOLS.md\">"));
        assert!(!prompt.contains("<persona file=\"HEARTBEAT.md\">"));
        assert!(prompt.contains("`people/alex.md` (13 B) — Alex"));
        assert!(prompt.contains("<memory file=\"MEMORY.md\">\nPrefers mornings."));
        assert!(!prompt.contains("`MEMORY.md`"));
        assert!(!prompt.contains(".hidden"));
        assert!(!prompt.contains("photo.jpg"));
        assert!(!prompt.contains("link.md"));
        assert!(prompt.contains("create_goal"));
        assert!(!prompt.contains("## Notices"));
        assert!(!prompt.contains("## Your browser"));
        let mut b = ctx("Nova");
        b.browser = Some("browser");
        assert!(assemble(&p, &b).contains("fill_login"));
        b.scope = Scope::Shared;
        assert!(!assemble(&p, &b).contains("## Your browser"));
    }

    #[test]
    fn assemble_reflects_context() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        let people = vec!["Sam".to_string(), "Riley".to_string()];
        let notices = vec!["The Claude engine needs the owner to sign in.".to_string()];
        let mut c = ctx("Iris");
        c.conversation_kind = "group";
        c.participants = &people;
        c.notices = &notices;
        c.app_tools = false;
        let prompt = assemble(&p, &c);
        assert!(prompt.contains("group conversation with: Sam, Riley"));
        let mut g = ctx("Iris");
        g.scope = Scope::Shared;
        std::fs::write(p.persona_dir().join("USER.md"), "secret-user-fact").unwrap();
        std::fs::write(p.persona_dir().join("TOOLS.md"), "secret-account-note").unwrap();
        std::fs::write(
            p.persona_dir().join("PROACTIVE_PREFERENCES.md"),
            "secret-interest",
        )
        .unwrap();
        std::fs::write(p.persona_dir().join("SOUL.md"), "calm-and-kind").unwrap();
        std::fs::write(p.memory_dir().join("MEMORY.md"), "secret-memory-fact").unwrap();
        std::fs::write(p.memory_dir().join("diary.md"), "# Secret diary").unwrap();
        let shared = assemble(&p, &g);
        assert!(!shared.contains("secret-user-fact") && !shared.contains("secret-memory-fact"));
        assert!(!shared.contains("Secret diary"));
        assert!(!shared.contains("secret-account-note") && !shared.contains("secret-interest"));
        assert!(
            shared.contains("calm-and-kind"),
            "the agent's character is shared"
        );
        assert!(shared.contains("only offers `create_card`"));
        // Another person's own chat: no owner files, but the full app tools.
        g.scope = Scope::Personal;
        let personal = assemble(&p, &g);
        assert!(!personal.contains("secret-user-fact") && !personal.contains("secret-memory-fact"));
        assert!(!personal.contains("Secret diary"));
        assert!(!personal.contains("secret-account-note") && !personal.contains("secret-interest"));
        assert!(!personal.contains(&p.memory_dir().display().to_string()));
        assert!(personal.contains("not available in this conversation"));
        assert!(personal.contains("create_goal"));
        let private = assemble(&p, &ctx("Iris"));
        assert!(private.contains("secret-user-fact") && private.contains("secret-memory-fact"));
        assert!(private.contains("secret-account-note") && private.contains("secret-interest"));
        assert!(private.contains("Secret diary"));
        std::fs::remove_file(p.memory_dir().join("diary.md")).unwrap();
        assert!(prompt.contains("## Notices\n\n- The Claude engine"));
        assert!(!prompt.contains("create_goal"));
        let mut c = ctx("Iris");
        c.purpose = Purpose::Scheduled;
        assert!(assemble(&p, &c).contains("scheduled background job"));
        assert!(assemble(&p, &ctx("Iris")).contains("(No other memory files yet.)"));
    }

    #[test]
    fn persona_hash_tracks_identity_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        let hash = |name: &str, kind: &str, scope: Scope| persona_hash(&p, name, kind, scope);
        let base = hash("Iris", "main", Scope::Private);
        assert_eq!(base, hash("Iris", "main", Scope::Private));
        assert_ne!(base, hash("Nova", "main", Scope::Private));
        assert_ne!(base, hash("Iris", "group", Scope::Private));
        std::fs::write(p.memory_dir().join("trip.md"), "# Trip").unwrap();
        assert_eq!(
            base,
            hash("Iris", "main", Scope::Private),
            "memory index ignored"
        );
        let shared = hash("Iris", "group", Scope::Shared);
        let group_private = hash("Iris", "group", Scope::Private);
        assert_ne!(shared, group_private, "becoming shared changes the hash");
        assert_eq!(hash_scope(&shared), Some("shared"));
        assert_eq!(hash_scope(&group_private), Some("private"));
        assert_eq!(hash_scope("0123abcd"), None);
        std::fs::write(p.memory_dir().join("MEMORY.md"), "new fact").unwrap();
        assert_ne!(base, hash("Iris", "main", Scope::Private));
        // Owner-only files do not affect sessions that never see them.
        assert_eq!(shared, hash("Iris", "group", Scope::Shared));
    }

    #[test]
    fn prompt_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        for f in [
            "IDENTITY.md",
            "SOUL.md",
            "USER.md",
            "TOOLS.md",
            "PROACTIVE_PREFERENCES.md",
        ] {
            std::fs::write(p.persona_dir().join(f), "x".repeat(30_000)).unwrap();
        }
        let prompt = assemble(&p, &ctx("Iris"));
        assert!(prompt.len() <= MAX_PROMPT_BYTES);
        // Multi-byte text is capped in bytes, not characters.
        for f in ["IDENTITY.md", "SOUL.md", "USER.md", "TOOLS.md"] {
            std::fs::write(p.persona_dir().join(f), "漢".repeat(24_000)).unwrap();
        }
        let prompt = assemble(&p, &ctx("Iris"));
        assert!(prompt.len() <= MAX_PROMPT_BYTES, "{}", prompt.len());
        assert_eq!(truncate_bytes("a漢b", 2), "a");
        assert_eq!(truncate_bytes("a漢b", 4), "a漢");
    }
}
