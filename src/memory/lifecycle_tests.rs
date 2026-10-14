use super::*;
use crate::db::Db;
use crate::memory::{Topic, get};

fn setup() -> (Db, tempfile::TempDir, Paths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_file: tmp.path().join("config.toml"),
        data_dir: tmp.path().join("data"),
    };
    crate::persona::seed(&paths).unwrap();
    (Db::open(&paths.db_file()).unwrap(), tmp, paths)
}
fn note(c: &Connection, text: &str) -> rusqlite::Result<i64> {
    Ok(recall::save(
        c,
        &recall::Edit {
            text: Some(text.into()),
            ..Default::default()
        },
        None,
        Actor::Owner,
        &json!({"type":"owner"}),
        "setup",
    )?["item"]["id"]
        .as_i64()
        .unwrap())
}

#[test]
fn expiry_and_decay_are_bounded_protected_recall_hidden_and_undoable() {
    let (db, _tmp, paths) = setup();
    db.with(|c| {
        let now = now_ms();
        let mut ids = Vec::new();
        for text in [
            "expired pinned",
            "stale fact",
            "pinned",
            "profile",
            "preference",
            "important",
            "used",
            "recent edit",
        ] {
            ids.push(note(c, text)?);
        }
        c.execute(
            "UPDATE memory_items SET created_at=?1,updated_at=?1",
            [now - 200 * DAY],
        )?;
        c.execute(
            "UPDATE memory_items SET pinned=1,valid_to=?2 WHERE id=?1",
            params![ids[0], now],
        )?;
        c.execute("UPDATE memory_items SET pinned=1 WHERE id=?1", [ids[2]])?;
        c.execute(
            "UPDATE memory_items SET kind='profile' WHERE id=?1",
            [ids[3]],
        )?;
        c.execute(
            "UPDATE memory_items SET kind='preference' WHERE id=?1",
            [ids[4]],
        )?;
        c.execute("UPDATE memory_items SET importance=4 WHERE id=?1", [ids[5]])?;
        c.execute(
            "UPDATE memory_items SET last_used_at=?2 WHERE id=?1",
            params![ids[6], now],
        )?;
        c.execute(
            "UPDATE memory_items SET updated_at=?2 WHERE id=?1",
            params![ids[7], now],
        )?;
        db::set_setting(c, super::super::dream::PAUSED, "true")?;
        assert_eq!(maintain(c, &paths, now)?, 2);
        assert!(get(c, ids[0])?.unwrap().archived);
        assert!(get(c, ids[1])?.unwrap().archived);
        for id in &ids[2..] {
            assert!(!get(c, *id)?.unwrap().archived);
        }
        let hits = recall::list(
            c,
            &recall::Filter {
                query: Some("stale".into()),
                ..Default::default()
            },
            true,
        )?;
        assert_eq!(hits["hits"], json!([]));
        assert!(
            !std::fs::read_to_string(paths.memory_dir().join("MEMORY.md"))
                .unwrap()
                .contains("stale fact")
        );
        let history = recall::history(c, ids[1])?;
        assert_eq!(history[0]["op"], "archive");
        recall::mutate(c, &paths, |c| {
            recall::undo(c, history[0]["id"].as_i64().unwrap(), "undo")
        })?;
        assert!(!get(c, ids[1])?.unwrap().archived);
        assert_eq!(
            maintain(c, &paths, now)?,
            0,
            "undo refreshes inactivity grace"
        );
        for n in 0..160 {
            let id = note(c, &format!("temporary fact {n}"))?;
            c.execute(
                "UPDATE memory_items SET valid_to=0,importance=5 WHERE id=?1",
                [id],
            )?;
        }
        assert_eq!(maintain(c, &paths, now)?, 50);
        assert_eq!(maintain(c, &paths, now)?, 50);
        assert_eq!(maintain(c, &paths, now)?, 50);
        assert_eq!(maintain(c, &paths, now)?, 10);
        Ok(())
    })
    .unwrap();
}

#[test]
fn scan_wraps_without_starving_later_low_score_items() {
    let (db, _tmp, paths) = setup();
    db.with(|c| {
        for n in 0..230 {
            note(c, &format!("stale {n}"))?;
        }
        c.execute("UPDATE memory_items SET created_at=0,updated_at=0", [])?;
        assert_eq!(maintain(c, &paths, now_ms())?, 100);
        assert_eq!(maintain(c, &paths, now_ms())?, 100);
        assert_eq!(maintain(c, &paths, now_ms())?, 30);
        assert_eq!(db::get_setting(c, SCAN_CURSOR)?.as_deref(), Some("0"));
        Ok(())
    })
    .unwrap();
}

