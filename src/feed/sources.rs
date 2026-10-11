//! Fetching each kind of source and turning user input (or a model
//! suggestion) into a source.

use super::http::{Fetcher, Mode, Request};
use super::markup::{self, encode_query};
use super::parse::{self, Item};
use super::store::Source;
use serde_json::{Value, json};

const FEED_ACCEPT: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.5";
const JSON_ACCEPT: &str = "application/json";
const MAX_FEED_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 1536 * 1024;
const DEFAULT_MASTODON: &str = "mastodon.social";

/// What one source fetch produced.
#[derive(Debug, Default)]
pub struct Fetched {
    pub items: Vec<Item>,
    /// New per-source state to store (ETag, resolved ids), if changed.
    pub state: Option<Value>,
}

fn get_json(f: &Fetcher, url: &str, bearer: Option<&str>) -> Result<Value, String> {
    let mut req = Request::new(url, Mode::Feed, JSON_ACCEPT, MAX_JSON_BYTES);
    req.bearer = bearer;
    let res = f.get(&req).map_err(|e| e.to_string())?;
    serde_json::from_slice(&res.body).map_err(|_| "not JSON".to_string())
}

/// Fetch one configured source.
pub fn fetch(f: &Fetcher, src: &Source, now_ms: i64) -> Result<Fetched, String> {
    let mut out = match src.kind.as_str() {
        "rss" => fetch_rss(f, &src.value, &src.state)?,
        "youtube" => fetch_youtube(f, &src.value, &src.state)?,
        "reddit" => fetch_reddit(f, &src.value)?,
        "hn" => Fetched {
            items: hn(f, &src.value, now_ms)?,
            state: None,
        },
        "bluesky" => Fetched {
            items: bluesky(f, &src.value)?,
            state: None,
        },
        "mastodon" => fetch_mastodon(f, &src.value, &src.state)?,
        other => return Err(format!("unknown source kind {other}")),
    };
    for it in &mut out.items {
        it.source_id = Some(src.id.clone());
        if it.topic.is_none() {
            it.topic = src.topic.clone();
        }
        if let Some(label) = src.label.as_ref().filter(|l| !l.trim().is_empty())
            && matches!(src.kind.as_str(), "rss" | "youtube")
        {
            it.source_name = label.clone();
        }
    }
    Ok(out)
}

fn fetch_rss(f: &Fetcher, url: &str, state: &Value) -> Result<Fetched, String> {
    let feed_url = state["feed_url"].as_str().unwrap_or(url).to_string();
    // Until the address has proven to be a feed it may be a web page Iris
    // would be crawling (to find its feed), so robots.txt applies; a
    // confirmed feed is read like a feed reader reads it. Read whole once a
    // day (no conditional request): items not picked today stay candidates
    // tomorrow.
    let mode = if state["confirmed"].as_bool() == Some(true) {
        Mode::Feed
    } else {
        Mode::Crawl
    };
    let req = Request::new(&feed_url, mode, FEED_ACCEPT, MAX_FEED_BYTES);
    let res = f.get(&req).map_err(|e| e.to_string())?;
    let text = res.text();
    let (doc, feed_url) = match parse::parse_feed(&text, &res.final_url) {
        Some(doc) => (doc, res.final_url.clone()),
        None => {
            // A web page: follow its advertised feed once and remember it.
            let link =
                discover_feed(&text, &res.final_url).ok_or("no feed found at that address")?;
            let r2 = f
                .get(&Request::new(
                    &link,
                    Mode::Crawl,
                    FEED_ACCEPT,
                    MAX_FEED_BYTES,
                ))
                .map_err(|e| e.to_string())?;
            let doc = parse::parse_feed(&r2.text(), &r2.final_url).ok_or("not a feed")?;
            (doc, r2.final_url)
        }
    };
    let name = doc
        .title
        .clone()
        .or_else(|| markup::host_of(&feed_url).map(|h| h.trim_start_matches("www.").to_string()))
        .unwrap_or_default();
    let icon = doc.icon.clone();
    let mut items = doc.items;
    for it in &mut items {
        if it.source_name.is_empty() {
            it.source_name = name.clone();
        }
        it.favicon = it.favicon.take().or_else(|| icon.clone());
        if it.source_kind.is_empty() {
            it.source_kind = "rss".into();
        }
    }
    let mut st = json!({"confirmed": true});
    if feed_url != url {
        st["feed_url"] = feed_url.into();
    }
    if !name.is_empty() {
        st["title"] = name.into();
    }
    Ok(Fetched {
        items,
        state: Some(st),
    })
}

