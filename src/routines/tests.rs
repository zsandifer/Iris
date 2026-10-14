//! Routine storage, proposals, and the run bookkeeping (against SQLite).

use super::run::{self, Due};
use super::*;
use crate::db::Db;

const CHI: &str = "America/Chicago";

fn setup() -> (Db, String, String) {
    let db = Db::open_in_memory().unwrap();
    let (owner, main) = db
        .with(|c| {
            let o = db::owner_user_id(c)?.unwrap();
            let m = db::ensure_main_conversation(c, &o)?;
            Ok((o, m))
        })
        .unwrap();
    (db, owner, main)
}

fn at(s: &str) -> i64 {
    schedule::resolve(
        &schedule::parse_tz(CHI).unwrap(),
        schedule::parse_local(s).unwrap(),
    )
    .unwrap()
    .as_millisecond()
}

fn saturday_dinner() -> Spec {
    Spec::from_json(
        &json!({
            "title": "Saturday dinner order",
            "emoji": "🍜",
            "instructions": "Find a Thai place that's open and build my usual order.",
            "schedule_kind": "rule",
            "rule": {"freq": "weekly", "days": [6], "time": "17:30"},
            "misfire": "skip",
        }),
        CHI,
    )
    .unwrap()
}

/// An active routine created by the owner, as of `now`.
fn active(db: &Db, owner: &str, mut spec: Spec, now: i64) -> Routine {
    spec.validate(now).unwrap();
    let (state, next) = settle(&spec, State::Active, 0, now);
    db.with(|c| insert_routine(c, owner, &spec, state, next, "owner"))
        .unwrap()
}

fn runs(db: &Db, id: &str) -> Vec<run::RunRow> {
    db.with(|c| run::list_runs(c, id, 100)).unwrap()
}

