//! Source formats: RSS 2.0, RSS 1.0 (RDF), Atom (including YouTube channel
//! feeds and Reddit's Atom feeds), and the JSON APIs of Hacker News
//! (Algolia), Reddit, Bluesky, Mastodon, and the YouTube Data API. Every
//! parser returns normalized [`Item`]s and never fails on odd input.

use super::markup::{self, Node, collapse_ws, resolve, strip_tags};
use serde_json::Value;

/// A product price found on a page.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Price {
    pub amount: f64,
    pub currency: String,
}

/// An event found on a page.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub start_ms: i64,
    #[serde(default)]
    pub location: String,
}

/// One candidate post, normalized from any source.
#[derive(Debug, Clone, Default)]
pub struct Item {
    /// Where "Visit" goes.
    pub url: String,
    pub title: String,
    /// Plain text.
    pub description: String,
    pub source_name: String,
    /// rss | youtube | reddit | hn | bluesky | mastodon
    pub source_kind: String,
    /// The `feed_sources` row this came from (None for searches).
    pub source_id: Option<String>,
    pub topic: Option<String>,
    /// Original image URLs, best first.
    pub images: Vec<String>,
    pub favicon: Option<String>,
    pub published_ms: Option<i64>,
    /// 0..1, from points/likes where the source reports them.
    pub popularity: f64,
    pub price: Option<Price>,
    pub event: Option<Event>,
    /// Fetch the page for OpenGraph data (missing image or description).
    pub needs_page: bool,
    /// Language code the source reported (Mastodon, Bluesky), if any.
    pub language: Option<String>,
}

impl Item {
    fn push_image(&mut self, base: &str, src: &str) {
        if let Some(u) = resolve(base, src).map(|u| https_image(&u))
            && !self.images.contains(&u)
            && !looks_like_icon(&u)
        {
            self.images.push(u);
        }
    }
}

/// Photos are shown in an https app: ask for the https copy (sites that
/// still link http images almost always serve them over https too).
pub fn https_image(url: &str) -> String {
    match url.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("http://") => format!("https://{}", &url[7..]),
        _ => url.to_string(),
    }
}

/// URLs that are almost never a real photo (logos, tracking pixels, icons).
pub fn looks_like_icon(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    let path = l.split(['?', '#']).next().unwrap_or("");
    path.ends_with(".svg")
        || path.ends_with(".ico")
        || [
            "favicon",
            "/logo",
            "-logo",
            "_logo",
            "sprite",
            "placeholder",
            "1x1",
            "pixel.",
            "blank.gif",
            "spacer",
            "/emoji/",
            "gravatar.com/avatar",
            "feeds.feedburner.com/~",
            "/avatar",
        ]
        .iter()
        .any(|p| path.contains(p))
}

// --------------------------------------------------------------------- dates

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn epoch_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, offset_min: i64) -> Option<i64> {
    if !(1970..=2200).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || s > 60
    {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s - offset_min * 60;
    Some(secs * 1000)
}

fn zone_offset(z: &str) -> Option<i64> {
    let z = z.trim();
    Some(match z.to_ascii_uppercase().as_str() {
        "" | "Z" | "GMT" | "UT" | "UTC" => 0,
        "EST" => -300,
        "EDT" => -240,
        "CST" => -360,
        "CDT" => -300,
        "MST" => -420,
        "MDT" => -360,
        "PST" => -480,
        "PDT" => -420,
        "CET" => 60,
        "CEST" => 120,
        "BST" => 60,
        _ => {
            let (sign, rest) = match z.as_bytes().first()? {
                b'+' => (1, &z[1..]),
                b'-' => (-1, &z[1..]),
                _ => return None,
            };
            let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
            let (h, m) = match digits.len() {
                4 => (
                    digits[..2].parse::<i64>().ok()?,
                    digits[2..].parse::<i64>().ok()?,
                ),
                2 => (digits.parse::<i64>().ok()?, 0),
                _ => return None,
            };
            sign * (h * 60 + m)
        }
    })
}

fn month(s: &str) -> Option<i64> {
    let m = s.get(..3)?.to_ascii_lowercase();
    [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|x| *x == m)
    .map(|i| i as i64 + 1)
}

/// Parse an RFC 822 (RSS) or ISO 8601 (Atom, JSON APIs) date to epoch ms.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.as_bytes()[0].is_ascii_digit() && s.len() >= 10 && s.as_bytes()[4] == b'-' {
        return parse_iso(s);
    }
    // RFC 822: [Day,] DD Mon YYYY HH:MM[:SS] [zone]
    let s = s.split_once(',').map_or(s, |(_, r)| r).trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }
    let d: i64 = parts[0].parse().ok()?;
    let mo = month(parts[1])?;
    let mut y: i64 = parts[2].parse().ok()?;
    if y < 100 {
        y += 2000;
    }
    let (mut h, mut mi, mut sec) = (0, 0, 0);
    if let Some(t) = parts.get(3) {
        let tp: Vec<&str> = t.split(':').collect();
        h = tp.first()?.parse().ok()?;
        mi = tp.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        sec = tp.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
    }
    let off = parts.get(4).and_then(|z| zone_offset(z)).unwrap_or(0);
    epoch_ms(y, mo, d, h, mi, sec, off)
}

