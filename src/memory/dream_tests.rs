use super::*;
use crate::config::{Config, Paths};
use crate::db::{Db, NewMessage};
use std::os::unix::fs::PermissionsExt;

async fn setup() -> (AppState, tempfile::TempDir, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("claude");
    std::fs::write(
        &script,
        r#"#!/bin/sh
D="$(dirname "$0")"
case "$1" in
--version) echo '9.9.9 (fake)'; exit 0;;
auth) echo '{"loggedIn":true}'; exit 0;;
esac
cat > "$D/input"
printf '%s\n' "$@" > "$D/args"
pwd > "$D/cwd"
echo run >> "$D/calls"
if [ -f "$D/gated" ]; then
    while [ ! -f "$D/release" ]; do sleep 0.01; done
fi
if [ -f "$D/fail" ]; then exit 1; fi
printf '{"type":"result","subtype":"success","is_error":false,"result":%s}\n' "$(cat "$D/reply")"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let paths = Paths {
        config_file: tmp.path().join("config.toml"),
        data_dir: tmp.path().join("data"),
    };
    crate::persona::seed(&paths).unwrap();
    let db = Db::open(&paths.db_file()).unwrap();
    let mut cfg = Config::default();
    for (name, h) in &mut cfg.harness {
        h.enabled = name == "claude";
    }
    cfg.harness.get_mut("claude").unwrap().command = script.to_string_lossy().into_owned();
    cfg.feed.enabled = false;
    cfg.push.enabled = false;
    let state = AppState::new(cfg, paths, db).unwrap();
    state.refresh_harnesses().await;
    let (owner, main) = state
        .db
        .with(|c| {
            let owner = db::owner_user_id(c)?.unwrap();
            let main = db::ensure_main_conversation(c, &owner)?;
            Ok((owner, main))
        })
        .unwrap();
    reply(&tmp, json!({"ops":[{"op":"NOOP"}]}));
    (state, tmp, owner, main)
}

