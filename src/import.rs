//! `iris import openclaw <workspace>`: copy persona and memory Markdown from
//! an OpenClaw workspace into the Iris data directory.
//!
//! The source is only ever read. Existing destination files are never
//! overwritten unless `--force` is given; identical files are left alone.
//! Symlinks, hidden files, non-text files, and very large files are skipped.

use crate::config::{Paths, ensure_private_dir, write_private_file};
use crate::persona::is_text_note;
use crate::util::Result;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 6;

/// Top-level persona files and where they go.
const PERSONA_FILES: &[&str] = &[
    "IDENTITY.md",
    "SOUL.md",
    "USER.md",
    "TOOLS.md",
    "HEARTBEAT.md",
    "PROACTIVE_PREFERENCES.md",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Create,
    Overwrite,
    Unchanged,
    /// Destination exists with different content and `--force` was not set.
    SkipExists,
    Skip(&'static str),
}

#[derive(Debug, Clone)]
pub struct Item {
    pub src: PathBuf,
    pub dest: Option<PathBuf>,
    pub action: Action,
}

pub struct Options {
    pub dry_run: bool,
    pub force: bool,
}

/// Map a path relative to the OpenClaw workspace to its Iris destination.
fn destination(paths: &Paths, rel: &Path) -> Option<PathBuf> {
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match comps.as_slice() {
        [name] if PERSONA_FILES.contains(&name.as_str()) => Some(paths.persona_dir().join(name)),
        [name] if name == "MEMORY.md" => Some(paths.memory_dir().join("MEMORY.md")),
        // OpenClaw's operating manual is kept as a reference note.
        [name] if name == "AGENTS.md" => Some(paths.memory_dir().join("imported/AGENTS.md")),
        [first, rest @ ..] if first == "memory" && !rest.is_empty() => {
            let mut p = paths.memory_dir();
            for r in rest {
                p.push(r);
            }
            Some(p)
        }
        _ => None,
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(PathBuf, Option<&'static str>)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            out.push((rel, Some("symlink")));
        } else if ft.is_dir() {
            // Only the memory tree is imported; don't wander elsewhere.
            if depth > 0 || name == "memory" {
                walk(root, &path, depth + 1, out);
            }
        } else if ft.is_file() {
            out.push((rel, None));
        }
    }
}

/// Plan (and unless `dry_run`, perform) the import.
pub fn import_openclaw(source: &Path, paths: &Paths, opts: &Options) -> Result<Vec<Item>> {
    let meta =
        std::fs::metadata(source).map_err(|e| format!("cannot read {}: {e}", source.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", source.display()).into());
    }
    let source = source.canonicalize()?;
    if paths.data_dir.exists()
        && let Ok(data) = paths.data_dir.canonicalize()
        && (source.starts_with(&data) || data.starts_with(&source))
    {
        return Err("source and Iris data directory must not overlap".into());
    }
    let mut found = Vec::new();
    walk(&source, &source, 0, &mut found);
    let mut items = Vec::new();
    for (rel, skip) in found {
        let src = source.join(&rel);
        if let Some(reason) = skip {
            items.push(Item {
                src,
                dest: None,
                action: Action::Skip(reason),
            });
            continue;
        }
        let Some(dest) = destination(paths, &rel) else {
            items.push(Item {
                src,
                dest: None,
                action: Action::Skip("not a persona or memory file"),
            });
            continue;
        };
        if !is_text_note(&src) {
            items.push(Item {
                src,
                dest: None,
                action: Action::Skip("not a text note"),
            });
            continue;
        }
        if std::fs::metadata(&src).map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            items.push(Item {
                src,
                dest: None,
                action: Action::Skip("larger than 2 MB"),
            });
            continue;
        }
        let action = match std::fs::symlink_metadata(&dest) {
            Err(_) => Action::Create,
            Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
                Action::Skip("destination is not a regular file")
            }
            Ok(_) => {
                let same = std::fs::read(&dest).ok() == std::fs::read(&src).ok();
                if same {
                    Action::Unchanged
                } else if opts.force {
                    Action::Overwrite
                } else {
                    Action::SkipExists
                }
            }
        };
        items.push(Item {
            src,
            dest: Some(dest),
            action,
        });
    }
    if !opts.dry_run {
        crate::persona::seed(paths)?;
        for it in &items {
            if let (Action::Create | Action::Overwrite, Some(dest)) = (&it.action, &it.dest) {
                if let Some(parent) = dest.parent() {
                    ensure_private_dir(parent)?;
                }
                let bytes = std::fs::read(&it.src)?;
                write_private_file(dest, &bytes)?;
            }
        }
    }
    Ok(items)
}

