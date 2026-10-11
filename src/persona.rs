//! Persona files ("system files") and system-prompt assembly.
//!
//! The prompt is rebuilt for every turn from `persona/*.md`, `memory/MEMORY.md`
//! and an index of `memory/`, so edits and agent renames apply immediately.
//! It is injected through the harness's own system-prompt mechanism
//! (`--append-system-prompt-file` / `developer_instructions`), before the
//! user's message.

use crate::config::{Paths, PersonDirs, ensure_private_dir, write_private_file};
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

pub fn system_file_path(dirs: &PersonDirs, f: &SystemFile) -> PathBuf {
    match f.area {
        Area::Persona => dirs.persona.join(f.name),
        Area::Memory => dirs.memory.join(f.name),
    }
}

const MAX_PERSONA_FILE_CHARS: usize = 24_000;
const MAX_MEMORY_INLINE_CHARS: usize = 12_000;
const MAX_MEMORY_INDEX_ENTRIES: usize = 200;
const MAX_MEMORY_DEPTH: usize = 4;
/// Keeps the prompt under Linux's 128 KiB per-argument limit, which matters
/// for codex where it travels as a `-c developer_instructions=...` value.
pub const MAX_PROMPT_CHARS: usize = 100_000;

/// Create the data directory layout and seed the owner's missing persona
/// files. Returns the files that were created.
pub fn seed(paths: &Paths) -> Result<Vec<String>> {
    ensure_private_dir(&paths.data_dir)?;
    // Folders that turns keep the agent out of must exist before any turn
    // builds its rules (a sandbox cannot protect a path that is not there).
    for dir in [
        paths.uploads_dir(),
        paths.library_dir(),
        paths.run_dir(),
        paths.voice_dir(),
        paths.data_dir.join("together"),
    ] {
        ensure_private_dir(&dir)?;
    }
    seed_person(&paths.owner_dirs())
}

/// Seed every person already in the database (folders and missing
/// templates; existing files are kept). Run at startup so people added
/// before they had their own Iris, or whose folders went missing, work.
pub fn seed_people(paths: &Paths, db: &crate::db::Db) -> Result<()> {
    for u in db.with(crate::db::list_users)? {
        seed_person(&paths.person_dirs(&u.id, u.is_owner))?;
    }
    Ok(())
}