/// `<link rel="alternate" type="application/rss+xml" href="...">` in a page.
pub fn discover_feed(html: &str, base: &str) -> Option<String> {
    markup::tokenize(html, true)
        .into_iter()
        .find_map(|t| match t {
            markup::Token::Start { name, attrs, .. } if name == "link" => {
                let rel = markup::attr(&attrs, "rel")
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let ty = markup::attr(&attrs, "type")
                    .unwrap_or("")
                    .to_ascii_lowercase();
                (rel.contains("alternate") && (ty.contains("rss") || ty.contains("atom")))
                    .then(|| markup::attr(&attrs, "href").and_then(|h| markup::resolve(base, h)))
                    .flatten()
            }
            _ => None,
        })
}

fn youtube_channel(f: &Fetcher, value: &str, state: &Value) -> Result<String, String> {
    if let Some(id) = state["channel_id"].as_str() {
        return Ok(id.to_string());
    }
    if parse::valid_channel_id(value) {
        return Ok(value.to_string());
    }
    let page = if value.starts_with('@') {
        format!("https://www.youtube.com/{value}")
    } else {
        value.to_string()
    };
    if let Some(id) = page
        .split("/channel/")
        .nth(1)
        .map(|r| r.chars().take(24).collect::<String>())
        .filter(|id| parse::valid_channel_id(id))
    {
        return Ok(id);
    }
    let mut req = Request::new(&page, Mode::Crawl, "text/html", MAX_PAGE_BYTES);
    req.truncate = true;
    let res = f.get(&req).map_err(|e| e.to_string())?;
    parse::youtube_channel_id_from_page(&res.text())
        .ok_or_else(|| "couldn't find that YouTube channel".into())
}

fn fetch_youtube(f: &Fetcher, value: &str, state: &Value) -> Result<Fetched, String> {
    let channel = youtube_channel(f, value, state)?;
    let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel}");
    let res = f
        .get(&Request::new(&url, Mode::Feed, FEED_ACCEPT, MAX_FEED_BYTES))
        .map_err(|e| e.to_string())?;
    let doc = parse::parse_feed(&res.text(), &url).ok_or("not a feed")?;
    let name = doc.title.clone().unwrap_or_else(|| "YouTube".into());
    let mut items = doc.items;
    for it in &mut items {
        it.source_kind = "youtube".into();
        it.source_name = name.clone();
        it.favicon = Some("https://www.youtube.com/favicon.ico".into());
        it.needs_page = false;
    }
    Ok(Fetched {
        items,
        state: Some(json!({"channel_id": channel, "title": name})),
    })
}

fn fetch_reddit(f: &Fetcher, sub: &str) -> Result<Fetched, String> {
    let label = format!("r/{sub}");
    let json_url = format!("https://www.reddit.com/r/{sub}/top.json?t=day&limit=25&raw_json=1");
    let blocked = &f.reddit_json_blocked;
    let json = if blocked.load(std::sync::atomic::Ordering::Relaxed) {
        Err(String::new())
    } else {
        get_json(f, &json_url, None)
            .inspect_err(|_| blocked.store(true, std::sync::atomic::Ordering::Relaxed))
    };
    let mut items = match json {
        Ok(v) => parse::reddit_items(&v),
        Err(_) => {
            // Reddit often refuses JSON without OAuth; its Atom feed works.
            let rss = format!("https://www.reddit.com/r/{sub}/top/.rss?t=day&limit=25");
            let res = f
                .get(&Request::new(&rss, Mode::Feed, FEED_ACCEPT, MAX_FEED_BYTES))
                .map_err(|e| e.to_string())?;
            parse::reddit_feed_items(&res.text(), &rss, &label)
        }
    };
    for it in &mut items {
        it.source_name = label.clone();
        it.favicon = Some("https://www.reddit.com/favicon.ico".into());
    }
    Ok(Fetched { items, state: None })
}

