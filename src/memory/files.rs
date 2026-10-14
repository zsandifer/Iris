//! The memory folder: a generated Markdown export of the notes, the
//! one-time import of the Markdown notes Iris kept before, and the
//! `read_memory` / `write_memory` file view the agent uses.
//!
//! The export is written inside the same write transaction as the change
//! it shows, so the server and `iris mcp` never leave an older export on
//! disk after a newer change.

use super::markdown::{self, HEADER};
use super::{Actor, Added, Item, KIND_FACT, NewItem, Topic};
use crate::config::{Paths, ensure_private_dir, write_private_file};
use crate::db::{Db, write_tx};
use crate::util::{Result, new_id, now_ms};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The always-in-prompt notes file.
pub const CORE_FILE: &str = "MEMORY.md";
/// Character budget for the core (whole notes only; the rest stay in their
/// topic files).
const CORE_BUDGET_CHARS: usize = 11_000;
/// Notes written to `MEMORY.md` (or imported from it) are kept in mind.
const CORE_IMPORTANCE: i64 = 4;
const DEFAULT_IMPORTANCE: i64 = 3;
const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 6;
/// The memory template Iris seeded before notes were stored as rows; never
/// imported as a note.
const LEGACY_TEMPLATE: &str = "# Memory\n\nCurated long-term facts the agent should always have in mind. Keep this file\nshort; put detailed notes in separate files in this folder (one topic per\nfile) and the agent will read them when relevant.\n";

/// Folder (in the data directory) for the original Markdown files.
pub fn backup_root(paths: &Paths) -> PathBuf {
    paths.data_dir.join("memory-backup")
}

/// Whether `rel` (relative to the memory folder) is a file Iris generates.
pub fn is_export_name(rel: &str) -> bool {
    rel == CORE_FILE || Topic::ALL.iter().any(|t| t.file() == rel)
}

fn is_generated(content: &str) -> bool {
    content.starts_with(HEADER)
}

fn io_err(e: impl Into<crate::util::BoxError>) -> rusqlite::Error {
    // Carries the I/O error out of the transaction (and rolls it back).
    rusqlite::Error::ToSqlConversionFailure(e.into())
}

/// The notes in `MEMORY.md`: pinned notes, profile and preference notes,
/// then important ones, oldest first, while they fit the budget.
fn core_items(live: &[Item]) -> Vec<&Item> {
    let mut candidates: Vec<&Item> = live.iter().filter(|i| i.core_candidate()).collect();
    candidates.sort_by_key(|i| {
        (
            !i.pinned,
            !super::CORE_KINDS.contains(&i.kind.as_str()),
            std::cmp::Reverse(i.importance),
            i.created_at,
            i.id,
        )
    });
    let mut used = 0;
    candidates
        .into_iter()
        .filter(|i| {
            let size = i.text.chars().count() + 3;
            let fits = used + size <= CORE_BUDGET_CHARS;
            if fits {
                used += size;
            }
            fits
        })
        .collect()
}

/// Rewrite the export from the database. Call inside the write
/// transaction that made the change.
pub fn render_export(conn: &Connection, paths: &Paths) -> rusqlite::Result<()> {
    let live = super::live(conn)?;
    let dir = paths.memory_dir();
    ensure_private_dir(&dir).map_err(io_err)?;
    let mut core = core_items(&live);
    core.sort_by_key(|i| (i.created_at, i.id));
    write_private_file(
        &dir.join(CORE_FILE),
        markdown::render_core(&core).as_bytes(),
    )
    .map_err(io_err)?;
    for topic in Topic::ALL {
        let mut of: Vec<&Item> = live.iter().filter(|i| i.topic == topic).collect();
        of.sort_by_key(|i| (!i.pinned, i.created_at, i.id));
        let path = dir.join(topic.file());
        if !of.is_empty() {
            write_private_file(&path, markdown::render_topic(topic, &of).as_bytes())
                .map_err(io_err)?;
        } else if std::fs::read_to_string(&path).is_ok_and(|c| is_generated(&c)) {
            std::fs::remove_file(&path).map_err(io_err)?;
        }
    }
    Ok(())
}

