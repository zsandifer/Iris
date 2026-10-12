//! The For you pipeline. Deterministic code does nearly everything; the
//! model is used at most twice a day, through the user's own CLI with no
//! tools (see `model.rs`).
//!
//! Once a day (the `feed` job):
//! 1. Interests: one background-slot call turns the user's notes, liked
//!    posts, and recent chats into topics, keywords, queries, and source
//!    suggestions (kept for the day; rule-based fallback without a model).
//! 2. Fetch: RSS/Atom, YouTube channel feeds (and the user's subscriptions
//!    when a Google token is available), Reddit, Hacker News, Bluesky, and
//!    Mastodon, politely (`http.rs`).
//! 3. Clean: dedupe against everything already shown, assign topics.
//! 4. Rank by likes-weighted topic affinity, keywords, recency, popularity,
//!    and source diversity (`rank.rs`); enrich the finalists from their
//!    pages (OpenGraph, Twitter cards, JSON-LD) and pick the day's posts.
//! 5. One batched feed-slot call writes a hook, summary, and "why" for the
//!    posts whose own description is not good enough.
//! 6. Cache the original photos and icons locally, publish, and push only
//!    for urgent personal items (`urgent.rs`).
//!
//! The daily size adapts to how the feed is used, and the feed pauses
//! itself (with one message and a resume card) after two weeks unused.

pub mod extract;
pub mod google;
pub mod http;
pub mod images;
pub mod markup;
pub mod model;
pub mod parse;
pub mod rank;
pub mod sources;
pub mod store;
pub mod urgent;

use crate::db::{self, NewMessage};
use crate::entities::{self, PipelinePost};
use crate::ranking::Slot;
use crate::state::AppState;
use crate::turn::{OneShotError, OneShotReply, OneShotRequest, one_shot};
use crate::util::{Result, now_ms, truncate_chars};
use http::{Fetcher, Mode, Request};
use parse::Item;
use rank::Topic;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use store::{DayStat, FeedState, Interests, RunReport};

const DAY_MS: i64 = 86_400_000;
/// The feed pauses after this many days without being opened.
pub const PAUSE_AFTER_DAYS: i64 = 14;
const MAX_SOURCES_PER_RUN: usize = 40;
const MAX_REDDIT_PER_RUN: usize = 4;
const MAX_ITEMS_PER_SOURCE: usize = 40;
const MAX_CANDIDATES: usize = 1500;
const MAX_SUGGESTED_SOURCES: usize = 40;
const MAX_AGE_DAYS: i64 = 7;
const WORKERS: usize = 4;
const PAGE_BYTES: u64 = 1536 * 1024;

// ------------------------------------------------------------ adaptive size

/// Tomorrow's daily size from the last week (newest first, today
/// excluded): grow by a quarter when the user reached the end on most days
/// they opened the feed, shrink when they rarely open it.
pub fn next_size(current: u32, days: &[DayStat]) -> u32 {
    let published: Vec<&DayStat> = days.iter().filter(|d| d.published > 0).take(7).collect();
    if published.len() < 3 {
        return current.clamp(store::MIN_DAILY_SIZE, store::MAX_DAILY_SIZE);
    }
    // Reaching the end of a batch means it was opened, even if the open
    // itself was not reported.
    let opened = published
        .iter()
        .filter(|d| d.opened > 0 || d.reached_end > 0)
        .count();
    let ended = published.iter().filter(|d| d.reached_end > 0).count();
    let next = if opened >= 3 && ended * 10 >= opened * 6 {
        current + (current / 4).max(2)
    } else if opened * 7 <= published.len() * 2 {
        current.saturating_sub((current / 5).max(2))
    } else {
        current
    };
    next.clamp(store::MIN_DAILY_SIZE, store::MAX_DAILY_SIZE)
}

/// Whether today's interests call is still to be made: not when today's
/// interests already came from the model, or the call was already made
/// today (successful or not).
pub fn interests_due(st: &FeedState, today: &str) -> bool {
    let fresh = st
        .interests
        .as_ref()
        .is_some_and(|i| i.by_model && i.generated_day == today);
    !fresh && st.interests_call_day.as_deref() != Some(today)
}

/// Whether today's batched summary call is still available.
pub fn summaries_due(st: &FeedState, today: &str) -> bool {
    st.summary_call_day.as_deref() != Some(today)
}