/// Hacker News: `front_page`, `best` (high scores this week), or a query.
pub fn hn(f: &Fetcher, value: &str, now_ms: i64) -> Result<Vec<Item>, String> {
    let since = now_ms / 1000 - 3 * 86_400;
    let url = match value {
        "front_page" | "" => {
            "https://hn.algolia.com/api/v1/search?tags=front_page&hitsPerPage=30".to_string()
        }
        "best" => format!(
            "https://hn.algolia.com/api/v1/search?tags=story&numericFilters=created_at_i%3E{},points%3E200&hitsPerPage=30",
            now_ms / 1000 - 7 * 86_400
        ),
        q => format!(
            "https://hn.algolia.com/api/v1/search?query={}&tags=story&numericFilters=created_at_i%3E{since},points%3E15&hitsPerPage=15",
            encode_query(q)
        ),
    };
    let v = get_json(f, &url, None)?;
    let mut items = parse::hn_items(&v);
    for it in &mut items {
        if it.favicon.is_none() {
            it.favicon = markup::origin_of(&it.url).map(|o| format!("{o}/favicon.ico"));
        }
    }
    Ok(items)
}

fn bluesky(f: &Fetcher, value: &str) -> Result<Vec<Item>, String> {
    let api = "https://public.api.bsky.app/xrpc";
    let url = if value.starts_with("at://") {
        format!(
            "{api}/app.bsky.feed.getFeed?feed={}&limit=30",
            encode_query(value)
        )
    } else {
        format!(
            "{api}/app.bsky.feed.getAuthorFeed?actor={}&limit=30&filter=posts_no_replies",
            encode_query(value)
        )
    };
    let v = get_json(f, &url, None)?;
    Ok(parse::bluesky_items(&v))
}

fn fetch_mastodon(f: &Fetcher, value: &str, state: &Value) -> Result<Fetched, String> {
    if let Some(tag) = value.strip_prefix('#') {
        let (tag, instance) = tag.split_once('@').unwrap_or((tag, DEFAULT_MASTODON));
        return Ok(Fetched {
            items: mastodon_tag(f, tag, instance)?,
            state: None,
        });
    }
    let (user, instance) = value.split_once('@').ok_or("expected user@instance")?;
    let id = match state["account_id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            let v = get_json(
                f,
                &format!(
                    "https://{instance}/api/v1/accounts/lookup?acct={}",
                    encode_query(user)
                ),
                None,
            )?;
            v["id"].as_str().ok_or("account not found")?.to_string()
        }
    };
    if !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("odd account id".into());
    }
    let v = get_json(
        f,
        &format!("https://{instance}/api/v1/accounts/{id}/statuses?exclude_replies=true&limit=30"),
        None,
    )?;
    let mut items = parse::mastodon_items(&v);
    for it in &mut items {
        it.favicon = it
            .favicon
            .take()
            .or_else(|| Some(format!("https://{instance}/favicon.ico")));
    }
    Ok(Fetched {
        items,
        state: Some(json!({"account_id": id})),
    })
}

/// Recent posts with a link card or a photo under a hashtag.
pub fn mastodon_tag(f: &Fetcher, tag: &str, instance: &str) -> Result<Vec<Item>, String> {
    let tag: String = tag
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if tag.is_empty() {
        return Ok(Vec::new());
    }
    let v = get_json(
        f,
        &format!(
            "https://{instance}/api/v1/timelines/tag/{}?limit=30",
            encode_query(&tag)
        ),
        None,
    )?;
    Ok(parse::mastodon_items(&v)
        .into_iter()
        .filter(|i| !i.images.is_empty() || !i.url.contains(instance))
        .map(|mut i| {
            i.popularity *= 0.7;
            i
        })
        .collect())
}