/// The core for the system prompt: `MEMORY.md` without the generated
/// header, or None when it holds no notes.
pub fn core_for_prompt(paths: &Paths) -> Option<String> {
    let body = std::fs::read_to_string(paths.memory_dir().join(CORE_FILE)).ok()?;
    let body = body.strip_prefix(HEADER).unwrap_or(&body).trim();
    if markdown::parse(body).notes.is_empty() {
        return None;
    }
    Some(body.to_string())
}

/// The topic a note from `rel` most likely belongs to: from the file's
/// path, then the headings above the note (nearest first), then the file
/// title.
fn infer_topic(rel: &str, headings: &[String], title: Option<&str>) -> Topic {
    let lowered = rel.to_lowercase();
    let file = lowered
        .trim_end_matches(".md")
        .trim_end_matches(".markdown");
    let hints = std::iter::once(file)
        .chain(headings.iter().rev().map(String::as_str))
        .chain(title);
    for hint in hints {
        let hint = hint.trim().to_lowercase();
        // The export's own headings.
        if let Some(t) = Topic::ALL
            .into_iter()
            .find(|t| t.title().eq_ignore_ascii_case(&hint) || t.name() == hint)
        {
            return t;
        }
        let has = |words: &[&str]| words.iter().any(|w| hint.contains(w));
        if has(&["people", "family", "friend", "contact"]) {
            return Topic::People;
        }
        if has(&["project"]) {
            return Topic::Projects;
        }
        if has(&["routine", "habit", "schedule"]) {
            return Topic::Routines;
        }
    }
    Topic::You
}

/// Context for notes from a file of its own (`people/alex.md`): its title,
/// or a name made from the file name.
fn file_title(rel: &str, title: Option<&str>) -> String {
    match title.filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => {
            let stem = Path::new(rel)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let words = stem.replace(['-', '_'], " ");
            let mut chars = words.trim().chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

fn is_legacy_template_block(text: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let block = norm(text);
    markdown::parse(LEGACY_TEMPLATE)
        .notes
        .iter()
        .any(|n| norm(&n.text) == block)
}

/// A note from Markdown, ready to add.
struct Candidate {
    text: String,
    topic: Topic,
}

/// Titles and headings the export itself uses: they group notes but say
/// nothing about them.
fn generic_heading(h: &str) -> bool {
    let h = h.trim();
    h.is_empty()
        || h.eq_ignore_ascii_case("memory")
        || h.eq_ignore_ascii_case("always in mind")
        || Topic::ALL
            .iter()
            .any(|t| h.eq_ignore_ascii_case(t.title()) || h.eq_ignore_ascii_case(t.name()))
}

/// Notes in `content`, as found in file `rel`. A note under a title or
/// heading that names its subject (`# Alex`, `## Work`) is prefixed with it
/// so it still makes sense on its own; a file without a title is named
/// after its file name. The export's own headings add nothing, so its
/// notes read back unchanged.
fn candidates(rel: &str, content: &str) -> Vec<Candidate> {
    let parsed = markdown::parse(content);
    let own_file = !is_export_name(rel);
    let title = match parsed.title.as_deref() {
        Some(t) if !generic_heading(t) => t.to_string(),
        None if own_file => file_title(rel, None),
        _ => String::new(),
    };
    let title = if generic_heading(&title) {
        String::new()
    } else {
        title
    };
    let fixed_topic = Topic::ALL.into_iter().find(|t| t.file() == rel);
    parsed
        .notes
        .iter()
        .filter(|n| !(rel == CORE_FILE && is_legacy_template_block(&n.text)))
        .map(|n| {
            let context = std::iter::once(title.as_str())
                .chain(
                    n.headings
                        .iter()
                        .map(String::as_str)
                        .filter(|h| !generic_heading(h) && *h != title),
                )
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(" — ");
            let text = if context.is_empty()
                || n.text.to_lowercase().starts_with(&context.to_lowercase())
            {
                n.text.clone()
            } else {
                format!("{context}: {}", n.text)
            };
            let topic = fixed_topic.unwrap_or_else(|| {
                infer_topic(
                    if own_file { rel } else { "" },
                    &n.headings,
                    parsed.title.as_deref().filter(|_| own_file),
                )
            });
            Candidate { text, topic }
        })
        .collect()
}

/// Counts from adding notes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub added: usize,
    /// Notes that were already there.
    pub existing: usize,
}

/// Add the notes in `content` (from file `rel`) without removing any.
/// Call inside a write transaction.
fn ingest(
    conn: &Connection,
    rel: &str,
    content: &str,
    actor: Actor,
    source: &Value,
    batch: &str,
) -> rusqlite::Result<Counts> {
    let importance = if rel == CORE_FILE {
        CORE_IMPORTANCE
    } else {
        DEFAULT_IMPORTANCE
    };
    let mut counts = Counts::default();
    for c in candidates(rel, content) {
        let new = NewItem {
            kind: KIND_FACT,
            topic: c.topic,
            text: &c.text,
            importance,
            source,
        };
        match super::add(conn, &new, actor, batch)? {
            Added::New => counts.added += 1,
            Added::Existing(item) => {
                counts.existing += 1;
                super::regroup(conn, &item, importance, item.topic, actor, batch)?;
            }
        }
    }
    Ok(counts)
}

/// Run `f` in a write transaction and rewrite the export before it
/// commits. If anything fails the transaction rolls back, and the export is
/// rewritten from what is stored, so a half-written export never outlives
/// the failure.
fn with_export<R>(
    db: &Db,
    paths: &Paths,
    f: impl FnOnce(&Connection) -> rusqlite::Result<R>,
) -> Result<R> {
    let out = db.with(|c| {
        write_tx(c, |tx| {
            let out = f(tx)?;
            render_export(tx, paths)?;
            Ok(out)
        })
    });
    if out.is_err() {
        let _ = db.with(|c| render_export(c, paths));
    }
    out
}

/// Import Markdown notes (`(path relative to the memory folder, content)`)
/// as notes and rewrite the export. Returns counts per file. Used by
/// `iris import openclaw`.
pub fn import_notes(db: &Db, paths: &Paths, files: &[(String, String)]) -> Result<Vec<Counts>> {
    with_export(db, paths, |tx| {
        let batch = new_id();
        let mut out = Vec::new();
        for (rel, content) in files {
            let source = json!({"type": "import", "file": rel});
            out.push(ingest(tx, rel, content, Actor::Import, &source, &batch)?);
        }
        Ok(out)
    })
}

/// A Markdown file in the memory folder that Iris did not generate.
struct Foreign {
    rel: String,
    content: String,
    legacy_template: bool,
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Foreign>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // file_type() does not follow symlinks; skip them entirely.
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            collect(root, &path, depth + 1, out);
            continue;
        }
        if !ft.is_file()
            || !crate::persona::is_text_note(&path)
            || entry
                .metadata()
                .map_or(true, |m| m.len() > MAX_IMPORT_BYTES)
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().into_owned();
        if is_export_name(&rel) && is_generated(&content) {
            continue;
        }
        let legacy_template = rel == CORE_FILE && content.trim() == LEGACY_TEMPLATE.trim();
        out.push(Foreign {
            rel,
            content,
            legacy_template,
        });
    }
}