fn parse_iso(s: &str) -> Option<i64> {
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let rest = s.get(10..).unwrap_or("");
    if rest.is_empty() {
        return epoch_ms(y, mo, d, 0, 0, 0, 0);
    }
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    let h: i64 = rest.get(0..2)?.parse().ok()?;
    let mi: i64 = rest.get(3..5)?.parse().ok()?;
    let mut idx = 5;
    let mut sec = 0;
    if rest.as_bytes().get(5) == Some(&b':') {
        sec = rest.get(6..8)?.parse().ok()?;
        idx = 8;
        if rest.as_bytes().get(8) == Some(&b'.') {
            idx = 9 + rest[9..].bytes().take_while(u8::is_ascii_digit).count();
        }
    }
    let off = zone_offset(rest.get(idx..).unwrap_or(""))?;
    epoch_ms(y, mo, d, h, mi, sec, off)
}

// --------------------------------------------------------------------- feeds

/// A parsed RSS/Atom document.
#[derive(Debug, Default)]
pub struct FeedDoc {
    pub title: Option<String>,
    pub site_url: Option<String>,
    pub icon: Option<String>,
    pub items: Vec<Item>,
}

fn clean_title(s: &str) -> String {
    let t = collapse_ws(&strip_tags(s));
    crate::util::truncate_chars(&t, 300)
}

