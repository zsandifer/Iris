//! The pipeline's only two model calls, each at most once a day:
//!
//! 1. Interests (background slot): the user's notes, liked posts, and
//!    recent chats become topics, keywords, search queries, and source
//!    suggestions.
//! 2. Summaries (feed slot): one batched call writes a hook, summary, and
//!    "why you'd like this" for the day's new posts that need it.
//!
//! Both are plain prompts to the user's own signed-in CLI with no tools.
//! Replies must be JSON; anything else is ignored and the pipeline falls
//! back to rule-based text.

use super::rank::Topic;
use serde_json::Value;

pub const INTERESTS_SYSTEM: &str = "You help a personal assistant pick reading material for its user. \
You never use tools. You reply with one JSON object and nothing else.";

pub const SUMMARY_SYSTEM: &str = "You write short, friendly feed blurbs for one person. Use only the \
facts in the text you are given; never invent details. You never use tools. You reply with one JSON \
array and nothing else.";

/// Everything the interests call sees.
#[derive(Debug, Default)]
pub struct InterestsInput {
    pub user_notes: String,
    pub feed_instructions: String,
    pub liked: Vec<String>,
    pub recent_chats: Vec<String>,
    pub youtube_likes: Vec<String>,
    pub current_topics: Vec<String>,
    pub current_sources: Vec<String>,
}

fn list(items: &[String], empty: &str) -> String {
    if items.is_empty() {
        empty.to_string()
    } else {
        items
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn interests_prompt(i: &InterestsInput) -> String {
    format!(
        "Work out what this person would enjoy in a small daily feed (about 10 posts).\n\n\
<user_notes>\n{}\n</user_notes>\n\n<feed_instructions>\n{}\n</feed_instructions>\n\n\
Feed posts they liked recently:\n{}\n\nThings they said in recent chats:\n{}\n\n\
YouTube videos they liked:\n{}\n\nCurrent topics: {}\nSources already followed:\n{}\n\n\
Reply with JSON exactly in this shape:\n\
{{\"topics\": [{{\"name\": \"1-3 word topic\", \"weight\": 0.0-1.0, \"keywords\": [\"lowercase words or short phrases that show a headline is about this topic\"], \
\"queries\": [\"a short web search query for fresh news on this topic\"]}}],\n \
\"sources\": [{{\"type\": \"rss\", \"url\": \"https://... (a real RSS/Atom feed URL)\", \"topic\": \"...\"}}, \
{{\"type\": \"reddit\", \"name\": \"subreddit\", \"topic\": \"...\"}}, {{\"type\": \"youtube\", \"channel\": \"@handle\", \"topic\": \"...\"}}, \
{{\"type\": \"bluesky\", \"handle\": \"name.bsky.social\", \"topic\": \"...\"}}, {{\"type\": \"mastodon\", \"account\": \"user@instance\", \"topic\": \"...\"}}],\n \
\"location\": \"their home city if the notes say it, else null\", \"language\": \"ISO 639-1 code of the language they write in\"}}\n\n\
Rules: 3 to 8 topics, specific rather than broad (\"trail running\", not \"sports\"); 4 to 10 keywords and 1 to 2 queries \
per topic. Suggest at most 8 new sources, only ones you are confident exist and are active, preferring original creators \
and publications' official feeds; skip anything already followed. Nothing adult, hateful, or gambling-related. \
Weight 1.0 means a core interest, 0.3 a passing one.",
        if i.user_notes.trim().is_empty() {
            "(none yet)"
        } else {
            i.user_notes.trim()
        },
        if i.feed_instructions.trim().is_empty() {
            "(none)"
        } else {
            i.feed_instructions.trim()
        },
        list(&i.liked, "(none yet)"),
        list(&i.recent_chats, "(none)"),
        list(&i.youtube_likes, "(not connected)"),
        if i.current_topics.is_empty() {
            "(none)".to_string()
        } else {
            i.current_topics.join(", ")
        },
        list(&i.current_sources, "(none)"),
    )
}

/// The first JSON value of the wanted kind (`{` or `[`) in a reply that may
/// wrap it in prose or a code fence.
pub fn find_json(reply: &str, open: char) -> Option<Value> {
    let close = if open == '{' { '}' } else { ']' };
    let start = reply.find(open)?;
    let mut end = reply.rfind(close)?;
    while end > start {
        if let Ok(v) = serde_json::from_str::<Value>(&reply[start..=end]) {
            return Some(v);
        }
        end = reply[..end].rfind(close)?;
    }
    None
}

fn clean(s: &str, max: usize) -> String {
    let t = super::markup::collapse_ws(s.trim());
    t.chars().take(max).collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// What the interests call returned.
#[derive(Debug, Default)]
pub struct ParsedInterests {
    pub topics: Vec<Topic>,
    pub sources: Vec<Value>,
    pub location: Option<String>,
    pub language: Option<String>,
}

pub fn parse_interests(reply: &str) -> Option<ParsedInterests> {
    let v = find_json(reply, '{')?;
    let mut topics = Vec::new();
    for t in v["topics"].as_array()?.iter().take(8) {
        let name = capitalize(&clean(t["name"].as_str().unwrap_or(""), 40));
        if name.is_empty()
            || topics
                .iter()
                .any(|x: &Topic| x.name.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        let strings = |k: &str, n: usize, max: usize| -> Vec<String> {
            t[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str())
                .map(|x| clean(x, max))
                .filter(|x| !x.is_empty())
                .take(n)
                .collect()
        };
        topics.push(Topic {
            name,
            weight: t["weight"].as_f64().unwrap_or(0.5).clamp(0.1, 1.0),
            keywords: strings("keywords", 12, 40)
                .into_iter()
                .map(|k| k.to_lowercase())
                .collect(),
            queries: strings("queries", 2, 80),
        });
    }
    if topics.is_empty() {
        return None;
    }
    let sources = v["sources"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(8)
        .collect();
    let location = v["location"]
        .as_str()
        .map(|l| clean(l, 80))
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("null"));
    let language = v["language"]
        .as_str()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| l.len() == 2 && l.chars().all(|c| c.is_ascii_lowercase()));
    Some(ParsedInterests {
        topics,
        sources,
        location,
        language,
    })
}

/// One post for the summary call.
#[derive(Debug, Clone)]
pub struct SummaryInput {
    pub id: String,
    pub title: String,
    pub source: String,
    pub topic: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Blurb {
    pub hook: Option<String>,
    pub summary: Option<String>,
    pub why: Option<String>,
}

pub fn summary_prompt(items: &[SummaryInput], interests: &[String], liked: &[String]) -> String {
    let lines: Vec<String> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "title": i.title,
                "source": i.source,
                "topic": i.topic,
                "text": crate::util::truncate_chars(&i.text, 700),
            })
            .to_string()
        })
        .collect();
    format!(
        "Write feed blurbs for these {} posts.\n\nThe person is into: {}\nPosts they liked recently: {}\n\n\
Posts (one JSON object per line):\n{}\n\n\
For every post, reply with an object in one JSON array:\n\
[{{\"id\": \"...\", \"hook\": \"a catchy line under 70 characters\", \
\"summary\": \"2 or 3 plain sentences, under 350 characters, saying what it is and why it matters\", \
\"why\": \"one short line, under 110 characters, on why this person in particular would like it, addressed to them as you\"}}]\n\n\
Rules: use only facts from the post; if the text is thin, keep the summary short and say what the link is. \
No hype, no emoji, no hashtags, no quotes around the hook.",
        items.len(),
        if interests.is_empty() {
            "(not known yet)".to_string()
        } else {
            interests.join(", ")
        },
        if liked.is_empty() {
            "(none yet)".to_string()
        } else {
            liked.join("; ")
        },
        lines.join("\n"),
    )
}