/// What [`prepare`] did.
#[derive(Debug, Default)]
pub struct Prepared {
    pub files: usize,
    pub counts: Counts,
    pub backup: Option<PathBuf>,
}

/// At startup: import the Markdown notes Iris did not generate (the notes
/// kept before memory moved into the database, or files placed in the
/// folder since), copy the originals to `memory-backup/`, and rewrite the
/// export. Importing is idempotent: a note already stored is not added
/// again.
pub fn prepare(db: &Db, paths: &Paths) -> Result<Prepared> {
    let root = paths.memory_dir();
    ensure_private_dir(&root)?;
    let mut found = Vec::new();
    collect(&root, &root, 0, &mut found);
    let mut report = Prepared::default();
    // Copy first: the export overwrites MEMORY.md.
    let notes: Vec<&Foreign> = found.iter().filter(|f| !f.legacy_template).collect();
    if !notes.is_empty() {
        let stamp = crate::util::rfc3339(now_ms() / 1000).replace(':', "-");
        let mut dest = backup_root(paths).join(&stamp);
        if dest.exists() {
            dest = backup_root(paths).join(format!("{stamp}-{}", new_id()));
        }
        for f in &notes {
            let to = dest.join(&f.rel);
            if let Some(parent) = to.parent() {
                ensure_private_dir(parent)?;
            }
            write_private_file(&to, f.content.as_bytes())?;
        }
        report.backup = Some(dest);
    }
    let imported = db.with(|c| {
        write_tx(c, |tx| {
            let batch = new_id();
            for f in &notes {
                let source = json!({"type": "import", "file": f.rel});
                let counts = ingest(tx, &f.rel, &f.content, Actor::Import, &source, &batch)?;
                report.counts.added += counts.added;
                report.counts.existing += counts.existing;
            }
            render_export(tx, paths)
        })
    });
    if let Err(e) = imported {
        // Nothing was stored: put back any original the export replaced, so
        // the next start imports it again.
        for f in &notes {
            let path = root.join(&f.rel);
            if std::fs::read_to_string(&path).is_ok_and(|c| is_generated(&c)) {
                let _ = write_private_file(&path, f.content.as_bytes());
            }
        }
        return Err(e);
    }
    report.files = notes.len();
    // The originals are in the backup and their notes in the database.
    for f in &found {
        let path = root.join(&f.rel);
        if std::fs::read_to_string(&path).is_ok_and(|c| !is_generated(&c)) {
            std::fs::remove_file(&path)?;
        }
    }
    remove_empty_dirs(&root, &root);
    Ok(report)
}