#[test]
fn reset_is_atomic_preserves_boundaries_and_ids_and_clears_derived_payloads() {
    let (db, _tmp, paths) = setup();
    db.with(|c| {
        let id = note(c, "Original garden note")?;
        let owner = db::owner_user_id(c)?.unwrap();
        let main = db::ensure_main_conversation(c, &owner)?;
        let msg = db::insert_message(
            c,
            &db::NewMessage {
                conversation_id: &main,
                role: "user",
                author_user_id: Some(&owner),
                content: "Original garden assertion",
                ..Default::default()
            },
        )?;
        let card = crate::entities::insert_card(
            c,
            &main,
            &json!({"type":"memory_morning","all_changes":[{"text":"Original garden note"}]}),
            "assistant",
        )?;
        c.execute(
            "UPDATE messages SET meta=?2 WHERE id=?1",
            params![
                msg.id,
                json!({"noted":[{"item_id":id,"text":"Original garden note"}],"retained":true})
                    .to_string()
            ],
        )?;
        let old_history = recall::history(c, id)?[0]["id"].as_i64().unwrap();
        files::render_export(c, &paths)?;
        // Fail after resetting the rows: rollback must restore history, the
        // boundary and the export, not leave a half-reset database.
        std::fs::create_dir(paths.memory_dir().join("MEMORY.md.blocked")).unwrap();
        std::fs::rename(
            paths.memory_dir().join("MEMORY.md"),
            paths.memory_dir().join("saved.md"),
        )
        .unwrap();
        std::fs::rename(
            paths.memory_dir().join("MEMORY.md.blocked"),
            paths.memory_dir().join("MEMORY.md"),
        )
        .unwrap();
        assert!(forget_everything(c, &paths).is_err());
        assert!(get(c, id)?.is_some());
        assert!(db::get_setting(c, RESET_AFTER)?.is_none());
        std::fs::remove_dir(paths.memory_dir().join("MEMORY.md")).unwrap();
        std::fs::rename(
            paths.memory_dir().join("saved.md"),
            paths.memory_dir().join("MEMORY.md"),
        )
        .unwrap();
        assert_eq!(forget_everything(c, &paths)?["forgotten"], 1);
        let after = db::get_setting(c, RESET_AFTER)?
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(after >= msg.created_at);
        assert!(write_allowed(c, after).is_err());
        assert!(write_allowed(c, after + 1).is_ok());
        assert!(get(c, id)?.is_none());
        assert!(recall::history(c, id)?.is_empty());
        assert!(recall::undo(c, old_history, "old").is_err());
        assert!(!export(c)?.contains("Original"));
        assert!(files::core_for_prompt(&paths).is_none());
        for topic in Topic::ALL {
            assert!(!paths.memory_dir().join(topic.file()).exists());
        }
        assert_eq!(
            db::get_message(c, &msg.id)?.unwrap().meta,
            json!({"retained":true})
        );
        assert!(db::get_message(c, &card.id)?.unwrap().card.is_none());
        let new = note(c, "New garden note")?;
        assert!(new > id);
        assert!(recall::history(c, new)?[0]["id"].as_i64().unwrap() > old_history);
        assert!(recall::undo(c, old_history, "stale").is_err());
        assert!(
            recall::list(
                c,
                &recall::Filter {
                    query: Some("Original".into()),
                    ..Default::default()
                },
                false
            )?["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn download_includes_full_current_archived_and_replaced_notes() {
    let (db, _tmp, _paths) = setup();
    db.with(|c| {
        let id = note(c, "Old garden fact")?;
        recall::save(
            c,
            &recall::Edit {
                text: Some("New garden fact".into()),
                ..Default::default()
            },
            Some(id),
            Actor::Agent,
            &json!({}),
            "update",
        )?;
        let text = export(c)?;
        assert!(text.contains("Old garden fact"));
        assert!(text.contains("New garden fact"));
        assert!(text.contains("Archived, expired and replaced"));
        Ok(())
    })
    .unwrap();
}