/// Parsed blurbs by post id.
pub fn parse_summaries(reply: &str) -> std::collections::HashMap<String, Blurb> {
    let mut out = std::collections::HashMap::new();
    let Some(v) =
        find_json(reply, '[').or_else(|| find_json(reply, '{').map(|o| o["items"].clone()))
    else {
        return out;
    };
    for o in v.as_array().into_iter().flatten() {
        let Some(id) = o["id"].as_str() else {
            continue;
        };
        let field = |k: &str, max: usize| {
            o[k].as_str()
                .map(|s| clean(s.trim_matches('"'), max))
                .filter(|s| !s.is_empty())
        };
        out.insert(
            id.to_string(),
            Blurb {
                hook: field("hook", 90),
                summary: field("summary", 500),
                why: field("why", 160),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interests_reply_parsing() {
        let reply = r#"Sure! ```json
{"topics": [{"name": "Trail running", "weight": 0.9, "keywords": ["Trail", "ultra", "trail running"], "queries": ["trail running news", "ultramarathon results", "extra"]},
            {"name": "trail running", "weight": 0.2},
            {"name": "", "keywords": []},
            {"name": "Home espresso", "keywords": ["espresso", "grinder"]}], "language": "english",
 "sources": [{"type": "reddit", "name": "trailrunning", "topic": "Trail running"}],
 "location": "null"}
```"#;
        let ParsedInterests {
            topics,
            sources,
            location: loc,
            language,
        } = parse_interests(reply).unwrap();
        assert_eq!(language, None);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].keywords, vec!["trail", "ultra", "trail running"]);
        assert_eq!(topics[0].queries.len(), 2);
        assert_eq!(topics[1].weight, 0.5);
        assert_eq!(sources.len(), 1);
        assert_eq!(loc, None);
        assert!(parse_interests("no json here").is_none());
        assert!(parse_interests(r#"{"topics": []}"#).is_none());
    }

    #[test]
    fn summary_reply_parsing() {
        let reply = r#"[{"id": "p1", "hook": "\"Comet tonight\"", "summary": "A comet is visible.", "why": "You like space."},
                        {"id": "p2", "hook": ""}, {"nope": 1}] trailing"#;
        let m = parse_summaries(reply);
        assert_eq!(m["p1"].hook.as_deref(), Some("Comet tonight"));
        assert_eq!(m["p1"].why.as_deref(), Some("You like space."));
        assert_eq!(m["p2"], Blurb::default());
        assert_eq!(m.len(), 2);
        let wrapped = parse_summaries(r#"{"items": [{"id": "a", "summary": "S"}]}"#);
        assert_eq!(wrapped["a"].summary.as_deref(), Some("S"));
        assert!(parse_summaries("sorry").is_empty());
    }

    #[test]
    fn prompts_carry_inputs() {
        let p = interests_prompt(&InterestsInput {
            user_notes: "Likes trail running.".into(),
            liked: vec!["A post".into()],
            ..Default::default()
        });
        assert!(
            p.contains("Likes trail running.")
                && p.contains("- A post")
                && p.contains("(not connected)")
        );
        let s = summary_prompt(
            &[SummaryInput {
                id: "p1".into(),
                title: "T".into(),
                source: "S".into(),
                topic: None,
                text: "X".into(),
            }],
            &["Space".into()],
            &[],
        );
        assert!(s.contains(r#""id":"p1""#) && s.contains("Space"));
    }
}
