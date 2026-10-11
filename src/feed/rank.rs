//! Rule-based ranking: dedupe, topic assignment, scoring (likes-weighted
//! topic affinity, keyword match, recency, popularity, source diversity),
//! and the final pick. Deterministic and model-free.

use super::markup;
use super::parse::Item;
use std::collections::{HashMap, HashSet};

/// A topic of interest with matching keywords and search queries.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Topic {
    pub name: String,
    /// 0..1, how much the user cares (from the interests call).
    #[serde(default = "half")]
    pub weight: f64,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub queries: Vec<String>,
}

fn half() -> f64 {
    0.5
}

/// What the user engaged with, from their feed history.
#[derive(Debug, Clone, Default)]
pub struct Engagement {
    /// Lowercase topic name -> number of liked (or discussed) posts.
    pub topic_likes: HashMap<String, f64>,
    /// Source name -> liked posts.
    pub source_likes: HashMap<String, f64>,
    /// Significant words from liked titles -> count.
    pub liked_words: HashMap<String, f64>,
    /// Lowercase topic name -> posts shown and never engaged with.
    pub topic_ignored: HashMap<String, f64>,
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has", "have", "how", "in",
    "into", "is", "it", "its", "new", "of", "on", "or", "our", "out", "s", "that", "the", "their",
    "this", "to", "up", "was", "we", "what", "when", "why", "will", "with", "you", "your", "after",
    "about", "over", "more", "can", "just", "says", "may", "not", "now", "all", "who", "than",
    "they", "but", "one", "two", "first", "here", "best", "get",
];

/// Lowercase alphanumeric words without stopwords.
pub fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

/// Normalized title used to spot the same story from different sources.
pub fn title_key(title: &str) -> String {
    let mut w = words(title);
    w.truncate(14);
    format!("t:{}", w.join(" "))
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

const TRACKING: &[&str] = &[
    "fbclid", "gclid", "dclid", "mc_cid", "mc_eid", "igshid", "ref", "ref_src", "ref_url", "cmpid",
    "ncid", "ocid", "smid", "taid", "sr_share", "share", "feature", "si", "source", "via",
    "_hsenc", "_hsmi", "mkt_tok", "spm",
];

/// Canonical key for a link: lowercase host without `www.`/`m.`, no
/// fragment, no tracking parameters, sorted query, no trailing slash.
/// YouTube videos collapse to their id.
pub fn url_key(url: &str) -> String {
    let Some((_, authority, rest)) = markup::split_url(url) else {
        return format!("u:{url}");
    };
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or("").to_string();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .or_else(|| host.strip_prefix("mobile."))
        .unwrap_or(&host)
        .to_string();
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if let Some(id) = youtube_id(&host, path, query) {
        return format!("yt:{id}");
    }
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("").to_ascii_lowercase();
            !p.is_empty() && !k.starts_with("utm_") && !TRACKING.contains(&k.as_str())
        })
        .collect();
    params.sort_unstable();
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix("/amp").unwrap_or(path);
    if params.is_empty() {
        format!("u:{host}{path}")
    } else {
        format!("u:{host}{path}?{}", params.join("&"))
    }
}

