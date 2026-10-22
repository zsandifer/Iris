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
        description: "What the agent always keeps in mind (a read-only copy of its memory)",
        in_prompt: false,
    },
];

impl SystemFile {
    /// Memory files are generated from the memory notes; only Iris writes them.
    pub fn read_only(&self) -> bool {
        self.area == Area::Memory
    }
}

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
/// Iris packs `MEMORY.md` under its budget (whole notes, see
/// `memory::packing`); this only bounds a file edited on disk.
const MAX_MEMORY_INLINE_CHARS: usize = crate::memory::packing::BUDGET_CHARS + 1_000;
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
    /// The agent's working notes rendered for the prompt
    /// (`proactive::render_notes`); only used in private scope.
    pub working_notes: &'a str,
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
        out.push_str("Google uses one owner-created login key and one sign-in for Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts and YouTube read-only. `request_connector_setup(\"google\")` opens the saved wizard; if `google_setup.more_access` is true, Google still works but was signed in before Docs, Sheets, Slides, Forms, Tasks and Contacts, and the card asks them to turn on the new APIs and sign in again. Coach only the current `google_setup` step: 1 create project (set the name to Iris even if Google prefills My Project; either name works), then copy the Project ID under its name into Iris, 2 enable all ten APIs (one link turns them all on), 3 Branding (required App name Iris, User support email chosen from their Gmail, Developer contact information with their Gmail at the very bottom; add `google_setup.authorized_domain` under Authorized domains, domain only with no scheme, port or path; leave logo, App domain links, home page and privacy policy empty to avoid triggering verification; Save at the bottom) and Audience (External, their own account as test user), 4 Web application key with the exact HTTPS callback shown by Iris, 5 publish on Audience to avoid the seven-day testing sign-out (if Publish app is greyed out, return to step 3, fill all three Branding fields and the Authorized domains entry, then Save), 6 paste the downloaded JSON or ID and secret together in the app, then sign in. Google uses the Project ID, not the display name. In steps 2–5, tell them to look for `google_setup.project_id` on Google’s page; if another project is shown, use the top project picker to choose that ID. They can change the saved ID in the wizard. Never ask for or accept the key in chat. The unverified-app warning is expected for their own private app: Advanced → Go to Iris. Sign-in and Google Console pages belong in the owner's real browser, never Iris's managed browser. Use reported error steps to help them fix setup; never claim Google Console steps were verified just because they tapped Done.\n");
        out.push_str("Services with a hosted server connect with one tap (for example Notion, Todoist, Airtable, Linear, Sentry, Supabase, Stripe; `request_connector_setup` lists the exact names): it posts a Connect card, and they approve access on the service's own page. These start asking before any change; money services (Stripe, Mercury) ask before everything. The owner chooses Allow, Ask or Off per action on each service's page in Connections (for Google, per service: Gmail, Calendar, Drive and the rest), and can turn a Google service off entirely; actions they turned off are not offered to you.\n");
    }
    if ctx.app_tools && private {
        out.push_str(ROUTINES_GUIDE);
    }
    if let Some(server) = ctx.browser.filter(|_| ctx.scope == Scope::Private) {
        out.push_str(&crate::browser::tools::guide(server));
    }

    out.push_str(&memory_prompt(paths, ctx.scope));

    if private {
        out.push_str(WORKING_NOTES_GUIDE);
        if ctx.working_notes.is_empty() {
            out.push_str("\n(No working notes yet.)\n");
        } else {
            out.push_str(ctx.working_notes);
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

const WORKING_NOTES_GUIDE: &str = "
## Your working notes

These are your own notes, one per domain you look after (not the user's \
memory): the plan, where things stand, what you are watching, open \
questions, and what you did last. They are what makes you a teammate \
rather than a chatbot: read them before acting in a domain, and keep them \
current with `update_working_note` whenever you learn or do something that \
changes the picture. When the user hands you something to own, start a note \
for it. The user can read and edit them in the app.
";

const SHARED_TOOLS_GUIDE: &str = "
## The Iris app

In this shared conversation the `iris` MCP server only offers `create_card`,
which shows a structured card (for example a `link` card) in the chat.
";

const ROUTINES_GUIDE: &str = "
## Routines

Routines are jobs you run for the owner on a schedule (\"order dinner every
Saturday\", \"rent reminder on the 1st\", \"gift ideas on Oct 29 at 8\").
When the owner asks for something to happen later, regularly, or as a
reminder, or a request would clearly be easier as a routine, propose one with
`create_routine`. Check `list_routines` first and change an existing one with
`update_routine` (also to pause or resume it) or `delete_routine` rather than
making a duplicate. Every proposal posts a card and does nothing until the
owner taps it, so pick sensible defaults instead of asking many questions, say
in one line what you picked, and never claim a routine is set up before they
approve it. Times are the owner's local wall-clock times; one-off reminders
use `schedule_kind` once with `run_at`. Write `instructions` as what you will
do on each run. Keep `ask_before_acting` on, and set `misfire` to skip, for
routines that buy, order, book, pay, or send anything. Reminders and
look-ups need neither. (The routines.md memory topic is for notes about the
owner's habits; it does not schedule anything.)
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

/// Current memory context, including the packed core, topic index and
/// capacity notices. Also sent on stdin when a CLI cannot refresh the
/// system prompt of a resumed session. Non-owner scopes read no memory.
pub fn memory_prompt(paths: &Paths, scope: Scope) -> String {
    let mut out = String::new();
    if scope == Scope::Shared {
        out.push_str(
            "\n## Memory\n\nThe owner's personal memory is not available in shared \
             conversations. Do not look for it or reveal personal details about anyone \
             that were not shared in this conversation.\n",
        );
    } else if scope != Scope::Private {
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
            "\n## Memory\n\nIris keeps the owner's long-term memory as notes. MEMORY.md \
             (below) holds the notes always in mind, as many as fit: pinned notes first, \
             then the most important and recently used. Every note is also listed in its \
             topic file (you.md, people.md, projects.md, routines.md; the ones with notes \
             are listed below). Use search_memory to recall relevant notes, get_memory for \
             their full text, sources and history, and search_conversations for past private \
             chats. Save individual notes with remember, correct them with update_memory, \
             and remove them with forget. Never save secrets or <private> spans. A similarity \
             hint is advisory: read that note and decide whether to update it; never merge \
             automatically. read_memory/write_memory remain available for the Markdown \
             view. Memory tools are only available in the owner's private chats.\n",
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
        if let Some(core) = crate::memory::files::core_for_prompt(paths) {
            out.push_str(&format!(
                "\n<memory file=\"MEMORY.md\">\n{}\n</memory>\n",
                truncate_chars(&core, MAX_MEMORY_INLINE_CHARS)
            ));
        }
    }

    out
}

/// Hash of the inputs that define who the agent is (name, conversation
/// kind, privacy scope, persona files, and in private scope
/// `memory_identity`: the pinned, profile and preference part of the memory
/// core, from `memory::packing::identity`), prefixed with the scope name
/// (`private-1a2b...`) so a scope change can be told apart from a persona
/// edit (see [`hash_scope`]). The rest of MEMORY.md, the memory index and
/// notices are left out so routine memory writes do not invalidate
/// sessions.
pub fn persona_hash(
    paths: &Paths,
    agent_name: &str,
    conversation_kind: &str,
    scope: Scope,
    memory_identity: &str,
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
        .filter(|f| f.in_prompt)
        .filter(|f| private || shared_with_others(f.name))
    {
        h.update([0]);
        h.update(std::fs::read(system_file_path(paths, f)).unwrap_or_default());
    }
    if private {
        h.update([0]);
        h.update(memory_identity.as_bytes());
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
            working_notes: "",
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
        let with_browser = assemble(&p, &b);
        assert!(with_browser.contains("`browser_snapshot`"));
        assert!(with_browser.contains("take over"));
        b.scope = Scope::Shared;
        assert!(!assemble(&p, &b).contains("## Your browser"));
    }

    #[test]
    fn working_notes_only_in_private_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        let mut c = ctx("Nova");
        c.working_notes = "<working_note domain=\"garden\">Water Tuesdays</working_note>";
        let private = assemble(&p, &c);
        assert!(private.contains("## Your working notes"));
        assert!(private.contains("Water Tuesdays"));
        assert!(private.contains("update_working_note"));
        for scope in [Scope::Personal, Scope::Shared] {
            c.scope = scope;
            let other = assemble(&p, &c);
            assert!(!other.contains("Water Tuesdays"), "{scope:?}");
            assert!(!other.contains("## Your working notes"), "{scope:?}");
        }
        c.scope = Scope::Private;
        c.working_notes = "";
        assert!(assemble(&p, &c).contains("(No working notes yet.)"));
    }

    #[test]
    fn memory_refresh_keeps_private_notes_out_of_other_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        seed(&p).unwrap();
        std::fs::write(
            p.memory_dir().join("MEMORY.md"),
            "- Synthetic private note\n",
        )
        .unwrap();
        std::fs::write(
            p.memory_dir().join("people.md"),
            "# Synthetic topic\n- Detail\n",
        )
        .unwrap();
        let private = memory_prompt(&p, Scope::Private);
        assert!(private.contains("Synthetic private note") && private.contains("people.md"));
        for scope in [Scope::Personal, Scope::Shared] {
            let other = memory_prompt(&p, scope);
            assert!(!other.contains("Synthetic") && !other.contains("people.md"));
        }
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
        let hash = |name: &str, kind: &str, scope: Scope| persona_hash(&p, name, kind, scope, "m1");
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
        // MEMORY.md counts only through its pinned and profile part.
        std::fs::write(p.memory_dir().join("MEMORY.md"), "new fact").unwrap();
        assert_eq!(base, hash("Iris", "main", Scope::Private));
        assert_ne!(
            base,
            persona_hash(&p, "Iris", "main", Scope::Private, "m2"),
            "pinned or profile notes changed"
        );
        // Owner-only files and memory do not affect sessions that never see them.
        std::fs::write(p.persona_dir().join("USER.md"), "changed").unwrap();
        assert_eq!(shared, hash("Iris", "group", Scope::Shared));
        assert_eq!(
            shared,
            persona_hash(&p, "Iris", "group", Scope::Shared, "m2")
        );
        assert_ne!(base, hash("Iris", "main", Scope::Private));
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