fn reply(tmp: &tempfile::TempDir, value: Value) {
    raw_reply(tmp, &value.to_string());
}
fn raw_reply(tmp: &tempfile::TempDir, text: &str) {
    std::fs::write(
        tmp.path().join("reply"),
        serde_json::to_string(text).unwrap(),
    )
    .unwrap();
}
fn add_messages(state: &AppState, owner: &str, conv: &str, count: usize) {
    state
        .db
        .with(|c| {
            for n in 0..count {
                db::insert_message(
                    c,
                    &NewMessage {
                        conversation_id: conv,
                        role: "user",
                        author_user_id: Some(owner),
                        content: &format!("I grow orchard apples in garden number {n}"),
                        ..Default::default()
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
}
fn add_op(text: &str) -> Value {
    json!({"op":"ADD","text":text,"kind":"fact","importance":3,"messages":[1]})
}
fn cards(state: &AppState) -> Vec<Message> {
    state.db.with(|c| {
        let mut s = c.prepare("SELECT id FROM messages WHERE json_extract(card,'$.type')='memory_morning' ORDER BY created_at")?;
        let ids: Vec<String> = s.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids.iter().map(|id| Ok(db::get_message(c,id)?.unwrap())).collect()
    }).unwrap()
}
fn progress(state: &AppState) -> i64 {
    state.db.with(watermark).unwrap()
}
fn note(c: &Connection, text: &str) -> rusqlite::Result<i64> {
    Ok(recall::save(
        c,
        &recall::Edit {
            text: Some(text.into()),
            kind: Some("fact".into()),
            ..Default::default()
        },
        None,
        Actor::Owner,
        &json!({"type":"owner"}),
        &crate::util::new_id(),
    )?["item"]["id"]
        .as_i64()
        .unwrap())
}

#[test]
fn strict_parser_and_local_schedule() {
    for invalid in [
        "no JSON",
        "```json\n{\"ops\":[{\"op\":\"NOOP\"}]}\n```",
        "{\"ops\":[]}",
        "{\"ops\":[{\"op\":\"DELETE\",\"alias\":1}]}",
        "{\"ops\":[{\"op\":\"NOOP\",\"alias\":1}]}",
        "{\"ops\":[{\"op\":\"NOOP\"}],\"extra\":1}",
        "{\"ops\":[{\"op\":\"NOOP\"}]} trailing",
    ] {
        assert!(parse(invalid).is_err(), "{invalid}");
    }
    assert!(parse("{\"ops\":[{\"op\":\"REJECT\",\"reason\":\"unsafe\"}]}").is_ok());
    let cfg = DreamConfig::default();
    let now = now_ms();
    assert!(!due(&cfg, None, now, 179));
    assert!(due(&cfg, None, now, 180));
    assert!(!due(&cfg, None, now, 240));
    assert!(!due(&cfg, None, now, 1439));
    assert!(!due(&cfg, Some(now), now, 180));
    assert!(due(&cfg, Some(now - 2 * 86_400_000), now, 180));
    assert!(Config::parse("[dream]\nhour=24").is_err());
    assert!(Config::parse("[dream]\nmin_messages=0").is_err());
    assert!(Config::parse("[dream]\nmin_messages=81").is_err());
}

#[tokio::test]
async fn fake_cli_applies_history_and_card_undo_and_only_one_card_a_day() {
    let (state, tmp, owner, main) = setup().await;
    // No CLI calls below the activity threshold, including manual runs.
    add_messages(&state, &owner, &main, 3);
    run(&state, &owner, true).await.unwrap();
    assert!(!tmp.path().join("calls").exists());
    add_messages(&state, &owner, &main, 1);
    reply(
        &tmp,
        json!({"ops":[add_op("Grows apples"),add_op("Likes pears"),add_op("Keeps a garden"),add_op("Likes plums")]}),
    );
    run(&state, &owner, true).await.unwrap();
    assert!(progress(&state) > 0);
    let all = cards(&state);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].conversation_id, main);
    let card = all[0].card.as_ref().unwrap();
    assert_eq!(card["changes"].as_array().unwrap().len(), 3);
    assert_eq!(card["all_changes"].as_array().unwrap().len(), 4);
    assert_eq!(card["more_count"], 1);
    let change = card["changes"][0]["history_id"].as_i64().unwrap();
    state
        .db
        .with(|c| {
            assert_eq!(super::super::live(c)?.len(), 4);
            let history = recall::history(c, 1)?;
            assert_eq!(history[0]["actor"], "dream");
            assert_eq!(
                history[0]["after"]["source"]["messages"][0]["conversation_id"],
                main
            );
            assert!(undo_card(c, &all[0].id, change + 900, &owner).is_err());
            recall::mutate(c, &state.paths, |c| {
                undo_card(c, &all[0].id, change, &owner)
            })?;
            assert_eq!(super::super::live(c)?.len(), 3);
            assert!(
                recall::mutate(c, &state.paths, |c| undo_card(
                    c, &all[0].id, change, &owner
                ))
                .is_err()
            );
            Ok(())
        })
        .unwrap();
    let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
    assert!(args.contains("--tools\n\n--strict-mcp-config"), "{args}");
    assert!(args.contains("--setting-sources\n"), "{args}");
    let cwd = std::fs::read_to_string(tmp.path().join("cwd")).unwrap();
    assert!(cwd.trim().ends_with("memory-dream"));
    assert!(!std::path::Path::new(cwd.trim()).starts_with(&state.paths.data_dir));
    add_messages(&state, &owner, &main, 4);
    reply(&tmp, json!({"ops":[add_op("Likes nectarines")]}));
    run(&state, &owner, true).await.unwrap();
    assert_eq!(cards(&state).len(), 1);
    assert!(
        std::fs::read_to_string(state.paths.memory_dir().join("you.md"))
            .unwrap()
            .contains("nectarines")
    );
}

#[tokio::test]
async fn invalid_batches_roll_back_everything_and_retain_watermark() {
    let (state, tmp, owner, main) = setup().await;
    add_messages(&state, &owner, &main, 4);
    for invalid in [
        json!({"ops":[add_op("A valid note"),{"op":"UPDATE","alias":99,"text":"Oops","kind":"fact","importance":3,"messages":[1]}]}),
        json!({"ops":[add_op("A valid note"),add_op("<private>secret</private>")]}),
        json!({"ops":[{"op":"ADD","text":"Invalid kind","kind":"unknown","importance":3,"messages":[1]}]}),
        json!({"ops":[{"op":"ADD","text":"Invalid evidence","kind":"fact","importance":3,"messages":[99]}]}),
        json!({"ops":[{"op":"REJECT","reason":"untrusted"}]}),
    ] {
        reply(&tmp, invalid);
        assert!(run(&state, &owner, true).await.is_err());
        assert_eq!(progress(&state), 0);
        assert!(state.db.with(super::super::live).unwrap().is_empty());
        assert_eq!(state.db.with(revision).unwrap(), 0);
        assert!(cards(&state).is_empty());
    }
    raw_reply(&tmp, "prose {\"ops\":[{\"op\":\"NOOP\"}]}");
    assert!(run(&state, &owner, true).await.is_err());
    std::fs::write(tmp.path().join("fail"), "").unwrap();
    assert!(run(&state, &owner, true).await.is_err());
    assert_eq!(progress(&state), 0);
    std::fs::remove_file(tmp.path().join("fail")).unwrap();
    reply(&tmp, json!({"ops":[{"op":"NOOP"}]}));
    run(&state, &owner, true).await.unwrap();
    assert!(progress(&state) > 0);
    assert!(cards(&state).is_empty());
}

#[tokio::test]
async fn loss_guard_aliases_pins_updates_and_snapshot_undo() {
    let (state, tmp, owner, main) = setup().await;

    state
        .db
        .with(|c| {
            for n in 1..=4 {
                note(c, &format!("Orchard apples garden fact {n}"))?;
            }
            Ok(())
        })
        .unwrap();
    state
        .db
        .with(|c| {
            c.execute("UPDATE memory_items SET updated_at=1000", [])?;
            c.execute("UPDATE memory_history SET at=1000", [])?;
            Ok(())
        })
        .unwrap();
    add_messages(&state, &owner, &main, 4);
    let input = state.db.with(|c| prepare(c, 4)).unwrap().unwrap();
    assert_eq!(input.candidates.len(), 4);
    reply(
        &tmp,
        json!({"ops":[{"op":"SUPERSEDE","alias":1,"valid_to":0,"messages":[1]}]}),
    );
    assert!(run(&state, &owner, true).await.is_err());
    assert_eq!(state.db.with(super::super::live).unwrap().len(), 4);
    assert_eq!(progress(&state), 0);
    // Unchanged UPDATEs cannot pad the loss denominator.
    let mut padded = vec![json!({"op":"SUPERSEDE","alias":1,"valid_to":0,"messages":[1]})];
    for n in 2..=4 {
        let old = &input.candidates[n - 1];
        padded.push(json!({"op":"UPDATE","alias":n,"text":old.text,"kind":old.kind,"importance":old.importance,"messages":[1]}));
    }
    reply(&tmp, json!({"ops":padded}));
    assert!(run(&state, &owner, true).await.is_err());
    assert_eq!(progress(&state), 0);
    let mut ops = vec![json!({"op":"SUPERSEDE","alias":1,"valid_to":1,"messages":[1]})];
    for n in 2..=4 {
        ops.push(json!({"op":"UPDATE","alias":n,"text":format!("Orchard apples garden corrected fact {n}"),"kind":"fact","importance":4,"messages":[1]}));
    }
    reply(&tmp, json!({"ops":ops}));
    run(&state, &owner, true).await.unwrap();
    let card = cards(&state).remove(0);
    state
        .db
        .with(|c| {
            let first = super::super::get(c, input.candidates[0].id)?.unwrap();
            assert!(first.archived);
            assert_eq!(first.valid_to, Some(1));
            let updated = super::super::get(c, input.candidates[1].id)?.unwrap();
            assert_eq!(updated.source["type"], "dream");
            let history = recall::history(c, updated.id)?;
            assert_eq!(history[0]["after"]["source"], updated.source);
            let change = card.card.as_ref().unwrap()["changes"][0]["history_id"]
                .as_i64()
                .unwrap();
            recall::mutate(c, &state.paths, |c| undo_card(c, &card.id, change, &owner))?;
            assert!(!super::super::get(c, first.id)?.unwrap().archived);
            Ok(())
        })
        .unwrap();
    add_messages(&state, &owner, &main, 4);
    state
        .db
        .with(|c| {
            c.execute("UPDATE memory_items SET pinned=1", [])?;
            Ok(())
        })
        .unwrap();
    reply(
        &tmp,
        json!({"ops":[{"op":"UPDATE","alias":1,"text":"Pinned edit","kind":"fact","importance":3,"messages":[1]}]}),
    );
    assert!(run(&state, &owner, true).await.is_err());
}

#[tokio::test]
async fn taint_gate_and_input_caps_and_scrubbed_only_progress() {
    let (state, _tmp, owner, main) = setup().await;
    state.db.with(|c| {
        let job=db::ensure_job_conversation(c,&owner,"heartbeat")?;
        let shared=db::create_conversation(c,&owner,"side","Shared",None)?.id;
        c.execute("INSERT INTO users(id,display_name,created_at) VALUES ('guest','Guest',0)",[])?;
        c.execute("INSERT INTO conversation_members(conversation_id,user_id,joined_at) VALUES (?1,'guest',0)",[&shared])?;
        for (conv,role,meta) in [(&job,"user",json!({})),(&shared,"user",json!({})),(&main,"assistant",json!({})),(&main,"user",json!({"untrusted":true})),(&main,"user",json!({"initiated_by":"feed"})),(&main,"user",json!({"browser":true})),(&main,"user",json!({"connector":true}))] {
            db::insert_message(c,&NewMessage {conversation_id:conv,role,author_user_id:Some(&owner),content:"TAINTED orchard content",meta:Some(&meta),..Default::default()})?;
        }
        db::insert_message(c,&NewMessage {conversation_id:&main,role:"user",author_user_id:Some(&owner),content:"TAINTED attachment",attachments:&["image".into()],..Default::default()})?;
        assert!(prepare(c,1)?.is_none());
        Ok(())
    }).unwrap();
    add_messages(&state, &owner, &main, 90);
    let input = state.db.with(|c| prepare(c, 4)).unwrap().unwrap();
    assert_eq!(input.sources.len(), 80);
    assert!(!prompt(&input).contains("TAINTED"));
    assert!(!prompt(&input).contains(&main));
    assert!(!prompt(&input).contains(&input.sources[0].id));
    state
        .db
        .with(|c| {
            c.execute("DELETE FROM messages", [])?;
            Ok(())
        })
        .unwrap();
    state
        .db
        .with(|c| {
            for _ in 0..4 {
                db::insert_message(
                    c,
                    &NewMessage {
                        conversation_id: &main,
                        role: "user",
                        author_user_id: Some(&owner),
                        content: "<private>Do not learn this</private>",
                        ..Default::default()
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();
    run(&state, &owner, true).await.unwrap();
    assert!(progress(&state) > 0);
    assert!(state.db.with(super::super::live).unwrap().is_empty());
}

#[tokio::test]
async fn pause_and_concurrent_privacy_or_note_changes_reject_stale_results() {
    for mutation in ["share", "edit", "pause"] {
        let (state, tmp, owner, main) = setup().await;
        add_messages(&state, &owner, &main, 4);
        reply(&tmp, json!({"ops":[add_op("Grows apples")]}));
        std::fs::write(tmp.path().join("gated"), "").unwrap();
        let st = state.clone();
        let uid = owner.clone();
        let task = tokio::spawn(async move { run(&st, &uid, true).await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !tmp.path().join("calls").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        state.db.with(|c| {
            match mutation {
                "share" => {c.execute("UPDATE conversations SET membership_version = membership_version+1 WHERE id=?1",[&main])?;}
                "edit" => {note(c,"Owner's new note")?;}
                _ => db::set_setting(c,PAUSED,"true")?,
            }
            Ok(())
        }).unwrap();
        std::fs::write(tmp.path().join("release"), "").unwrap();
        assert!(task.await.unwrap().is_err(), "{mutation}");
        assert_eq!(progress(&state), 0);
        assert!(cards(&state).is_empty());
    }
}

#[tokio::test]
async fn settings_disable_learning_or_only_morning_cards() {
    let (state, tmp, owner, main) = setup().await;
    add_messages(&state, &owner, &main, 4);
    reply(&tmp, json!({"ops":[add_op("Grows apples")]}));
    state
        .db
        .with(|c| db::set_setting(c, PAUSED, "true"))
        .unwrap();
    run(&state, &owner, true).await.unwrap();
    assert!(!tmp.path().join("calls").exists());
    assert!(
        super::super::files::write(
            &state.db,
            &state.paths,
            "MEMORY.md",
            "- A note",
            &json!({}),
            0,
            "test"
        )
        .is_err()
    );
    state
        .db
        .with(|c| {
            db::set_setting(c, PAUSED, "false")?;
            db::set_setting(c, MORNING, "false")
        })
        .unwrap();
    run(&state, &owner, true).await.unwrap();
    assert!(progress(&state) > 0);
    assert!(cards(&state).is_empty());
    assert_eq!(state.db.with(super::super::live).unwrap().len(), 1);
}

#[tokio::test]
async fn long_sources_still_retrieve_candidates_with_bounded_prompt_data() {
    let (state, _tmp, owner, main) = setup().await;
    state
        .db
        .with(|c| {
            for n in 0..30 {
                note(
                    c,
                    &format!("Orchard apples candidate {n} {}", "detail ".repeat(260)),
                )?;
            }
            for n in 0..10 {
                db::insert_message(
                    c,
                    &NewMessage {
                        conversation_id: &main,
                        role: "user",
                        author_user_id: Some(&owner),
                        content: &format!("Orchard apples source {n} {}", "a ".repeat(1900)),
                        ..Default::default()
                    },
                )?;
            }
            let input = prepare(c, 4)?.unwrap();
            assert!(input.sources.len() < 10);
            assert!(
                input
                    .sources
                    .iter()
                    .map(|s| s.text.chars().count())
                    .sum::<usize>()
                    <= MAX_INPUT_CHARS
            );
            assert!(!input.candidates.is_empty());
            assert!(input.candidates.len() <= MAX_CANDIDATES);
            assert!(
                input
                    .candidates
                    .iter()
                    .map(|s| s.text.chars().count())
                    .sum::<usize>()
                    <= 16_000
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn forget_and_owner_corrections_override_older_evidence_but_allow_new_assertions() {
    let (state, tmp, owner, main) = setup().await;
    add_messages(&state, &owner, &main, 4);
    let (forgotten, corrected) = state
        .db
        .with(|c| {
            // These chat statements predate the owner's removal/correction.
            c.execute(
                "UPDATE messages SET created_at=1000+rowid, updated_at=1000+rowid",
                [],
            )?;
            let forgotten = note(c, "Orchard apples are a favorite")?;
            recall::forget(c, forgotten, Actor::Owner, "forget-before-dream")?;
            let corrected = note(c, "Orchard apples grow in the east garden")?;
            recall::save(
                c,
                &recall::Edit {
                    text: Some("Orchard apples grow in the west garden".into()),
                    ..Default::default()
                },
                Some(corrected),
                Actor::Owner,
                &json!({}),
                "correction-before-dream",
            )?;
            Ok((forgotten, corrected))
        })
        .unwrap();
    // Every old source is excluded independently of proposed wording, so a
    // paraphrased ADD cannot resurrect the forgotten fact or correction.
    assert!(state.db.with(|c| prepare(c, 4)).unwrap().is_none());
    reply(
        &tmp,
        json!({"ops":[add_op("My favorite fruit is orchard apples"),{"op":"UPDATE","alias":1,"text":"My orchard apple trees are in the east","kind":"fact","importance":3,"messages":[1]}]}),
    );
    run(&state, &owner, true).await.unwrap();
    assert!(!tmp.path().join("calls").exists());
    assert_eq!(progress(&state), 0);
    state
        .db
        .with(|c| {
            assert!(super::super::get(c, forgotten)?.is_none());
            assert_eq!(
                super::super::get(c, corrected)?.unwrap().text,
                "Orchard apples grow in the west garden"
            );
            Ok(())
        })
        .unwrap();
    assert!(cards(&state).is_empty());
    // New statements after the owner's decision can teach the fact again.
    let boundary = state.db.with(|c| cursor(c, EVIDENCE_AFTER)).unwrap();
    add_messages(&state, &owner, &main, 4);
    state
        .db
        .with(|c| {
            c.execute(
                "UPDATE messages SET created_at=?1+rowid WHERE created_at>10000",
                [boundary + 10],
            )?;
            Ok(())
        })
        .unwrap();
    reply(
        &tmp,
        json!({"ops":[add_op("Orchard apples are a favorite"),{"op":"UPDATE","alias":1,"text":"Orchard apples now grow in the north garden","kind":"fact","importance":3,"messages":[1]}]}),
    );
    run(&state, &owner, true).await.unwrap();
    state
        .db
        .with(|c| {
            assert_eq!(super::super::live(c)?.len(), 2);
            assert_eq!(
                super::super::get(c, corrected)?.unwrap().text,
                "Orchard apples now grow in the north garden"
            );
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn dream_backlog_uses_assertion_time_instead_of_execution_time() {
    let (state, tmp, owner, main) = setup().await;
    for (batch, text) in [
        (1, "Orchard apples grow in the east garden"),
        (2, "Orchard apples grow in the west garden"),
        (3, "Orchard apples grow in the east garden"),
    ] {
        add_messages(&state, &owner, &main, 4);
        state.db.with(|c| {c.execute("UPDATE messages SET created_at=?1+rowid WHERE role='user' AND created_at>100000",[batch*1000])?;Ok(())}).unwrap();
        let op = if batch == 1 {
            add_op(text)
        } else {
            json!({"op":"UPDATE","alias":1,"text":text,"kind":"fact","importance":3,"messages":[1]})
        };
        reply(&tmp, json!({"ops":[op]}));
        run(&state, &owner, true).await.unwrap();
        let item = state
            .db
            .with(|c| Ok(super::super::live(c)?.remove(0)))
            .unwrap();
        assert_eq!(item.text, text);
        assert!(item.mentioned_at.unwrap() < 10000);
        assert_eq!(item.source["asserted_at"], item.mentioned_at.unwrap());
    }
}

#[tokio::test]
async fn legacy_idea_starters_are_excluded_and_owner_followups_remain_eligible() {
    let (state, _tmp, owner, _main) = setup().await;
    state
        .db
        .with(|c| {
            let legacy = db::create_conversation(
                c,
                &owner,
                "side",
                "Idea",
                Some(&json!({"type":"idea","id":"synthetic-idea"})),
            )?
            .id;
            let starter = db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &legacy,
                    role: "user",
                    author_user_id: Some(&owner),
                    content: "TAINTED generated orchard claim",
                    ..Default::default()
                },
            )?;
            let followup = db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &legacy,
                    role: "user",
                    author_user_id: Some(&owner),
                    content: "Owner's orchard follow-up",
                    ..Default::default()
                },
            )?;
            let input = prepare(c, 1)?.unwrap();
            assert_eq!(input.sources.len(), 1);
            assert_eq!(input.sources[0].id, followup.id);
            assert!(!prompt(&input).contains(&starter.content));
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn legacy_removals_without_provenance_exclude_backlog_once_and_allow_new_evidence() {
    let (state, tmp, owner, main) = setup().await;
    add_messages(&state, &owner, &main, 4);
    state
        .db
        .with(|c| {
            let id = note(c, "Prefers orchard apples")?;
            recall::forget(c, id, Actor::Owner, "legacy-forget")?;
            // Simulate a pre-Dream database: history exists but no source boundary.
            db::delete_setting(c, EVIDENCE_AFTER)?;
            assert!(prepare(c, 4)?.is_none());
            let after = cursor(c, EVIDENCE_AFTER)?;
            let latest: i64 =
                c.query_row("SELECT MAX(created_at) FROM messages", [], |r| r.get(0))?;
            assert!(after >= latest);
            Ok(())
        })
        .unwrap();
    reply(
        &tmp,
        json!({"ops":[add_op("The favorite fruit is orchard apples")]}),
    );
    run(&state, &owner, true).await.unwrap();
    assert!(!tmp.path().join("calls").exists());
    // A source created after the durable boundary is new evidence.
    add_messages(&state, &owner, &main, 4);
    state
        .db
        .with(|c| {
            let after = cursor(c, EVIDENCE_AFTER)?;
            c.execute(
                "UPDATE messages SET created_at=?1+rowid WHERE created_at>?1",
                [after],
            )?;
            Ok(())
        })
        .unwrap();
    run(&state, &owner, true).await.unwrap();
    assert_eq!(state.db.with(super::super::live).unwrap().len(), 1);
}

#[test]
fn removal_boundary_rolls_back_with_history_and_preserves_returned_history_id() {
    let db = Db::open_in_memory().unwrap();
    db.with(|c| {
        let id = note(c, "Prefers orchard apples")?;
        let original = revision(c)?;
        let failed: rusqlite::Result<()> = db::write_tx(c, |c| {
            recall::forget(c, id, Actor::Owner, "rolled-back-forget")?;
            assert!(cursor(c, EVIDENCE_AFTER)? > 0);
            Err(recall::invalid("Synthetic failure"))
        });
        assert!(failed.is_err());
        assert!(super::super::get(c, id)?.is_some());
        assert_eq!(revision(c)?, original);
        assert!(db::get_setting(c, EVIDENCE_AFTER)?.is_none());
        let forgotten = db::write_tx(c, |c| recall::forget(c, id, Actor::Owner, "forget"))?;
        assert_eq!(forgotten["history_id"], revision(c)?);
        assert!(cursor(c, EVIDENCE_AFTER)? > 0);
        Ok(())
    })
    .unwrap();
}

#[path = "eval.rs"]
mod eval;

#[tokio::test]
async fn reset_blocks_inflight_dream_and_old_backlog_but_new_assertions_learn() {
    let (state, tmp, owner, main) = setup().await;
    add_messages(&state, &owner, &main, 4);
    let input = state.db.with(|c| prepare(c, 4)).unwrap().unwrap();
    state
        .db
        .with(|c| {
            crate::memory::lifecycle::forget_everything(c, &state.paths)?;
            assert!(
                apply(
                    c,
                    &input,
                    parse(&json!({"ops":[add_op("Old orchard note")]}).to_string())?,
                    &owner,
                    "stale-dream"
                )
                .is_err()
            );
            assert!(prepare(c, 4)?.is_none());
            Ok(())
        })
        .unwrap();
    add_messages(&state, &owner, &main, 4);
    reply(&tmp, json!({"ops":[add_op("New orchard note")]}));
    run(&state, &owner, true).await.unwrap();
    assert_eq!(
        state.db.with(super::super::live).unwrap()[0].text,
        "New orchard note"
    );
}
