//! The memory folder: a generated Markdown export of the notes, the
//! one-time import of the Markdown notes Iris kept before, and the
//! `read_memory` / `write_memory` file view the agent uses.
//!
//! The export is written inside the same write transaction as the change
//! it shows, so the server and `iris mcp` never leave an older export on
//! disk after a newer change.

use super::markdown::{self, HEADER, LeftOut};
use super::packing::{self, CORE_IMPORTANCE};
use super::{Actor, Added, Item, KIND_FACT, NewItem, Topic};
use crate::config::{Paths, ensure_private_dir, write_private_file};
use crate::db::{Db, write_tx};
use crate::util::{Result, new_id, now_ms};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The always-in-prompt notes file.
pub const CORE_FILE: &str = "MEMORY.md";
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

/// [`is_generated`] reading only the start of the file.
fn starts_with_header(path: &Path) -> bool {
    use std::io::Read;
    let mut start = vec![0; HEADER.len()];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut start))
        .is_ok_and(|_| start == HEADER.as_bytes())
}

fn io_err(e: impl Into<crate::util::BoxError>) -> rusqlite::Error {
    // Carries the I/O error out of the transaction (and rolls it back).
    rusqlite::Error::ToSqlConversionFailure(e.into())
}

/// Content hashes of the notes in `MEMORY.md`.
pub type CoreHashes = std::collections::HashSet<String>;