// ------------------------------------------------------------ YouTube (API)

/// (subscribed channels as (id, title), liked video titles)
pub type YoutubeAccount = (Vec<(String, String)>, Vec<String>);

/// YouTube Data API with the user's Google token: subscribed channel ids
/// (with titles) and the titles of recently liked videos.
pub fn youtube_account(f: &Fetcher, token: &str) -> Result<YoutubeAccount, String> {
    let base = "https://www.googleapis.com/youtube/v3";
    // Every subscription (up to 250); the run's source rotation then reads
    // them a few at a time.
    let mut subs = Vec::new();
    let mut page = String::new();
    for _ in 0..5 {
        let v = get_json(
            f,
            &format!(
                "{base}/subscriptions?part=snippet&mine=true&maxResults=50&order=alphabetical{page}"
            ),
            Some(token),
        )?;
        subs.extend(parse::youtube_subscriptions(&v));
        match v["nextPageToken"].as_str().filter(|t| {
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }) {
            Some(t) => page = format!("&pageToken={t}"),
            None => break,
        }
    }
    let likes = get_json(
        f,
        &format!("{base}/videos?part=snippet&myRating=like&maxResults=25"),
        Some(token),
    )
    .map(|v| parse::youtube_likes(&v))
    .unwrap_or_default();
    Ok((
        subs,
        likes
            .into_iter()
            .map(|(t, c)| format!("{t} ({c})"))
            .collect(),
    ))
}

// ----------------------------------------------------------- source input