fn youtube_id(host: &str, path: &str, query: &str) -> Option<String> {
    let id = if host == "youtu.be" {
        path.trim_start_matches('/')
            .split('/')
            .next()
            .map(str::to_string)
    } else if host == "youtube.com" || host.ends_with(".youtube.com") {
        if path == "/watch" {
            query
                .split('&')
                .find_map(|p| p.strip_prefix("v="))
                .map(str::to_string)
        } else {
            path.strip_prefix("/shorts/")
                .or_else(|| path.strip_prefix("/embed/"))
                .or_else(|| path.strip_prefix("/live/"))
                .map(|s| s.split('/').next().unwrap_or("").to_string())
        }
    } else {
        None
    };
    id.filter(|i| {
        (6..=20).contains(&i.len())
            && i.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Recurring community threads ("Weekly Questions Thread") are not posts.
pub fn is_housekeeping(title: &str) -> bool {
    let t = title.to_lowercase();
    t.contains("megathread")
        || (["daily", "weekly", "monthly"].iter().any(|w| t.contains(w))
            && ["thread", "questions", "discussion", "q&a"]
                .iter()
                .any(|w| t.contains(w)))
}

/// Drop items already seen (by link or title) and duplicates within the
/// batch (same link, or titles that share most words). The first
/// occurrence wins, so pass better sources first.
pub fn dedupe(items: Vec<Item>, seen: &HashSet<String>) -> Vec<Item> {
    let mut keys: HashSet<String> = HashSet::new();
    let mut kept_titles: Vec<HashSet<String>> = Vec::new();
    let mut out = Vec::new();
    for it in items {
        let uk = url_key(&it.url);
        let tk = title_key(&it.title);
        if seen.contains(&uk) || seen.contains(&tk) || keys.contains(&uk) || keys.contains(&tk) {
            continue;
        }
        let tw: HashSet<String> = words(&it.title).into_iter().collect();
        if tw.len() >= 4 && kept_titles.iter().any(|k| jaccard(k, &tw) >= 0.7) {
            continue;
        }
        keys.insert(uk);
        keys.insert(tk);
        kept_titles.push(tw);
        out.push(it);
    }
    out
}

/// How many of a topic's keywords (and name words) appear in the text.
fn keyword_hits(topic: &Topic, text_words: &HashSet<String>, text_lower: &str) -> usize {
    let mut hits = 0;
    let name_words = words(&topic.name);
    for k in topic
        .keywords
        .iter()
        .map(|k| k.to_lowercase())
        .chain(name_words)
    {
        let k = k.trim().to_string();
        if k.is_empty() {
            continue;
        }
        let found = if k.contains(' ') {
            text_lower.contains(&k)
        } else {
            text_words.contains(&k)
        };
        if found {
            hits += 1;
        }
    }
    hits
}

/// Give topic-less items the best-matching topic (if any keyword matches).
pub fn assign_topics(items: &mut [Item], topics: &[Topic]) {
    for it in items.iter_mut() {
        let text = format!("{} {}", it.title, it.description);
        let lower = text.to_lowercase();
        let ws: HashSet<String> = words(&text).into_iter().collect();
        if let Some(t) = &it.topic
            && let Some(known) = topics.iter().find(|x| x.name.eq_ignore_ascii_case(t))
        {
            // One spelling per topic, so story groups do not split.
            it.topic = Some(known.name.clone());
            continue;
        }
        let best = topics
            .iter()
            .map(|t| (keyword_hits(t, &ws, &lower), t))
            .filter(|(h, _)| *h > 0)
            .max_by(|a, b| {
                a.0.cmp(&b.0)
                    .then(b.1.weight.total_cmp(&a.1.weight).reverse())
            });
        it.topic = best.map(|(_, t)| t.name.clone()).or(it.topic.take());
    }
}

/// Effective weight of a topic: its stated weight, raised by likes and
/// lowered when its posts keep getting ignored.
pub fn topic_weight(topic: &Topic, e: &Engagement) -> f64 {
    let key = topic.name.to_lowercase();
    let likes = e.topic_likes.get(&key).copied().unwrap_or(0.0).min(6.0);
    let ignored = e.topic_ignored.get(&key).copied().unwrap_or(0.0).min(20.0);
    (topic.weight.clamp(0.0, 1.0) + 0.12 * likes - 0.015 * ignored).clamp(0.05, 1.6)
}

/// Score one item (higher is better). Pure function of its inputs.
pub fn score(it: &Item, topics: &[Topic], e: &Engagement, now_ms: i64) -> f64 {
    let topic = it
        .topic
        .as_ref()
        .and_then(|t| topics.iter().find(|x| x.name.eq_ignore_ascii_case(t)));
    let affinity = topic.map_or(0.15, |t| topic_weight(t, e));
    let text = format!("{} {}", it.title, it.description);
    let lower = text.to_lowercase();
    let ws: HashSet<String> = words(&text).into_iter().collect();
    let kw = topic.map_or(0.0, |t| {
        (keyword_hits(t, &ws, &lower) as f64 / 3.0).min(1.0)
    });
    let title_words: HashSet<String> = words(&it.title).into_iter().collect();
    let liked_overlap = title_words
        .iter()
        .map(|w| e.liked_words.get(w).copied().unwrap_or(0.0).min(3.0))
        .sum::<f64>()
        .min(6.0)
        / 6.0;
    let recency = match it.published_ms {
        Some(t) => {
            let age_h = ((now_ms - t).max(0) as f64) / 3_600_000.0;
            0.5f64.powf(age_h / 36.0)
        }
        None => 0.4,
    };
    let source_like = (e.source_likes.get(&it.source_name).copied().unwrap_or(0.0) / 3.0).min(1.0);
    let has_image = if it.images.is_empty() { 0.0 } else { 1.0 };
    let has_text = if it.description.chars().count() >= 80 {
        1.0
    } else {
        0.0
    };
    // Sources the user follows beat strangers found by topic searches.
    let followed = if it.source_id.is_some() { 1.0 } else { 0.85 };
    followed
        * (0.32 * affinity
            + 0.18 * kw
            + 0.18 * recency
            + 0.10 * it.popularity.clamp(0.0, 1.0)
            + 0.10 * has_image
            + 0.04 * has_text
            + 0.05 * source_like
            + 0.05 * liked_overlap)
}

/// Pick up to `n` items. First the best post of each topic that is
/// competitive (so the day covers what the user cares about), then greedy
/// by score with diminishing returns for repeated sources and topics. At
/// most 3 per source, 5 per topic (story groups hold 1-5 stories), and
/// about a third from topic searches rather than followed sources.
pub fn select(mut scored: Vec<(f64, Item)>, n: usize) -> Vec<(f64, Item)> {
    let mut p = Picker {
        max_search: (n * 3 / 10).max(2),
        ..Default::default()
    };
    // Coverage: each topic's best post, if within reach of the overall best.
    let top = scored.iter().map(|(s, _)| *s).fold(0.0, f64::max);
    let mut topics: Vec<(String, f64)> = Vec::new();
    for (s, it) in &scored {
        if let Some(t) = topic_key(it) {
            match topics.iter_mut().find(|(x, _)| *x == t) {
                Some(e) => e.1 = e.1.max(*s),
                None => topics.push((t, *s)),
            }
        }
    }
    topics.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (t, best) in topics {
        if p.out.len() >= n || best < top * 0.6 {
            break;
        }
        let pick = scored
            .iter()
            .enumerate()
            .filter(|(_, (_, it))| topic_key(it).as_deref() == Some(t.as_str()) && p.eligible(it))
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .map(|(i, _)| i);
        if let Some(i) = pick {
            p.take(scored.swap_remove(i));
        }
    }
    // Then the rest by score, with diminishing returns for repeats.
    while p.out.len() < n {
        let best = scored
            .iter()
            .enumerate()
            .filter(|(_, (_, it))| p.eligible(it))
            .map(|(i, (s, it))| (i, s * p.repeat_factor(it)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let Some((i, _)) = best else {
            break;
        };
        p.take(scored.swap_remove(i));
    }
    // Present in score order.
    p.out.sort_by(|a, b| b.0.total_cmp(&a.0));
    p.out
}

fn topic_key(it: &Item) -> Option<String> {
    it.topic.as_ref().map(|t| t.to_lowercase())
}

#[derive(Default)]
struct Picker {
    out: Vec<(f64, Item)>,
    by_source: HashMap<String, usize>,
    by_topic: HashMap<String, usize>,
    searches: usize,
    max_search: usize,
}

impl Picker {
    const PER_SOURCE: usize = 3;
    const PER_TOPIC: usize = 5;

    fn count_source(&self, it: &Item) -> usize {
        self.by_source.get(&it.source_name).copied().unwrap_or(0)
    }
    fn count_topic(&self, it: &Item) -> usize {
        topic_key(it).map_or(0, |t| self.by_topic.get(&t).copied().unwrap_or(0))
    }
    fn eligible(&self, it: &Item) -> bool {
        self.count_source(it) < Self::PER_SOURCE
            && self.count_topic(it) < Self::PER_TOPIC
            && (it.source_id.is_some() || self.searches < self.max_search)
    }
    fn repeat_factor(&self, it: &Item) -> f64 {
        0.7f64.powi(self.count_source(it) as i32) * 0.78f64.powi(self.count_topic(it) as i32)
    }
    fn take(&mut self, (s, it): (f64, Item)) {
        *self.by_source.entry(it.source_name.clone()).or_default() += 1;
        if let Some(t) = topic_key(&it) {
            *self.by_topic.entry(t).or_default() += 1;
        }
        if it.source_id.is_none() {
            self.searches += 1;
        }
        self.out.push((s, it));
    }
}

/// Whether the item's own description can stand as the post text (no model
/// summary needed): long enough, not cut off, not link soup.
pub fn description_good_enough(it: &Item) -> bool {
    let d = it.description.trim();
    let n = d.chars().count();
    let cut = ["...", "…", "[…]", "[...]", "Read more", "Continue reading"]
        .iter()
        .any(|s| d.ends_with(s));
    let links = d.matches("http").count();
    let hashtags = d.matches('#').count();
    (110..=600).contains(&n)
        && !cut
        && links < 2
        && hashtags < 4
        && d.split_whitespace().count() >= 18
        && !d.eq_ignore_ascii_case(it.title.trim())
}

/// A plain "why you'd like this" line from ranking signals (used when the
/// model did not write one).
pub fn rule_why(it: &Item, topics: &[Topic], e: &Engagement) -> Option<String> {
    let topic = it
        .topic
        .as_ref()
        .and_then(|t| topics.iter().find(|x| x.name.eq_ignore_ascii_case(t)));
    if let Some(t) = topic
        && e.topic_likes
            .get(&t.name.to_lowercase())
            .copied()
            .unwrap_or(0.0)
            >= 1.0
    {
        return Some(format!(
            "You've liked posts about {} lately.",
            t.name.to_lowercase()
        ));
    }
    if e.source_likes.get(&it.source_name).copied().unwrap_or(0.0) >= 1.0 {
        return Some(format!(
            "You've liked things from {} before.",
            it.source_name
        ));
    }
    if let Some(t) = topic {
        return Some(format!("Because you're into {}.", t.name.to_lowercase()));
    }
    if it.source_id.is_some() && !it.source_name.is_empty() {
        return Some(format!("New from {}, one of your sources.", it.source_name));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, src: &str, topic: Option<&str>) -> Item {
        Item {
            url: format!("https://{src}.example/{}", title.replace(' ', "-")),
            title: title.into(),
            source_name: src.into(),
            source_id: Some(format!("id-{src}")),
            topic: topic.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn url_keys_ignore_tracking_and_trivia() {
        assert_eq!(
            url_key("https://www.Example.com/a/b/?utm_source=rss&id=2&fbclid=x&a=1#top"),
            "u:example.com/a/b?a=1&id=2"
        );
        assert_eq!(
            url_key("https://m.example.com/a/b"),
            url_key("http://example.com/a/b/")
        );
        assert_eq!(
            url_key("https://example.com/story/amp"),
            "u:example.com/story"
        );
        assert_eq!(
            url_key("https://www.youtube.com/watch?v=abcDEF12345&t=10"),
            "yt:abcDEF12345"
        );
        assert_eq!(
            url_key("https://youtu.be/abcDEF12345?si=x"),
            "yt:abcDEF12345"
        );
        assert_eq!(
            url_key("https://www.youtube.com/shorts/abcDEF12345"),
            "yt:abcDEF12345"
        );
    }

    #[test]
    fn dedupe_by_link_title_and_similarity() {
        let mut seen = HashSet::new();
        seen.insert(url_key("https://old.example/x"));
        let mut a = item("NASA launches new moon rover mission today", "one", None);
        a.url = "https://one.example/rover?utm_campaign=z".into();
        let mut b = item("NASA launches new moon rover mission", "two", None);
        b.url = "https://two.example/rover".into();
        let mut c = item("Totally different", "three", None);
        c.url = "https://one.example/rover".into();
        let mut d = item("Old", "four", None);
        d.url = "https://old.example/x".into();
        let e = item("Fresh story about gardens in winter", "five", None);
        let out = dedupe(vec![a, b, c, d, e], &seen);
        let titles: Vec<&str> = out.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "NASA launches new moon rover mission today",
                "Fresh story about gardens in winter"
            ]
        );
    }

    #[test]
    fn topics_assignment_and_scoring() {
        let topics = vec![
            Topic {
                name: "Space".into(),
                weight: 0.6,
                keywords: vec!["nasa".into(), "rocket".into(), "moon".into()],
                queries: vec![],
            },
            Topic {
                name: "Gardening".into(),
                weight: 0.4,
                keywords: vec!["garden".into(), "tomato plants".into()],
                queries: vec![],
            },
        ];
        let mut items = vec![
            item("NASA rocket heads to the Moon", "news", None),
            item("Growing tomato plants indoors", "blog", None),
            item("Stock markets wobble", "biz", None),
        ];
        items.push(item("Anything", "src", Some("space")));
        assign_topics(&mut items, &topics);
        assert_eq!(items.pop().unwrap().topic.as_deref(), Some("Space"));
        assert_eq!(items[0].topic.as_deref(), Some("Space"));
        assert_eq!(items[1].topic.as_deref(), Some("Gardening"));
        assert_eq!(items[2].topic, None);

        let now = 2_000_000_000_000;
        let mut e = Engagement::default();
        let base_garden = score(&items[1], &topics, &e, now);
        e.topic_likes.insert("gardening".into(), 4.0);
        assert!(
            score(&items[1], &topics, &e, now) > base_garden,
            "likes raise affinity"
        );
        assert!(score(&items[0], &topics, &e, now) > score(&items[2], &topics, &e, now));

        let mut fresh = items[0].clone();
        fresh.published_ms = Some(now - 3_600_000);
        let mut stale = items[0].clone();
        stale.published_ms = Some(now - 6 * 86_400_000);
        assert!(score(&fresh, &topics, &e, now) > score(&stale, &topics, &e, now));

        e.topic_ignored.insert("space".into(), 20.0);
        assert!(topic_weight(&topics[0], &e) < 0.6);
    }

    #[test]
    fn selection_is_diverse_and_capped() {
        let mut scored = Vec::new();
        for i in 0..10 {
            scored.push((
                1.0 - i as f64 * 0.01,
                item(&format!("A{i}"), "big", Some("Space")),
            ));
        }
        for i in 0..3 {
            scored.push((0.5, item(&format!("B{i}"), "small", Some("Food"))));
        }
        let out = select(scored, 8);
        let big = out.iter().filter(|(_, i)| i.source_name == "big").count();
        assert_eq!(big, 3, "at most 3 per source");
        assert_eq!(out.len(), 6, "only 6 eligible");

        let mut many = Vec::new();
        for i in 0..8 {
            many.push((
                1.0,
                item(&format!("S{i}"), &format!("src{i}"), Some("Space")),
            ));
        }
        assert_eq!(select(many, 8).len(), 5, "at most 5 per topic");

        // Every competitive topic gets a post before a strong topic fills
        // up; searches are capped.
        let mut mixed = Vec::new();
        for i in 0..6 {
            mixed.push((
                1.0,
                item(&format!("Space{i}"), &format!("s{i}"), Some("Space")),
            ));
        }
        mixed.push((0.7, item("Run", "runblog", Some("Running"))));
        mixed.push((0.3, item("Weak", "weak", Some("Weak"))));
        for i in 0..6 {
            let mut it = item(&format!("Found{i}"), &format!("f{i}"), None);
            it.source_id = None;
            mixed.push((0.95, it));
        }
        let out = select(mixed, 10);
        let titles: Vec<&str> = out.iter().map(|(_, i)| i.title.as_str()).collect();
        assert!(titles.contains(&"Run"), "{titles:?}");
        assert_eq!(out.iter().filter(|(_, i)| i.source_id.is_none()).count(), 3);
        assert!(out.windows(2).all(|w| w[0].0 >= w[1].0), "score order");
    }

    #[test]
    fn housekeeping_threads() {
        assert!(is_housekeeping("Weekly Trail Running Shoes Questions"));
        assert!(is_housekeeping("Daily Discussion Thread - October 11"));
        assert!(is_housekeeping("Launch megathread"));
        assert!(!is_housekeeping("Weekly roundup: rockets and rovers"));
    }

    #[test]
    fn description_quality_and_why() {
        let mut it = item("Title", "src", Some("Space"));
        it.description = "short".into();
        assert!(!description_good_enough(&it));
        it.description = "The probe spent six months mapping ice deposits near the lunar south pole, and the team now says the deposits are larger than expected, which matters for future crewed bases.".into();
        assert!(description_good_enough(&it));
        it.description.push_str(" Read more");
        assert!(!description_good_enough(&it));

        let topics = vec![Topic {
            name: "Space".into(),
            weight: 0.5,
            keywords: vec![],
            queries: vec![],
        }];
        let mut e = Engagement::default();
        assert_eq!(
            rule_why(&it, &topics, &e).as_deref(),
            Some("Because you're into space.")
        );
        e.topic_likes.insert("space".into(), 2.0);
        assert_eq!(
            rule_why(&it, &topics, &e).as_deref(),
            Some("You've liked posts about space lately.")
        );
    }
}