/// Human-readable report for the CLI.
pub fn report(items: &[Item], dry_run: bool) -> String {
    let mut out = String::new();
    let verb = |a: &Action| match a {
        Action::Create => {
            if dry_run {
                "would copy"
            } else {
                "copied"
            }
        }
        Action::Overwrite => {
            if dry_run {
                "would overwrite"
            } else {
                "overwrote"
            }
        }
        Action::Unchanged => "unchanged",
        Action::SkipExists => "exists (use --force)",
        Action::Skip(_) => "skipped",
    };
    for it in items {
        let reason = match &it.action {
            Action::Skip(r) => format!(" ({r})"),
            _ => String::new(),
        };
        let dest = it
            .dest
            .as_ref()
            .map(|d| format!(" -> {}", d.display()))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:<22} {}{dest}{reason}\n",
            verb(&it.action),
            it.src.display()
        ));
    }
    let count = |f: fn(&Action) -> bool| items.iter().filter(|i| f(&i.action)).count();
    let (copy, overwrite) = if dry_run {
        ("to copy", "to overwrite")
    } else {
        ("copied", "overwritten")
    };
    out.push_str(&format!(
        "\n{} {copy}, {} {overwrite}, {} unchanged, {} existing kept, {} skipped{}\n",
        count(|a| *a == Action::Create),
        count(|a| *a == Action::Overwrite),
        count(|a| *a == Action::Unchanged),
        count(|a| *a == Action::SkipExists),
        count(|a| matches!(a, Action::Skip(_))),
        if dry_run {
            " (dry run; nothing written)"
        } else {
            ""
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("openclaw-workspace");
        std::fs::create_dir_all(src.join("memory/people")).unwrap();
        std::fs::create_dir_all(src.join("skills/x")).unwrap();
        std::fs::write(src.join("SOUL.md"), "# Soul\nCalm.").unwrap();
        std::fs::write(src.join("USER.md"), "# User\nLikes tea.").unwrap();
        std::fs::write(src.join("MEMORY.md"), "Remember the garden.").unwrap();
        std::fs::write(src.join("AGENTS.md"), "# Agents").unwrap();
        std::fs::write(src.join("BOOTSTRAP.md"), "bootstrap").unwrap();
        std::fs::write(src.join("memory/2026-01-01.md"), "Daily note").unwrap();
        std::fs::write(src.join("memory/people/alex.md"), "# Alex").unwrap();
        std::fs::write(src.join("memory/photo.jpg"), [0u8; 3]).unwrap();
        std::fs::write(src.join("skills/x/SKILL.md"), "skill").unwrap();
        std::fs::write(src.join(".env"), "SECRET=1").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("memory/link.md")).unwrap();
        let paths = Paths {
            config_file: tmp.path().join("cfg/config.toml"),
            data_dir: tmp.path().join("iris-data"),
        };
        (tmp, src, paths)
    }

    fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(e.path());
                } else if ft.is_file() {
                    out.push((e.path(), std::fs::read(e.path()).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (_t, src, paths) = setup();
        let items = import_openclaw(
            &src,
            &paths,
            &Options {
                dry_run: true,
                force: false,
            },
        )
        .unwrap();
        assert!(!paths.data_dir.exists());
        let creates = items.iter().filter(|i| i.action == Action::Create).count();
        assert_eq!(creates, 6, "{items:#?}");
        let text = report(&items, true);
        assert!(text.contains("would copy") && text.contains("dry run"));
    }

    #[test]
    fn imports_and_respects_existing_files() {
        let (_t, src, paths) = setup();
        let before = snapshot(&src);
        let items = import_openclaw(
            &src,
            &paths,
            &Options {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        // Source untouched.
        assert_eq!(snapshot(&src), before);
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(
            read(paths.persona_dir().join("USER.md")),
            "# User\nLikes tea."
        );
        assert_eq!(
            read(paths.memory_dir().join("MEMORY.md")),
            "Remember the garden."
        );
        assert_eq!(read(paths.memory_dir().join("people/alex.md")), "# Alex");
        assert_eq!(
            read(paths.memory_dir().join("imported/AGENTS.md")),
            "# Agents"
        );
        assert!(!paths.memory_dir().join("link.md").exists());
        assert!(!paths.memory_dir().join("photo.jpg").exists());
        assert!(!paths.data_dir.join("skills").exists());
        assert!(items.iter().any(|i| i.action == Action::Skip("symlink")));
        // IDENTITY.md was not in the source, so it comes from the template.
        assert!(read(paths.persona_dir().join("IDENTITY.md")).contains("{{agent_name}}"));

        // Second run: everything is unchanged.
        let again = import_openclaw(
            &src,
            &paths,
            &Options {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        assert!(
            again
                .iter()
                .all(|i| matches!(i.action, Action::Unchanged | Action::Skip(_)))
        );

        // Local edits are kept without --force, replaced with it.
        std::fs::write(paths.persona_dir().join("USER.md"), "edited").unwrap();
        let kept = import_openclaw(
            &src,
            &paths,
            &Options {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        assert!(kept.iter().any(|i| i.action == Action::SkipExists));
        assert_eq!(read(paths.persona_dir().join("USER.md")), "edited");
        import_openclaw(
            &src,
            &paths,
            &Options {
                dry_run: false,
                force: true,
            },
        )
        .unwrap();
        assert_eq!(
            read(paths.persona_dir().join("USER.md")),
            "# User\nLikes tea."
        );
    }

    #[test]
    fn rejects_bad_sources() {
        let (_t, src, paths) = setup();
        assert!(
            import_openclaw(
                &src.join("SOUL.md"),
                &paths,
                &Options {
                    dry_run: true,
                    force: false
                }
            )
            .is_err()
        );
        assert!(
            import_openclaw(
                &src.join("missing"),
                &paths,
                &Options {
                    dry_run: true,
                    force: false
                }
            )
            .is_err()
        );
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let inside = Paths {
            config_file: paths.config_file.clone(),
            data_dir: src.join("memory"),
        };
        assert!(
            import_openclaw(
                &src,
                &inside,
                &Options {
                    dry_run: true,
                    force: false
                }
            )
            .is_err()
        );
    }
}