/// Turn what a person typed (a link, `r/name`, `@handle`, `#tag`) into a
/// (kind, value).
pub fn parse_input(input: &str) -> Result<(String, String), String> {
    let t = input.trim();
    let bad = || {
        "I couldn't tell what kind of source that is. Try a website or feed link, r/subreddit, a YouTube channel link, a Bluesky handle, or user@mastodon.instance.".to_string()
    };
    if t.is_empty()
        || t.len() > 500
        || t.contains(char::is_whitespace) && !t.eq_ignore_ascii_case("hacker news")
    {
        return Err(bad());
    }
    let lower = t.to_ascii_lowercase();
    if matches!(lower.as_str(), "hn" | "hacker news" | "hackernews") {
        return Ok(("hn".into(), "front_page".into()));
    }
    let subreddit = |s: &str| {
        let name = s.trim_matches('/');
        (2..=21).contains(&name.len())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if let Some(r) = lower
        .strip_prefix("r/")
        .or_else(|| lower.strip_prefix("/r/"))
    {
        let name = r.trim_matches('/');
        return if subreddit(name) {
            Ok(("reddit".into(), name.to_ascii_lowercase()))
        } else {
            Err(bad())
        };
    }
    if let Some(tag) = t.strip_prefix('#') {
        let (tag, inst) = tag.split_once('@').unwrap_or((tag, ""));
        if tag.is_empty()
            || !tag.chars().all(|c| c.is_alphanumeric() || c == '_')
            || !(inst.is_empty() || valid_host(inst))
        {
            return Err(bad());
        }
        return Ok((
            "mastodon".into(),
            if inst.is_empty() {
                format!("#{tag}")
            } else {
                format!("#{tag}@{inst}")
            },
        ));
    }
    if parse::valid_channel_id(t) {
        return Ok(("youtube".into(), t.into()));
    }
    if let Some(h) = t.strip_prefix('@') {
        // @user@instance (Mastodon) or @handle.bsky.social (Bluesky).
        if let Some((user, inst)) = h.split_once('@') {
            return if valid_user(user) && valid_host(inst) {
                Ok(("mastodon".into(), format!("{user}@{inst}")))
            } else {
                Err(bad())
            };
        }
        return if valid_host(h) {
            Ok(("bluesky".into(), h.to_ascii_lowercase()))
        } else {
            Err(bad())
        };
    }
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        if let Some((user, inst)) = t.split_once('@')
            && valid_user(user)
            && valid_host(inst)
        {
            return Ok(("mastodon".into(), format!("{user}@{inst}")));
        }
        if lower.ends_with(".bsky.social") && valid_host(t) {
            return Ok(("bluesky".into(), lower));
        }
        if valid_host(t) && t.contains('.') {
            return Ok(("rss".into(), format!("https://{t}")));
        }
        return Err(bad());
    }
    let host = markup::host_of(t).ok_or_else(bad)?;
    let (_, _, rest) = markup::split_url(t).ok_or_else(bad)?;
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let host_bare = host
        .trim_start_matches("www.")
        .trim_start_matches("m.")
        .trim_start_matches("old.");
    match host_bare {
        "youtube.com" => {
            if let Some(id) = path
                .strip_prefix("/channel/")
                .map(|r| r.split('/').next().unwrap_or(""))
                && parse::valid_channel_id(id)
            {
                return Ok(("youtube".into(), id.into()));
            }
            if let Some(h) = path
                .strip_prefix("/@")
                .map(|r| r.split('/').next().unwrap_or(""))
                && valid_user(h)
            {
                return Ok(("youtube".into(), format!("@{h}")));
            }
            if path == "/feeds/videos.xml"
                && let Some(id) = rest
                    .split("channel_id=")
                    .nth(1)
                    .map(|r| r.chars().take(24).collect::<String>())
                && parse::valid_channel_id(&id)
            {
                return Ok(("youtube".into(), id));
            }
            Err(bad())
        }
        "reddit.com" => {
            let name = path
                .strip_prefix("/r/")
                .map(|r| r.split('/').next().unwrap_or(""))
                .unwrap_or("");
            if subreddit(name) {
                // One spelling per subreddit, so removals and choices stick.
                Ok(("reddit".into(), name.to_ascii_lowercase()))
            } else {
                Err(bad())
            }
        }
        "news.ycombinator.com" => Ok(("hn".into(), "front_page".into())),
        "bsky.app" => {
            let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
            match parts.as_slice() {
                ["profile", h, "feed", rkey] if valid_host(h) || h.starts_with("did:") => Ok((
                    "bluesky".into(),
                    format!("at://{h}/app.bsky.feed.generator/{rkey}"),
                )),
                ["profile", h, ..] if valid_host(h) => {
                    Ok(("bluesky".into(), h.to_ascii_lowercase()))
                }
                _ => Err(bad()),
            }
        }
        _ => {
            if let Some(user) = path.strip_prefix("/@").map(|r| r.trim_end_matches('/'))
                && valid_user(user)
                && host_bare != "medium.com"
            {
                return Ok(("mastodon".into(), format!("{user}@{host}")));
            }
            if host_bare == "medium.com"
                && let Some(user) = path.strip_prefix("/@")
            {
                return Ok((
                    "rss".into(),
                    format!("https://medium.com/feed/@{}", user.trim_end_matches('/')),
                ));
            }
            Ok(("rss".into(), t.to_string()))
        }
    }
}

fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 64
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

