//! `iris demo-seed`: fill a fresh data directory with fake demo content
//! (chats, feed posts with drawn illustrations, ideas, goals, library items,
//! connections, logins) for screenshots and trying the app. Everything is
//! synthetic: no real people, places, or accounts. Refuses to touch a data
//! directory that already has conversations or feed posts.

use crate::config::Paths;
use crate::db::{self, Db, NewMessage};
use crate::entities::{self, NewFeedPost};
use crate::secrets::Vault;
use crate::util::{Result, new_id, now_ms};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

/// Images expected in the assets folder (see scripts/demo/make-images.mjs).
const IMAGES: &[&str] = &[
    "plant.jpg",
    "kindred.jpg",
    "bench-1.jpg",
    "bench-2.jpg",
    "bench-3.jpg",
    "maze-1.jpg",
    "maze-2.jpg",
    "ai.jpg",
    "garden-1.jpg",
    "garden-2.jpg",
    "deal.jpg",
    "trail.jpg",
];

struct Seeder<'a> {
    c: &'a Connection,
    paths: &'a Paths,
    owner: String,
}

impl Seeder<'_> {
    /// Store an image as an attachment owned by the owner; returns its id.
    fn attach(&self, assets: &Path, name: &str) -> Result<String> {
        let bytes = std::fs::read(assets.join(name))
            .map_err(|e| format!("missing demo image {name}: {e}"))?;
        let (mime, ext) =
            crate::server::sniff_image(&bytes).ok_or("demo images must be JPEG/PNG")?;
        let id = new_id();
        let file = format!("{id}.{ext}");
        crate::config::write_private_file(&self.paths.uploads_dir().join(&file), &bytes)?;
        let sha = {
            use sha2::{Digest, Sha256};
            crate::util::hex(&Sha256::digest(&bytes))
        };
        db::insert_attachment(
            self.c,
            &id,
            &self.owner,
            mime,
            bytes.len() as i64,
            &sha,
            &file,
        )?;
        Ok(id)
    }

    /// Insert a message and pin its timestamp.
    fn msg(&self, at: i64, m: NewMessage) -> Result<db::Message> {
        let msg = db::insert_message(self.c, &m)?;
        self.c.execute(
            "UPDATE messages SET created_at = ?2, updated_at = ?2 WHERE id = ?1",
            params![msg.id, at],
        )?;
        Ok(msg)
    }

    fn user(&self, conv: &str, at: i64, author: &str, text: &str, photos: &[String]) -> Result<()> {
        self.msg(
            at,
            NewMessage {
                conversation_id: conv,
                role: "user",
                author_user_id: Some(author),
                content: text,
                attachments: photos,
                ..Default::default()
            },
        )?;
        Ok(())
    }

    fn reply(&self, conv: &str, at: i64, text: &str, activity: Value, secs: i64) -> Result<()> {
        let meta = json!({
            "routing": null,
            "purpose": "chat",
            "activity": activity,
            "duration_ms": secs * 1000,
        });
        self.msg(
            at,
            NewMessage {
                conversation_id: conv,
                role: "assistant",
                content: text,
                meta: Some(&meta),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    fn card(&self, conv: &str, at: i64, card: &Value, role: &str) -> Result<()> {
        let title = card["title"].as_str().unwrap_or("");
        self.msg(
            at,
            NewMessage {
                conversation_id: conv,
                role,
                kind: "card",
                content: title,
                card: Some(card),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    fn conversation(
        &self,
        kind: &str,
        title: &str,
        seed: Option<Value>,
        at: i64,
    ) -> Result<String> {
        let conv = db::create_conversation(self.c, &self.owner, kind, title, seed.as_ref())?;
        self.c.execute(
            "UPDATE conversations SET created_at = ?2, updated_at = ?2 WHERE id = ?1",
            params![conv.id, at],
        )?;
        self.c.execute(
            "UPDATE conversation_members SET joined_at = ?2 WHERE conversation_id = ?1",
            params![conv.id, at],
        )?;
        Ok(conv.id)
    }
}

fn act(id: &str, kind: &str, name: &str, summary: &str) -> Value {
    json!({"id": id, "kind": kind, "name": name, "status": "completed", "summary": summary})
}

/// Local midnight today, in epoch ms.
fn today_midnight() -> i64 {
    let now = now_ms();
    now - (crate::jobs::local_minutes_now() as i64) * MIN - (now % MIN)
}

pub fn seed(paths: &Paths, assets: &Path) -> Result<()> {
    for name in IMAGES {
        if !assets.join(name).is_file() {
            return Err(format!(
                "{} has no {name}; pass --assets scripts/demo/images",
                assets.display()
            )
            .into());
        }
    }
    crate::persona::seed(paths)?;
    let db = Db::open(&paths.db_file())?;
    let vault = Vault::open(&paths.vault_key_file())?;
    db.with(|c| {
        c.query_row(
            "SELECT (SELECT COUNT(*) FROM messages) + (SELECT COUNT(*) FROM feed_posts)",
            [],
            |r| r.get::<_, i64>(0),
        )
    })
    .and_then(|n| {
        if n > 0 {
            Err("this data directory already has chats or feed posts; demo-seed only fills a fresh one".into())
        } else {
            Ok(())
        }
    })?;
    std::fs::create_dir_all(paths.uploads_dir())?;
    std::fs::create_dir_all(paths.library_dir())?;

    let conn_guard = db.clone();
    conn_guard.with(|c| {
        let tx = c.unchecked_transaction()?;
        let r = seed_all(&tx, paths, assets, &vault)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()));
        match r {
            Ok(()) => tx.commit(),
            Err(e) => Err(e),
        }
    })?;
    Ok(())
}

fn seed_all(c: &Connection, paths: &Paths, assets: &Path, vault: &Vault) -> Result<()> {
    let owner = db::owner_user_id(c)?.ok_or("no owner")?;
    db::rename_user(c, &owner, "Alex")?;
    let s = Seeder {
        c,
        paths,
        owner: owner.clone(),
    };
    let now = now_ms();
    let yday = today_midnight() - 24 * HOUR;

    // A second person, for a "Together" chat.
    let sam = new_id();
    c.execute(
        "INSERT INTO users (id, display_name, is_owner, created_at) VALUES (?1, 'Sam', 0, ?2)",
        params![sam, now - 30 * 24 * HOUR],
    )?;
    db::ensure_main_conversation(c, &sam)?;
    // Sam's own Iris (persona and memory files).
    crate::persona::seed_person(&paths.person_dirs(&sam, false))?;

    let img = |n: &str| s.attach(assets, n);
    let url = |id: &str| format!("/api/attachments/{id}");

    // ------------------------------------------------------------ main chat
    let main = db::ensure_main_conversation(c, &owner)?;
    let plant = img("plant.jpg")?;
    let t = yday + 18 * HOUR + 2 * MIN;
    s.user(
        &main,
        t,
        &owner,
        "what is this plant on our porch?",
        std::slice::from_ref(&plant),
    )?;
    s.reply(
        &main,
        t + 9_000,
        "That's a chrysanthemum (a mum). The soil looks dry, so give it a good soak tonight and keep it where it gets morning sun. 🌼",
        json!([]),
        8,
    )?;
    let hb_meta = json!({"initiated_by": "heartbeat", "routing": null});
    s.msg(
        now - 52 * MIN,
        NewMessage {
            conversation_id: &main,
            role: "assistant",
            content: "Morning! The flood watch lifts at 2 today, so the corn maze at Hill Farm is still on for Saturday. Want me to grab tickets for the two of you?",
            meta: Some(&hb_meta),
            ..Default::default()
        },
    )?;
    s.user(
        &main,
        now - 2 * MIN,
        &owner,
        "do I have anything tomorrow before noon?",
        &[],
    )?;

    // ------------------------------------------------- Friday dinner chat
    let dinner = s.conversation("side", "Friday dinner", None, now - 41 * MIN)?;
    s.user(
        &dinner,
        now - 41 * MIN,
        &owner,
        "find us a dinner spot friday, patio, around 7",
        &[],
    )?;
    s.reply(
        &dinner,
        now - 40 * MIN,
        "Kindred has a patio table at 7. Ostra and Bistro Lune have tables too, but indoors only. Want me to book Kindred?",
        json!([
            act("a1", "web_search", "WebSearch", "patio dinner downtown friday"),
            act("a2", "mcp", "tables/find_tables", "Kindred, Ostra"),
            act("a3", "mcp", "tables/find_tables", "Bistro Lune"),
            act("a4", "mcp", "calendar/list_events", "Friday evening"),
        ]),
        41,
    )?;
    let kindred = img("kindred.jpg")?;
    s.card(
        &dinner,
        now - 40 * MIN + 500,
        &json!({"type": "result", "title": "Kindred · 7:00 PM patio",
                "body": "New American · 12 min away · ★ 4.7", "url": "https://tables.example/kindred",
                "image_url": url(&kindred), "action_label": "Book for 2"}),
        "assistant",
    )?;
    s.user(
        &dinner,
        now - 37 * MIN,
        &owner,
        "yes, book it and email Sam the details",
        &[],
    )?;
    s.reply(
        &dinner,
        now - 36 * MIN,
        "Booked: Kindred, Friday at 7:00 PM, patio for two. Here's the email to Sam. It only goes out if you approve it.",
        json!([act("b1", "mcp", "tables/book_table", "Kindred, Fri 7:00 PM, 2 people")]),
        12,
    )?;
    let approval = entities::approval_card(&entities::ApprovalRequest {
        title: "Send email from your Gmail",
        service: "gmail",
        to: "sam@example.com",
        subject: "Dinner Friday 🍝",
        preview: "Booked us a patio table at Kindred, Friday at 7. See you there!",
        action_label: "Send",
        approver_user_id: &owner,
    });
    s.card(&dinner, now - 36 * MIN + 500, &approval, "assistant")?;

    // ----------------------------------------------------------- feed posts
    struct P<'a> {
        title: &'a str,
        body: &'a str,
        topic: &'a str,
        source: &'a str,
        url: &'a str,
        why: &'a str,
        images: &'a [&'a str],
        hours_ago: i64,
        liked: bool,
    }
    let posts = [
        P {
            title: "Hill Farm opens its corn maze Saturday",
            body: "Pick-your-own pumpkins, hayrides, and cider donuts. It's 25 minutes away, and the flood watch lifts Friday afternoon, so the paths should be dry by Saturday morning.",
            topic: "This weekend",
            source: "Hill Farm",
            url: "https://hillfarm.example/corn-maze",
            why: "You saved two fall outings last year",
            images: &["maze-1.jpg", "maze-2.jpg"],
            hours_ago: 2,
            liked: false,
        },
        P {
            title: "A walnut slab bench, start to finish",
            body: "A one-weekend build with photos at every step: a live edge, hairpin legs, and an oil finish you probably already own. The trickiest part is flattening the slab; the post shows a router sled you can make from scrap.",
            topic: "Woodshop",
            source: "Woodshop Weekly",
            url: "https://woodshop-weekly.example/slab-bench",
            why: "Because you liked 2 slab-table posts",
            images: &["bench-1.jpg", "bench-2.jpg", "bench-3.jpg"],
            hours_ago: 3,
            liked: true,
        },
        P {
            title: "Small models keep getting cheaper",
            body: "Prices for fast, small AI models dropped again this week. That makes background work like summaries and check-ins cost even less.",
            topic: "AI this week",
            source: "Tech Digest",
            url: "https://techdigest.example/small-models",
            why: "You asked me about model costs last week",
            images: &["ai.jpg"],
            hours_ago: 6,
            liked: false,
        },
        P {
            title: "Three planters that love a shady porch",
            body: "Ferns, coral bells, and impatiens all do well with only morning sun. Group them in deep pots so they dry out more slowly.",
            topic: "Porch & garden",
            source: "Green Stoop",
            url: "https://greenstoop.example/shade-planters",
            why: "Your porch gets morning sun only",
            images: &["garden-1.jpg", "garden-2.jpg"],
            hours_ago: 8,
            liked: false,
        },
        P {
            title: "The headphones you looked at are 30% off",
            body: "The over-ear pair you compared last month dropped to its lowest price this year. The sale ends Sunday.",
            topic: "Deals you watch",
            source: "Gadget Deals",
            url: "https://gadgetdeals.example/headphones",
            why: "Price drop on something you watched",
            images: &["deal.jpg"],
            hours_ago: 10,
            liked: true,
        },
        P {
            title: "An easy 4-mile loop with a lake view",
            body: "Mostly flat, shaded, and dog friendly, with a picnic spot at the halfway point.",
            topic: "This weekend",
            source: "Trail Notes",
            url: "https://trailnotes.example/lake-loop",
            why: "Short hikes you liked lately",
            images: &["trail.jpg"],
            hours_ago: 14,
            liked: false,
        },
        P {
            title: "When to bring mums inside",
            body: "Potted mums can stay out until the first hard frost. Water them at the base in the morning and pinch off spent blooms to keep them going.",
            topic: "Porch & garden",
            source: "Green Stoop",
            url: "https://greenstoop.example/mums",
            why: "You sent me a photo of your mums",
            images: &["plant.jpg"],
            hours_ago: 20,
            liked: false,
        },
    ];
    let mut bench_post = None;
    for p in &posts {
        let mut urls = Vec::new();
        for name in p.images {
            urls.push(url(&img(name)?));
        }
        // Demo photos are served from Iris itself; real posts link to the
        // source's original images over https.
        let post = entities::create_feed_post(
            c,
            &owner,
            &NewFeedPost {
                title: p.title.into(),
                body: p.body.into(),
                topic: Some(p.topic.into()),
                source_url: Some(p.url.into()),
                image_url: None,
                images: urls,
                source_name: Some(p.source.into()),
                why: Some(p.why.into()),
            },
        )?;
        let at = now - p.hours_ago * HOUR;
        c.execute(
            "UPDATE feed_posts SET created_at = ?2, updated_at = ?2, liked = ?3 WHERE id = ?1",
            params![post.id, at, p.liked],
        )?;
        if p.topic == "Woodshop" {
            bench_post = Some((post.id, p.title, p.body, p.url));
        }
    }

    // A chat about a feed post.
    if let Some((id, title, body, link)) = bench_post {
        let conv = s.conversation(
            "side",
            title,
            Some(json!({"type": "feed_post", "id": id})),
            now - 2 * HOUR,
        )?;
        c.execute(
            "UPDATE feed_posts SET discuss_conversation_id = ?2 WHERE id = ?1",
            params![id, conv],
        )?;
        let card =
            json!({"type": "feed_post", "title": title, "body": body, "url": link, "ref_id": id});
        s.msg(
            now - 2 * HOUR,
            NewMessage {
                conversation_id: &conv,
                role: "system",
                kind: "card",
                content: &format!("Feed post \"{title}\": {body}"),
                card: Some(&card),
                ..Default::default()
            },
        )?;
        s.user(
            &conv,
            now - 2 * HOUR + MIN,
            &owner,
            "could I build this with just a circular saw?",
            &[],
        )?;
        s.reply(
            &conv,
            now - 2 * HOUR + 2 * MIN,
            "Mostly, yes. A circular saw with a straightedge handles the crosscuts. For flattening you'd want a router and the sled from the post, or ask the lumberyard to surface the slab for you (usually a small fee).",
            json!([act("c1", "tool", "WebFetch", "https://woodshop-weekly.example/slab-bench")]),
            9,
        )?;
    }

    // -------------------------------------------------------------- ideas
    entities::create_idea(
        c,
        &owner,
        "Stage this week's meal order",
        "I'll pick from your usual recipes and stop at checkout for your OK.",
        Some(
            "Put together this week's grocery order from my usual recipes and stop before checkout.",
        ),
    )?;
    entities::create_idea(
        c,
        &owner,
        "Find a sitter for Saturday night",
        "You have dinner plans Saturday. I can check with your usual sitter.",
        None,
    )?;
    entities::create_idea(
        c,
        &owner,
        "Plan a fall day at Hill Farm",
        "Maze, pumpkins, and lunch nearby, timed around the weather.",
        None,
    )?;
    let japan = s.conversation(
        "side",
        "Japan trip",
        Some(json!({"type": "idea"})),
        now - 26 * HOUR,
    )?;
    let idea = entities::create_idea(
        c,
        &owner,
        "Draft a Japan itinerary",
        "Ten days, trains between cities, one slow day in the middle.",
        None,
    )?;
    entities::update_idea(c, &owner, &idea.id, Some("started"), Some(&japan))?;
    c.execute(
        "UPDATE conversations SET seed = ?2 WHERE id = ?1",
        params![japan, json!({"type": "idea", "id": idea.id}).to_string()],
    )?;
    s.user(
        &japan,
        now - 26 * HOUR,
        &owner,
        "Draft a Japan itinerary",
        &[],
    )?;
    s.reply(
        &japan,
        now - 26 * HOUR + 2 * MIN,
        "Here's a first draft: Tokyo (4 days), Hakone (1), Kyoto (4), and a slow day in Nara. I saved it to your Library so you can mark it up.",
        json!([
            act("d1", "web_search", "WebSearch", "Japan rail pass 10 days"),
            act("d2", "mcp", "iris/save_to_library", "Japan itinerary (draft)"),
        ]),
        74,
    )?;

    // -------------------------------------------------------------- goals
    entities::create_goal(
        c,
        &owner,
        "Run a 10k by spring",
        "You've run 3 times this month. A 12-week plan would get you there.",
        true,
    )?;
    entities::create_goal(
        c,
        &owner,
        "Save $4,000 for Japan",
        "$2,650 saved · on track for March",
        false,
    )?;
    entities::create_goal(
        c,
        &owner,
        "Read 12 books this year",
        "8 of 12 · halfway through a mystery",
        false,
    )?;
    let done = entities::create_goal(c, &owner, "Fix the porch light", "", false)?;
    entities::update_goal(
        c,
        &owner,
        &done.id,
        &entities::GoalChanges {
            done: Some(true),
            ..Default::default()
        },
    )?;

    // ------------------------------------------------------------ library
    let docs = [
        (
            "Japan itinerary (draft)",
            "Ten days, trains between cities",
            "# Japan, 10 days (draft)\n\n- **Days 1-4: Tokyo.** Markets, a day trip to Kamakura.\n- **Day 5: Hakone.** Onsen and the ropeway.\n- **Days 6-9: Kyoto.** Temples early, food late.\n- **Day 10: Nara.** A slow day with the deer.\n\nRail pass: worth it for this route.\n",
            Some(japan.clone()),
        ),
        (
            "Weekly meal plan",
            "Five dinners, one shopping list",
            "# This week\n\n1. Sheet-pan chicken and squash\n2. Lentil soup\n3. Tacos\n4. Pasta with greens\n5. Leftovers night\n",
            None,
        ),
    ];
    for (title, desc, content, conv) in docs {
        let id = new_id();
        let file = format!("{id}.md");
        crate::config::write_private_file(&paths.library_dir().join(&file), content.as_bytes())?;
        entities::insert_library_item(
            c,
            &owner,
            &entities::NewLibraryItem {
                id: &id,
                kind: "artifact",
                title,
                description: desc,
                mime: "text/markdown",
                file_name: &file,
                size: content.len() as i64,
                conversation_id: conv.as_deref(),
            },
        )?;
    }
    let sketch = std::fs::read(assets.join("garden-2.jpg"))?;
    let id = new_id();
    let file = format!("{id}.jpg");
    crate::config::write_private_file(&paths.library_dir().join(&file), &sketch)?;
    entities::insert_library_item(
        c,
        &owner,
        &entities::NewLibraryItem {
            id: &id,
            kind: "media",
            title: "Porch planter sketch",
            description: "Five pots, shade-friendly",
            mime: "image/jpeg",
            file_name: &file,
            size: sketch.len() as i64,
            conversation_id: None,
        },
    )?;

    // ------------------------------------------------------ together chat
    let us = s.conversation("group", "Us two", None, now - 5 * HOUR)?;
    c.execute(
        "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at) VALUES (?1, ?2, 'member', ?3)",
        params![us, sam, now - 5 * HOUR],
    )?;
    s.user(
        &us,
        now - 5 * HOUR,
        &sam,
        "can you add oat milk and coffee to our list?",
        &[],
    )?;
    s.reply(
        &us,
        now - 5 * HOUR + 20_000,
        "Added both to your shared grocery list.",
        json!([]),
        4,
    )?;
    s.user(
        &us,
        now - 4 * HOUR,
        &owner,
        "thanks! and remind us about the vet on Thursday",
        &[],
    )?;
    s.reply(
        &us,
        now - 4 * HOUR + 15_000,
        "Will do. I'll remind you both Thursday at 9 AM.",
        json!([]),
        5,
    )?;

    // ------------------------------------------------- background activity
    let job = db::ensure_job_conversation(c, &owner, "heartbeat")?;
    let meta = json!({"initiated_by": "heartbeat", "purpose": "scheduled", "duration_ms": 18000,
        "activity": [act("h1", "mcp", "calendar/list_events", "today"), act("h2", "web_search", "WebSearch", "flood watch update")]});
    s.msg(
        now - 53 * MIN,
        NewMessage {
            conversation_id: &job,
            role: "assistant",
            content: "posted to Main chat",
            meta: Some(&meta),
            ..Default::default()
        },
    )?;

    // --------------------------------------------------- connections, wallet
    for (name, desc) in [
        ("gmail", "Reads freely · asks before sending"),
        ("calendar", "Reads · asks before inviting"),
        ("youtube", "Feeds your For you"),
    ] {
        entities::insert_connector(
            c,
            &new_id(),
            name,
            desc,
            "http",
            &entities::ConnectorConfig {
                url: Some(format!("https://mcp.example/{name}")),
                ..Default::default()
            },
            true,
        )?;
    }
    for (kind, label, meta, secret) in [
        (
            "login",
            "Grocery store",
            json!({"site": "grocer.example", "username": "alex@example.com"}),
            json!({"password": "demo-only"}),
        ),
        (
            "login",
            "Restaurant booking",
            json!({"site": "tables.example", "username": "alex@example.com"}),
            json!({"password": "demo-only"}),
        ),
        (
            "login",
            "Library card",
            json!({"site": "library.example", "username": "alex"}),
            json!({"password": "demo-only"}),
        ),
        (
            "card",
            "Everyday card",
            json!({"brand": "visa", "last4": "4242", "exp_month": 8, "exp_year": 2029, "cardholder": "Alex Demo"}),
            json!({"number": "4242424242424242", "cvc": "123"}),
        ),
    ] {
        let id = new_id();
        let ct = vault.encrypt(secret.to_string().as_bytes(), &id)?;
        entities::insert_credential(c, &id, &owner, kind, label, &meta, &ct)?;
    }
    // Conversations sort by their newest message.
    c.execute(
        "UPDATE conversations SET updated_at = COALESCE(
             (SELECT MAX(created_at) FROM messages WHERE conversation_id = conversations.id), created_at)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_a_fresh_data_dir_once() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_file: tmp.path().join("config.toml"),
            data_dir: tmp.path().join("data"),
        };
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/demo/images");
        seed(&paths, &assets).unwrap();
        let db = Db::open(&paths.db_file()).unwrap();
        let (owner, posts, approvals, people) = db
            .with(|c| {
                let owner = db::owner_user_id(c)?.unwrap();
                let posts = entities::list_feed(c, &owner, None, 50)?;
                let approvals: i64 = c.query_row(
                    "SELECT COUNT(*) FROM messages WHERE json_extract(card, '$.type') = 'approval'",
                    [],
                    |r| r.get(0),
                )?;
                let people: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
                Ok((owner, posts, approvals, people))
            })
            .unwrap();
        assert!(
            posts.len() >= 6
                && posts
                    .iter()
                    .all(|p| !p.images.is_empty() && p.why.is_some())
        );
        assert_eq!((approvals, people), (1, 2));
        let activity = db
            .with(|c| db::recent_activity(c, &owner, true, None, 50))
            .unwrap();
        assert!(activity.len() >= 4);
        assert!(
            seed(&paths, &assets).is_err(),
            "never seeds over existing data"
        );
        assert!(seed(&paths, tmp.path()).is_err(), "needs the demo images");
    }
}