fn is_image_url(u: &str) -> bool {
    let p = u
        .to_ascii_lowercase()
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_string();
    [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"]
        .iter()
        .any(|e| p.ends_with(e))
}

fn media_images(node: &Node, base: &str, item: &mut Item) {
    for c in &node.children {
        match c.name.as_str() {
            "media:group" => media_images(c, base, item),
            "media:content" => {
                let ty = c.attr("type").unwrap_or("");
                let medium = c.attr("medium").unwrap_or("");
                if let Some(u) = c.attr("url")
                    && (medium == "image" || ty.starts_with("image/") || is_image_url(u))
                {
                    item.push_image(base, u);
                }
                media_images(c, base, item);
            }
            "media:thumbnail" => {
                if let Some(u) = c.attr("url") {
                    item.push_image(base, u);
                }
            }
            "enclosure" => {
                let ty = c.attr("type").unwrap_or("");
                if let Some(u) = c.attr("url").or(c.attr("href"))
                    && (ty.starts_with("image/") || (ty.is_empty() && is_image_url(u)))
                {
                    item.push_image(base, u);
                }
            }
            "itunes:image" => {
                if let Some(u) = c.attr("href") {
                    item.push_image(base, u);
                }
            }
            "link" if c.attr("rel") == Some("enclosure") => {
                let ty = c.attr("type").unwrap_or("");
                if let Some(u) = c.attr("href")
                    && ty.starts_with("image/")
                {
                    item.push_image(base, u);
                }
            }
            _ => {}
        }
    }
}

fn collect_imgs(node: &Node, base: &str, item: &mut Item) {
    for c in &node.children {
        if c.name == "img"
            && let Some(u) = c.attr("src")
        {
            item.push_image(base, u);
        }
        collect_imgs(c, base, item);
    }
}

/// HTML of a text construct: escaped HTML arrives decoded as text; XHTML
/// content arrives as child elements (their text is used).
fn html_of(node: &Node) -> String {
    if node.children.is_empty() {
        node.text.clone()
    } else {
        node.all_text()
    }
}

fn atom_link(node: &Node) -> Option<String> {
    let links: Vec<&Node> = node.children_named("link").collect();
    links
        .iter()
        .find(|l| l.attr("rel").is_none_or(|r| r == "alternate") && l.attr("href").is_some())
        .and_then(|l| l.attr("href"))
        .map(str::to_string)
        .or_else(|| {
            // RSS <link>text</link>
            links
                .iter()
                .find_map(|l| Some(l.text.trim().to_string()).filter(|t| !t.is_empty()))
        })
}

fn feed_item(node: &Node, base: &str, atom: bool) -> Option<Item> {
    let mut item = Item {
        source_kind: "rss".into(),
        popularity: 0.3,
        ..Default::default()
    };
    item.title = node
        .child_text("title")
        .map(|t| clean_title(&t))
        .unwrap_or_default();
    let link = atom_link(node).or_else(|| {
        node.child("guid")
            .filter(|g| g.attr("ispermalink") != Some("false"))
            .map(|g| g.text.trim().to_string())
            .filter(|g| g.starts_with("http"))
    })?;
    item.url = resolve(base, &link)?;
    let mut html = String::new();
    for name in [
        "content:encoded",
        "content",
        "description",
        "summary",
        "media:description",
    ] {
        if let Some(n) = node.child(name).or_else(|| {
            (name == "media:description")
                .then(|| node.child("media:group").and_then(|g| g.child(name)))
                .flatten()
        }) {
            let h = html_of(n);
            if !h.trim().is_empty() {
                if html.is_empty() {
                    html = h;
                } else if item.description.is_empty() {
                    // Prefer the shorter summary for the description.
                    let short = collapse_ws(&strip_tags(&h));
                    if !short.is_empty() {
                        item.description = short;
                    }
                }
                collect_imgs(n, base, &mut item);
            }
        }
    }
    if item.description.is_empty() {
        item.description = collapse_ws(&strip_tags(&html));
    }
    item.description = crate::util::truncate_chars(&item.description, 1500);
    media_images(node, base, &mut item);
    for src in markup::img_srcs(&html) {
        item.push_image(base, &src);
    }
    if item.title.is_empty() {
        item.title = crate::util::truncate_chars(&item.description, 120);
    }
    if item.title.is_empty() {
        return None;
    }
    let date = [
        "pubdate",
        "published",
        "dc:date",
        "updated",
        "a10:updated",
        "issued",
    ]
    .iter()
    .find_map(|n| node.child_text(n).and_then(|d| parse_date(&d)));
    item.published_ms = date;
    if let Some(id) = node.child_text("yt:videoid") {
        item.source_kind = "youtube".into();
        if item.images.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            item.images
                .push(format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"));
        }
    }
    let _ = atom;
    item.needs_page = item.images.is_empty() || item.description.chars().count() < 80;
    Some(item)
}

/// Parse an RSS or Atom document. `None` when it is not a feed.
pub fn parse_feed(xml: &str, feed_url: &str) -> Option<FeedDoc> {
    let root = markup::parse_xml(xml);
    let mut doc = FeedDoc::default();
    if let Some(feed) = root.child("feed") {
        doc.title = feed.child_text("title").map(|t| clean_title(&t));
        doc.site_url = atom_link(feed).and_then(|l| resolve(feed_url, &l));
        doc.icon = feed
            .child_text("icon")
            .or_else(|| feed.child_text("logo"))
            .and_then(|i| resolve(feed_url, &i));
        let base = doc.site_url.clone().unwrap_or_else(|| feed_url.to_string());
        doc.items = feed
            .children_named("entry")
            .filter_map(|e| feed_item(e, &base, true))
            .collect();
        return Some(doc);
    }
    let top = root.child("rss").or_else(|| root.child("rdf:rdf"))?;
    let channel = top.child("channel");
    if let Some(ch) = channel {
        doc.title = ch.child_text("title").map(|t| clean_title(&t));
        doc.site_url = atom_link(ch).and_then(|l| resolve(feed_url, &l));
        doc.icon = ch
            .child("image")
            .and_then(|i| i.child_text("url"))
            .and_then(|u| resolve(feed_url, &u));
    }
    let base = doc.site_url.clone().unwrap_or_else(|| feed_url.to_string());
    let mut items: Vec<Item> = channel
        .map(|ch| {
            ch.children_named("item")
                .filter_map(|e| feed_item(e, &base, false))
                .collect()
        })
        .unwrap_or_default();
    // RSS 1.0 puts items next to the channel.
    items.extend(
        top.children_named("item")
            .filter_map(|e| feed_item(e, &base, false)),
    );
    doc.items = items;
    Some(doc)
}

/// Reddit's Atom entries link to the discussion; the story itself is the
/// "[link]" anchor in the content.
pub fn reddit_external_link(content_html: &str) -> Option<String> {
    markup::links(content_html)
        .into_iter()
        .find(|(_, text)| text == "[link]")
        .map(|(h, _)| h)
        .filter(|h| {
            markup::host_of(h).is_some_and(|host| {
                !host.ends_with("reddit.com")
                    && !host.ends_with("redd.it")
                    && !host.ends_with("redditmedia.com")
            })
        })
}

// ----------------------------------------------------------------- JSON APIs

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn log_popularity(n: f64, full: f64) -> f64 {
    ((n.max(0.0) + 1.0).ln() / (full + 1.0).ln()).clamp(0.0, 1.0)
}

/// Hacker News search results (Algolia `search` / `search_by_date`).
pub fn hn_items(v: &Value) -> Vec<Item> {
    let Some(hits) = v["hits"].as_array() else {
        return Vec::new();
    };
    hits.iter()
        .filter_map(|h| {
            let title = clean_title(s(&h["title"]));
            if title.is_empty() {
                return None;
            }
            let id = h["objectID"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_default();
            let discussion = format!("https://news.ycombinator.com/item?id={id}");
            let url = h["url"]
                .as_str()
                .filter(|u| markup::split_url(u).is_some())
                .map(str::to_string)
                .unwrap_or(discussion);
            let points = h["points"].as_f64().unwrap_or(0.0);
            Some(Item {
                source_name: markup::host_of(&url)
                    .map(|h| h.trim_start_matches("www.").to_string())
                    .unwrap_or_else(|| "Hacker News".into()),
                description: collapse_ws(&strip_tags(s(&h["story_text"]))),
                url,
                title,
                source_kind: "hn".into(),
                published_ms: h["created_at_i"].as_i64().map(|t| t * 1000),
                popularity: log_popularity(points, 1000.0),
                needs_page: true,
                ..Default::default()
            })
        })
        .collect()
}

fn unescape_amp(u: &str) -> String {
    u.replace("&amp;", "&")
}

/// Reddit listing JSON (`/r/<sub>/top.json`).
pub fn reddit_items(v: &Value) -> Vec<Item> {
    let Some(children) = v["data"]["children"].as_array() else {
        return Vec::new();
    };
    children
        .iter()
        .filter_map(|c| {
            let d = &c["data"];
            if d["over_18"].as_bool() == Some(true) || d["stickied"].as_bool() == Some(true) {
                return None;
            }
            let title = clean_title(s(&d["title"]));
            let permalink = format!("https://www.reddit.com{}", s(&d["permalink"]));
            let is_self = d["is_self"].as_bool().unwrap_or(false);
            let url = if is_self {
                permalink.clone()
            } else {
                d["url_overridden_by_dest"]
                    .as_str()
                    .or(d["url"].as_str())
                    .filter(|u| markup::split_url(u).is_some())
                    .map(str::to_string)
                    .unwrap_or_else(|| permalink.clone())
            };
            let mut item = Item {
                title,
                description: crate::util::truncate_chars(&collapse_ws(s(&d["selftext"])), 1500),
                source_name: s(&d["subreddit_name_prefixed"]).to_string(),
                source_kind: "reddit".into(),
                published_ms: d["created_utc"].as_f64().map(|t| (t * 1000.0) as i64),
                popularity: log_popularity(d["score"].as_f64().unwrap_or(0.0), 20_000.0),
                url,
                ..Default::default()
            };
            if let Some(imgs) = d["preview"]["images"].as_array() {
                for im in imgs {
                    if let Some(u) = im["source"]["url"].as_str() {
                        item.push_image(&permalink, &unescape_amp(u));
                    }
                }
            }
            if let Some(m) = d["media_metadata"].as_object() {
                for (_, meta) in m.iter().take(6) {
                    if let Some(u) = meta["s"]["u"].as_str() {
                        item.push_image(&permalink, &unescape_amp(u));
                    }
                }
            }
            item.needs_page = !is_self && (item.images.is_empty() || item.description.len() < 80);
            (!item.title.is_empty()).then_some(item)
        })
        .collect()
}

/// Turn Reddit Atom entries (the RSS fallback) into items that point at the
/// linked story when there is one.
pub fn reddit_feed_items(xml: &str, feed_url: &str, subreddit_label: &str) -> Vec<Item> {
    let root = markup::parse_xml(xml);
    let Some(feed) = root.child("feed") else {
        return Vec::new();
    };
    feed.children_named("entry")
        .filter_map(|e| {
            let mut item = feed_item(e, feed_url, true)?;
            let content = e.child("content").map(html_of).unwrap_or_default();
            item.source_kind = "reddit".into();
            item.source_name = subreddit_label.to_string();
            item.popularity = 0.5;
            if let Some(ext) = reddit_external_link(&content) {
                item.url = ext;
                item.description.clear();
                item.needs_page = true;
            } else {
                // A text post: the description is the post body.
                let body = collapse_ws(&strip_tags(&content));
                let body = body
                    .split(" submitted by ")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                item.description = crate::util::truncate_chars(&body, 1500);
                item.needs_page = false;
            }
            Some(item)
        })
        .collect()
}

const BAD_LABELS: &[&str] = &["porn", "sexual", "nudity", "graphic-media", "gore", "nsfl"];

fn bsky_rkey(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or("")
}

/// Bluesky `getAuthorFeed` / `getFeed` responses.
pub fn bluesky_items(v: &Value) -> Vec<Item> {
    let Some(feed) = v["feed"].as_array() else {
        return Vec::new();
    };
    feed.iter()
        .filter_map(|entry| {
            if entry.get("reason").is_some_and(|r| !r.is_null())
                || entry.get("reply").is_some_and(|r| !r.is_null())
            {
                return None; // reposts and replies
            }
            let p = &entry["post"];
            let labels = p["labels"].as_array().cloned().unwrap_or_default();
            if labels.iter().any(|l| BAD_LABELS.contains(&s(&l["val"]))) {
                return None;
            }
            let handle = s(&p["author"]["handle"]);
            let name = p["author"]["displayName"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(handle);
            let text = collapse_ws(s(&p["record"]["text"]));
            let post_url = format!(
                "https://bsky.app/profile/{handle}/post/{}",
                bsky_rkey(s(&p["uri"]))
            );
            let mut item = Item {
                source_name: name.to_string(),
                source_kind: "bluesky".into(),
                published_ms: parse_date(s(&p["record"]["createdAt"])),
                language: p["record"]["langs"][0].as_str().map(str::to_lowercase),
                popularity: log_popularity(
                    p["likeCount"].as_f64().unwrap_or(0.0)
                        + 2.0 * p["repostCount"].as_f64().unwrap_or(0.0),
                    5_000.0,
                ),
                url: post_url.clone(),
                favicon: p["author"]["avatar"].as_str().map(str::to_string),
                ..Default::default()
            };
            let mut embed = &p["embed"];
            if s(&embed["$type"]).starts_with("app.bsky.embed.recordWithMedia") {
                embed = &embed["media"];
            }
            let ty = s(&embed["$type"]);
            if ty.starts_with("app.bsky.embed.external") {
                let ext = &embed["external"];
                if let Some(u) = ext["uri"]
                    .as_str()
                    .filter(|u| markup::split_url(u).is_some())
                {
                    item.url = u.to_string();
                    item.title = clean_title(s(&ext["title"]));
                    let d = collapse_ws(s(&ext["description"]));
                    item.description = if text.is_empty() { d } else { text.clone() };
                    if let Some(t) = ext["thumb"].as_str() {
                        item.push_image(&post_url, t);
                    }
                    item.needs_page = true;
                }
            } else if ty.starts_with("app.bsky.embed.images") {
                for im in embed["images"].as_array().into_iter().flatten() {
                    if let Some(u) = im["fullsize"].as_str().or(im["thumb"].as_str()) {
                        item.push_image(&post_url, u);
                    }
                }
            } else if ty.starts_with("app.bsky.embed.video")
                && let Some(t) = embed["thumbnail"].as_str()
            {
                item.push_image(&post_url, t);
            }
            if item.title.is_empty() {
                item.title = first_sentence(&text, 120);
            }
            if item.description.is_empty() {
                item.description = text;
            }
            (!item.title.is_empty()).then_some(item)
        })
        .collect()
}

/// First sentence or line of a short post, as a title.
pub fn first_sentence(text: &str, max: usize) -> String {
    let t = text.trim();
    let cut = t
        .char_indices()
        .find(|&(i, c)| {
            (c == '.' || c == '!' || c == '?') && t[i + c.len_utf8()..].starts_with(' ') && i > 20
        })
        .map(|(i, c)| &t[..i + c.len_utf8()])
        .unwrap_or(t);
    let line = cut.lines().next().unwrap_or("");
    crate::util::truncate_chars(line, max)
}

/// Mastodon statuses (account statuses or a tag timeline).
pub fn mastodon_items(v: &Value) -> Vec<Item> {
    let Some(list) = v.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|st| {
            let st = if st["reblog"].is_object() {
                &st["reblog"]
            } else {
                st
            };
            if st["sensitive"].as_bool() == Some(true) || !s(&st["spoiler_text"]).is_empty() {
                return None;
            }
            let url = st["url"]
                .as_str()
                .or(st["uri"].as_str())
                .filter(|u| markup::split_url(u).is_some())?;
            let text = collapse_ws(&strip_tags(s(&st["content"])));
            let account = &st["account"];
            let name = account["display_name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(s(&account["acct"]));
            let mut item = Item {
                url: url.to_string(),
                source_name: name.to_string(),
                source_kind: "mastodon".into(),
                published_ms: parse_date(s(&st["created_at"])),
                language: st["language"].as_str().map(str::to_lowercase),
                popularity: log_popularity(
                    st["favourites_count"].as_f64().unwrap_or(0.0)
                        + 2.0 * st["reblogs_count"].as_f64().unwrap_or(0.0),
                    2_000.0,
                ),
                favicon: account["avatar_static"].as_str().map(str::to_string),
                ..Default::default()
            };
            for m in st["media_attachments"].as_array().into_iter().flatten() {
                if s(&m["type"]) == "image"
                    && let Some(u) = m["url"].as_str()
                {
                    item.push_image(url, u);
                }
            }
            let card = &st["card"];
            if let Some(cu) = card["url"]
                .as_str()
                .filter(|u| markup::split_url(u).is_some())
            {
                item.url = cu.to_string();
                item.title = clean_title(s(&card["title"]));
                if let Some(im) = card["image"].as_str() {
                    item.push_image(cu, im);
                }
                if let Some(p) = card["provider_name"].as_str().filter(|p| !p.is_empty()) {
                    item.source_name = p.to_string();
                }
                let d = collapse_ws(s(&card["description"]));
                item.description = if d.len() > 60 { d } else { text.clone() };
                item.needs_page = item.images.is_empty();
            }
            if item.title.is_empty() {
                item.title = first_sentence(&text, 120);
            }
            if item.description.is_empty() {
                item.description = text;
            }
            (!item.title.is_empty()).then_some(item)
        })
        .collect()
}

/// Channel ids from a YouTube Data API `subscriptions.list` page.
pub fn youtube_subscriptions(v: &Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let sn = &i["snippet"];
            let id = sn["resourceId"]["channelId"].as_str()?;
            valid_channel_id(id).then(|| (id.to_string(), s(&sn["title"]).to_string()))
        })
        .collect()
}

/// (title, channel title) of liked videos from `videos.list?myRating=like`.
pub fn youtube_likes(v: &Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            let sn = &i["snippet"];
            (
                s(&sn["title"]).to_string(),
                s(&sn["channelTitle"]).to_string(),
            )
        })
        .filter(|(t, _)| !t.is_empty())
        .collect()
}