fn valid_host(h: &str) -> bool {
    h.contains('.')
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// A suggestion from the interests call, as (kind, value).
pub fn from_suggestion(s: &Value) -> Option<(String, String)> {
    let get = |k: &str| s[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let input = match s["type"].as_str()? {
        "rss" => get("url")?.to_string(),
        "reddit" => format!(
            "r/{}",
            get("name").or(get("subreddit"))?.trim_start_matches("r/")
        ),
        "youtube" => {
            let c = get("channel")?;
            if c.starts_with("http") || parse::valid_channel_id(c) {
                c.to_string()
            } else {
                format!("https://www.youtube.com/@{}", c.trim_start_matches('@'))
            }
        }
        "bluesky" => format!("@{}", get("handle")?.trim_start_matches('@')),
        "mastodon" => {
            let a = get("account").or(get("handle"))?.trim_start_matches('@');
            if a.starts_with('#') {
                a.to_string()
            } else {
                format!("@{a}")
            }
        }
        "hn" => "hn".to_string(),
        _ => return None,
    };
    parse_input(&input).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_parsing() {
        let p = |s: &str| parse_input(s).map(|(k, v)| format!("{k}:{v}"));
        assert_eq!(p("r/space").unwrap(), "reddit:space");
        assert_eq!(
            p("https://www.reddit.com/r/AskScience/").unwrap(),
            "reddit:askscience"
        );
        assert_eq!(
            p("https://www.youtube.com/@SomeCreator").unwrap(),
            "youtube:@SomeCreator"
        );
        assert_eq!(
            p("https://www.youtube.com/channel/UCabcdefghijklmnopqrstuv").unwrap(),
            "youtube:UCabcdefghijklmnopqrstuv"
        );
        assert_eq!(
            p("UCabcdefghijklmnopqrstuv").unwrap(),
            "youtube:UCabcdefghijklmnopqrstuv"
        );
        assert_eq!(
            p("@someone.bsky.social").unwrap(),
            "bluesky:someone.bsky.social"
        );
        assert_eq!(
            p("https://bsky.app/profile/someone.example.com").unwrap(),
            "bluesky:someone.example.com"
        );
        assert_eq!(
            p("https://bsky.app/profile/someone.example.com/feed/science").unwrap(),
            "bluesky:at://someone.example.com/app.bsky.feed.generator/science"
        );
        assert_eq!(
            p("@user@social.example").unwrap(),
            "mastodon:user@social.example"
        );
        assert_eq!(
            p("https://social.example/@user").unwrap(),
            "mastodon:user@social.example"
        );
        assert_eq!(p("#astronomy").unwrap(), "mastodon:#astronomy");
        assert_eq!(p("hacker news").unwrap(), "hn:front_page");
        assert_eq!(
            p("https://blog.example.com/feed.xml").unwrap(),
            "rss:https://blog.example.com/feed.xml"
        );
        assert_eq!(
            p("blog.example.com").unwrap(),
            "rss:https://blog.example.com"
        );
        assert_eq!(
            p("https://medium.com/@writer").unwrap(),
            "rss:https://medium.com/feed/@writer"
        );
        assert!(p("hello world").is_err());
        assert!(p("javascript:alert(1)").is_err());
        assert!(p("r/no spaces").is_err());
        assert!(p("").is_err());
    }

    #[test]
    fn suggestions() {
        let s = |v: Value| from_suggestion(&v).map(|(k, v)| format!("{k}:{v}"));
        assert_eq!(
            s(json!({"type": "reddit", "name": "r/space"})).unwrap(),
            "reddit:space"
        );
        assert_eq!(
            s(json!({"type": "youtube", "channel": "SomeCreator"})).unwrap(),
            "youtube:@SomeCreator"
        );
        assert_eq!(
            s(json!({"type": "rss", "url": "https://x.example/rss"})).unwrap(),
            "rss:https://x.example/rss"
        );
        assert_eq!(
            s(json!({"type": "mastodon", "account": "a@social.example"})).unwrap(),
            "mastodon:a@social.example"
        );
        assert_eq!(
            s(json!({"type": "bluesky", "handle": "x.bsky.social"})).unwrap(),
            "bluesky:x.bsky.social"
        );
        assert!(s(json!({"type": "rss", "url": "ftp://x"})).is_none());
        assert!(s(json!({"type": "telegram", "name": "x"})).is_none());
    }

    #[test]
    fn feed_discovery() {
        let html = r#"<head><link rel="alternate" type="application/rss+xml" title="Feed" href="/feed.xml"></head>"#;
        assert_eq!(
            discover_feed(html, "https://blog.example.com/post/1").as_deref(),
            Some("https://blog.example.com/feed.xml")
        );
        assert_eq!(discover_feed("<p>none</p>", "https://x.example/"), None);
    }
}