/// Rewrite the export from the database and record which notes are in the
/// core. Call inside the write transaction that made the change. Returns
/// the core's notes.
pub fn render_export(conn: &Connection, paths: &Paths) -> rusqlite::Result<CoreHashes> {
    let live = super::live(conn)?;
    let dir = paths.memory_dir();
    ensure_private_dir(&dir).map_err(io_err)?;
    let now = now_ms();
    let packed = packing::pack(&live, now);
    packing::record(conn, &live, &packed.items, now)?;
    let mut core = packed.items.clone();
    core.sort_by_key(|i| (i.created_at, i.id));
    write_private_file(
        &dir.join(CORE_FILE),
        markdown::render_core(&core, packed.left_out).as_bytes(),
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
    Ok(core.iter().map(|i| i.content_hash.clone()).collect())
}

/// The core for the system prompt: `MEMORY.md` without the generated
/// header, or None when it is the empty export.
pub fn core_for_prompt(paths: &Paths) -> Option<String> {
    let body = std::fs::read_to_string(paths.memory_dir().join(CORE_FILE)).ok()?;
    let strip = |s: &str| s.strip_prefix(HEADER).unwrap_or(s).trim().to_string();
    let body = strip(&body);
    let empty = strip(&markdown::render_core(&[], LeftOut::default()));
    (body != empty && !body.is_empty()).then_some(body)
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
) -> Result<(R, CoreHashes)> {
    let out = db.with(|c| {
        write_tx(c, |tx| {
            let out = f(tx)?;
            let core = render_export(tx, paths)?;
            Ok((out, core))
        })
    });
    if out.is_err() {
        // Serialized like any change, so it never overwrites a newer export.
        let _ = db.with(|c| write_tx(c, |tx| render_export(tx, paths)));
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
    .map(|(counts, _)| counts)
}

/// A file in the memory folder that Iris did not generate.
struct Foreign {
    rel: String,
    /// None when it cannot be read as notes (too large, not UTF-8). Kept
    /// only in the backup when the export replaces it.
    content: Option<String>,
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
        if !ft.is_file() || !crate::persona::is_text_note(&path) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().into_owned();
        // A generated export of any size is Iris's own.
        if is_export_name(&rel) && starts_with_header(&path) {
            continue;
        }
        let content = entry
            .metadata()
            .is_ok_and(|m| m.len() <= MAX_IMPORT_BYTES)
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten();
        match &content {
            Some(c) if is_export_name(&rel) && is_generated(c) => continue,
            // Left alone unless the export would replace it.
            None if !is_export_name(&rel) => continue,
            _ => {}
        }
        let legacy_template = rel == CORE_FILE
            && content
                .as_deref()
                .is_some_and(|c| c.trim() == LEGACY_TEMPLATE.trim());
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
    /// Files too large or not text, replaced by the export and kept only
    /// in the backup.
    pub unreadable: usize,
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
    // Copy first: the export overwrites MEMORY.md and the topic files.
    let kept: Vec<&Foreign> = found.iter().filter(|f| !f.legacy_template).collect();
    if !kept.is_empty() {
        let stamp = crate::util::rfc3339(now_ms() / 1000).replace(':', "-");
        let mut dest = backup_root(paths).join(&stamp);
        if dest.exists() {
            dest = backup_root(paths).join(format!("{stamp}-{}", new_id()));
        }
        for f in &kept {
            let to = dest.join(&f.rel);
            if let Some(parent) = to.parent() {
                ensure_private_dir(parent)?;
            }
            write_private_file(&to, &std::fs::read(root.join(&f.rel))?)?;
        }
        report.backup = Some(dest);
    }
    let notes: Vec<(&str, &str)> = kept
        .iter()
        .filter_map(|f| Some((f.rel.as_str(), f.content.as_deref()?)))
        .collect();
    report.unreadable = kept.len() - notes.len();
    // Store the notes before touching any file: if this stops early, the
    // originals are still in place and the next start imports them (notes
    // already stored are not added twice).
    db.with(|c| {
        write_tx(c, |tx| {
            let batch = new_id();
            for (rel, content) in &notes {
                let source = json!({"type": "import", "file": rel});
                let counts = ingest(tx, rel, content, Actor::Import, &source, &batch)?;
                report.counts.added += counts.added;
                report.counts.existing += counts.existing;
            }
            Ok(())
        })
    })?;
    report.files = notes.len();
    db.with(|c| write_tx(c, |tx| render_export(tx, paths)))?;
    // The originals are in the backup and their notes in the database.
    for f in &found {
        let path = root.join(&f.rel);
        if f.content.is_some() && std::fs::read_to_string(&path).is_ok_and(|c| !is_generated(&c)) {
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
    /// Notes another chat saved or changed after this turn began, or that
    /// joined `MEMORY.md` after it began (another change made room): left
    /// out of the content, but kept, since this turn may not have seen them.
    pub kept_recent: Vec<String>,
    /// Notes in the content that were removed or corrected after this
    /// turn began: not saved again.
    pub not_restored: Vec<String>,
    /// Notes written to `MEMORY.md` that are saved but did not fit in it
    /// (they are in their topic files).
    pub outside_core: Vec<String>,
}

/// `write_memory` for the owner's private chat: make the file at `rel`
/// say `content`.
///
/// For an export file (`MEMORY.md`, `people.md`, ...) the notes it shows
/// become exactly the notes in `content`: new ones are added, ones left out
/// are archived (kept in history, so it can be undone). A note also appears
/// in its topic file, so removing it from `MEMORY.md` removes it there
/// too. `MEMORY.md` shows the notes packed into the core when the export
/// was last written (see [`packing`]). `batch` identifies the turn's
/// changes: notes another change saved or changed since `turn_started`
/// (ms), or that joined `MEMORY.md` since then, are never removed, and notes
/// it removed are not brought back, since the turn may have read the file
/// before. Any other path only adds notes.
pub fn write(
    db: &Db,
    paths: &Paths,
    rel: &str,
    content: &str,
    source: &Value,
    turn_started: i64,
    batch: &str,
) -> Result<Written> {
    let export = is_export_name(rel);
    if !export && markdown::parse(content).notes.is_empty() {
        return Err("write at least one note (one per bullet)".into());
    }
    let (mut out, saved, core) = with_export(db, paths, |tx| {
        let mut out = Written::default();
        let live = super::live(tx)?;
        let fixed_topic = Topic::ALL.into_iter().find(|t| t.file() == rel);
        // The notes this file shows (as last written); other paths only add.
        let view: Vec<&Item> = match (export, fixed_topic) {
            (false, _) => Vec::new(),
            (true, Some(topic)) => live.iter().filter(|i| i.topic == topic).collect(),
            (true, None) => live.iter().filter(|i| i.core_since.is_some()).collect(),
        };
        let shown: std::collections::HashSet<&str> =
            view.iter().map(|i| i.content_hash.as_str()).collect();
        let importance = if rel == CORE_FILE {
            CORE_IMPORTANCE
        } else {
            DEFAULT_IMPORTANCE
        };
        let mut wanted = std::collections::HashSet::new();
        // Notes in the content that are stored after this write.
        let mut saved = Vec::new();
        for c in candidates(rel, content) {
            let hash = super::content_hash(&markdown::normalize(&c.text));
            if !wanted.insert(hash.clone()) {
                continue;
            }
            if shown.contains(hash.as_str()) {
                out.unchanged += 1;
                saved.push((hash, c.text));
                continue;
            }
            // Removed or corrected elsewhere after this turn began: the
            // turn is writing back what it read before that.
            if super::set_aside_since(tx, &hash, turn_started, batch)? {
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
            match super::add(tx, &new, Actor::Agent, batch)? {
                Added::New => out.added += 1,
                // Stored elsewhere: show it here as well.
                Added::Existing(item) => {
                    let topic = fixed_topic.unwrap_or(item.topic);
                    super::regroup(tx, &item, importance, topic, Actor::Agent, batch)?;
                    out.unchanged += 1;
                }
            }
            saved.push((hash, c.text));
        }
        for item in view.iter().filter(|i| !wanted.contains(&i.content_hash)) {
            if item.pinned {
                out.kept_pinned.push(item.text.clone());
                continue;
            }
            // Changed elsewhere, or moved into MEMORY.md by another change,
            // after this turn began: it may not have seen the note here.
            let since = item.updated_at.max(if fixed_topic.is_none() {
                item.core_since.unwrap_or(i64::MIN)
            } else {
                i64::MIN
            });
            if since >= turn_started && super::last_batch(tx, item.id)?.as_deref() != Some(batch) {
                out.kept_recent.push(item.text.clone());
            } else {
                super::archive(tx, item, Actor::Agent, batch)?;
                out.removed.push(item.text.clone());
            }
        }
        Ok((out, saved))
    })
    .map(|((out, saved), core)| (out, saved, core))?;
    if rel == CORE_FILE {
        out.outside_core = saved
            .into_iter()
            .filter(|(hash, _)| !core.contains(hash))
            .map(|(_, text)| text)
            .collect();
    }
    Ok(out)
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
            markdown::render_core(&[], LeftOut::default())
        );
        let report = prepare(&db, &paths).unwrap();
        assert_eq!((report.files, report.counts), (0, Counts::default()));
        assert!(report.backup.is_none() && !backup_root(&paths).exists());
        assert_eq!(
            read(&paths, CORE_FILE),
            markdown::render_core(&[], LeftOut::default())
        );
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
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!((w.added, w.removed.len()), (1, 0));
        let w = write(
            &db,
            &paths,
            "people/sam.md",
            "# Sam\n- Plays chess\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!((w.added, w.removed.len()), (1, 0));
        assert!(read(&paths, "people.md").contains("- Sam: Likes jazz\n- Sam: Plays chess\n"));

        // MEMORY.md: read it, add a line under a topic heading, write it back.
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Prefers mornings\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!(w.added, 1);
        let core = read(&paths, CORE_FILE);
        let edited = format!("{core}\n## Projects\n\n- Building a bookshelf\n");
        let w = write(&db, &paths, CORE_FILE, &edited, &src, i64::MAX, "t").unwrap();
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
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!((w.added, w.unchanged), (0, 3));
        assert!(read(&paths, CORE_FILE).contains("Sam: Plays chess"));
        assert_eq!(live_texts(&db).len(), 4);

        // Editing a line: the old note is set aside, the new one added.
        let core = read(&paths, CORE_FILE).replace("Prefers mornings", "Prefers evenings");
        let w = write(&db, &paths, CORE_FILE, &core, &src, i64::MAX, "t").unwrap();
        assert_eq!(
            (w.added, w.removed.clone()),
            (1, vec!["Prefers mornings".to_string()])
        );
        let texts: Vec<String> = live_texts(&db).into_iter().map(|t| t.0).collect();
        assert!(texts.contains(&"Prefers evenings".to_string()));
        assert!(!texts.contains(&"Prefers mornings".to_string()));
        // Removing a note from its topic file removes it everywhere.
        let people = read(&paths, "people.md").replace("- Sam: Plays chess\n", "");
        let w = write(&db, &paths, "people.md", &people, &src, i64::MAX, "t").unwrap();
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
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Something else\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!(w.kept_pinned, ["Prefers evenings"]);
        assert!(read(&paths, CORE_FILE).contains("## Always in mind\n\n- Prefers evenings\n"));
        // Old-style paths need at least one note; an export file can be emptied.
        assert!(write(&db, &paths, "notes.md", "# Notes\n", &src, i64::MAX, "t").is_err());
        let w = write(&db, &paths, "people.md", "# People\n", &src, i64::MAX, "t").unwrap();
        assert_eq!(w.removed, ["Sam: Likes jazz"]);
        assert!(!paths.memory_dir().join("people.md").exists());
    }

    #[test]
    fn notes_another_chat_saved_during_the_turn_are_kept() {
        let (_t, paths, db) = setup();
        let mine = json!({"type": "conversation", "conversation_id": "c1", "message_ids": ["m1"]});
        let other = json!({"type": "conversation", "conversation_id": "c2", "message_ids": ["m2"]});
        write(
            &db,
            &paths,
            "people.md",
            "- Old friend\n",
            &other,
            i64::MAX,
            "other1",
        )
        .unwrap();
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
            i64::MAX,
            "other2",
        )
        .unwrap();
        let w = write(
            &db,
            &paths,
            "people.md",
            &format!("{seen}- Just added here\n"),
            &mine,
            started,
            "mine",
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
            "mine",
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
        write(
            &db,
            &paths,
            CORE_FILE,
            "- Prefers mornings\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        let before = read(&paths, CORE_FILE);
        // people.md cannot be written; MEMORY.md is written first.
        std::fs::create_dir(paths.memory_dir().join("people.md")).unwrap();
        let edited = format!("{before}\n## People\n\n- Has a brother\n");
        assert!(write(&db, &paths, CORE_FILE, &edited, &src, i64::MAX, "t").is_err());
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
    fn an_interrupted_first_import_is_finished_on_the_next_start() {
        let (_t, paths, db) = setup();
        let mem = paths.memory_dir();
        let original = "# Memory\n\n## Routines\n- Run at 7\n";
        std::fs::write(mem.join(CORE_FILE), original).unwrap();
        // routines.md cannot be written, so the export fails after the
        // notes are stored.
        std::fs::create_dir(mem.join("routines.md")).unwrap();
        assert!(prepare(&db, &paths).is_err());
        let stored = live_texts(&db);
        assert_eq!(stored, [("Run at 7".to_string(), Topic::Routines, 4)]);
        std::fs::remove_dir(mem.join("routines.md")).unwrap();
        prepare(&db, &paths).unwrap();
        assert_eq!(live_texts(&db), stored);
        assert!(read(&paths, "routines.md").contains("- Run at 7"));
        // Stopped before the notes were stored: the original is untouched
        // and imported next time.
        let (_t2, paths2, db2) = setup();
        std::fs::write(paths2.memory_dir().join(CORE_FILE), original).unwrap();
        db2.with(|c| c.execute_batch("CREATE TRIGGER fail BEFORE INSERT ON memory_items BEGIN SELECT RAISE(ABORT, 'full'); END;"))
            .unwrap();
        assert!(prepare(&db2, &paths2).is_err());
        assert_eq!(read(&paths2, CORE_FILE), original);
        db2.with(|c| c.execute_batch("DROP TRIGGER fail")).unwrap();
        prepare(&db2, &paths2).unwrap();
        assert_eq!(live_texts(&db2), stored);
    }

    #[test]
    fn an_oversized_legacy_core_is_backed_up_before_it_is_replaced() {
        let (_t, paths, db) = setup();
        let big = format!("- {}\n", "x".repeat(MAX_IMPORT_BYTES as usize + 1));
        std::fs::write(paths.memory_dir().join(CORE_FILE), &big).unwrap();
        std::fs::write(paths.memory_dir().join("huge-notes.md"), &big).unwrap();
        let report = prepare(&db, &paths).unwrap();
        assert_eq!((report.files, report.unreadable), (0, 1));
        let backup = report.backup.unwrap();
        assert_eq!(
            std::fs::read_to_string(backup.join(CORE_FILE)).unwrap(),
            big
        );
        assert!(is_generated(&read(&paths, CORE_FILE)));
        // A large note file that is not an export name is left alone.
        assert!(paths.memory_dir().join("huge-notes.md").exists());
        // A generated export larger than the import limit is not imported
        // or backed up again.
        let huge = format!("{HEADER}\n# People\n\n{big}");
        std::fs::write(paths.memory_dir().join("people.md"), &huge).unwrap();
        let again = prepare(&db, &paths).unwrap();
        assert!(again.backup.is_none() && again.unreadable == 0);
    }

    #[test]
    fn a_turn_can_correct_what_it_changed_but_not_what_others_changed() {
        let (_t, paths, db) = setup();
        let import = json!({"type": "import", "file": "people.md"});
        import_notes(
            &db,
            &paths,
            &[("people.md".into(), "- Neighbour Kim\n".into())],
        )
        .unwrap();
        let started = crate::util::now_ms() + 1;
        while crate::util::now_ms() < started {}
        let me = json!({"type": "conversation", "conversation_id": "c", "message_ids": ["m"]});
        // This turn promotes an imported note into MEMORY.md, then corrects it.
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Neighbour Kim\n",
            &me,
            started,
            "turn-m",
        )
        .unwrap();
        assert_eq!((w.added, w.unchanged), (0, 1));
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Neighbour Kim has a cat\n",
            &me,
            started,
            "turn-m",
        )
        .unwrap();
        assert_eq!(w.removed, ["Neighbour Kim"]);
        assert!(w.kept_recent.is_empty());
        // Another turn promotes a note this turn has not seen in MEMORY.md.
        write(
            &db,
            &paths,
            "people.md",
            "- Neighbour Kim has a cat\n- Sam\n",
            &me,
            started,
            "turn-m",
        )
        .unwrap();
        write(
            &db,
            &paths,
            CORE_FILE,
            &format!("{}- Sam\n", read(&paths, CORE_FILE)),
            &import,
            started,
            "turn-x",
        )
        .unwrap();
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Neighbour Kim has a cat\n",
            &me,
            started,
            "turn-m",
        )
        .unwrap();
        assert_eq!(w.kept_recent, ["Sam"]);
    }

    #[test]
    fn a_stale_write_does_not_bring_back_what_another_chat_removed() {
        let (_t, paths, db) = setup();
        let a = json!({"type": "conversation", "conversation_id": "a", "message_ids": ["ma"]});
        let b = json!({"type": "conversation", "conversation_id": "b", "message_ids": ["mb"]});
        write(
            &db,
            &paths,
            "people.md",
            "- Alex lives in Boston\n",
            &b,
            i64::MAX,
            "b1",
        )
        .unwrap();
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
            "b2",
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
            "a",
        )
        .unwrap();
        assert_eq!(w.added, 1);
        assert_eq!(w.not_restored, ["Alex lives in Boston"]);
        assert_eq!(w.kept_recent, ["Alex lives in Seattle"]);
        let people = read(&paths, "people.md");
        assert!(people.contains("Seattle") && !people.contains("Boston"));
    }

    fn core_ids(db: &Db) -> Vec<i64> {
        db.with(super::super::live)
            .unwrap()
            .into_iter()
            .filter(|i| i.core_since.is_some())
            .map(|i| i.id)
            .collect()
    }

    fn wait_past(ms: i64) {
        while crate::util::now_ms() <= ms {}
    }

    #[test]
    fn the_core_is_packed_and_its_notes_recorded() {
        let (_t, paths, db) = setup();
        let src = json!({"type": "owner"});
        write(
            &db,
            &paths,
            "people.md",
            "- Sam bakes bread\n- Alex teaches chemistry\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        // Ordinary notes stay in their topic files.
        assert!(core_ids(&db).is_empty());
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            "- Prefers mornings\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert!(w.outside_core.is_empty());
        let core = read(&paths, CORE_FILE);
        assert!(core.contains("- Prefers mornings\n"));
        assert!(
            !core.contains("Sam")
                && core.contains("<!-- 2 more notes are only in the topic files. -->")
        );
        let prompt = core_for_prompt(&paths).unwrap();
        assert!(prompt.contains("Prefers mornings") && prompt.contains("2 more notes"));
        let live = db.with(super::super::live).unwrap();
        let mornings = live.iter().find(|i| i.text == "Prefers mornings").unwrap();
        assert_eq!(core_ids(&db), [mornings.id]);
        let since = mornings.core_since.unwrap();
        // Membership keeps its date while the note stays in the core, and
        // is cleared when it leaves.
        write(
            &db,
            &paths,
            "you.md",
            "- Prefers mornings\n- Has a bike\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        let again = db
            .with(|c| super::super::get(c, mornings.id))
            .unwrap()
            .unwrap();
        assert_eq!(again.core_since, Some(since));
        write(&db, &paths, CORE_FILE, "", &src, i64::MAX, "t").unwrap();
        let gone = db
            .with(|c| super::super::get(c, mornings.id))
            .unwrap()
            .unwrap();
        assert!(gone.archived && gone.core_since.is_none());

        // Pinned notes that do not fit are counted in the prompt.
        let big = |c: char| format!("- {}\n", c.to_string().repeat(4_000));
        write(
            &db,
            &paths,
            CORE_FILE,
            &format!("{}{}{}", big('a'), big('b'), big('c')),
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        db.with(|c| {
            c.execute(
                "UPDATE memory_items SET pinned = 1 WHERE length(text) = 4000",
                [],
            )
        })
        .unwrap();
        let w = write(
            &db,
            &paths,
            "routines.md",
            "- Runs at 7\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!(w.added, 1);
        let prompt = core_for_prompt(&paths).unwrap();
        assert!(prompt.contains("## Always in mind"));
        assert!(
            prompt
                .contains("<!-- 1 pinned note did not fit here; read them in the topic files. -->")
        );
        assert_eq!(core_ids(&db).len(), 2);
        assert!(prompt.chars().count() <= packing::BUDGET_CHARS + 200);
    }

    #[test]
    fn notes_too_big_for_the_core_are_reported() {
        let (_t, paths, db) = setup();
        let src = json!({"type": "owner"});
        let huge = "z".repeat(packing::BUDGET_CHARS);
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            &format!("- Likes tea\n- {huge}\n"),
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!(w.added, 2);
        assert_eq!(w.outside_core, std::slice::from_ref(&huge));
        assert!(read(&paths, "you.md").contains(&huge));
        assert!(!read(&paths, CORE_FILE).contains(&huge));
        // Only MEMORY.md writes report it.
        let w = write(
            &db,
            &paths,
            "you.md",
            &read(&paths, "you.md"),
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert!(w.outside_core.is_empty());
    }

    #[test]
    fn a_stale_core_write_keeps_notes_that_joined_after_the_turn_began() {
        let (_t, paths, db) = setup();
        let other = json!({"type": "conversation", "conversation_id": "b", "message_ids": ["mb"]});
        let mine = json!({"type": "conversation", "conversation_id": "a", "message_ids": ["ma"]});
        // Twelve notes; eleven fit in the core.
        let notes: String = (0..12)
            .map(|i| format!("- {i:02} {}\n", "n".repeat(947)))
            .collect();
        write(&db, &paths, CORE_FILE, &notes, &other, i64::MAX, "b1").unwrap();
        let live = db.with(super::super::live).unwrap();
        let outside: Vec<&Item> = live.iter().filter(|i| i.core_since.is_none()).collect();
        assert_eq!((live.len(), outside.len()), (12, 1));
        let unseen = outside[0].text.clone();
        let removed = live
            .iter()
            .find(|i| i.core_since.is_some())
            .unwrap()
            .text
            .clone();

        let started = crate::util::now_ms() + 1;
        wait_past(started);
        // This turn reads MEMORY.md and saves a topic note; then another
        // chat removes a core note, which makes room for the unseen one.
        let seen = read(&paths, CORE_FILE);
        assert!(!seen.contains(&unseen));
        write(
            &db,
            &paths,
            "people.md",
            "- Sam bakes bread\n",
            &mine,
            started,
            "a",
        )
        .unwrap();
        let without = seen.replace(&format!("- {removed}\n"), "");
        write(&db, &paths, CORE_FILE, &without, &other, started, "b2").unwrap();
        assert!(read(&paths, CORE_FILE).contains(&unseen));

        // The turn writes back what it read, plus a new note.
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            &format!("{seen}- Likes tea\n"),
            &mine,
            started,
            "a",
        )
        .unwrap();
        assert_eq!(w.added, 1);
        assert_eq!(w.not_restored, std::slice::from_ref(&removed));
        assert_eq!(w.kept_recent, std::slice::from_ref(&unseen));
        assert!(w.removed.is_empty());
        let texts: Vec<String> = live_texts(&db).into_iter().map(|t| t.0).collect();
        assert!(texts.contains(&unseen) && texts.contains(&"Sam bakes bread".to_string()));
        assert!(!texts.contains(&removed));
        // What the turn saw and wrote itself it can still remove.
        let w = write(
            &db,
            &paths,
            CORE_FILE,
            &read(&paths, CORE_FILE).replace("- Likes tea\n", ""),
            &mine,
            started,
            "a",
        )
        .unwrap();
        assert_eq!(w.removed, ["Likes tea"]);
    }

    #[test]
    fn routine_writes_keep_the_persona_hash() {
        let (_t, paths, db) = setup();
        let src = json!({"type": "owner"});
        let hash = |db: &Db| {
            let identity = db.with(packing::identity).unwrap();
            crate::persona::persona_hash(
                &paths,
                "Iris",
                "main",
                crate::persona::Scope::Private,
                &identity,
            )
        };
        let base = hash(&db);
        write(
            &db,
            &paths,
            CORE_FILE,
            "- Prefers mornings\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        write(
            &db,
            &paths,
            "people.md",
            "- Sam bakes bread\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert!(
            core_for_prompt(&paths)
                .unwrap()
                .contains("Prefers mornings")
        );
        assert_eq!(hash(&db), base, "ordinary notes do not restart sessions");
        // Pinning a note, or a note saying who the owner is, does.
        db.with(|c| {
            c.execute(
                "UPDATE memory_items SET pinned = 1 WHERE text = 'Prefers mornings'",
                [],
            )
        })
        .unwrap();
        write(
            &db,
            &paths,
            "people.md",
            "- Sam bakes bread\n- Kim\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        let pinned = hash(&db);
        assert_ne!(pinned, base);
        db.with(|c| {
            c.execute(
                "UPDATE memory_items SET kind = 'profile' WHERE text = 'Kim'",
                [],
            )
        })
        .unwrap();
        write(
            &db,
            &paths,
            "routines.md",
            "- Runs at 7\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        let profile = hash(&db);
        assert_ne!(profile, pinned);
        write(
            &db,
            &paths,
            "routines.md",
            "- Runs at 8\n",
            &src,
            i64::MAX,
            "t",
        )
        .unwrap();
        assert_eq!(hash(&db), profile);
    }
}