/// The feed has been published for two weeks and not opened since.
pub fn should_pause(st: &FeedState, now: i64) -> bool {
    if st.paused {
        return false;
    }
    let Some(first) = st.first_batch_at else {
        return false;
    };
    let anchor = [st.last_open_at, st.resumed_at, Some(first)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(first);
    now - anchor >= PAUSE_AFTER_DAYS * DAY_MS
}

// ----------------------------------------------------------------- helpers

/// Run `f` over `items` on up to `workers` threads, keeping order.
fn parallel<T: Send, R: Send>(items: Vec<T>, workers: usize, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let queue = std::sync::Mutex::new(items.into_iter().enumerate().collect::<Vec<_>>());
    let results = std::sync::Mutex::new((0..n).map(|_| None).collect::<Vec<Option<R>>>());
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, n.max(1)) {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap_or_else(|p| p.into_inner()).pop();
                    let Some((i, item)) = next else {
                        break;
                    };
                    let r = f(item);
                    results.lock().unwrap_or_else(|p| p.into_inner())[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|p| p.into_inner())
        .into_iter()
        .map(|r| r.expect("every item processed"))
        .collect()
}

fn read_persona(state: &AppState, name: &str, max: usize) -> String {
    crate::persona::system_file(name)
        .and_then(|f| {
            std::fs::read_to_string(crate::persona::system_file_path(&state.paths, f)).ok()
        })
        .map(|t| truncate_chars(t.trim(), max))
        .unwrap_or_default()
}

/// Status for the app (`GET /api/feed` and `GET /api/feed/status`).
pub async fn status_json(state: &AppState, user_id: &str) -> Value {
    let uid = user_id.to_string();
    let (st, sources) = state
        .db
        .call(move |c| Ok((store::get_state(c, &uid)?, store::list_sources(c, &uid)?)))
        .await
        .unwrap_or_default();
    let owner = state
        .db
        .call(db::owner_user_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|o| o == user_id);
    json!({
        // The daily pipeline runs for the owner (it reads the owner's notes).
        "pipeline": owner,
        "enabled": owner && state.cfg.feed.enabled,
        "running": owner && crate::jobs::is_running(crate::jobs::Job::Feed),
        "paused": st.paused,
        "paused_at": st.paused_at,
        "paused_reason": st.paused.then_some(if st.paused_by_user { "user" } else { "unused" }),
        "daily_size": st.daily_size,
        "last_batch_at": st.last_batch_at,
        "last_batch_count": st.last_batch_count,
        "last_batch_end_id": st.last_batch_end_id,
        "topics": st.interests.as_ref().map(|i| i.topics.iter().map(|t| t.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "sources": sources.iter().filter(|s| s.enabled).count(),
        "last_run": st.last_run,
    })
}

// --------------------------------------------------------------- activity

/// The day a feed event counts for: the day of the batch being read (so a
/// batch finished the next morning still counts as read), else today.
pub fn activity_day(batch_at: Option<i64>, now: i64) -> String {
    match batch_at {
        Some(b) if b <= now && now - b <= PAUSE_AFTER_DAYS * DAY_MS => crate::jobs::local_day(b),
        _ => crate::jobs::local_day(now),
    }
}

/// Record that the user opened the feed or reached the end of the latest
/// batch (`batch_at` is that batch's `last_batch_at`, as the app saw it).
pub async fn record_activity(
    state: &AppState,
    user_id: &str,
    event: &str,
    batch_at: Option<i64>,
) -> Result<()> {
    let (uid, ev) = (user_id.to_string(), event.to_string());
    let day = activity_day(batch_at, now_ms());
    state
        .db
        .call(move |c| {
            match ev.as_str() {
                "opened" => store::record_opened(c, &uid, &day)?,
                _ => store::record_reached_end(c, &uid, &day)?,
            }
            // Either one means the feed is in use (it never pauses then).
            store::update_state(c, &uid, |s| s.last_open_at = Some(now_ms()))?;
            Ok(())
        })
        .await
}

/// Resume a paused feed. Starts today's batch if there is none yet.
pub async fn resume(state: &AppState, user_id: &str) -> Result<bool> {
    let uid = user_id.to_string();
    let was_paused = state
        .db
        .call(move |c| {
            store::update_state(c, &uid, |s| {
                let was = s.paused;
                s.paused = false;
                s.paused_at = None;
                s.resumed_at = Some(now_ms());
                was
            })
        })
        .await?;
    let owner = state.db.call(db::owner_user_id).await?;
    if owner.as_deref() == Some(user_id) && state.cfg.feed.enabled && !state.is_paused().await {
        crate::jobs::trigger(state, crate::jobs::Job::Feed, false);
    }
    Ok(was_paused)
}

/// Pause the feed and say so once in the Main chat, with a resume card.
async fn pause(state: &AppState, user_id: &str) -> Result<()> {
    let uid = user_id.to_string();
    let msgs = state
        .db
        .call(move |c| {
            // State, message, and card together: never paused in silence.
            crate::db::write_tx(c, |c| pause_locked(c, &uid))
        })
        .await?;
    for m in msgs {
        state.hub.to_users(
            &[user_id.to_string()],
            json!({"type": "message.created", "conversation_id": m.conversation_id, "message": m}),
        );
    }
    crate::info!("feed paused after {PAUSE_AFTER_DAYS} days unused");
    Ok(())
}

fn pause_locked(c: &rusqlite::Connection, uid: &str) -> rusqlite::Result<Vec<db::Message>> {
    store::update_state(c, uid, |s| {
        s.paused = true;
        s.paused_by_user = false;
        s.paused_at = Some(now_ms());
    })?;
    let main = db::ensure_main_conversation(c, uid)?;
    let text = "I paused your For you feed. It hasn't been opened in two weeks, so I \
                        stopped looking for posts instead of piling them up. Tap Resume whenever \
                        you want them again.";
    let m1 = db::insert_message(
        c,
        &NewMessage {
            conversation_id: &main,
            role: "assistant",
            content: text,
            meta: Some(&json!({"initiated_by": "feed"})),
            ..Default::default()
        },
    )?;
    let card = json!({
        "type": "action",
        "title": "Resume my feed",
        "body": "I'll start finding posts again right away.",
        "action_label": "Resume",
        "action": {"method": "POST", "path": "/api/feed/resume"},
        "owner_only": false,
    });
    let m2 = entities::insert_card(c, &main, &card, "assistant")?;
    Ok(vec![m1, m2])
}

// --------------------------------------------------------------- the run

/// Run the pipeline for `user_id`. Scheduled runs publish at most one batch
/// per local day; `manual` (the app's "find something now", or resuming)
/// always runs. Returns None when there was nothing to do.
pub async fn run(state: &AppState, user_id: &str, manual: bool) -> Result<Option<RunReport>> {
    let run_id = crate::util::new_id();
    let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
    {
        let mut running = state.running.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = cancel_tx.send(true);
        }
        running.insert(run_id.clone(), cancel_tx);
    }
    let result = run_pipeline(state, user_id, manual, &cancel).await;
    state
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&run_id);
    result
}

/// Each bounded fetch phase finishes before observing Stop; a cancelled
/// run stays stopped even if the owner resumes Iris in the meantime.
async fn stopped(state: &AppState, cancel: &tokio::sync::watch::Receiver<bool>) -> bool {
    *cancel.borrow()
        || state.stopping.load(std::sync::atomic::Ordering::SeqCst)
        || state.is_paused().await
}

async fn run_pipeline(
    state: &AppState,
    user_id: &str,
    manual: bool,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> Result<Option<RunReport>> {
    if stopped(state, cancel).await {
        return Ok(None);
    }
    let now = now_ms();
    let today = crate::jobs::local_day(now);
    let uid = user_id.to_string();
    let st = state.db.call(move |c| store::get_state(c, &uid)).await?;
    if st.paused && !manual {
        return Ok(None);
    }
    if !manual && should_pause(&st, now) {
        pause(state, user_id).await?;
        return Ok(None);
    }
    if !manual && st.last_batch_day.as_deref() == Some(today.as_str()) {
        return Ok(None);
    }
    if st.paused {
        let uid = user_id.to_string();
        state
            .db
            .call(move |c| {
                store::update_state(c, &uid, |s| {
                    s.paused = false;
                    s.paused_at = None;
                    s.resumed_at = Some(now_ms());
                })
            })
            .await?;
    }

    // Adapt the daily size once a day.
    let (uid, t) = (user_id.to_string(), today.clone());
    let size = state
        .db
        .call(move |c| {
            let st = store::get_state(c, &uid)?;
            if st.size_day.as_deref() == Some(t.as_str()) {
                return Ok(st.daily_size);
            }
            let days = store::recent_days(c, &uid, &t, 7)?;
            let next = next_size(st.daily_size, &days);
            store::update_state(c, &uid, |s| {
                s.daily_size = next;
                s.size_day = Some(t.clone());
            })?;
            Ok(next)
        })
        .await?;

    let fetcher = Arc::new(Fetcher::new());
    let mut report = RunReport {
        at: now,
        ..Default::default()
    };
    let mut model_notes: Vec<(OneShotReply, &'static str)> = Vec::new();

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // YouTube via Google, when a token is available.
    let mut youtube_likes = Vec::new();
    if let Some(token) = google::access_token(state, user_id).await {
        let f = fetcher.clone();
        match tokio::task::spawn_blocking(move || sources::youtube_account(&f, &token)).await? {
            Ok((subs, likes)) => {
                youtube_likes = likes;
                let uid = user_id.to_string();
                state
                    .db
                    .call(move |c| {
                        for (id, title) in subs.iter().take(30) {
                            store::add_source(c, &uid, "youtube", id, Some(title), None, "google")?;
                        }
                        Ok(())
                    })
                    .await?;
            }
            Err(e) => crate::warn!("feed: YouTube account unavailable: {e}"),
        }
    }

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // 1. Interests: at most one model call a day, even across manual runs
    // and failures.
    let interests = match st.interests.clone() {
        prev if !interests_due(&st, &today) => prev.unwrap_or_else(|| fallback_interests(&[])),
        _ => {
            let (i, reply) = refresh_interests(state, user_id, &today, youtube_likes).await?;
            if let Some(r) = reply {
                report.model_used = true;
                model_notes.push((r, "interests"));
            }
            i
        }
    };
    let topics = interests.topics.clone();

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // 2. Fetch.
    let uid = user_id.to_string();
    let (srcs, seen, engagement) = state
        .db
        .call(move |c| {
            let mut all = store::list_sources(c, &uid)?;
            if !all.iter().any(|s| s.enabled) {
                store::add_source(
                    c,
                    &uid,
                    "hn",
                    "front_page",
                    Some("Hacker News"),
                    None,
                    "suggested",
                )?;
                all = store::list_sources(c, &uid)?;
            }
            Ok((all, store::seen_keys(c, &uid)?, store::engagement(c, &uid)?))
        })
        .await?;
    let enabled = plan_sources(srcs.into_iter().filter(|s| s.enabled).collect());
    report.sources_checked = enabled.len() as u32;

    let searches: Vec<(String, String, String)> = search_plan(&topics);
    let f = fetcher.clone();
    let fetch_now = now;
    let (source_results, search_items) = tokio::task::spawn_blocking(move || {
        let src_results = parallel(enabled, WORKERS, |s| {
            let r = sources::fetch(&f, &s, fetch_now);
            (s, r)
        });
        let search_items: Vec<Item> = parallel(searches, WORKERS, |(kind, q, topic)| {
            let items = match kind.as_str() {
                "hn" => sources::hn(&f, &q, fetch_now),
                _ => sources::mastodon_tag(&f, &q, "mastodon.social"),
            };
            items
                .map(|v| {
                    v.into_iter()
                        .map(|mut i| {
                            i.topic = Some(topic.clone());
                            i
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .into_iter()
        .flatten()
        .collect();
        (src_results, search_items)
    })
    .await?;

    let mut items: Vec<Item> = Vec::new();
    let mut results: Vec<(String, std::result::Result<Option<Value>, String>)> = Vec::new();
    for (src, r) in source_results {
        match r {
            Ok(fetched) => {
                items.extend(fetched.items);
                results.push((src.id.clone(), Ok(fetched.state)));
            }
            Err(e) => {
                crate::warn!("feed: source {} ({}) failed: {e}", src.id, src.kind);
                results.push((src.id.clone(), Err(e)));
            }
        }
    }
    state
        .db
        .call(move |c| {
            for (id, r) in &results {
                match r {
                    Ok(s) => store::record_fetch(c, id, Ok(s.as_ref()))?,
                    Err(e) => store::record_fetch(c, id, Err(e))?,
                }
            }
            Ok(())
        })
        .await?;
    // Followed sources first so they win ties in dedupe.
    items.extend(search_items);
    report.items_found = items.len() as u32;

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // 3. Clean and 4. rank (CPU work, off the async workers), enrich the
    // finalists from their pages, then pick.
    let lang = interests.language.clone().unwrap_or_else(|| "en".into());
    let (t2, e2) = (topics.clone(), engagement.clone());
    let seen_after = seen.clone();
    let finalists: Vec<Item> = tokio::task::spawn_blocking(move || {
        let items = rank::dedupe(clean(items, now, &lang), &seen);
        let mut items = items;
        rank::assign_topics(&mut items, &t2);
        let mut scored: Vec<(f64, Item)> = items
            .into_iter()
            .map(|i| (rank::score(&i, &t2, &e2, now), i))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        shortlist(scored, (size as usize * 3).clamp(15, 60))
    })
    .await?;
    let f = fetcher.clone();
    let mut enriched =
        tokio::task::spawn_blocking(move || parallel(finalists, WORKERS, |it| enrich(&f, it)))
            .await?;
    // Pages can reveal an old publication date, or that two links (or a
    // link and an earlier post) are the same page.
    let min_time = now - MAX_AGE_DAYS * DAY_MS;
    enriched.retain(|i| i.published_ms.is_none_or(|t| t >= min_time));
    let mut enriched = rank::dedupe(enriched, &seen_after);
    rank::assign_topics(&mut enriched, &topics);
    let rescored: Vec<(f64, Item)> = enriched
        .into_iter()
        .map(|i| (rank::score(&i, &topics, &engagement, now), i))
        .collect();
    let picked = rank::select(rescored, size as usize);

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // 5. Blurbs: one batched call for the posts that need one.
    let mut blurbs: HashMap<String, model::Blurb> = HashMap::new();
    let need: Vec<model::SummaryInput> = picked
        .iter()
        .enumerate()
        .filter(|(_, (_, it))| !rank::description_good_enough(it))
        .map(|(i, (_, it))| model::SummaryInput {
            id: format!("p{i}"),
            title: it.title.clone(),
            source: it.source_name.clone(),
            topic: it.topic.clone(),
            text: it.description.clone(),
        })
        .collect();
    // At most one summary call a day: later runs use the descriptions.
    // The day is read again here: a run that started before midnight
    // charges the call to the day it is actually made.
    let call_day = crate::jobs::local_day(now_ms());
    if !need.is_empty() && summaries_due(&st, &call_day) {
        let topic_names: Vec<String> = topics.iter().map(|t| t.name.clone()).collect();
        let (b, reply) = write_blurbs(state, user_id, &call_day, &need, &topic_names).await;
        blurbs = b;
        if let Some(reply) = reply {
            report.model_used = true;
            model_notes.push((reply, "summaries"));
        }
    }

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // 6. Cache photos and icons, then publish.
    let f = fetcher.clone();
    let data_dir = state.paths.data_dir.clone();
    let to_cache: Vec<(Vec<String>, Option<String>)> = picked
        .iter()
        .map(|(_, it)| (it.images.clone(), it.favicon.clone()))
        .collect();
    // Original URL -> cached file.
    let cached: Vec<BTreeMap<String, String>> = tokio::task::spawn_blocking(move || {
        parallel(to_cache, WORKERS, |(imgs, icon)| {
            cache_assets(&f, &data_dir, &imgs, icon.as_deref())
        })
    })
    .await?;

    let mut posts = Vec::with_capacity(picked.len());
    let mut seen_new = Vec::new();
    let mut urgent_candidates: Vec<(String, parse::Event, Option<String>, bool)> = Vec::new();
    for (rank_i, ((_, it), cache)) in picked.into_iter().zip(cached).enumerate() {
        let b = blurbs.remove(&format!("p{rank_i}")).unwrap_or_default();
        let good = rank::description_good_enough(&it);
        let body = if good {
            it.description.clone()
        } else {
            b.summary
                .clone()
                .or_else(|| {
                    (!it.description.is_empty()).then(|| truncate_chars(&it.description, 400))
                })
                .unwrap_or_else(|| it.title.clone())
        };
        if b.summary.is_some() {
            report.summarized += 1;
        }
        let why = b
            .why
            .clone()
            .or_else(|| rank::rule_why(&it, &topics, &engagement));
        // Only photos Iris could fetch and check itself are shown (the app
        // never loads a post's image URLs directly): anything refused by
        // robots.txt, the public-address rule, or the photo checks is left
        // out.
        let images: Vec<String> = it
            .images
            .iter()
            .filter(|u| cache.contains_key(*u))
            .cloned()
            .collect();
        let favicon = it.favicon.clone().filter(|f| cache.contains_key(f));
        let mut facts = json!({});
        if let Some(p) = &it.price {
            facts["price"] = json!(p);
        }
        if let Some(e) = &it.event {
            facts["event"] = json!(e);
            urgent_candidates.push((
                it.title.clone(),
                e.clone(),
                it.topic.clone(),
                it.source_id.is_some(),
            ));
        }
        let key = rank::url_key(&it.url);
        let ids = rank::identity_keys(&it);
        seen_new.extend(ids.iter().cloned());
        posts.push(PipelinePost {
            title: it.title.clone(),
            body,
            hook: b.hook,
            why,
            topic: it.topic.clone(),
            source_url: it.url.clone(),
            source_name: if it.source_name.is_empty() {
                markup::host_of(&it.url).unwrap_or_default()
            } else {
                it.source_name.clone()
            },
            source_kind: it.source_kind.clone(),
            url_key: key,
            seen_keys: ids,
            images,
            favicon_url: favicon,
            cached: cache,
            facts,
        });
    }
    let uid = user_id.to_string();
    // Published today, by the clock now (not when the run started).
    let t = crate::jobs::local_day(now_ms());
    let rep = report.clone().with_stats(fetcher.stats());
    if stopped(state, cancel).await {
        return Ok(None);
    }

    let published: Vec<String> = state
        .db
        .call(move |c| publish_batch(c, &uid, &posts, &seen_new, &t, rep))
        .await?;
    report.published = published.len() as u32;
    urgent_candidates.retain(|(title, ..)| published.contains(title));
    state.hub.to_users(
        &[user_id.to_string()],
        json!({"type": "entities.changed", "kinds": ["feed"]}),
    );

    if stopped(state, cancel).await {
        return Ok(None);
    }

    // Urgent pushes: events in today's batch, then price drops.
    for (title, ev, topic, followed) in urgent_candidates {
        if stopped(state, cancel).await {
            return Ok(None);
        }
        let affinity = topic
            .as_ref()
            .and_then(|t| topics.iter().find(|x| x.name.eq_ignore_ascii_case(t)))
            .map_or(0.0, |t| rank::topic_weight(t, &engagement));
        let score =
            urgent::event_score(&ev, now, affinity, interests.location.as_deref(), followed);
        if score >= urgent::URGENT_AT {
            // The day the event page states, not a UTC conversion of it.
            let when = if ev.local_date.is_empty() {
                crate::jobs::local_day(ev.start_ms)
            } else {
                ev.local_date.clone()
            };
            let body = format!("{} · {}", truncate_chars(&title, 90), when);
            push_urgent(state, user_id, "Something near you soon", &body).await;
        }
    }
    if stopped(state, cancel).await {
        return Ok(None);
    }

    check_price_watches(state, user_id, &fetcher, cancel).await;

    // Housekeeping: photos no post uses any more.
    let referenced = state
        .db
        .call(store::referenced_images)
        .await
        .unwrap_or_default();
    let dd = state.paths.data_dir.clone();
    let _ = tokio::task::spawn_blocking(move || images::prune(&dd, &referenced)).await;

    let report = report.with_stats(fetcher.stats());
    record_run(state, user_id, &report, &model_notes).await;
    crate::info!(
        "feed: {} sources, {} items, {} published, {} summarized, {} requests",
        report.sources_checked,
        report.items_found,
        report.published,
        report.summarized,
        report.requests
    );
    Ok(Some(report))
}

/// Publish a ranked batch (best first) in one transaction: posts that are
/// not already in the feed, their seen keys, the day's count, and the batch
/// bookkeeping. Returns the titles actually published. An empty batch
/// leaves the day open for another try.
fn publish_batch(
    c: &rusqlite::Connection,
    uid: &str,
    posts: &[PipelinePost],
    seen_new: &[String],
    day: &str,
    mut rep: RunReport,
) -> rusqlite::Result<Vec<String>> {
    crate::db::write_tx(c, |c| {
        // Recheck inside the transaction: the agent may have posted the
        // same story (through the bridge) while this run worked.
        if db::get_setting(c, "paused")?.is_some() {
            return Ok(Vec::new());
        }
        let seen_now = store::seen_keys(c, uid)?;
        let fresh: Vec<&PipelinePost> = posts
            .iter()
            .filter(|p| !p.seen_keys.iter().any(|k| seen_now.contains(k)))
            .collect();
        // Lowest ranked first: each insert gets a later unique timestamp,
        // so the best post is the newest.
        let mut first: Option<entities::FeedPost> = None;
        for p in fresh.iter().rev() {
            let post = entities::insert_pipeline_post(c, uid, p)?;
            first.get_or_insert(post);
        }
        store::add_seen(c, uid, seen_new)?;
        let n = fresh.len();
        rep.published = n as u32;
        store::update_state(c, uid, |s| {
            s.last_run = Some(rep);
            let Some(first) = &first else {
                return;
            };
            s.last_batch_at = Some(first.created_at);
            s.last_batch_end_id = Some(first.id.clone());
            s.last_batch_day = Some(day.to_string());
            s.last_batch_count = n as u32;
            if s.first_batch_at.is_none() {
                s.first_batch_at = Some(first.created_at);
            }
        })?;
        if n > 0 {
            store::record_published(c, uid, day, n as i64)?;
        }
        store::prune(c, uid)?;
        Ok(fresh.iter().map(|p| p.title.clone()).collect())
    })
}

/// Topic queries to run: one Hacker News search per topic (its first
/// query) and one Mastodon hashtag for single-word keywords.
fn search_plan(topics: &[Topic]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for t in topics.iter().take(6) {
        if let Some(q) = t.queries.first() {
            out.push(("hn".to_string(), q.clone(), t.name.clone()));
        }
    }
    for t in topics.iter().take(4) {
        if let Some(k) = t
            .keywords
            .iter()
            .find(|k| k.len() >= 4 && k.chars().all(|c| c.is_alphanumeric()))
        {
            out.push(("mastodon".to_string(), k.clone(), t.name.clone()));
        }
    }
    out
}

/// Fill in a candidate from its page: photos, description, site name,
/// icon, and the facts urgent alerts need (price, event). Every finalist's
/// page is read (robots.txt decides whether it may be), except posts that
/// live on a social platform or video site and carry no such facts.
fn enrich(f: &Fetcher, mut it: Item) -> Item {
    let native = markup::host_of(&it.url).is_some_and(|h| {
        let h = h.trim_start_matches("www.");
        [
            "bsky.app",
            "reddit.com",
            "news.ycombinator.com",
            "youtube.com",
            "youtu.be",
        ]
        .iter()
        .any(|p| h == *p || h.ends_with(&format!(".{p}")))
    }) || it.native;
    if native {
        return it;
    }
    let mut req = Request::new(
        &it.url,
        Mode::Crawl,
        "text/html,application/xhtml+xml",
        PAGE_BYTES,
    );
    req.truncate = true;
    let Ok(res) = f.get(&req) else {
        return it;
    };
    if !res.content_type.is_empty() && !res.content_type.contains("html") {
        return it;
    }
    let m = extract::extract(&res.text(), &res.final_url);
    // The same page under other URLs (redirects, its canonical link).
    let own = rank::url_key(&it.url);
    for u in [Some(res.final_url.clone()), m.canonical.clone()]
        .into_iter()
        .flatten()
    {
        let k = rank::url_key(&u);
        if k != own && !it.aliases.contains(&k) {
            it.aliases.push(k);
        }
    }
    // The page's photos back up the feed's (thumbnails may be too small to
    // keep or fail to load).
    for u in m.images {
        if it.images.len() >= parse::MAX_ITEM_IMAGES {
            break;
        }
        if !it.images.contains(&u) {
            it.images.push(u);
        }
    }
    if let Some(d) = m.description
        && d.chars().count() > it.description.chars().count()
    {
        it.description = truncate_chars(&d, 1500);
    }
    // A link shared on Hacker News, Bluesky, or Mastodon: credit the
    // publisher (Reddit posts keep their subreddit).
    let shared_link = matches!(it.source_kind.as_str(), "hn" | "bluesky" | "mastodon")
        && markup::host_of(&it.url)
            .is_some_and(|h| !h.ends_with("bsky.app") && !h.ends_with("ycombinator.com"));
    if shared_link
        && let Some(site) = m.site_name.or_else(|| {
            markup::host_of(&res.final_url).map(|h| h.trim_start_matches("www.").to_string())
        })
    {
        it.source_name = site;
    }
    if let Some(icon) = m.favicon {
        it.favicon = Some(icon);
    }
    // The article's own date wins when older (a fresh share of an old
    // story is still an old story).
    if let Some(page) = m.published_ms {
        it.published_ms = Some(it.published_ms.map_or(page, |t| t.min(page)));
    }
    it.price = it.price.or(m.price);
    it.event = it.event.or(m.event);
    it
}

/// Ask the background model for today's interests; fall back to the last
/// ones, or to topics from liked posts.
async fn refresh_interests(
    state: &AppState,
    user_id: &str,
    today: &str,
    youtube_likes: Vec<String>,
) -> Result<(Interests, Option<OneShotReply>)> {
    let uid = user_id.to_string();
    let (st, liked, chats, srcs) = state
        .db
        .call(move |c| {
            Ok((
                store::get_state(c, &uid)?,
                store::liked_posts(c, &uid, 20)?,
                store::recent_user_messages(c, &uid, now_ms() - 14 * DAY_MS, 30)?,
                store::list_sources(c, &uid)?,
            ))
        })
        .await?;
    let notes = format!(
        "{}\n\n{}",
        read_persona(state, "USER.md", 5000),
        read_persona(state, "PROACTIVE_PREFERENCES.md", 2000)
    );
    let input = model::InterestsInput {
        user_notes: notes,
        feed_instructions: state.feed_prompt().await,
        liked: liked
            .iter()
            .map(|(t, topic)| match topic {
                Some(tp) => format!("{t} ({tp})"),
                None => t.clone(),
            })
            .collect(),
        recent_chats: {
            let mut total = 0;
            chats
                .into_iter()
                .map(|m| truncate_chars(&markup::collapse_ws(&m), 240))
                .take_while(|m| {
                    total += m.len();
                    total < 6000
                })
                .collect()
        },
        youtube_likes,
        current_topics: st
            .interests
            .as_ref()
            .map(|i| i.topics.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default(),
        current_sources: srcs
            .iter()
            .take(40)
            .map(|s| format!("{} {}", s.kind, s.value))
            .collect(),
    };
    let prompt = model::interests_prompt(&input);
    if !reserve_call(state, user_id, today, Call::Interests).await {
        let keep = st
            .interests
            .clone()
            .unwrap_or_else(|| fallback_interests(&liked));
        return Ok((keep, None));
    }
    let reply = one_shot(
        state,
        OneShotRequest {
            slot: Slot::Background,
            shared: false,
            system_prompt: model::INTERESTS_SYSTEM,
            prompt: &prompt,
            timeout: Duration::from_secs(180),
            scope: "feed-interests",
            // One real model call; a signed-out CLI falls through to the next.
            max_attempts: 1,
            take_slot: true,
        },
    )
    .await;
    // No model ran: give the day's call back.
    if matches!(reply, Err(OneShotError::NoEngine)) {
        release_call(state, user_id, Call::Interests).await;
    }
    let parsed = reply
        .as_ref()
        .ok()
        .and_then(|r| model::parse_interests(&r.text));
    let Some(model::ParsedInterests {
        topics,
        sources: suggestions,
        location,
        language,
    }) = parsed
    else {
        if let Err(e) = &reply {
            crate::warn!("feed: interests unavailable ({e})");
        } else {
            crate::warn!("feed: interests reply was not usable JSON");
        }
        let fallback = match st.interests.clone() {
            Some(i) => i,
            None => {
                // Keep the derived topics for later runs today (no second
                // model call).
                let f = fallback_interests(&liked);
                let (uid, keep) = (user_id.to_string(), f.clone());
                state
                    .db
                    .call(move |c| store::update_state(c, &uid, |s| s.interests = Some(keep)))
                    .await?;
                f
            }
        };
        return Ok((fallback, None));
    };
    let uid = user_id.to_string();
    let added = state
        .db
        .call(move |c| {
            let mut added = 0;
            for s in &suggestions {
                if let Some((kind, value)) = sources::from_suggestion(s) {
                    let topic = s["topic"].as_str().map(|t| truncate_chars(t, 40));
                    // Arbitrary-host suggestions are stored off (see
                    // `auto_enable`), in the same statement.
                    if store::add_source(
                        c,
                        &uid,
                        &kind,
                        &value,
                        None,
                        topic.as_deref(),
                        "suggested",
                    )?
                    .is_some()
                    {
                        added += 1;
                    }
                }
            }
            store::trim_suggested(c, &uid, MAX_SUGGESTED_SOURCES)?;
            Ok(added)
        })
        .await?;
    crate::info!(
        "feed: interests updated ({} topics, {added} new sources)",
        topics.len()
    );
    let interests = Interests {
        topics,
        location,
        generated_at: now_ms(),
        generated_day: today.to_string(),
        by_model: true,
        language,
    };
    let (uid, keep) = (user_id.to_string(), interests.clone());
    state
        .db
        .call(move |c| store::update_state(c, &uid, |s| s.interests = Some(keep)))
        .await?;
    Ok((interests, reply.ok()))
}

/// Interests without a model: one topic per liked-post topic.
fn fallback_interests(liked: &[(String, Option<String>)]) -> Interests {
    Interests {
        topics: topics_from_likes(liked),
        location: None,
        generated_at: now_ms(),
        generated_day: String::new(),
        by_model: false,
        language: None,
    }
}

#[derive(Clone, Copy)]
enum Call {
    Interests,
    Summaries,
}

/// Record that today's model call of this kind happened.
/// Reserve today's call of this kind before making it (so a restart
/// mid-call cannot lead to a second one). False when already used today.
async fn reserve_call(state: &AppState, user_id: &str, today: &str, call: Call) -> bool {
    let (uid, day) = (user_id.to_string(), today.to_string());
    state
        .db
        .call(move |c| {
            store::update_state(c, &uid, |s| {
                let slot = match call {
                    Call::Interests => &mut s.interests_call_day,
                    Call::Summaries => &mut s.summary_call_day,
                };
                if slot.as_deref() == Some(day.as_str()) {
                    return false;
                }
                *slot = Some(day);
                true
            })
        })
        .await
        .unwrap_or(false)
}

/// Give back a reservation when no model ran (no engine available).
async fn release_call(state: &AppState, user_id: &str, call: Call) {
    let uid = user_id.to_string();
    let _ = state
        .db
        .call(move |c| {
            store::update_state(c, &uid, |s| match call {
                Call::Interests => s.interests_call_day = None,
                Call::Summaries => s.summary_call_day = None,
            })
        })
        .await;
}

/// The day's one batched summary call: blurbs by post id.
async fn write_blurbs(
    state: &AppState,
    user_id: &str,
    today: &str,
    need: &[model::SummaryInput],
    topic_names: &[String],
) -> (HashMap<String, model::Blurb>, Option<OneShotReply>) {
    if !reserve_call(state, user_id, today, Call::Summaries).await {
        return (HashMap::new(), None);
    }
    let uid = user_id.to_string();
    let liked: Vec<String> = state
        .db
        .call(move |c| store::liked_posts(c, &uid, 8))
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let prompt = model::summary_prompt(need, topic_names, &liked);
    match one_shot(
        state,
        OneShotRequest {
            slot: Slot::Feed,
            shared: false,
            system_prompt: model::SUMMARY_SYSTEM,
            prompt: &prompt,
            timeout: Duration::from_secs(300),
            scope: "feed-summaries",
            // One real model run; a signed-out CLI falls through.
            max_attempts: 1,
            take_slot: true,
        },
    )
    .await
    {
        Ok(reply) => (model::parse_summaries(&reply.text), Some(reply)),
        Err(OneShotError::NoEngine) => {
            crate::warn!("feed: no engine for summaries; using descriptions");
            release_call(state, user_id, Call::Summaries).await;
            (HashMap::new(), None)
        }
        Err(OneShotError::Failed(e)) => {
            crate::warn!("feed: summaries failed ({e}); using descriptions");
            (HashMap::new(), None)
        }
    }
}

/// Which sources this run reads: the user's own first, then Google, then
/// suggested; within each, the least recently read first, so a cap never
/// starves a source. At most a few subreddits (Reddit allows about a
/// request a minute without OAuth).
fn plan_sources(mut enabled: Vec<store::Source>) -> Vec<store::Source> {
    let rank = |o: &str| match o {
        "user" => 0,
        "google" => 1,
        _ => 2,
    };
    enabled.sort_by_key(|s| (rank(&s.origin), s.last_fetch_at.unwrap_or(0)));
    let mut reddit = 0;
    enabled.retain(|s| {
        reddit += usize::from(s.kind == "reddit");
        s.kind != "reddit" || reddit <= MAX_REDDIT_PER_RUN
    });
    enabled.truncate(MAX_SOURCES_PER_RUN);
    enabled
}

/// Drop old, foreign-language, housekeeping, and malformed items, and cap
/// how many each source contributes (newest first) before deduplication.
fn clean(items: Vec<Item>, now: i64, lang: &str) -> Vec<Item> {
    let min_time = now - MAX_AGE_DAYS * DAY_MS;
    let mut per_source: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<Item> = items
        .into_iter()
        .filter(|i| {
            i.published_ms
                .is_none_or(|t| t >= min_time && t <= now + DAY_MS)
                && i.language.as_deref().is_none_or(|l| l.starts_with(lang))
                && i.title.chars().count() >= 8
                && !rank::is_housekeeping(&i.title)
                && markup::split_url(&i.url).is_some()
        })
        .collect();
    // Stable: followed sources keep their place ahead of searches.
    items.sort_by_key(|i| std::cmp::Reverse(i.published_ms.unwrap_or(0)));
    items.sort_by_key(|i| i.source_id.is_none());
    items.retain(|i| {
        let key = i.source_id.clone().unwrap_or_else(|| i.source_name.clone());
        let n = per_source.entry(key).or_default();
        *n += 1;
        *n <= MAX_ITEMS_PER_SOURCE
    });
    items.truncate(MAX_CANDIDATES);
    for it in &mut items {
        if it.title.len() > 1200 {
            it.title = truncate_chars(&it.title, 300);
        }
        if it.description.len() > 6000 {
            it.description = truncate_chars(&it.description, 1500);
        }
    }
    items
}

/// The best `n` candidates by score, at most twice the per-source cap from
/// any one source, so the final pick (3 per source) can still fill the day.
fn shortlist(scored: Vec<(f64, Item)>, n: usize) -> Vec<Item> {
    const PER_SOURCE: usize = 6;
    let mut per_source: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(n);
    for (_, it) in scored {
        if out.len() >= n {
            break;
        }
        let c = per_source.entry(it.source_name.clone()).or_default();
        if *c < PER_SOURCE {
            *c += 1;
            out.push(it);
        }
    }
    out
}

/// Suggested sources that only ever contact a fixed, well-known host
/// (Reddit, YouTube, Hacker News) start enabled; feeds on arbitrary sites,
/// Bluesky handles (resolved through their own domain), and Mastodon
/// accounts (on any instance) wait for the owner.
pub fn auto_enable(kind: &str) -> bool {
    matches!(kind, "reddit" | "youtube" | "hn")
}

/// Without a model: one topic per liked-post topic.
fn topics_from_likes(liked: &[(String, Option<String>)]) -> Vec<Topic> {
    let mut by_topic: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (title, topic) in liked {
        if let Some(t) = topic {
            by_topic
                .entry(t.clone())
                .or_default()
                .extend(rank::words(title));
        }
    }
    by_topic
        .into_iter()
        .take(8)
        .map(|(name, mut words)| {
            words.sort();
            words.dedup();
            words.truncate(10);
            Topic {
                name,
                weight: 0.6,
                keywords: words,
                queries: Vec::new(),
            }
        })
        .collect()
}

/// Send an urgent push (counts toward the daily cap). Quiet hours delay it.
async fn push_urgent(state: &AppState, user_id: &str, title: &str, body: &str) {
    if state.is_paused().await || state.stopping.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let uid = user_id.to_string();
    let day = crate::jobs::local_day(now_ms());
    let allowed = state
        .db
        .call(move |c| {
            store::update_state(c, &uid, |s| {
                if s.urgent_day.as_deref() != Some(day.as_str()) {
                    s.urgent_day = Some(day.clone());
                    s.urgent_count = 0;
                }
                if s.urgent_count >= urgent::MAX_PER_DAY {
                    return false;
                }
                s.urgent_count += 1;
                true
            })
        })
        .await
        .unwrap_or(false);
    if !allowed {
        return;
    }
    crate::notify::push_users(
        state,
        &[user_id.to_string()],
        json!({"title": title, "body": truncate_chars(body, 180), "kind": "feed_urgent", "url": "/#/foryou"}),
    );
}

/// Re-check saved products' prices; a real drop becomes a post and a push.
async fn check_price_watches(
    state: &AppState,
    user_id: &str,
    fetcher: &Arc<Fetcher>,
    cancel: &tokio::sync::watch::Receiver<bool>,
) {
    let uid = user_id.to_string();
    let due = state
        .db
        .call(move |c| store::due_watches(c, &uid, 10))
        .await
        .unwrap_or_default();
    if due.is_empty() {
        return;
    }
    let f = fetcher.clone();
    let data_dir = state.paths.data_dir.clone();
    // (watch, new price if read, drop post if urgent)
    type Checked = (store::Watch, Option<parse::Price>, Option<PipelinePost>);
    let checked: Vec<Checked> = tokio::task::spawn_blocking(move || {
        parallel(due, WORKERS, |w| {
            let mut req = Request::new(&w.url, Mode::Crawl, "text/html", PAGE_BYTES);
            req.truncate = true;
            let Some(meta) = f
                .get(&req)
                .ok()
                .map(|r| extract::extract(&r.text(), &r.final_url))
            else {
                return (w, None, None);
            };
            let old = parse::Price {
                amount: w.price,
                currency: w.currency.clone(),
            };
            let Some(new) = meta.price.clone() else {
                return (w, None, None);
            };
            if urgent::price_drop_score(&old, &new) < urgent::URGENT_AT {
                return (w, Some(new), None);
            }
            // Same photo rules as every post: only copies Iris checked.
            let images: Vec<String> = meta
                .images
                .iter()
                .take(parse::MAX_ITEM_IMAGES)
                .cloned()
                .collect();
            let cached = cache_assets(&f, &data_dir, &images, meta.favicon.as_deref());
            let post = PipelinePost {
                title: format!("Price drop: {}", truncate_chars(&w.title, 200)),
                body: urgent::price_drop_text(&w.title, &old, &new),
                hook: Some("Something you saved got cheaper".into()),
                why: Some("You saved this, and the price just dropped.".into()),
                topic: None,
                source_url: w.url.clone(),
                source_name: meta
                    .site_name
                    .clone()
                    .or_else(|| markup::host_of(&w.url))
                    .unwrap_or_default(),
                source_kind: "price_drop".into(),
                url_key: format!("price:{}:{}", rank::url_key(&w.url), new.amount),
                seen_keys: Vec::new(),
                images: images
                    .into_iter()
                    .filter(|u| cached.contains_key(u))
                    .collect(),
                favicon_url: meta.favicon.clone().filter(|u| cached.contains_key(u)),
                cached,
                facts: json!({"price": new, "was": old}),
            };
            (w, Some(new), Some(post))
        })
    })
    .await
    .unwrap_or_default();
    for (w, _new, post) in checked {
        if stopped(state, cancel).await {
            return;
        }
        let uid = user_id.to_string();
        let push = post.as_ref().map(|p| p.body.clone());
        // The lower reference price and the alert post commit together, so
        // an interruption never loses the drop.
        let ok = state
            .db
            .call(move |c| {
                crate::db::write_tx(c, |c| {
                    // Unliked or deleted while the page was being read:
                    // nothing to alert about any more.
                    if db::get_setting(c, "paused")?.is_some() || !store::watch_active(c, &w.id)? {
                        return Ok(false);
                    }
                    let drop_to = post
                        .as_ref()
                        .and_then(|p| p.facts["price"]["amount"].as_f64());
                    store::checked_watch(c, &w.id, drop_to)?;
                    if let Some(p) = &post {
                        entities::insert_pipeline_post(c, &uid, p)?;
                    }
                    Ok(true)
                })
            })
            .await
            .unwrap_or(false);
        if let (true, Some(text)) = (ok, push) {
            state.hub.to_users(
                &[user_id.to_string()],
                json!({"type": "entities.changed", "kinds": ["feed"]}),
            );
            push_urgent(state, user_id, "Price drop", &text).await;
        }
    }
}

/// Download up to 3 photos and the icon into the cache (robots.txt, public
/// addresses, photo checks); returns original URL -> cached file.
fn cache_assets(
    f: &Fetcher,
    data_dir: &std::path::Path,
    images: &[String],
    icon: Option<&str>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for u in images {
        if map.len() >= 3 {
            break;
        }
        if let Ok(name) = images::cache(f, data_dir, u, false) {
            map.insert(u.clone(), name);
        }
    }
    if let Some(icon) = icon
        && let Ok(name) = images::cache(f, data_dir, icon, true)
    {
        map.insert(icon.to_string(), name);
    }
    map
}

/// Keep a record of the run in the hidden feed job conversation, so the
/// activity log shows what happened and which engine wrote the text.
async fn record_run(
    state: &AppState,
    user_id: &str,
    report: &RunReport,
    calls: &[(OneShotReply, &'static str)],
) {
    let uid = user_id.to_string();
    let mut activity = Vec::new();
    let mut n = 0;
    let mut act = |name: &str, summary: String| {
        n += 1;
        json!({"id": format!("feed-{n}"), "kind": "mcp", "name": name, "status": "completed", "summary": summary})
    };
    if calls.iter().any(|(_, k)| *k == "interests") {
        activity.push(act("iris/feed_interests", "Updated your topics".into()));
    }
    activity.push(act(
        "iris/feed_sources",
        format!(
            "{} sources, {} new items",
            report.sources_checked, report.items_found
        ),
    ));
    activity.push(act(
        "iris/feed_publish",
        format!("{} posts", report.published),
    ));
    let engine = calls.last().map(|(r, _)| r.clone());
    let content = format!(
        "Published {} posts from {} sources ({} summarized).",
        report.published, report.sources_checked, report.summarized
    );
    let meta = json!({
        "job": "feed",
        "activity": activity,
        "feed_run": report,
        "routing": {"calls": calls.iter().map(|(r, k)| json!({"step": k, "model": r.model})).collect::<Vec<_>>()},
    });
    let _ = state
        .db
        .call(move |c| {
            let conv = db::ensure_job_conversation(c, &uid, "feed")?;
            c.execute(
                "DELETE FROM messages WHERE conversation_id = ?1 AND id NOT IN (
                     SELECT id FROM messages WHERE conversation_id = ?1
                     ORDER BY created_at DESC LIMIT 20)",
                [&conv],
            )?;
            db::insert_message(
                c,
                &NewMessage {
                    conversation_id: &conv,
                    role: "assistant",
                    content: &content,
                    harness: engine.as_ref().map(|e| e.harness.as_str()),
                    model: engine.as_ref().map(|e| e.model.as_str()),
                    meta: Some(&meta),
                    ..Default::default()
                },
            )
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An AppState whose only engine is a fake `claude` script that counts
    /// its runs and answers according to `<dir>/mode` (`ok`, `auth`,
    /// anything else fails) with the reply in `<dir>/reply`.
    async fn fake_engine() -> (AppState, tempfile::TempDir, String) {
        fake_engines(false).await
    }

    /// Same, plus (with `codex`) a fake `codex` ranked after Claude whose
    /// mode is `<dir>/cmode` and which counts runs in `<dir>/ccalls`.
    async fn fake_engines(codex: bool) -> (AppState, tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            r#"#!/bin/sh
D="$(dirname "$0")"
case "$1" in
  --version) echo "9.9.9 (fake)"; exit 0;;
  auth) echo '{"loggedIn": true}'; exit 0;;
esac
cat >/dev/null
echo run >> "$D/calls"
pwd > "$D/cwd"
printf '%s\n' "$@" > "$D/args"
case "$(cat "$D/mode")" in
  slow) sleep 60;;
  ok) printf '{"type":"result","subtype":"success","is_error":false,"result":%s}\n' "$(cat "$D/reply")";;
  auth) echo '{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}';;
  *) echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}';;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let codex_script = dir.join("codex");
        std::fs::write(
            &codex_script,
            r#"#!/bin/sh
D="$(dirname "$0")"
case "$1" in
  --version) echo "codex-cli 9.9.9"; exit 0;;
  login) echo "Logged in using ChatGPT"; exit 0;;
  mcp) echo "[]"; exit 0;;
esac
cat >/dev/null
echo run >> "$D/ccalls"
echo '{"type":"thread.started","thread_id":"t1"}'
case "$(cat "$D/cmode")" in
  ok) echo '{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"[{\"id\": \"p0\", \"summary\": \"From Codex\"}]"}}'
      echo '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}';;
  *) echo '{"type":"turn.failed","error":{"message":"boom"}}';;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&codex_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = crate::config::Paths {
            config_file: dir.join("config.toml"),
            data_dir: dir.join("data"),
        };
        crate::persona::seed(&paths).unwrap();
        let db = crate::db::Db::open(&paths.db_file()).unwrap();
        let mut cfg = crate::config::Config::default();
        for (name, h) in cfg.harness.iter_mut() {
            h.enabled = name == "claude" || (codex && name == "codex");
        }
        cfg.harness.get_mut("claude").unwrap().command = script.to_string_lossy().into_owned();
        let cx = cfg.harness.get_mut("codex").unwrap();
        cx.command = codex_script.to_string_lossy().into_owned();
        // Only the configured models (not the real machine's Codex cache).
        cx.discover_models = false;
        cfg.feed.enabled = false;
        cfg.push.enabled = false;
        let state = AppState::new(cfg, paths, db).unwrap();
        state.refresh_harnesses().await;
        let owner = state.db.with(db::owner_user_id).unwrap().unwrap();
        (state, tmp, owner)
    }

    fn set(tmp: &tempfile::TempDir, mode: &str, reply: &str) {
        std::fs::write(tmp.path().join("mode"), mode).unwrap();
        std::fs::write(
            tmp.path().join("reply"),
            serde_json::to_string(reply).unwrap(),
        )
        .unwrap();
    }

    fn runs(tmp: &tempfile::TempDir) -> usize {
        std::fs::read_to_string(tmp.path().join("calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn feed_state(state: &AppState, owner: &str) -> FeedState {
        state.db.with(|c| store::get_state(c, owner)).unwrap()
    }

    fn codex_runs(tmp: &tempfile::TempDir) -> usize {
        std::fs::read_to_string(tmp.path().join("ccalls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn feed_helpers_use_scoped_workdirs_and_disable_project_context() {
        let (state, tmp, _) = fake_engine().await;
        set(&tmp, "ok", "A summary");
        let mut dirs = Vec::new();
        for scope in ["feed-interests", "feed-summaries"] {
            let reply = one_shot(
                &state,
                OneShotRequest {
                    slot: Slot::Feed,
                    shared: false,
                    system_prompt: "Summarize this story.",
                    prompt: "A comet",
                    timeout: Duration::from_secs(5),
                    scope,
                    max_attempts: 1,
                    take_slot: true,
                },
            )
            .await
            .unwrap();
            assert_eq!(reply.text, "A summary");
            let cwd = std::fs::read_to_string(tmp.path().join("cwd")).unwrap();
            let dir = state.paths.workspace_dir().join(scope);
            assert_eq!(cwd.trim(), dir.to_str().unwrap());
            dirs.push(dir);
            let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
            assert!(args.contains("--tools\n\n--strict-mcp-config"));
            assert!(args.contains("--setting-sources\n\n"));
            assert!(args.contains("--disallowedTools"));
            assert!(args.contains(&format!("Read(/{}/**)", state.paths.data_dir.display())));
        }
        assert_ne!(dirs[0], dirs[1]);
        assert!(
            std::fs::read_dir(state.paths.run_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn paused_feed_and_helpers_do_not_start_and_running_helpers_stop() {
        let (state, tmp, owner) = fake_engine().await;
        set(&tmp, "slow", "");
        state
            .db
            .with(|c| db::set_setting(c, "paused", "1"))
            .unwrap();
        assert!(run(&state, &owner, true).await.unwrap().is_none());
        let request = || OneShotRequest {
            slot: Slot::Feed,
            shared: false,
            system_prompt: "Summarize this story.",
            prompt: "A comet",
            timeout: Duration::from_secs(60),
            scope: "feed-summaries",
            max_attempts: 1,
            take_slot: true,
        };
        assert!(one_shot(&state, request()).await.is_err());
        assert_eq!(runs(&tmp), 0);
        assert!(state.running.lock().unwrap().is_empty());
        state.db.with(|c| db::delete_setting(c, "paused")).unwrap();

        let st = state.clone();
        let task = tokio::spawn(async move { one_shot(&st, request()).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while runs(&tmp) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        state
            .db
            .with(|c| db::set_setting(c, "paused", "2"))
            .unwrap();
        let ids: Vec<String> = state.running.lock().unwrap().keys().cloned().collect();
        assert_eq!(ids.len(), 1);
        for id in ids {
            assert!(crate::turn::cancel(&state, &id));
        }
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert!(state.running.lock().unwrap().is_empty());
        assert!(
            std::fs::read_dir(state.paths.run_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_failed_call_never_falls_through_to_another_engine() {
        let (state, tmp, owner) = fake_engines(true).await;
        let need = vec![model::SummaryInput {
            id: "p0".into(),
            title: "A comet".into(),
            source: "Sky".into(),
            topic: None,
            text: "Short".into(),
        }];
        std::fs::write(tmp.path().join("cmode"), "ok").unwrap();

        // Claude fails: that was the day's call; Codex never runs.
        set(&tmp, "fail", "");
        let (b, _) = write_blurbs(&state, &owner, "2026-10-11", &need, &[]).await;
        assert!(b.is_empty());
        assert_eq!((runs(&tmp), codex_runs(&tmp)), (1, 0));
        assert_eq!(
            feed_state(&state, &owner).summary_call_day.as_deref(),
            Some("2026-10-11")
        );

        // Claude is signed out: no model ran there, so Codex makes the one
        // real call.
        set(&tmp, "auth", "");
        let (b, reply) = write_blurbs(&state, &owner, "2026-10-12", &need, &[]).await;
        assert_eq!(b["p0"].summary.as_deref(), Some("From Codex"));
        assert_eq!(reply.unwrap().harness, "codex");
        assert_eq!((runs(&tmp), codex_runs(&tmp)), (2, 1));
        assert_eq!(
            feed_state(&state, &owner).summary_call_day.as_deref(),
            Some("2026-10-12")
        );
    }

    #[tokio::test]
    async fn daily_calls_are_counted_through_the_real_helper_path() {
        let (state, tmp, owner) = fake_engine().await;
        let need = vec![model::SummaryInput {
            id: "p0".into(),
            title: "A comet".into(),
            source: "Sky".into(),
            topic: None,
            text: "Short".into(),
        }];
        let topics = vec!["Space".to_string()];

        // A failed summary call uses up the day: no second run.
        set(&tmp, "fail", "");
        let (b, reply) = write_blurbs(&state, &owner, "2026-10-11", &need, &topics).await;
        assert!(b.is_empty() && reply.is_none());
        assert_eq!(runs(&tmp), 1);
        assert_eq!(
            feed_state(&state, &owner).summary_call_day.as_deref(),
            Some("2026-10-11")
        );
        set(&tmp, "ok", r#"[{"id": "p0", "summary": "S"}]"#);
        let (b, _) = write_blurbs(&state, &owner, "2026-10-11", &need, &topics).await;
        assert!(b.is_empty());
        assert_eq!(runs(&tmp), 1, "same day: no second call");

        // Next day it works, once.
        let (b, reply) = write_blurbs(&state, &owner, "2026-10-12", &need, &topics).await;
        assert_eq!(b["p0"].summary.as_deref(), Some("S"));
        assert!(reply.is_some());
        assert_eq!(runs(&tmp), 2);

        // An unusable interests reply also uses up the day, and the derived
        // fallback is kept.
        set(&tmp, "ok", "sorry, no JSON today");
        let (i, reply) = refresh_interests(&state, &owner, "2026-10-12", Vec::new())
            .await
            .unwrap();
        assert!(reply.is_none() && !i.by_model);
        assert_eq!(runs(&tmp), 3);
        let st = feed_state(&state, &owner);
        assert!(st.interests.is_some());
        assert!(!interests_due(&st, "2026-10-12"));
        let _ = refresh_interests(&state, &owner, "2026-10-12", Vec::new())
            .await
            .unwrap();
        assert_eq!(runs(&tmp), 3, "reserved: no second call");

        // A signed-out CLI is one run, gives the reservation back, and is
        // marked signed out (so nothing else runs today).
        set(&tmp, "auth", "");
        let (_, reply) = write_blurbs(&state, &owner, "2026-10-13", &need, &topics).await;
        assert!(reply.is_none());
        assert_eq!(runs(&tmp), 4);
        assert_eq!(feed_state(&state, &owner).summary_call_day, None);
        let (_, reply) = write_blurbs(&state, &owner, "2026-10-13", &need, &topics).await;
        assert!(reply.is_none());
        assert_eq!(runs(&tmp), 4, "no usable engine left");
    }

    fn day(published: i64, opened: i64, reached_end: i64) -> DayStat {
        DayStat {
            day: String::new(),
            published,
            opened,
            reached_end,
        }
    }

    #[test]
    fn adaptive_size() {
        // Not enough history: unchanged.
        assert_eq!(next_size(10, &[day(10, 1, 1), day(10, 1, 1)]), 10);
        // Reached the end most days: grow by a quarter (at least 2).
        let busy = vec![day(10, 1, 1); 5];
        assert_eq!(next_size(10, &busy), 12);
        assert_eq!(next_size(28, &busy), 30, "capped at 30");
        let mut s = 10;
        for _ in 0..10 {
            s = next_size(s, &busy);
        }
        assert_eq!(s, 30);
        // Opened but rarely finished: unchanged.
        let browsing = vec![day(10, 1, 0); 5];
        assert_eq!(next_size(10, &browsing), 10);
        // Rarely opened: shrink, never below 5.
        let idle = vec![
            day(10, 0, 0),
            day(10, 0, 0),
            day(10, 1, 0),
            day(10, 0, 0),
            day(10, 0, 0),
        ];
        assert_eq!(next_size(10, &idle), 8);
        assert_eq!(next_size(6, &idle), 5);
        // Days without a batch are ignored.
        let gaps = vec![day(0, 1, 1), day(0, 1, 1), day(10, 1, 1)];
        assert_eq!(next_size(10, &gaps), 10);
    }

    #[test]
    fn pause_after_two_weeks_unused() {
        let now = 2_000_000_000_000;
        let mut st = FeedState::default();
        assert!(!should_pause(&st, now), "never published");
        st.first_batch_at = Some(now - 13 * DAY_MS);
        assert!(!should_pause(&st, now));
        st.first_batch_at = Some(now - 20 * DAY_MS);
        assert!(
            should_pause(&st, now),
            "published for 20 days, never opened"
        );
        st.last_open_at = Some(now - 3 * DAY_MS);
        assert!(!should_pause(&st, now));
        st.last_open_at = Some(now - 15 * DAY_MS);
        assert!(should_pause(&st, now));
        st.resumed_at = Some(now - DAY_MS);
        assert!(!should_pause(&st, now), "just resumed");
        st.paused = true;
        assert!(!should_pause(&st, now), "already paused");
    }

    #[test]
    fn model_calls_at_most_once_a_day() {
        let mut st = FeedState::default();
        assert!(interests_due(&st, "2026-10-11") && summaries_due(&st, "2026-10-11"));
        // A failed (or unusable) call still uses up the day.
        st.interests_call_day = Some("2026-10-11".into());
        st.summary_call_day = Some("2026-10-11".into());
        assert!(!interests_due(&st, "2026-10-11") && !summaries_due(&st, "2026-10-11"));
        assert!(interests_due(&st, "2026-10-12") && summaries_due(&st, "2026-10-12"));
        // Interests the model wrote today are kept even without the marker.
        st.interests_call_day = None;
        st.interests = Some(Interests {
            by_model: true,
            generated_day: "2026-10-12".into(),
            ..Default::default()
        });
        assert!(!interests_due(&st, "2026-10-12"));
    }

    #[test]
    fn source_plan_rotates_and_limits_reddit() {
        let src = |id: &str, kind: &str, origin: &str, fetched: Option<i64>| store::Source {
            id: id.into(),
            kind: kind.into(),
            value: id.into(),
            label: None,
            topic: None,
            origin: origin.into(),
            enabled: true,
            title: None,
            state: Value::Null,
            last_fetch_at: fetched,
            last_ok_at: fetched,
            last_error: None,
            fail_count: 0,
            created_at: 0,
        };
        let mut all = vec![src("new-suggested", "rss", "suggested", None)];
        for i in 0..45 {
            all.push(src(&format!("u{i}"), "rss", "user", Some(1000 + i)));
        }
        for i in 0..6 {
            all.push(src(&format!("r{i}"), "reddit", "suggested", Some(i)));
        }
        let plan = plan_sources(all);
        assert_eq!(plan.len(), MAX_SOURCES_PER_RUN);
        assert_eq!(plan[0].id, "u0", "least recently read first");
        assert!(
            !plan.iter().any(|s| s.id == "u44"),
            "most recent waits for the next run"
        );
        let plan2 = plan_sources(
            (0..6)
                .map(|i| src(&format!("r{i}"), "reddit", "user", Some(i)))
                .chain([src("x", "rss", "suggested", None)])
                .collect(),
        );
        assert_eq!(
            plan2.iter().filter(|s| s.kind == "reddit").count(),
            MAX_REDDIT_PER_RUN
        );
        assert_eq!(plan2.last().unwrap().id, "x");
    }

    #[test]
    fn clean_caps_and_filters() {
        let now = 2_000_000_000_000;
        let item = |src: &str, title: &str, age_days: i64, lang: Option<&str>| Item {
            url: format!("https://{src}.example/{}", title.replace(' ', "-")),
            title: title.into(),
            source_id: Some(src.into()),
            source_name: src.into(),
            published_ms: Some(now - age_days * DAY_MS),
            language: lang.map(String::from),
            ..Default::default()
        };
        let mut items: Vec<Item> = (0..60)
            .map(|i| item("big", &format!("Story number {i}"), 1, None))
            .collect();
        items.push(item("a", "An old story here", 9, None));
        items.push(item("a", "Weekly Questions Thread", 1, None));
        items.push(item("a", "Une histoire en français", 1, Some("fr")));
        items.push(item("a", "A fresh story here", 1, Some("en")));
        let out = clean(items, now, "en");
        assert_eq!(
            out.iter().filter(|i| i.source_name == "big").count(),
            MAX_ITEMS_PER_SOURCE
        );
        let a: Vec<&str> = out
            .iter()
            .filter(|i| i.source_name == "a")
            .map(|i| i.title.as_str())
            .collect();
        assert_eq!(a, vec!["A fresh story here"]);
    }

    #[test]
    fn shortlist_leaves_room_for_other_sources() {
        let it = |src: &str, i: usize| Item {
            title: format!("{src} {i}"),
            source_name: src.into(),
            ..Default::default()
        };
        let mut scored: Vec<(f64, Item)> = (0..40).map(|i| (1.0, it("big", i))).collect();
        scored.extend((0..10).map(|i| (0.5, it(&format!("s{i}"), i))));
        let out = shortlist(scored, 30);
        assert_eq!(out.iter().filter(|i| i.source_name == "big").count(), 6);
        assert_eq!(out.len(), 16);
    }

    #[tokio::test]
    async fn suggestions_on_arbitrary_hosts_wait_for_the_owner() {
        for (kind, auto) in [
            ("rss", false),
            ("mastodon", false),
            ("bluesky", false),
            ("reddit", true),
            ("youtube", true),
            ("hn", true),
        ] {
            assert_eq!(auto_enable(kind), auto, "{kind}");
        }
        // Through the real interests call: a reply steered by an injected
        // title cannot make Iris contact a host of the attacker's choosing.
        let (state, tmp, owner) = fake_engine().await;
        set(
            &tmp,
            "ok",
            r#"{"topics": [{"name": "Space", "keywords": ["nasa"]}],
                "sources": [{"type": "rss", "url": "https://evil.example/leak?d=secret"},
                            {"type": "mastodon", "account": "x@evil.example"},
                            {"type": "reddit", "name": "space"}]}"#,
        );
        let (i, _) = refresh_interests(&state, &owner, "2026-10-11", Vec::new())
            .await
            .unwrap();
        assert!(i.by_model);
        let all = state.db.with(|c| store::list_sources(c, &owner)).unwrap();
        assert_eq!(all.len(), 3);
        let plan = plan_sources(all.into_iter().filter(|s| s.enabled).collect());
        let kinds: Vec<&str> = plan.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, vec!["reddit"]);
    }

    #[test]
    fn activity_counts_for_the_batch_being_read() {
        let now = 2_000_000_000_000;
        let monday = now - 26 * 3_600_000;
        // Reading Monday's batch on Tuesday counts for Monday.
        assert_eq!(
            activity_day(Some(monday), now),
            crate::jobs::local_day(monday)
        );
        assert_eq!(activity_day(None, now), crate::jobs::local_day(now));
        assert_eq!(
            activity_day(Some(now + DAY_MS), now),
            crate::jobs::local_day(now),
            "future"
        );
        assert_eq!(
            activity_day(Some(now - 30 * DAY_MS), now),
            crate::jobs::local_day(now),
            "stale"
        );
        // With batches on alternate days read the morning after, the size
        // does not shrink.
        let days: Vec<DayStat> = (0..6)
            .map(|i| DayStat {
                day: String::new(),
                published: if i % 2 == 0 { 10 } else { 0 },
                opened: if i % 2 == 0 { 1 } else { 0 },
                reached_end: if i % 2 == 0 { 1 } else { 0 },
            })
            .collect();
        assert!(next_size(10, &days) >= 10);
    }

    #[test]
    fn publication_is_atomic_ordered_and_skips_late_duplicates() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let post = |title: &str, url: &str| PipelinePost {
            title: title.into(),
            body: "b".into(),
            source_url: url.into(),
            source_name: "S".into(),
            source_kind: "rss".into(),
            url_key: rank::url_key(url),
            seen_keys: vec![rank::url_key(url), rank::title_key(title)],
            ..Default::default()
        };
        db.with(|c| {
            let u = db::owner_user_id(c)?.unwrap();
            let u = u.as_str();
            // Nothing new: the day stays open.
            let out = publish_batch(c, u, &[], &[], "2026-10-11", RunReport::default())?;
            assert!(out.is_empty());
            assert_eq!(store::get_state(c, u)?.last_batch_day, None);

            // The agent posted one of these stories meanwhile.
            entities::create_feed_post(
                c,
                u,
                &entities::NewFeedPost {
                    title: "Agent story".into(),
                    body: "b".into(),
                    source_url: Some("https://x.example/dup".into()),
                    ..Default::default()
                },
            )?;
            let batch = vec![
                post("Best story of the day", "https://x.example/1"),
                post("Different title, same link", "https://x.example/dup"),
                post("Third story of the day", "https://x.example/3"),
            ];
            let keys: Vec<String> = batch.iter().flat_map(|p| p.seen_keys.clone()).collect();
            let out = publish_batch(c, u, &batch, &keys, "2026-10-11", RunReport::default())?;
            assert_eq!(out, vec!["Best story of the day", "Third story of the day"]);
            let feed = entities::list_feed(c, u, None, 10)?;
            assert_eq!(feed[0].title, "Best story of the day", "best is newest");
            let st = store::get_state(c, u)?;
            let end = feed
                .iter()
                .find(|p| p.title == "Third story of the day")
                .unwrap();
            assert_eq!(st.last_batch_end_id.as_deref(), Some(end.id.as_str()));
            assert_eq!(st.last_batch_at, Some(end.created_at));
            assert_eq!(
                (st.last_batch_day.as_deref(), st.last_batch_count),
                (Some("2026-10-11"), 2)
            );

            // A failure part-way rolls everything back.
            // (feed_days is written after the posts, seen keys, and state.)
            c.execute("DROP TABLE feed_days", [])?;
            let before = entities::list_feed(c, u, None, 50)?.len();
            let seen_before = store::seen_keys(c, u)?.len();
            let more = vec![post("Fourth story here", "https://x.example/4")];
            assert!(publish_batch(c, u, &more, &[], "2026-10-12", RunReport::default()).is_err());
            assert_eq!(entities::list_feed(c, u, None, 50)?.len(), before);
            assert_eq!(store::seen_keys(c, u)?.len(), seen_before);
            assert_eq!(
                store::get_state(c, u)?.last_batch_day.as_deref(),
                Some("2026-10-11")
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn parallel_keeps_order() {
        let out = parallel((0..50).collect(), 4, |i: i32| i * 2);
        assert_eq!(out, (0..50).map(|i| i * 2).collect::<Vec<_>>());
        assert!(parallel(Vec::<i32>::new(), 4, |i| i).is_empty());
    }

    #[test]
    fn plan_and_fallback_topics() {
        let topics = vec![Topic {
            name: "Space".into(),
            weight: 0.5,
            keywords: vec!["nasa".into(), "rocket launch".into(), "astronomy".into()],
            queries: vec!["space news".into(), "second".into()],
        }];
        let plan = search_plan(&topics);
        assert_eq!(plan[0], ("hn".into(), "space news".into(), "Space".into()));
        assert_eq!(plan[1], ("mastodon".into(), "nasa".into(), "Space".into()));
        let t = topics_from_likes(&[
            ("Comet show tonight".into(), Some("Space".into())),
            ("No topic".into(), None),
        ]);
        assert_eq!(t.len(), 1);
        assert!(t[0].keywords.contains(&"comet".to_string()));
    }
}