fn remove_empty_dirs(root: &Path, dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty_dirs(root, &entry.path());
        }
    }
    if dir != root {
        // Fails (and is left alone) when not empty.
        let _ = std::fs::remove_dir(dir);
    }
}

/// What a `write_memory` call changed.
#[derive(Debug, Default)]
pub struct Written {
    pub added: usize,
    pub unchanged: usize,
    /// Texts of notes set aside because the new content left them out.
    pub removed: Vec<String>,
    /// Pinned notes are the owner's; the agent cannot remove them.
    pub kept_pinned: Vec<String>,
    /// Notes another chat saved or changed after this turn began: left
    /// out of the content, but kept, since this turn may not have seen them.
    pub kept_recent: Vec<String>,
    /// Notes in the content that were removed or corrected after this
    /// turn began: not saved again.
    pub not_restored: Vec<String>,
}

/// `write_memory` for the owner's private chat: make the file at `rel`
/// say `content`.
///
/// For an export file (`MEMORY.md`, `people.md`, ...) the notes it shows
/// become exactly the notes in `content`: new ones are added, ones left out
/// are archived (kept in history, so it can be undone). A note also appears
/// in its topic file, so removing it from `MEMORY.md` removes it there
/// too. Notes saved or changed since `turn_started` (ms) by anything but
/// this turn (`source`) are never removed: the turn may have read the file
/// before they existed. Any other path only adds notes.
pub fn write(
    db: &Db,
    paths: &Paths,
    rel: &str,
    content: &str,
    source: &Value,
    turn_started: i64,
) -> Result<Written> {
    let export = is_export_name(rel);
    if !export && markdown::parse(content).notes.is_empty() {
        return Err("write at least one note (one per bullet)".into());
    }
    with_export(db, paths, |tx| {
        let batch = new_id();
        let mut out = Written::default();
        let live = super::live(tx)?;
        let fixed_topic = Topic::ALL.into_iter().find(|t| t.file() == rel);
        // The notes this file shows; other paths only add.
        let view: Vec<&Item> = match (export, fixed_topic) {
            (false, _) => Vec::new(),
            (true, Some(topic)) => live.iter().filter(|i| i.topic == topic).collect(),
            (true, None) => core_items(&live),
        };
        let shown: std::collections::HashSet<&str> =
            view.iter().map(|i| i.content_hash.as_str()).collect();
        let importance = if rel == CORE_FILE {
            CORE_IMPORTANCE
        } else {
            DEFAULT_IMPORTANCE
        };
        let mut wanted = std::collections::HashSet::new();
        for c in candidates(rel, content) {
            let hash = super::content_hash(&markdown::normalize(&c.text));
            if !wanted.insert(hash.clone()) {
                continue;
            }
            if shown.contains(hash.as_str()) {
                out.unchanged += 1;
                continue;
            }
            // Removed or corrected elsewhere after this turn began: the
            // turn is writing back what it read before that.
            if super::set_aside_since(tx, &hash, turn_started)? {
                out.not_restored.push(c.text.clone());
                continue;
            }
            let new = NewItem {
                kind: KIND_FACT,
                topic: c.topic,
                text: &c.text,
                importance,
                source,
            };
            match super::add(tx, &new, Actor::Agent, &batch)? {
                Added::New => out.added += 1,
                // Stored elsewhere: show it here as well.
                Added::Existing(item) => {
                    let topic = fixed_topic.unwrap_or(item.topic);
                    super::regroup(tx, &item, importance, topic, Actor::Agent, &batch)?;
                    out.unchanged += 1;
                }
            }
        }
        for item in view.iter().filter(|i| !wanted.contains(&i.content_hash)) {
            if item.pinned {
                out.kept_pinned.push(item.text.clone());
            } else if item.updated_at >= turn_started && item.source != *source {
                out.kept_recent.push(item.text.clone());
            } else {
                super::archive(tx, item, Actor::Agent, &batch)?;
                out.removed.push(item.text.clone());
            }
        }
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Paths, Db) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        (tmp, paths, db)
    }

    fn live_texts(db: &Db) -> Vec<(String, Topic, i64)> {
        db.with(super::super::live)
            .unwrap()
            .into_iter()
            .map(|i| (i.text, i.topic, i.importance))
            .collect()
    }

    fn read(paths: &Paths, rel: &str) -> String {
        std::fs::read_to_string(paths.memory_dir().join(rel)).unwrap()
    }

    fn history_count(db: &Db) -> i64 {
        db.with(|c| c.query_row("SELECT COUNT(*) FROM memory_history", [], |r| r.get(0)))
            .unwrap()
    }

    #[test]
    fn the_seeded_template_is_the_empty_export() {
        let (_t, paths, db) = setup();
        assert_eq!(
            crate::persona::system_file(CORE_FILE).unwrap().template,
            markdown::render_core(&[])
        );
        let report = prepare(&db, &paths).unwrap();
        assert_eq!((report.files, report.counts), (0, Counts::default()));
        assert!(report.backup.is_none() && !backup_root(&paths).exists());
        assert_eq!(read(&paths, CORE_FILE), markdown::render_core(&[]));
        assert!(core_for_prompt(&paths).is_none());
    }

    #[test]
    fn existing_notes_are_imported_once_and_backed_up() {
        let (_t, paths, db) = setup();
        let mem = paths.memory_dir();
        // An install from before: the old template with the owner's facts.
        std::fs::write(
            mem.join(CORE_FILE),
            format!("{LEGACY_TEMPLATE}\n- Prefers mornings.\n- Allergic to peanuts.\n"),
        )
        .unwrap();
        std::fs::create_dir_all(mem.join("people")).unwrap();
        std::fs::write(
            mem.join("people/alex.md"),
            "# Alex\n\n- Friend from the climbing gym\n- Birthday in March\n\n## Work\n\nTeaches chemistry.\n",
        )
        .unwrap();
        std::fs::write(mem.join("trip-notes.md"), "Window seat on long flights.\n").unwrap();
        std::fs::write(mem.join("routines.md"), "# Mornings\n- Run at 7\n").unwrap();
        std::fs::write(mem.join("photo.jpg"), [0u8; 4]).unwrap();
        std::fs::write(mem.join(".hidden.md"), "not a note").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", mem.join("link.md")).unwrap();

        let report = prepare(&db, &paths).unwrap();
        assert_eq!(report.files, 4);
        assert_eq!(
            report.counts,
            Counts {
                added: 7,
                existing: 0
            }
        );
        let notes = live_texts(&db);
        assert_eq!(
            notes,
            [
                ("Prefers mornings.".to_string(), Topic::You, 4),
                ("Allergic to peanuts.".to_string(), Topic::You, 4),
                (
                    "Alex: Friend from the climbing gym".to_string(),
                    Topic::People,
                    3
                ),
                ("Alex: Birthday in March".to_string(), Topic::People, 3),
                (
                    "Alex — Work: Teaches chemistry.".to_string(),
                    Topic::People,
                    3
                ),
                ("Mornings: Run at 7".to_string(), Topic::Routines, 3),
                (
                    "Trip notes: Window seat on long flights.".to_string(),
                    Topic::You,
                    3
                ),
            ]
        );
        let sources: Vec<Value> = db
            .with(super::super::live)
            .unwrap()
            .into_iter()
            .map(|i| i.source)
            .collect();
        assert_eq!(sources[0], json!({"type": "import", "file": "MEMORY.md"}));
        assert_eq!(
            sources[2],
            json!({"type": "import", "file": "people/alex.md"})
        );
        // The originals are kept, unchanged, in the backup.
        let backup = report.backup.unwrap();
        assert!(backup.starts_with(backup_root(&paths)));
        assert!(
            std::fs::read_to_string(backup.join("people/alex.md"))
                .unwrap()
                .contains("Teaches chemistry.")
        );
        assert!(
            std::fs::read_to_string(backup.join(CORE_FILE))
                .unwrap()
                .contains("Allergic")
        );
        // The memory folder now holds the export (and what is not a note).
        assert!(!mem.join("people").exists() && !mem.join("trip-notes.md").exists());
        assert!(mem.join("photo.jpg").exists() && mem.join(".hidden.md").exists());
        let core = read(&paths, CORE_FILE);
        assert!(core.starts_with(HEADER) && core.contains("## About you\n\n- Prefers mornings.\n"));
        assert!(!core.contains("Curated long-term facts") && !core.contains("Alex"));
        let people = read(&paths, "people.md");
        assert!(people.contains("# People\n\n- Alex: Friend from the climbing gym\n"));
        assert!(read(&paths, "routines.md").contains("- Mornings: Run at 7"));
        assert!(!mem.join("projects.md").exists());
        let prompt = core_for_prompt(&paths).unwrap();
        assert!(prompt.starts_with("# Memory") && prompt.contains("Allergic to peanuts."));

        // Again: nothing to import, nothing changes.
        let history = history_count(&db);
        let again = prepare(&db, &paths).unwrap();
        assert_eq!((again.files, again.backup.is_none()), (0, true));
        assert_eq!(live_texts(&db), notes);
        assert_eq!(history_count(&db), history);
        // A note file placed later (or left by a crash before the originals
        // were removed) is imported without duplicating notes.
        std::fs::write(mem.join("extra.md"), "- New fact\n").unwrap();
        std::fs::write(
            mem.join(CORE_FILE),
            "- Prefers mornings.\n- Allergic to peanuts.\n",
        )
        .unwrap();
        let later = prepare(&db, &paths).unwrap();
        assert_eq!(
            later.counts,
            Counts {
                added: 1,
                existing: 2
            }
        );
        assert_eq!(live_texts(&db).len(), 8);
        assert!(!mem.join("extra.md").exists());
        assert!(is_generated(&read(&paths, CORE_FILE)));
    }

    #[test]
    fn writing_an_export_file_changes_only_what_was_edited() {
        let (_t, paths, db) = setup();
        let src = json!({"type": "conversation", "conversation_id": "c1", "message_ids": ["m1"]});
        // Old-style paths still work and only add.
        let w = write(
            &db,
            &paths,
            "people/sam.md",
            "# Sam\n- Likes jazz\n",
            &src,
            0,
        )
        .unwrap();
        assert_eq!((w.added, w.removed.len()), (1, 0));
        let w = write(
            &db,
            &paths,
            "people/sam.md",
            "# Sam\n- Plays chess\n",
            &src,
            0,
        )
        .unwrap();
        assert_eq!((w.added, w.removed.len()), (1, 0));
        assert!(read(&paths, "people.md").contains("- Sam: Likes jazz\n- Sam: Plays chess\n"));

        // MEMORY.md: read it, add a line under a topic heading, write it back.
        let w = write(&db, &paths, CORE_FILE, "- Prefers mornings\n", &src, 0).unwrap();
        assert_eq!(w.added, 1);
        let core = read(&paths, CORE_FILE);
        let edited = format!("{core}\n## Projects\n\n- Building a bookshelf\n");
        let w = write(&db, &paths, CORE_FILE, &edited, &src, 0).unwrap();
        assert_eq!((w.added, w.unchanged, w.removed.len()), (1, 1, 0));
        assert!(read(&paths, "projects.md").contains("- Building a bookshelf"));
        assert!(read(&paths, CORE_FILE).contains("## Projects\n\n- Building a bookshelf\n"));
        // A note already stored elsewhere joins the core instead of repeating.
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            &format!("{}\n- Sam: Plays chess\n", read(&paths, CORE_FILE)),
            &src,
            0,
        )
        .unwrap();
        assert_eq!((w.added, w.unchanged), (0, 3));
        assert!(read(&paths, CORE_FILE).contains("Sam: Plays chess"));
        assert_eq!(live_texts(&db).len(), 4);

        // Editing a line: the old note is set aside, the new one added.
        let core = read(&paths, CORE_FILE).replace("Prefers mornings", "Prefers evenings");
        let w = write(&db, &paths, CORE_FILE, &core, &src, 0).unwrap();
        assert_eq!(
            (w.added, w.removed.clone()),
            (1, vec!["Prefers mornings".to_string()])
        );
        let texts: Vec<String> = live_texts(&db).into_iter().map(|t| t.0).collect();
        assert!(texts.contains(&"Prefers evenings".to_string()));
        assert!(!texts.contains(&"Prefers mornings".to_string()));
        // Removing a note from its topic file removes it everywhere.
        let people = read(&paths, "people.md").replace("- Sam: Plays chess\n", "");
        let w = write(&db, &paths, "people.md", &people, &src, 0).unwrap();
        assert_eq!(w.removed, ["Sam: Plays chess"]);
        assert!(!read(&paths, CORE_FILE).contains("chess"));
        // Archived, not deleted, and every change has a history row.
        let (archived, agent_rows): (i64, i64) = db
            .with(|c| {
                Ok((
                    c.query_row(
                        "SELECT COUNT(*) FROM memory_items WHERE archived = 1",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row(
                        "SELECT COUNT(*) FROM memory_history WHERE actor = 'agent'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!(archived, 2);
        assert_eq!(agent_rows, 8);
        let source: String = db
            .with(|c| c.query_row("SELECT source FROM memory_items LIMIT 1", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&source).unwrap(), src);
        // Pinned notes are the owner's to remove.
        db.with(|c| {
            c.execute(
                "UPDATE memory_items SET pinned = 1 WHERE text = 'Prefers evenings'",
                [],
            )
        })
        .unwrap();
        let w = write(&db, &paths, CORE_FILE, "- Something else\n", &src, 0).unwrap();
        assert_eq!(w.kept_pinned, ["Prefers evenings"]);
        assert!(read(&paths, CORE_FILE).contains("## Always in mind\n\n- Prefers evenings\n"));
        // Old-style paths need at least one note; an export file can be emptied.
        assert!(write(&db, &paths, "notes.md", "# Notes\n", &src, 0).is_err());
        let w = write(&db, &paths, "people.md", "# People\n", &src, 0).unwrap();
        assert_eq!(w.removed, ["Sam: Likes jazz"]);
        assert!(!paths.memory_dir().join("people.md").exists());
    }

    #[test]
    fn notes_another_chat_saved_during_the_turn_are_kept() {
        let (_t, paths, db) = setup();
        let mine = json!({"type": "conversation", "conversation_id": "c1", "message_ids": ["m1"]});
        let other = json!({"type": "conversation", "conversation_id": "c2", "message_ids": ["m2"]});
        write(&db, &paths, "people.md", "- Old friend\n", &other, 0).unwrap();
        let started = crate::util::now_ms() + 1;
        while crate::util::now_ms() < started {}
        // This turn reads people.md; meanwhile another chat adds a note.
        let seen = read(&paths, "people.md");
        write(
            &db,
            &paths,
            "people.md",
            "- Old friend\n- New neighbour\n",
            &other,
            0,
        )
        .unwrap();
        let w = write(
            &db,
            &paths,
            "people.md",
            &format!("{seen}- Just added here\n"),
            &mine,
            started,
        )
        .unwrap();
        assert_eq!(
            (w.added, w.kept_recent.clone()),
            (1, vec!["New neighbour".to_string()])
        );
        // This turn's own new note can be removed again in the same turn.
        let w = write(
            &db,
            &paths,
            "people.md",
            &seen.replace("- Old friend\n", ""),
            &mine,
            started,
        )
        .unwrap();
        assert_eq!(w.removed, ["Old friend", "Just added here"]);
        assert_eq!(w.kept_recent, ["New neighbour"]);
        assert!(read(&paths, "people.md").contains("- New neighbour"));
    }

    #[test]
    fn a_failed_change_leaves_the_export_as_stored() {
        let (_t, paths, db) = setup();
        let src = json!({"type": "owner"});
        write(&db, &paths, CORE_FILE, "- Prefers mornings\n", &src, 0).unwrap();
        let before = read(&paths, CORE_FILE);
        // people.md cannot be written; MEMORY.md is written first.
        std::fs::create_dir(paths.memory_dir().join("people.md")).unwrap();
        let edited = format!("{before}\n## People\n\n- Has a brother\n");
        assert!(write(&db, &paths, CORE_FILE, &edited, &src, 0).is_err());
        assert_eq!(read(&paths, CORE_FILE), before);
        assert_eq!(live_texts(&db).len(), 1);
    }

    #[test]
    fn imported_headings_keep_their_subject() {
        let (_t, paths, db) = setup();
        let mem = paths.memory_dir();
        std::fs::write(
            mem.join("people.md"),
            "# People\n\n## Alex\n- Birthday in March\n### Work\n- Teaches chemistry\n\n\
             ## Sam\n- Birthday in March\n### Work\n- Teaches chemistry\n",
        )
        .unwrap();
        std::fs::write(
            mem.join(CORE_FILE),
            "# Memory\n\n## Health\n- Allergic to peanuts\n",
        )
        .unwrap();
        prepare(&db, &paths).unwrap();
        let texts: Vec<String> = live_texts(&db).into_iter().map(|t| t.0).collect();
        assert_eq!(
            texts,
            [
                "Health: Allergic to peanuts",
                "Alex: Birthday in March",
                "Alex — Work: Teaches chemistry",
                "Sam: Birthday in March",
                "Sam — Work: Teaches chemistry",
            ]
        );
    }

    #[test]
    fn a_failed_first_import_leaves_the_originals_to_import_again() {
        let (_t, paths, db) = setup();
        let mem = paths.memory_dir();
        let original = "# Memory\n\n## Routines\n- Run at 7\n";
        std::fs::write(mem.join(CORE_FILE), original).unwrap();
        // routines.md cannot be written; MEMORY.md is written first.
        std::fs::create_dir(mem.join("routines.md")).unwrap();
        assert!(prepare(&db, &paths).is_err());
        assert_eq!(read(&paths, CORE_FILE), original);
        assert!(live_texts(&db).is_empty());
        std::fs::remove_dir(mem.join("routines.md")).unwrap();
        prepare(&db, &paths).unwrap();
        assert_eq!(
            live_texts(&db),
            [("Run at 7".to_string(), Topic::Routines, 4)]
        );
    }

    #[test]
    fn a_stale_write_does_not_bring_back_what_another_chat_removed() {
        let (_t, paths, db) = setup();
        let a = json!({"type": "conversation", "conversation_id": "a", "message_ids": ["ma"]});
        let b = json!({"type": "conversation", "conversation_id": "b", "message_ids": ["mb"]});
        write(&db, &paths, "people.md", "- Alex lives in Boston\n", &b, 0).unwrap();
        let started = crate::util::now_ms() + 1;
        while crate::util::now_ms() < started {}
        // Chat A reads people.md; chat B then corrects the note.
        let seen = read(&paths, "people.md");
        write(
            &db,
            &paths,
            "people.md",
            "- Alex lives in Seattle\n",
            &b,
            started,
        )
        .unwrap();
        // A writes its stale copy with an unrelated addition.
        let w = write(
            &db,
            &paths,
            "people.md",
            &format!("{seen}- Sam bakes bread\n"),
            &a,
            started,
        )
        .unwrap();
        assert_eq!(w.added, 1);
        assert_eq!(w.not_restored, ["Alex lives in Boston"]);
        assert_eq!(w.kept_recent, ["Alex lives in Seattle"]);
        let people = read(&paths, "people.md");
        assert!(people.contains("Seattle") && !people.contains("Boston"));
    }

    #[test]
    fn the_core_keeps_whole_notes_within_its_budget() {
        let mut live = Vec::new();
        for i in 0..30 {
            let mut item =
                crate::memory::tests::item(i, &format!("{i} {}", "x".repeat(900)), Topic::You);
            item.importance = if i % 2 == 0 { 4 } else { 3 };
            live.push(item);
        }
        live[29].pinned = true;
        let core = core_items(&live);
        assert_eq!(core[0].id, 29, "pinned first");
        assert!(core.iter().all(|i| i.pinned || i.importance == 4));
        let total: usize = core.iter().map(|i| i.text.chars().count() + 3).sum();
        assert!(total <= CORE_BUDGET_CHARS);
        assert_eq!(core.len(), 12);
    }
}