fn card_state(db: &Db, id: &str) -> String {
    db.with(|c| db::get_message(c, id))
        .unwrap()
        .unwrap()
        .card
        .unwrap()["state"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn defaults_and_validation() {
    let mut s = saturday_dinner();
    assert!(s.ask_before_acting && s.notify_when_done);
    assert_eq!((s.approval_timeout_mins, s.max_retries), (120, 2));
    assert_eq!(s.misfire, Misfire::Skip);
    s.validate(at("2026-10-13T12:00")).unwrap();
    assert_eq!(s.schedule_text(), "Every Sat · 5:30 PM");
    let bad = [
        json!({"title": " ", "instructions": "x", "schedule_kind": "once", "run_at": "2026-10-29T08:00"}),
        json!({"title": "x", "instructions": "x", "schedule_kind": "once"}),
        json!({"title": "x", "instructions": "x", "schedule_kind": "cron", "cron_expr": "* * * * *"}),
        json!({"title": "x", "instructions": "x", "schedule_kind": "rule",
               "rule": {"freq": "weekly", "days": [1], "time": "09:00"},
               "starts_at": "2026-11-01", "end_at": "2026-10-01"}),
        json!({"title": "x", "instructions": "x", "schedule_kind": "once", "run_at": "2026-10-29T08:00", "tz": "Mars/Base"}),
        json!({"title": "x", "instructions": "x", "schedule_kind": "once", "run_at": "2026-10-29T08:00", "max_retries": 9}),
    ];
    for b in bad {
        let r = Spec::from_json(&b, CHI).and_then(|mut s| s.validate(0).map(|_| ()));
        assert!(r.is_err(), "{b}");
    }
    // An interval gets a first date to count from.
    let mut every_other = Spec::from_json(
        &json!({"title": "Water the plants", "instructions": "Remind me", "schedule_kind": "rule",
                "rule": {"freq": "weekly", "days": [0], "every": 2, "time": "10:00"}}),
        CHI,
    )
    .unwrap();
    every_other.validate(at("2026-10-13T12:00")).unwrap();
    assert_eq!(
        every_other.rule.unwrap().anchor.as_deref(),
        Some("2026-10-13")
    );
}

#[test]
fn claiming_never_runs_a_slot_twice_and_moves_next_run_on() {
    let (db, owner, _) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let slot = at("2026-10-17T17:30");
    assert_eq!(r.next_run_at, Some(slot));
    assert!(
        db.with(|c| run::claim_due(c, slot - 60_000))
            .unwrap()
            .is_empty()
    );
    let first = db.with(|c| run::claim_due(c, slot + 1_000)).unwrap();
    assert_eq!(first.len(), 1);
    // The same tick again (or a second scheduler) finds nothing due.
    assert!(
        db.with(|c| run::claim_due(c, slot + 2_000))
            .unwrap()
            .is_empty()
    );
    let r = db.with(|c| get_routine(c, &owner, &r.id)).unwrap().unwrap();
    assert_eq!(r.next_run_at, Some(at("2026-10-24T17:30")));
    assert_eq!(r.run_count, 1);
    // Even if next_run_at were moved back, the slot's row stops a rerun.
    db.with(|c| {
        c.execute(
            "UPDATE routines SET next_run_at = ?2 WHERE id = ?1",
            params![r.id, slot],
        )
    })
    .unwrap();
    db.with(|c| run::claim_due(c, slot + 3_000)).unwrap();
    let all = runs(&db, &r.id);
    assert_eq!(all.iter().filter(|x| x.scheduled_for == slot).count(), 1);
}

#[test]
fn a_routine_never_overlaps_itself() {
    let (db, owner, _) = setup();
    let mut spec = saturday_dinner();
    spec.rule = Some(serde_json::from_value(json!({"freq": "daily", "time": "08:00"})).unwrap());
    let r = active(&db, &owner, spec, at("2026-10-13T00:00"));
    let day1 = at("2026-10-13T08:00");
    let claimed = db.with(|c| run::claim_due(c, day1)).unwrap();
    assert_eq!(claimed.len(), 1);
    // Run now while it is going: refused.
    let e = db
        .with(|c| Ok(run::claim_manual(c, &owner, &r.id, day1 + 1)))
        .unwrap()
        .unwrap_err();
    assert!(e.contains("already running"));
    // The next slot comes while the first run is still going: skipped.
    let day2 = at("2026-10-14T08:00");
    assert!(db.with(|c| run::claim_due(c, day2)).unwrap().is_empty());
    let skipped = runs(&db, &r.id)
        .into_iter()
        .find(|x| x.scheduled_for == day2)
        .unwrap();
    assert_eq!(skipped.status, "skipped");
    assert!(skipped.reason.contains("still going"));
    // Once it is over, run now works.
    db.with(|c| {
        c.execute(
            "UPDATE routine_runs SET status = 'ok' WHERE id = ?1",
            [&claimed[0]],
        )
    })
    .unwrap();
    assert!(
        db.with(|c| Ok(run::claim_manual(c, &owner, &r.id, day2 + 5)))
            .unwrap()
            .is_ok()
    );
}

#[test]
fn missed_slots_follow_the_misfire_policy() {
    let (db, owner, _) = setup();
    let created = at("2026-10-13T12:00");
    let skip = active(&db, &owner, saturday_dinner(), created);
    let mut spec = saturday_dinner();
    spec.misfire = Misfire::RunLatestInWindow;
    let catch_up = active(&db, &owner, spec, created);
    // Back 40 minutes after Saturday's slot.
    let now = at("2026-10-17T18:10");
    let claimed = db.with(|c| run::claim_due(c, now)).unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        runs(&db, &catch_up.id)[0].status,
        "running",
        "late but within the hour"
    );
    let s = &runs(&db, &skip.id)[0];
    assert_eq!(s.status, "skipped");
    assert!(s.reason.contains("offline"));
    for id in [&skip.id, &catch_up.id] {
        let r = db.with(|c| get_routine(c, &owner, id)).unwrap().unwrap();
        assert_eq!(r.next_run_at, Some(at("2026-10-24T17:30")));
    }
    let p = saturday_dinner().plan().unwrap();
    assert!(matches!(
        run::plan_due(&p, at("2026-10-17T17:30"), now, Misfire::Skip).due,
        Due::Skip(_)
    ));
}

#[test]
fn once_and_max_runs_finish_the_routine() {
    let (db, owner, _) = setup();
    let once = Spec::from_json(
        &json!({"title": "Gift ideas", "instructions": "Find three gift ideas.",
                "schedule_kind": "once", "run_at": "2026-10-29T08:00", "ask_before_acting": false}),
        CHI,
    )
    .unwrap();
    let r = active(&db, &owner, once, at("2026-10-13T12:00"));
    db.with(|c| run::claim_due(c, at("2026-10-29T08:00")))
        .unwrap();
    let r = db.with(|c| get_routine(c, &owner, &r.id)).unwrap().unwrap();
    assert_eq!((r.state, r.next_run_at), (State::Done, None));

    let mut twice = saturday_dinner();
    twice.max_runs = Some(2);
    let r = active(&db, &owner, twice, at("2026-10-13T12:00"));
    for slot in ["2026-10-17T17:30", "2026-10-24T17:30"] {
        let ids = db.with(|c| run::claim_due(c, at(slot))).unwrap();
        db.with(|c| {
            c.execute(
                "UPDATE routine_runs SET status = 'ok' WHERE id = ?1",
                [&ids[0]],
            )
        })
        .unwrap();
    }
    let r = db.with(|c| get_routine(c, &owner, &r.id)).unwrap().unwrap();
    assert_eq!(
        (r.state, r.next_run_at, r.run_count),
        (State::Done, None, 2)
    );
}

#[test]
fn iris_proposals_wait_for_the_owner() {
    let (db, owner, main) = setup();
    let target = CardTarget {
        conversation_id: &main,
        status: "complete",
    };
    let now = at("2026-10-13T12:00");
    // Create: nothing is scheduled until the owner taps.
    let p = db
        .with(|c| Ok(propose_create(c, &owner, saturday_dinner(), &target)))
        .unwrap()
        .unwrap();
    assert_eq!(p.routine.state, State::PendingApproval);
    assert_eq!(p.routine.next_run_at, None);
    assert_eq!(p.routine.created_by, "iris");
    let card = p.card.card.clone().unwrap();
    assert_eq!(card["type"], "routine");
    assert_eq!(card["action"], "create");
    assert_eq!(card["body"], "Every Sat · 5:30 PM · asks before acting");
    assert!(
        db.with(|c| run::claim_due(c, now + 30 * 86_400_000))
            .unwrap()
            .is_empty()
    );
    // Pending proposals are stored: a fresh connection would see them.
    let pending = db
        .with(|c| pending_change(c, &p.routine.id))
        .unwrap()
        .unwrap();
    assert_eq!(pending.id, p.change.id);
    let d = db
        .with(|c| Ok(approve_change(c, &owner, &p.change.id, None, now)))
        .unwrap()
        .unwrap();
    let r = d.routine.unwrap();
    assert_eq!(r.state, State::Active);
    assert_eq!(r.next_run_at, Some(at("2026-10-17T17:30")));
    assert_eq!(card_state(&db, &p.card.id), "approved");
    // A second tap is refused.
    assert!(
        db.with(|c| Ok(approve_change(c, &owner, &p.change.id, None, now)))
            .unwrap()
            .is_err()
    );

    // Update: a diff summary, applied only on approval.
    let u = db
        .with(|c| {
            Ok(propose_update(
                c,
                &owner,
                &r.id,
                &json!({"rule": {"freq": "weekly", "days": [6, 0], "time": "18:00"}}),
                &target,
            ))
        })
        .unwrap()
        .unwrap();
    assert_eq!(u.change.summary, "Time 5:30 PM → 6:00 PM, adds Sundays");
    let unchanged = db.with(|c| get_routine(c, &owner, &r.id)).unwrap().unwrap();
    assert_eq!(unchanged.spec.rule, r.spec.rule);
    // The owner edits something else meanwhile; approving keeps that edit.
    let mut edited = unchanged.spec.clone();
    edited.title = "Dinner".into();
    db.with(|c| save_routine(c, &r.id, &edited, State::Active, unchanged.next_run_at))
        .unwrap();
    let d = db
        .with(|c| Ok(approve_change(c, &owner, &u.change.id, None, now)))
        .unwrap()
        .unwrap();
    let r = d.routine.unwrap();
    assert_eq!(r.spec.title, "Dinner");
    assert_eq!(r.spec.schedule_text(), "Every Sun, Sat · 6:00 PM");
    assert_eq!(r.next_run_at, Some(at("2026-10-17T18:00")));

    // Pause through update; a newer proposal replaces an older one.
    let first = db
        .with(|c| {
            Ok(propose_update(
                c,
                &owner,
                &r.id,
                &json!({"state": "paused"}),
                &target,
            ))
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.change.summary, "Pauses it");
    let second = db
        .with(|c| {
            Ok(propose_update(
                c,
                &owner,
                &r.id,
                &json!({"approval_timeout_mins": 240}),
                &target,
            ))
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        second.change.summary,
        "Waits 2 hours → 4 hours for your answer"
    );
    assert_eq!(card_state(&db, &first.card.id), "superseded");
    assert!(
        db.with(|c| Ok(approve_change(c, &owner, &first.change.id, None, now)))
            .unwrap()
            .is_err()
    );
    // Declining an update leaves the routine as it was.
    db.with(|c| Ok(decline_change(c, &owner, &second.change.id)))
        .unwrap()
        .unwrap();
    assert_eq!(card_state(&db, &second.card.id), "declined");
    let r = db.with(|c| get_routine(c, &owner, &r.id)).unwrap().unwrap();
    assert_eq!(r.spec.approval_timeout_mins, 120);
    assert!(
        db.with(|c| Ok(propose_update(
            c,
            &owner,
            &r.id,
            &json!({"title": "Dinner"}),
            &target
        )))
        .unwrap()
        .is_err(),
        "a change that changes nothing"
    );

    // Delete: only on approval.
    let del = db
        .with(|c| Ok(propose_delete(c, &owner, &r.id, &target)))
        .unwrap()
        .unwrap();
    assert!(
        db.with(|c| get_routine(c, &owner, &r.id))
            .unwrap()
            .is_some()
    );
    let d = db
        .with(|c| Ok(approve_change(c, &owner, &del.change.id, None, now)))
        .unwrap()
        .unwrap();
    assert!(d.routine.is_none());
    assert!(
        db.with(|c| get_routine(c, &owner, &r.id))
            .unwrap()
            .is_none()
    );
    assert_eq!(card_state(&db, &del.card.id), "approved");

    // Declining a new routine removes it.
    let p = db
        .with(|c| Ok(propose_create(c, &owner, saturday_dinner(), &target)))
        .unwrap()
        .unwrap();
    db.with(|c| Ok(decline_change(c, &owner, &p.change.id)))
        .unwrap()
        .unwrap();
    assert!(
        db.with(|c| get_routine(c, &owner, &p.routine.id))
            .unwrap()
            .is_none()
    );
    assert_eq!(card_state(&db, &p.card.id), "declined");
    // Edited before being set up.
    let p = db
        .with(|c| Ok(propose_create(c, &owner, saturday_dinner(), &target)))
        .unwrap()
        .unwrap();
    let mut mine = saturday_dinner();
    mine.title = "Thai night".into();
    let d = db
        .with(|c| Ok(approve_change(c, &owner, &p.change.id, Some(mine), now)))
        .unwrap()
        .unwrap();
    assert_eq!(d.routine.unwrap().spec.title, "Thai night");
}

#[test]
fn documents_are_pasted_into_the_prompt_with_a_cap() {
    let (db, owner, _) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let big = "x".repeat(docs::MAX_DOC_CHARS + 500);
    for (id, title, text) in [
        ("d1", "Takeout SOP.md", "Fallback: Noodle Bar."),
        ("d2", "Huge.txt", big.as_str()),
    ] {
        db.with(|c| {
            entities::insert_library_item(
                c,
                &owner,
                &entities::NewLibraryItem {
                    id,
                    kind: "artifact",
                    title,
                    description: "",
                    mime: "text/plain",
                    file_name: "f.txt",
                    size: 1,
                    conversation_id: None,
                },
            )?;
            c.execute(
                "UPDATE library_items SET extracted_text = ?2 WHERE id = ?1",
                params![id, text],
            )
        })
        .unwrap();
    }
    // Library items without extracted text cannot be attached.
    db.with(|c| {
        entities::insert_library_item(
            c,
            &owner,
            &entities::NewLibraryItem {
                id: "img",
                kind: "media",
                title: "photo.png",
                description: "",
                mime: "image/png",
                file_name: "p.png",
                size: 1,
                conversation_id: None,
            },
        )
    })
    .unwrap();
    assert!(
        db.with(|c| Ok(set_docs(c, &owner, &r.id, &["img".into()])))
            .unwrap()
            .is_err()
    );
    db.with(|c| Ok(set_docs(c, &owner, &r.id, &["d1".into(), "d2".into()])))
        .unwrap()
        .unwrap();
    let texts = db.with(|c| doc_texts(c, &r.id)).unwrap();
    let prompt = run::build_prompt(&r, &texts, at("2026-10-17T17:30"), false);
    assert!(prompt.contains("Saturday dinner order"));
    assert!(prompt.contains("Sat Oct 17, 5:30 PM, America/Chicago"));
    assert!(prompt.contains("<instructions>\nFind a Thai place"));
    assert!(
        prompt.contains("<document title=\"Takeout SOP.md\">\nFallback: Noodle Bar.\n</document>")
    );
    assert!(prompt.contains("[Truncated: only the first"));
    assert!(prompt.chars().count() < docs::MAX_DOC_CHARS + 4_000);
    assert!(prompt.contains(run::NEEDS_OK), "asks before acting");
    let mut quick = r.clone();
    quick.spec.ask_before_acting = false;
    assert!(!run::build_prompt(&quick, &[], 0, true).contains(run::NEEDS_OK));
    // Removing a document only unlinks it; it stays in the Library.
    db.with(|c| unlink_doc(c, &r.id, "d2")).unwrap();
    assert_eq!(db.with(|c| list_docs(c, &r.id)).unwrap().len(), 1);
    assert!(
        db.with(|c| entities::get_library_item(c, &owner, "d2"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn runs_use_a_private_job_conversation() {
    // Routines are the owner's individual use: each run is a hidden job
    // conversation with nobody but the owner, never a shared chat.
    let (db, owner, _) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let id = db
        .with(|c| Ok(run::claim_manual(c, &owner, &r.id, at("2026-10-13T12:05"))))
        .unwrap()
        .unwrap();
    let (conv, user, assistant) = db
        .with(|c| run::start_conversation(c, &id, &owner, &r.spec.title, "do it"))
        .unwrap();
    let (kind, members) = db
        .with(|c| {
            Ok((
                db::get_conversation_any(c, &conv)?.unwrap().kind,
                db::conversation_member_ids(c, &conv)?,
            ))
        })
        .unwrap();
    assert_eq!(kind, "job");
    assert_eq!(members, std::slice::from_ref(&owner));
    assert_eq!(user.role, "user");
    assert_eq!(assistant.status, "streaming");
    // A retry starts over in a new conversation; the old one is removed.
    let (again, _, _) = db
        .with(|c| run::start_conversation(c, &id, &owner, &r.spec.title, "do it"))
        .unwrap();
    assert_ne!(again, conv);
    assert!(
        db.with(|c| db::get_conversation_any(c, &conv))
            .unwrap()
            .is_none()
    );
}

#[test]
fn unanswered_approvals_expire_into_skipped_runs() {
    let (db, owner, main) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let now = at("2026-10-17T17:40");
    let mut ids = Vec::new();
    for (slot, pending) in [("2026-10-10T17:30", true), ("2026-10-03T17:30", false)] {
        let card = json!({"type": "approval", "title": "Ready", "state": if pending { "pending" } else { "approved" }});
        let m = db
            .with(|c| entities::insert_card(c, &main, &card, "assistant"))
            .unwrap();
        let rid = new_id();
        db.with(|c| {
            c.execute(
                "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, attempts,
                     card_message_id, expires_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'waiting', 1, ?4, ?5, 0, 0)",
                params![rid, r.id, at(slot), m.id, now - 1],
            )
        })
        .unwrap();
        ids.push((rid, m.id));
    }
    let closed = db.with(|c| run::expire_waiting(c, now)).unwrap();
    assert_eq!(closed.len(), 1);
    let expired = db.with(|c| run::get_run(c, &ids[0].0)).unwrap().unwrap();
    assert_eq!(expired.status, "skipped");
    assert_eq!(
        expired.reason,
        "You didn't answer within 2 hours, so I didn't do it."
    );
    assert_eq!(card_state(&db, &ids[0].1), "expired");
    // A decision moves its run on in the same step, so a run still waiting
    // with a decided card (saved by an older version) never blocks the
    // routine: it is skipped too.
    let stuck = db.with(|c| run::get_run(c, &ids[1].0)).unwrap().unwrap();
    assert_eq!(stuck.status, "skipped");
}

#[test]
fn approval_decisions_move_the_run_on_at_once_and_respect_the_deadline() {
    let (db, owner, _) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let now = at("2026-10-17T18:00");
    let mk = |slot: &str, expires_at: i64| {
        let id = new_id();
        db.with(|c| {
            c.execute(
                "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, attempts,
                     expires_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'waiting', 1, ?4, 0, 0)",
                params![id, r.id, at(slot), expires_at],
            )
        })
        .unwrap();
        id
    };
    let get = |id: &str| db.with(|c| run::get_run(c, id)).unwrap().unwrap();
    let decide = |id: &str, yes: bool| db.with(|c| run::decide_in_tx(c, id, yes, now)).unwrap();
    // Before the deadline: a yes starts the acting step.
    let yes = mk("2026-10-17T17:30", now + 60_000);
    assert_eq!(decide(&yes, true), run::RunDecision::Proceed(owner.clone()));
    let y = get(&yes);
    assert_eq!((y.status.as_str(), y.approved_at), ("running", Some(now)));
    // A second tap finds nothing waiting.
    assert_eq!(decide(&yes, true), run::RunDecision::Closed);
    // A no skips it.
    let no = mk("2026-10-10T17:30", now + 60_000);
    assert_eq!(decide(&no, false), run::RunDecision::Proceed(owner.clone()));
    assert_eq!(get(&no).status, "skipped");
    // Past the deadline (the minute tick has not caught up yet, or Iris
    // was paused): too late, nothing is done.
    let late = mk("2026-10-03T17:30", now);
    assert_eq!(decide(&late, true), run::RunDecision::Closed);
    let l = get(&late);
    assert_eq!((l.status.as_str(), l.approved_at), ("skipped", None));
    assert_eq!(
        l.reason,
        "You didn't answer within 2 hours, so I didn't do it."
    );
}

#[test]
fn interrupted_runs_are_recovered() {
    let (db, owner, _) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let now = at("2026-10-17T17:40");
    let mk = |slot: &str, attempts: i64, approved: Option<i64>| {
        let id = new_id();
        db.with(|c| {
            c.execute(
                "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, attempts,
                     approved_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5, 0, 0)",
                params![id, r.id, at(slot), attempts, approved],
            )
        })
        .unwrap();
        id
    };
    let fresh = mk("2026-10-17T17:30", 1, None);
    // Inside the retry window with retries left: only its approval stops it.
    let acting = mk("2026-10-17T17:20", 1, Some(1));
    let old = mk("2026-10-03T17:30", 1, None);
    // A routine that acts without asking first: its run may have acted.
    let mut direct = saturday_dinner();
    direct.title = "Rent reminder".into();
    direct.ask_before_acting = false;
    let d = active(&db, &owner, direct, at("2026-10-13T12:00"));
    let direct_run = new_id();
    db.with(|c| {
        c.execute(
            "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, attempts,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, 'running', 1, 0, 0)",
            params![direct_run, d.id, at("2026-10-17T17:30")],
        )
    })
    .unwrap();
    assert_eq!(db.with(|c| run::recover(c, now)).unwrap(), 4);
    let get = |id: &str| db.with(|c| run::get_run(c, id)).unwrap().unwrap();
    let f = get(&fresh);
    assert_eq!(
        (f.status.as_str(), f.retry_at),
        ("failed", Some(now + 60_000))
    );
    // Due retries start on the next tick.
    assert_eq!(
        db.with(|c| run::claim_due(c, now + 60_000)).unwrap(),
        std::slice::from_ref(&fresh)
    );
    assert_eq!(get(&fresh).attempts, 2);
    let a = get(&acting);
    assert_eq!(
        (a.status.as_str(), a.retry_at),
        ("failed", None),
        "never redo an approved action"
    );
    assert_eq!(get(&old).retry_at, None, "too late to retry");
    let dr = get(&direct_run);
    assert_eq!(
        (dr.status.as_str(), dr.retry_at),
        ("failed", None),
        "never redo a run that may have acted"
    );
    // Nothing else is claimed later.
    assert!(
        db.with(|c| run::claim_due(c, now + 2 * 60_000))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retries_missed_while_offline_follow_the_missed_run_rule() {
    let (db, owner, _) = setup();
    let skip = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let mut late_ok = saturday_dinner();
    late_ok.title = "Late is fine".into();
    late_ok.misfire = Misfire::RunLatestInWindow;
    let window = active(&db, &owner, late_ok, at("2026-10-13T12:00"));
    let now = at("2026-10-17T17:00");
    let mk = |routine: &str, slot: &str, retry_at: i64| {
        let id = new_id();
        db.with(|c| {
            c.execute(
                "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, attempts,
                     retry_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'failed', 1, ?4, 0, 0)",
                params![id, routine, at(slot), retry_at],
            )
        })
        .unwrap();
        id
    };
    let stale = mk(&skip.id, "2026-10-10T17:30", now - 20 * 60_000);
    let fine = mk(&window.id, "2026-10-10T17:30", now - 20 * 60_000);
    let too_old = mk(&window.id, "2026-10-03T17:30", now - 3 * 3_600_000);
    let claimed = db.with(|c| run::claim_due(c, now)).unwrap();
    assert_eq!(claimed, std::slice::from_ref(&fine));
    let get = |id: &str| db.with(|c| run::get_run(c, id)).unwrap().unwrap();
    for id in [&stale, &too_old] {
        let r = get(id);
        assert_eq!((r.status.as_str(), r.retry_at), ("failed", None));
        assert_eq!(
            r.reason,
            "I was offline or paused when I meant to try again, so I stopped."
        );
    }
}

#[test]
fn deleting_a_routine_closes_its_open_approval() {
    let (db, owner, main) = setup();
    let r = active(&db, &owner, saturday_dinner(), at("2026-10-13T12:00"));
    let m = db
        .with(|c| {
            entities::insert_card(
                c,
                &main,
                &json!({"type": "approval", "title": "Ready", "state": "pending"}),
                "assistant",
            )
        })
        .unwrap();
    db.with(|c| {
        c.execute(
            "INSERT INTO routine_runs (id, routine_id, scheduled_for, status, card_message_id,
                 created_at, updated_at)
             VALUES ('r1', ?1, 1, 'waiting', ?2, 0, 0)",
            params![r.id, m.id],
        )
    })
    .unwrap();
    let cards = db
        .with(|c| delete_routine(c, &owner, &r.id))
        .unwrap()
        .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(card_state(&db, &m.id), "cancelled");
    assert!(db.with(|c| run::get_run(c, "r1")).unwrap().is_none());
}