/// `UC` + 22 URL-safe base64 characters.
pub fn valid_channel_id(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("UC")
        && id[2..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A channel id in a YouTube channel page.
pub fn youtube_channel_id_from_page(html: &str) -> Option<String> {
    for needle in [
        "<link rel=\"canonical\" href=\"https://www.youtube.com/channel/",
        "\"channelId\":\"",
        "\"externalId\":\"",
        "itemprop=\"identifier\" content=\"",
    ] {
        if let Some(p) = html.find(needle) {
            let rest = &html[p + needle.len()..];
            let id: String = rest.chars().take(24).collect();
            if valid_channel_id(&id) {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dates() {
        assert_eq!(
            parse_date("Sat, 10 Oct 2026 14:36:15 +0000"),
            Some(1_791_642_975_000)
        );
        assert_eq!(
            parse_date("10 Oct 2026 10:36:15 EDT"),
            Some(1_791_642_975_000)
        );
        assert_eq!(
            parse_date("2026-10-10T14:36:15+00:00"),
            Some(1_791_642_975_000)
        );
        assert_eq!(
            parse_date("2026-10-10T16:36:15.289+02:00"),
            Some(1_791_642_975_000)
        );
        assert_eq!(parse_date("2026-10-10T14:36:15Z"), Some(1_791_642_975_000));
        assert_eq!(parse_date("2026-10-10"), Some(1_791_590_400_000));
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(parse_date("Mon, 99 Foo 2026"), None);
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:media="http://search.yahoo.com/mrss/">
<channel><title>Example Science</title><link>https://science.example.com/</link>
<image><url>https://science.example.com/icon.png</url></image>
<item><title>Comet &amp; friends visible tonight</title><link>https://science.example.com/comet?utm_source=rss</link>
<description><![CDATA[<p>A bright comet will be visible <b>after sunset</b> for most of the week, astronomers say.</p><img src="/img/comet.jpg">]]></description>
<pubDate>Sat, 10 Oct 2026 14:36:15 GMT</pubDate>
<media:content url="https://cdn.science.example.com/comet-big.jpg" medium="image"/>
</item>
<item><title>No link here</title></item>
<item><guid isPermaLink="true">https://science.example.com/2</guid><description>Short</description></item>
</channel></rss>"#;

    #[test]
    fn rss_items() {
        let doc = parse_feed(RSS, "https://science.example.com/feed.xml").unwrap();
        assert_eq!(doc.title.as_deref(), Some("Example Science"));
        assert_eq!(
            doc.icon.as_deref(),
            Some("https://science.example.com/icon.png")
        );
        assert_eq!(doc.items.len(), 2, "the item without a link is skipped");
        let a = &doc.items[0];
        assert_eq!(a.title, "Comet & friends visible tonight");
        assert!(
            a.description
                .starts_with("A bright comet will be visible after sunset")
        );
        assert_eq!(
            a.images,
            vec![
                "https://cdn.science.example.com/comet-big.jpg",
                "https://science.example.com/img/comet.jpg"
            ]
        );
        assert_eq!(a.published_ms, Some(1_791_642_975_000));
        assert!(!a.needs_page);
        let b = &doc.items[1];
        assert_eq!(b.url, "https://science.example.com/2");
        assert_eq!(b.title, "Short");
        assert!(b.needs_page);
    }

    const YT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
 <title>Example Channel</title>
 <link rel="alternate" href="https://www.youtube.com/channel/UCxxxxxxxxxxxxxxxxxxxxxx"/>
 <entry>
  <id>yt:video:abcDEF12345</id><yt:videoId>abcDEF12345</yt:videoId>
  <title>How rockets land</title>
  <link rel="alternate" href="https://www.youtube.com/watch?v=abcDEF12345"/>
  <published>2026-10-08T14:07:00+00:00</published>
  <media:group><media:title>How rockets land</media:title>
   <media:thumbnail url="https://i4.ytimg.com/vi/abcDEF12345/hqdefault.jpg" width="480" height="360"/>
   <media:description>We look at how boosters come back to the pad, step by step, with animations and interviews with engineers.</media:description>
  </media:group>
 </entry>
</feed>"#;

    #[test]
    fn youtube_atom() {
        let doc = parse_feed(YT, "https://www.youtube.com/feeds/videos.xml?channel_id=x").unwrap();
        assert_eq!(doc.title.as_deref(), Some("Example Channel"));
        let v = &doc.items[0];
        assert_eq!(v.source_kind, "youtube");
        assert_eq!(v.url, "https://www.youtube.com/watch?v=abcDEF12345");
        assert_eq!(
            v.images,
            vec!["https://i4.ytimg.com/vi/abcDEF12345/hqdefault.jpg"]
        );
        assert!(v.description.starts_with("We look at how boosters"));
        assert!(!v.needs_page);
    }

    #[test]
    fn reddit_atom_points_at_the_story() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/">
<entry><title>Probe finds water</title><link href="https://www.reddit.com/r/space/comments/abc/probe/"/>
<content type="html">&lt;a href=&quot;https://www.reddit.com/r/space/comments/abc/probe/&quot;&gt;&lt;img src=&quot;https://external-preview.redd.it/x.jpeg?width=640&amp;amp;s=1&quot;/&gt;&lt;/a&gt; submitted by &lt;a href=&quot;https://www.reddit.com/user/u1&quot;&gt;/u/u1&lt;/a&gt; &lt;a href=&quot;https://news.example.com/probe&quot;&gt;[link]&lt;/a&gt; &lt;a href=&quot;https://www.reddit.com/r/space/comments/abc/probe/&quot;&gt;[comments]&lt;/a&gt;</content>
<media:thumbnail url="https://external-preview.redd.it/x.jpeg?width=640&amp;s=1"/>
<updated>2026-10-10T14:36:15+00:00</updated></entry>
<entry><title>Question about orbits</title><link href="https://www.reddit.com/r/space/comments/def/q/"/>
<content type="html">&lt;div&gt;&lt;p&gt;Why do orbits decay?&lt;/p&gt;&lt;/div&gt; submitted by &lt;a href=&quot;https://www.reddit.com/user/u2&quot;&gt;/u/u2&lt;/a&gt; &lt;a href=&quot;https://www.reddit.com/r/space/comments/def/q/&quot;&gt;[link]&lt;/a&gt;</content></entry>
</feed>"#;
        let items = reddit_feed_items(xml, "https://www.reddit.com/r/space/top/.rss", "r/space");
        assert_eq!(items[0].url, "https://news.example.com/probe");
        assert_eq!(items[0].source_name, "r/space");
        assert!(
            items[0].images[0].starts_with("https://external-preview.redd.it/x.jpeg?width=640&s=1")
        );
        assert_eq!(
            items[1].url,
            "https://www.reddit.com/r/space/comments/def/q/"
        );
        assert_eq!(items[1].description, "Why do orbits decay?");
    }

    #[test]
    fn hn_and_reddit_json() {
        let hn = json!({"hits": [
            {"title": "Show HN: A tiny database", "url": "https://db.example.org/post", "points": 250, "created_at_i": 1_791_600_000, "objectID": "1"},
            {"title": "Ask HN: Favorite books?", "url": null, "points": 10, "created_at_i": 1_791_600_000, "objectID": "2", "story_text": "<p>Looking for recs</p>"},
            {"title": "", "objectID": "3"}
        ]});
        let items = hn_items(&hn);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].source_name, "db.example.org");
        assert!(items[0].popularity > 0.7);
        assert_eq!(items[1].url, "https://news.ycombinator.com/item?id=2");
        assert_eq!(items[1].description, "Looking for recs");

        let r = json!({"data": {"children": [
            {"data": {"title": "Big launch", "permalink": "/r/space/comments/1/big/", "url": "https://news.example.com/launch", "is_self": false,
                      "score": 5000, "created_utc": 1_791_600_000.0, "subreddit_name_prefixed": "r/space",
                      "preview": {"images": [{"source": {"url": "https://preview.redd.it/a.jpg?s=1&amp;t=2"}}]}}},
            {"data": {"title": "NSFW", "over_18": true, "permalink": "/x"}},
            {"data": {"title": "Pinned", "stickied": true, "permalink": "/y"}}
        ]}});
        let items = reddit_items(&r);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].images,
            vec!["https://preview.redd.it/a.jpg?s=1&t=2"]
        );
        assert_eq!(items[0].url, "https://news.example.com/launch");
    }

    #[test]
    fn bluesky_and_mastodon_json() {
        let b = json!({"feed": [
            {"post": {"uri": "at://did:plc:x/app.bsky.feed.post/3abc", "author": {"handle": "sky.example.com", "displayName": "Sky Watcher"},
                      "record": {"text": "Tonight's meteor shower peaks after midnight. Look east!", "createdAt": "2026-10-10T14:36:15Z"},
                      "embed": {"$type": "app.bsky.embed.images#view", "images": [{"fullsize": "https://cdn.bsky.example/img/full/1.jpg", "thumb": "https://cdn.bsky.example/img/t/1.jpg"}]},
                      "likeCount": 40, "repostCount": 5}},
            {"post": {"uri": "at://x/app.bsky.feed.post/2", "author": {"handle": "a.example"}, "record": {"text": "repost"}}, "reason": {"$type": "repost"}},
            {"post": {"uri": "at://x/app.bsky.feed.post/3", "author": {"handle": "a.example"}, "record": {"text": "bad"}, "labels": [{"val": "porn"}]}},
            {"post": {"uri": "at://did:plc:y/app.bsky.feed.post/4", "author": {"handle": "news.example.com"}, "record": {"text": "Good read"},
                      "embed": {"$type": "app.bsky.embed.external#view", "external": {"uri": "https://news.example.com/a", "title": "A story", "description": "d", "thumb": "https://cdn.bsky.example/t.jpg"}}}}
        ]});
        let items = bluesky_items(&b);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].url,
            "https://bsky.app/profile/sky.example.com/post/3abc"
        );
        assert_eq!(
            items[0].title,
            "Tonight's meteor shower peaks after midnight."
        );
        assert_eq!(
            items[0].images,
            vec!["https://cdn.bsky.example/img/full/1.jpg"]
        );
        assert_eq!(items[1].url, "https://news.example.com/a");
        assert_eq!(items[1].title, "A story");

        let m = json!([
            {"url": "https://social.example/@a/1", "content": "<p>New telescope images are in! <a href=\"#\">#space</a></p>", "created_at": "2026-10-10T14:36:15.000Z",
             "account": {"display_name": "Astro A", "acct": "a"}, "media_attachments": [{"type": "image", "url": "https://files.social.example/1.png"}],
             "favourites_count": 10, "reblogs_count": 2, "sensitive": false, "spoiler_text": ""},
            {"url": "https://social.example/@b/2", "content": "<p>cw</p>", "account": {"acct": "b"}, "sensitive": true, "spoiler_text": "cw"}
        ]);
        let items = mastodon_items(&m);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "New telescope images are in!");
        assert_eq!(items[0].images, vec!["https://files.social.example/1.png"]);
    }

    #[test]
    fn youtube_api_and_page_helpers() {
        let subs = json!({"items": [{"snippet": {"title": "Chan", "resourceId": {"channelId": "UCabcdefghijklmnopqrstuv"}}},
                                    {"snippet": {"title": "Bad", "resourceId": {"channelId": "nope"}}}]});
        assert_eq!(
            youtube_subscriptions(&subs),
            vec![("UCabcdefghijklmnopqrstuv".to_string(), "Chan".to_string())]
        );
        let likes = json!({"items": [{"snippet": {"title": "Video", "channelTitle": "Chan"}}]});
        assert_eq!(
            youtube_likes(&likes),
            vec![("Video".to_string(), "Chan".to_string())]
        );
        let page = r#"<html><link rel="canonical" href="https://www.youtube.com/channel/UCabcdefghijklmnopqrstuv"></html>"#;
        assert_eq!(
            youtube_channel_id_from_page(page).as_deref(),
            Some("UCabcdefghijklmnopqrstuv")
        );
        assert!(looks_like_icon("https://x.example/static/logo.png"));
        assert!(!looks_like_icon("https://x.example/photos/launch.jpg"));
    }
}
