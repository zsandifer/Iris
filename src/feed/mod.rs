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
use crate::turn::{OneShotReply, OneShotRequest, one_shot};
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
    let opened = published.iter().filter(|d| d.opened > 0).count();
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
        "topics": st.interests.as_ref().map(|i| i.topics.iter().map(|t| t.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        "sources": sources.iter().filter(|s| s.enabled).count(),
        "last_run": st.last_run,
    })
}

// --------------------------------------------------------------- activity

/// Record that the user opened the feed or reached the end of today's
/// posts.
pub async fn record_activity(state: &AppState, user_id: &str, event: &str) -> Result<()> {
    let (uid, ev) = (user_id.to_string(), event.to_string());
    let day = crate::jobs::local_day(now_ms());
    state
        .db
        .call(move |c| {
            match ev.as_str() {
                "opened" => {
                    store::record_opened(c, &uid, &day)?;
                    store::update_state(c, &uid, |s| s.last_open_at = Some(now_ms()))?;
                }
                _ => store::record_reached_end(c, &uid, &day)?,
            }
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
    if owner.as_deref() == Some(user_id) && state.cfg.feed.enabled {
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
            store::update_state(c, &uid, |s| {
                s.paused = true;
                s.paused_by_user = false;
                s.paused_at = Some(now_ms());
            })?;
            let main = db::ensure_main_conversation(c, &uid)?;
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

// --------------------------------------------------------------- the run

/// Run the pipeline for `user_id`. Scheduled runs publish at most one batch
/// per local day; `manual` (the app's "find something now", or resuming)
/// always runs. Returns None when there was nothing to do.
pub async fn run(state: &AppState, user_id: &str, manual: bool) -> Result<Option<RunReport>> {
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

    // 1. Interests (once a day).
    let interests = match st.interests.clone() {
        Some(i) if i.generated_day == today && i.by_model => i,
        _ => {
            let (i, reply) = refresh_interests(state, user_id, &today, youtube_likes).await?;
            if let Some(r) = reply {
                model_notes.push((r, "interests"));
            }
            i
        }
    };
    let topics = interests.topics.clone();

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
    let mut enabled: Vec<store::Source> = srcs.into_iter().filter(|s| s.enabled).collect();
    // Reddit allows about a request a minute without OAuth: read at most a
    // few subreddits per run, least recently read first.
    enabled.sort_by_key(|s| s.last_fetch_at.unwrap_or(0));
    let mut reddit = 0;
    enabled.retain(|s| {
        reddit += usize::from(s.kind == "reddit");
        s.kind != "reddit" || reddit <= MAX_REDDIT_PER_RUN
    });
    // The user's own first, then the ones that worked most recently.
    enabled.sort_by_key(|s| {
        (
            s.origin != "user",
            std::cmp::Reverse(s.last_ok_at.unwrap_or(0)),
        )
    });
    enabled.truncate(MAX_SOURCES_PER_RUN);
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

    // 3. Clean.
    let min_time = now - MAX_AGE_DAYS * DAY_MS;
    let lang = interests.language.clone().unwrap_or_else(|| "en".into());
    items.retain(|i| {
        i.published_ms
            .is_none_or(|t| t >= min_time && t <= now + DAY_MS)
            && i.language
                .as_deref()
                .is_none_or(|l| l.starts_with(lang.as_str()))
            && i.title.chars().count() >= 8
            && !rank::is_housekeeping(&i.title)
            && markup::split_url(&i.url).is_some()
    });
    let mut items = rank::dedupe(items, &seen);
    rank::assign_topics(&mut items, &topics);

    // 4. Rank, enrich finalists, pick.
    let mut scored: Vec<(f64, Item)> = items
        .into_iter()
        .map(|i| (rank::score(&i, &topics, &engagement, now), i))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate((size as usize * 3).clamp(15, 60));
    let finalists: Vec<Item> = scored.into_iter().map(|(_, i)| i).collect();
    let f = fetcher.clone();
    let enriched =
        tokio::task::spawn_blocking(move || parallel(finalists, WORKERS, |it| enrich(&f, it)))
            .await?;
    let mut enriched = enriched;
    rank::assign_topics(&mut enriched, &topics);
    let rescored: Vec<(f64, Item)> = enriched
        .into_iter()
        .map(|i| (rank::score(&i, &topics, &engagement, now), i))
        .collect();
    let picked = rank::select(rescored, size as usize);

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
    if !need.is_empty() {
        let uid = user_id.to_string();
        let liked: Vec<String> = state
            .db
            .call(move |c| store::liked_posts(c, &uid, 8))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let topic_names: Vec<String> = topics.iter().map(|t| t.name.clone()).collect();
        let prompt = model::summary_prompt(&need, &topic_names, &liked);
        match one_shot(
            state,
            OneShotRequest {
                slot: Slot::Feed,
                system_prompt: model::SUMMARY_SYSTEM,
                prompt: &prompt,
                timeout: Duration::from_secs(300),
                scope: "feed-summaries",
                max_attempts: 2,
                take_slot: true,
            },
        )
        .await
        {
            Ok(reply) => {
                blurbs = model::parse_summaries(&reply.text);
                report.model_used = true;
                model_notes.push((reply, "summaries"));
            }
            Err(e) => crate::warn!("feed: summaries unavailable ({e}); using descriptions"),
        }
    }

    // 6. Cache photos and icons, then publish.
    let f = fetcher.clone();
    let data_dir = state.paths.data_dir.clone();
    let to_cache: Vec<(Vec<String>, Option<String>)> = picked
        .iter()
        .map(|(_, it)| {
            (
                it.images.iter().take(4).cloned().collect(),
                it.favicon.clone(),
            )
        })
        .collect();
    let cached: Vec<BTreeMap<String, String>> = tokio::task::spawn_blocking(move || {
        parallel(to_cache, WORKERS, |(imgs, icon)| {
            let mut map = BTreeMap::new();
            for u in imgs {
                if map.len() >= 3 {
                    break;
                }
                if let Some(name) = images::cache(&f, &data_dir, &u, false) {
                    map.insert(u, name);
                }
            }
            if let Some(icon) = icon
                && let Some(name) = images::cache(&f, &data_dir, &icon, true)
            {
                map.insert(icon, name);
            }
            map
        })
    })
    .await?;

    let n = picked.len();
    let mut posts = Vec::with_capacity(n);
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
        // Photos that could not be cached (robots.txt, errors) are dropped
        // unless none could be: then the originals are shown as-is.
        let images: Vec<String> = if cache.keys().any(|k| it.images.contains(k)) {
            it.images
                .iter()
                .filter(|u| cache.contains_key(*u))
                .cloned()
                .collect()
        } else {
            it.images
                .iter()
                .filter(|u| u.starts_with("https://"))
                .take(3)
                .cloned()
                .collect()
        };
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
        seen_new.push(key.clone());
        seen_new.push(rank::title_key(&it.title));
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
            images,
            favicon_url: it.favicon.clone(),
            cached: cache,
            facts,
            // Highest ranked shows first (newest).
            created_at: now + (n - rank_i) as i64,
        });
    }
    report.published = posts.len() as u32;

    let uid = user_id.to_string();
    let t = today.clone();
    let rep = report.clone().with_stats(fetcher.stats());
    let interests_store = interests.clone();
    state
        .db
        .call(move |c| {
            for p in &posts {
                entities::insert_pipeline_post(c, &uid, p)?;
            }
            store::add_seen(c, &uid, &seen_new)?;
            store::record_published(c, &uid, &t, posts.len() as i64)?;
            store::update_state(c, &uid, |s| {
                s.last_batch_at = Some(now);
                s.last_batch_day = Some(t.clone());
                s.last_batch_count = posts.len() as u32;
                if s.first_batch_at.is_none() && !posts.is_empty() {
                    s.first_batch_at = Some(now);
                }
                s.interests = Some(interests_store);
                s.last_run = Some(rep);
            })?;
            store::prune(c, &uid)?;
            Ok(())
        })
        .await?;
    state.hub.to_users(
        &[user_id.to_string()],
        json!({"type": "entities.changed", "kinds": ["feed"]}),
    );

    // Urgent pushes: events in today's batch, then price drops.
    for (title, ev, topic, followed) in urgent_candidates {
        let affinity = topic
            .as_ref()
            .and_then(|t| topics.iter().find(|x| x.name.eq_ignore_ascii_case(t)))
            .map_or(0.0, |t| rank::topic_weight(t, &engagement));
        let score =
            urgent::event_score(&ev, now, affinity, interests.location.as_deref(), followed);
        if score >= urgent::URGENT_AT {
            let when = crate::util::rfc3339(ev.start_ms / 1000);
            let body = format!("{} · {}", truncate_chars(&title, 90), &when[..10]);
            push_urgent(state, user_id, "Something near you soon", &body).await;
        }
    }
    check_price_watches(state, user_id, &fetcher).await;

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
/// icon, price, event. Robots.txt decides whether the page may be read.
fn enrich(f: &Fetcher, mut it: Item) -> Item {
    if !it.needs_page {
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
    if it.images.is_empty() {
        it.images = m.images;
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
    if it.published_ms.is_none() {
        it.published_ms = m.published_ms;
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
    let reply = one_shot(
        state,
        OneShotRequest {
            slot: Slot::Background,
            system_prompt: model::INTERESTS_SYSTEM,
            prompt: &prompt,
            timeout: Duration::from_secs(180),
            scope: "feed-interests",
            max_attempts: 2,
            take_slot: true,
        },
    )
    .await;
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
        let fallback = st.interests.clone().unwrap_or_else(|| Interests {
            topics: topics_from_likes(&liked),
            location: None,
            generated_at: now_ms(),
            generated_day: String::new(),
            by_model: false,
            language: None,
        });
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
    Ok((
        Interests {
            topics,
            location,
            generated_at: now_ms(),
            generated_day: today.to_string(),
            by_model: true,
            language,
        },
        reply.ok(),
    ))
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
async fn check_price_watches(state: &AppState, user_id: &str, fetcher: &Arc<Fetcher>) {
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
    let checked = tokio::task::spawn_blocking(move || {
        parallel(due, WORKERS, |w| {
            let mut req = Request::new(&w.url, Mode::Crawl, "text/html", PAGE_BYTES);
            req.truncate = true;
            let meta = f
                .get(&req)
                .ok()
                .map(|r| extract::extract(&r.text(), &r.final_url));
            (w, meta)
        })
    })
    .await
    .unwrap_or_default();
    for (w, meta) in checked {
        let old = parse::Price {
            amount: w.price,
            currency: w.currency.clone(),
        };
        let new_price = meta.as_ref().and_then(|m| m.price.clone());
        let score = new_price
            .as_ref()
            .map_or(0, |p| urgent::price_drop_score(&old, p));
        let id = w.id.clone();
        let drop_to = (score >= urgent::URGENT_AT)
            .then(|| new_price.as_ref().map(|p| p.amount))
            .flatten();
        let _ = state
            .db
            .call(move |c| store::checked_watch(c, &id, drop_to))
            .await;
        let (Some(new), true) = (new_price, score >= urgent::URGENT_AT) else {
            continue;
        };
        let text = urgent::price_drop_text(&w.title, &old, &new);
        let meta = meta.unwrap_or_default();
        let uid = user_id.to_string();
        let post = PipelinePost {
            title: format!("Price drop: {}", truncate_chars(&w.title, 200)),
            body: text.clone(),
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
            images: meta.images.iter().take(3).cloned().collect(),
            favicon_url: meta.favicon.clone(),
            cached: BTreeMap::new(),
            facts: json!({"price": new, "was": old}),
            created_at: now_ms(),
        };
        let _ = state
            .db
            .call(move |c| entities::insert_pipeline_post(c, &uid, &post))
            .await;
        state.hub.to_users(
            &[user_id.to_string()],
            json!({"type": "entities.changed", "kinds": ["feed"]}),
        );
        push_urgent(state, user_id, "Price drop", &text).await;
    }
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