/// Create one person's Iris folders and seed missing persona files from
/// the generic templates. Returns the files that were created.
pub fn seed_person(dirs: &PersonDirs) -> Result<Vec<String>> {
    for dir in dirs.all() {
        ensure_private_dir(dir)?;
    }
    let mut created = Vec::new();
    for f in SYSTEM_FILES {
        let path = system_file_path(dirs, f);
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
    /// More than one person in the conversation: only who the agent is
    /// (IDENTITY.md, SOUL.md) is included; everyone's private files
    /// (USER.md, TOOLS.md, PROACTIVE_PREFERENCES.md) and memory are left
    /// out.
    pub shared: bool,
    /// Display name of the person this Iris works for (personal chats).
    pub user_name: Option<&'a str>,
    /// Short notes such as "the Claude engine needs the owner to sign in".
    pub notices: &'a [String],
}

/// Assemble the system prompt appended to the harness's own system prompt.
pub fn assemble(dirs: &PersonDirs, ctx: &PromptContext) -> String {
    let name = ctx.agent_name;
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
    // "Owner" is the placeholder name before the person sets their own.
    if let Some(user) = ctx
        .user_name
        .map(str::trim)
        .filter(|u| !u.is_empty() && *u != "Owner" && !ctx.shared)
    {
        out.push_str(&format!(
            "\nThe person you work for is called {user} in the app.\n"
        ));
    }

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
        .filter(|f| f.in_prompt && (!ctx.shared || SHARED_FILES.contains(&f.name)))
    {
        let Ok(body) = crate::config::read_own_text(&system_file_path(dirs, f)) else {
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

    if ctx.app_tools && ctx.shared {
        out.push_str(SHARED_TOOLS_GUIDE);
    } else if ctx.app_tools {
        out.push_str(APP_TOOLS_GUIDE);
    }

    if ctx.shared {
        out.push_str(
            "\n## Memory\n\nNobody's personal memory or files are available in shared \
             conversations. Do not look for them or reveal personal details about anyone \
             that were not shared in this conversation.\n",
        );
    } else {
        let memory_dir = dirs.memory.clone();
        let mut entries = Vec::new();
        collect_memory(&memory_dir, &mut entries);
        entries.retain(|(rel, _, _)| rel != "MEMORY.md");
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        out.push_str(&format!(
            "\n## Memory\n\nLong-term memory is stored as Markdown files in `{}`. \
             Read a file when its topic comes up instead of guessing. When you \
             learn something durable about the user, update the relevant file \
             (or MEMORY.md for short, always-relevant facts) if you are able to \
             write files.\n",
            memory_dir.display()
        ));
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
        if let Ok(body) = crate::config::read_own_text(&memory_dir.join("MEMORY.md")) {
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
    truncate_chars(&out, MAX_PROMPT_CHARS)
}

/// Version of the system prompt's shape (see `persona_hash`). 2: per-person
/// Iris; shared chats carry only IDENTITY.md and SOUL.md.
const PROMPT_VERSION: &str = "2";

/// Persona files used in shared conversations: who the agent is, never
/// facts about anyone.
const SHARED_FILES: &[&str] = &["IDENTITY.md", "SOUL.md"];

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
- Cards: `create_card` shows a structured card in the current chat. Whenever
  the user asks to set up, connect, or link an account or service (for
  example \"help me set up Google\"), call `create_card` with type `connect`,
  the `service` name, and the `url` of the official page where they can
  connect it, then explain the next step in a sentence or two. Use `link`
  cards for other links worth highlighting instead of pasting raw URLs, and
  a `result` card (with `image_url`, details in `body`, and one action) for
  one concrete result such as a restaurant or product.
- Connectors: `list_connectors` shows which services are connected.
";

/// Hash of the inputs that define who the agent is (name, conversation
/// kind, persona files, MEMORY.md). The memory index and notices are left
/// out so routine memory writes do not invalidate sessions.
pub fn persona_hash(dirs: &PersonDirs, agent_name: &str, conversation_kind: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // Bumped when the prompt's shape changes, so CLIs that keep their first
    // instructions on resume (Codex) start over with the new prompt.
    h.update(PROMPT_VERSION.as_bytes());
    h.update([0]);
    h.update(agent_name.as_bytes());
    h.update([0]);
    h.update(conversation_kind.as_bytes());
    for f in SYSTEM_FILES
        .iter()
        .filter(|f| f.in_prompt || f.name == "MEMORY.md")
    {
        h.update([0]);
        h.update(crate::config::read_own_text(&system_file_path(dirs, f)).unwrap_or_default());
    }
    crate::util::hex(&h.finalize()[..12])
}

/// Memory notes (relative path, title, size), read through folder
/// descriptors so a folder swapped for a link is never followed.
fn collect_memory(root: &Path, out: &mut Vec<(String, String, u64)>) {
    let keep = |name: &str| is_text_note(Path::new(name));
    for n in crate::fsafe::notes(root, MAX_MEMORY_DEPTH, &keep, 4096) {
        out.push((n.rel, note_title(&n.head), n.size));
    }
}

pub fn is_text_note(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown" | "txt")
    )
}

/// First Markdown heading or first non-empty line, truncated.
fn note_title(head: &str) -> String {
    for line in head.lines().take(20) {
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
            shared: false,
            user_name: None,
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

        let prompt = assemble(&p.owner_dirs(), &ctx("Nova"));
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
        let prompt = assemble(&p.owner_dirs(), &c);
        assert!(prompt.contains("group conversation with: Sam, Riley"));
        let mut g = ctx("Iris");
        g.shared = true;
        std::fs::write(p.persona_dir().join("USER.md"), "secret-user-fact").unwrap();
        std::fs::write(p.memory_dir().join("MEMORY.md"), "secret-memory-fact").unwrap();
        std::fs::write(p.persona_dir().join("TOOLS.md"), "secret-tools-fact").unwrap();
        std::fs::write(p.persona_dir().join("SOUL.md"), "warm-and-brief").unwrap();
        let shared = assemble(&p.owner_dirs(), &g);
        assert!(!shared.contains("secret-user-fact") && !shared.contains("secret-memory-fact"));
        assert!(!shared.contains("secret-tools-fact"));
        assert!(shared.contains("warm-and-brief"), "who the agent is stays");
        assert!(!shared.contains(&p.memory_dir().display().to_string()));
        assert!(shared.contains("only offers `create_card`"));
        assert!(prompt.contains("## Notices\n\n- The Claude engine"));
        assert!(!prompt.contains("create_goal"));
        let mut c = ctx("Iris");
        c.purpose = Purpose::Scheduled;
        assert!(assemble(&p.owner_dirs(), &c).contains("scheduled background job"));
        assert!(assemble(&p.owner_dirs(), &ctx("Iris")).contains("(No other memory files yet.)"));
    }

    #[test]
    fn persona_hash_tracks_identity_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        let base = persona_hash(&p.owner_dirs(), "Iris", "main");
        assert_eq!(base, persona_hash(&p.owner_dirs(), "Iris", "main"));
        assert_ne!(base, persona_hash(&p.owner_dirs(), "Nova", "main"));
        assert_ne!(base, persona_hash(&p.owner_dirs(), "Iris", "group"));
        std::fs::write(p.memory_dir().join("trip.md"), "# Trip").unwrap();
        assert_eq!(
            base,
            persona_hash(&p.owner_dirs(), "Iris", "main"),
            "memory index ignored"
        );
        std::fs::write(p.memory_dir().join("MEMORY.md"), "new fact").unwrap();
        assert_ne!(base, persona_hash(&p.owner_dirs(), "Iris", "main"));
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
        let prompt = assemble(&p.owner_dirs(), &ctx("Iris"));
        assert!(prompt.chars().count() <= MAX_PROMPT_CHARS + 1);
    }

    #[test]
    fn each_person_gets_their_own_persona_and_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        let other = p.person_dirs("0123456789abcdef0123456789abcdef", false);
        assert_eq!(seed_person(&other).unwrap().len(), SYSTEM_FILES.len());
        std::fs::write(p.memory_dir().join("MEMORY.md"), "owner-secret").unwrap();
        std::fs::write(other.memory.join("MEMORY.md"), "partner-fact").unwrap();
        std::fs::write(other.persona.join("USER.md"), "partner-user").unwrap();
        let mut c = ctx("Juno");
        c.user_name = Some("Riley");
        let prompt = assemble(&other, &c);
        assert!(prompt.contains("partner-fact") && prompt.contains("partner-user"));
        assert!(prompt.contains("called Riley"));
        assert!(!prompt.contains("owner-secret"));
        assert!(!prompt.contains(&p.memory_dir().display().to_string()));
        let owner = assemble(&p.owner_dirs(), &ctx("Iris"));
        assert!(owner.contains("owner-secret") && !owner.contains("partner-fact"));
        assert!(!owner.contains("called "), "no placeholder name");
        assert_ne!(
            persona_hash(&other, "Iris", "main"),
            persona_hash(&p.owner_dirs(), "Iris", "main")
        );
    }
}
