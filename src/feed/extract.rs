//! Page metadata from OpenGraph, Twitter cards, JSON-LD, and plain HTML:
//! title, description, site name, favicon, the page's own images (never
//! generated ones), and, for urgent alerts, a product price or event date.

use super::markup::{self, Token, attr, collapse_ws, resolve};
use super::parse::{Event, Price, looks_like_icon, parse_date};
use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    /// Original image URLs, best first.
    pub images: Vec<String>,
    pub favicon: Option<String>,
    pub canonical: Option<String>,
    pub published_ms: Option<i64>,
    pub price: Option<Price>,
    pub event: Option<Event>,
}

const MAX_IMAGES: usize = 6;

fn non_empty(s: &str) -> Option<String> {
    let t = collapse_ws(s);
    (!t.is_empty()).then_some(t)
}

/// Extract metadata from an HTML page fetched from `url`.
pub fn extract(html: &str, url: &str) -> PageMeta {
    let mut m = PageMeta::default();
    let mut og_images: Vec<String> = Vec::new();
    let mut tw_images: Vec<String> = Vec::new();
    let mut html_title = None;
    let mut meta_desc = None;
    let mut og_desc = None;
    let mut tw_desc = None;
    let mut og_title = None;
    let mut tw_title = None;
    let mut icons: Vec<(i32, String)> = Vec::new();
    let mut price_amount: Option<String> = None;
    let mut price_currency: Option<String> = None;
    let mut ld_blocks: Vec<String> = Vec::new();
    let mut base = url.to_string();

    let tokens = markup::tokenize(html, true);
    let mut i = 0;
    while i < tokens.len() {
        if let Token::Start { name, attrs, .. } = &tokens[i] {
            match name.as_str() {
                "base" => {
                    if let Some(b) = attr(attrs, "href").and_then(|h| resolve(url, h)) {
                        base = b;
                    }
                }
                "title" if html_title.is_none() => {
                    if let Some(Token::Text(t)) = tokens.get(i + 1) {
                        html_title = non_empty(t);
                    }
                }
                "meta" => {
                    let key = attr(attrs, "property")
                        .or_else(|| attr(attrs, "name"))
                        .or_else(|| attr(attrs, "itemprop"))
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let content = attr(attrs, "content").unwrap_or("");
                    match key.as_str() {
                        "og:title" => og_title = og_title.or(non_empty(content)),
                        "twitter:title" => tw_title = tw_title.or(non_empty(content)),
                        "og:description" => og_desc = og_desc.or(non_empty(content)),
                        "twitter:description" => tw_desc = tw_desc.or(non_empty(content)),
                        "description" => meta_desc = meta_desc.or(non_empty(content)),
                        "og:site_name" | "application-name" if m.site_name.is_none() => {
                            m.site_name = non_empty(content)
                        }
                        "og:image" | "og:image:url" | "og:image:secure_url" => {
                            if let Some(u) = resolve(&base, content) {
                                og_images.push(u);
                            }
                        }
                        "twitter:image" | "twitter:image:src" | "image" | "thumbnailurl" => {
                            if let Some(u) = resolve(&base, content) {
                                tw_images.push(u);
                            }
                        }
                        "article:published_time"
                        | "og:published_time"
                        | "date"
                        | "datepublished" => {
                            m.published_ms = m.published_ms.or_else(|| parse_date(content));
                        }
                        "product:price:amount" | "og:price:amount" => {
                            price_amount = price_amount.or(non_empty(content))
                        }
                        "product:price:currency" | "og:price:currency" => {
                            price_currency = price_currency.or(non_empty(content))
                        }
                        _ => {}
                    }
                }
                "link" => {
                    let rel = attr(attrs, "rel").unwrap_or("").to_ascii_lowercase();
                    let href = attr(attrs, "href").and_then(|h| resolve(&base, h));
                    if let Some(h) = href {
                        let rels: Vec<&str> = rel.split_whitespace().collect();
                        if rels.contains(&"canonical") {
                            m.canonical = m.canonical.take().or(Some(h));
                        } else if rels.contains(&"apple-touch-icon") || rels.contains(&"icon") {
                            if h.to_ascii_lowercase()
                                .split('?')
                                .next()
                                .unwrap_or("")
                                .ends_with(".svg")
                            {
                                // Skip SVG icons (scripts can hide in them).
                            } else {
                                let size = attr(attrs, "sizes")
                                    .and_then(|s| s.split(['x', 'X']).next())
                                    .and_then(|n| n.trim().parse::<i32>().ok())
                                    .unwrap_or(if rels.contains(&"apple-touch-icon") {
                                        180
                                    } else {
                                        32
                                    });
                                icons.push((size, h));
                            }
                        }
                    }
                }
                "script" => {
                    let ty = attr(attrs, "type").unwrap_or("").to_ascii_lowercase();
                    if ty.contains("ld+json")
                        && let Some(Token::Text(t)) = tokens.get(i + 1)
                        && ld_blocks.len() < 8
                    {
                        ld_blocks.push(t.clone());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    let mut ld = LdData::default();
    for block in &ld_blocks {
        if let Some(v) = parse_ld(block) {
            walk_ld(&v, &base, &mut ld, 0);
        }
    }

    m.title = og_title.or(tw_title).or(ld.headline.clone()).or(html_title);
    m.description = og_desc.or(tw_desc).or(ld.description.clone()).or(meta_desc);
    if m.site_name.is_none() {
        m.site_name = ld.publisher.clone();
    }
    m.published_ms = m.published_ms.or(ld.published_ms);
    let mut images = Vec::new();
    for u in og_images.into_iter().chain(ld.images).chain(tw_images) {
        if !images.contains(&u) && !looks_like_icon(&u) && images.len() < MAX_IMAGES {
            images.push(u);
        }
    }
    m.images = images;
    icons.sort_by_key(|(size, _)| {
        // Prefer icons near 64-192px.
        let s = *size;
        if (48..=256).contains(&s) {
            0
        } else {
            1 + (s - 96).abs()
        }
    });
    m.favicon = icons
        .into_iter()
        .map(|(_, h)| h)
        .next()
        .or_else(|| markup::origin_of(url).map(|o| format!("{o}/favicon.ico")));
    m.price = ld.price.or_else(|| {
        let amount = parse_amount(&price_amount?)?;
        Some(Price {
            amount,
            currency: price_currency.unwrap_or_default().to_ascii_uppercase(),
        })
    });
    m.event = ld.event;
    m
}

/// "1,299.00" / "19.99" -> number. Rejects nonsense.
fn parse_amount(s: &str) -> Option<f64> {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v: f64 = cleaned.parse().ok()?;
    (v.is_finite() && v > 0.0 && v < 10_000_000.0).then_some(v)
}

#[derive(Default)]
struct LdData {
    headline: Option<String>,
    description: Option<String>,
    publisher: Option<String>,
    images: Vec<String>,
    published_ms: Option<i64>,
    price: Option<Price>,
    event: Option<Event>,
}

fn parse_ld(block: &str) -> Option<Value> {
    let t = block.trim();
    let t = t.strip_prefix("<!--").unwrap_or(t);
    let t = t.strip_suffix("-->").unwrap_or(t).trim();
    serde_json::from_str(t).ok()
}

fn types(v: &Value) -> Vec<String> {
    match &v["@type"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn ld_images(v: &Value, base: &str, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if let Some(u) = resolve(base, s)
                && !out.contains(&u)
            {
                out.push(u);
            }
        }
        Value::Array(a) => a.iter().take(6).for_each(|x| ld_images(x, base, out)),
        Value::Object(o) => {
            if let Some(u) = o.get("url").or_else(|| o.get("contentUrl")) {
                ld_images(u, base, out);
            }
        }
        _ => {}
    }
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => non_empty(&markup::strip_tags(s)),
        Value::Object(o) => o.get("name").and_then(text_of),
        Value::Array(a) => a.first().and_then(text_of),
        _ => None,
    }
}

fn walk_ld(v: &Value, base: &str, ld: &mut LdData, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(a) => {
            for x in a.iter().take(30) {
                walk_ld(x, base, ld, depth + 1);
            }
        }
        Value::Object(o) => {
            if let Some(g) = o.get("@graph") {
                walk_ld(g, base, ld, depth + 1);
            }
            let ts = types(v);
            let is = |t: &str| ts.iter().any(|x| x.eq_ignore_ascii_case(t));
            let article = [
                "Article",
                "NewsArticle",
                "BlogPosting",
                "Report",
                "VideoObject",
                "Recipe",
                "Review",
            ]
            .iter()
            .any(|t| is(t))
                || ts.iter().any(|t| t.ends_with("Article"));
            if article {
                ld.headline = ld
                    .headline
                    .take()
                    .or_else(|| text_of(&v["headline"]).or_else(|| text_of(&v["name"])));
                ld.description = ld.description.take().or_else(|| text_of(&v["description"]));
                ld.publisher = ld.publisher.take().or_else(|| text_of(&v["publisher"]));
                ld.published_ms = ld
                    .published_ms
                    .or_else(|| v["datePublished"].as_str().and_then(parse_date))
                    .or_else(|| v["uploadDate"].as_str().and_then(parse_date));
                ld_images(&v["image"], base, &mut ld.images);
                ld_images(&v["thumbnailUrl"], base, &mut ld.images);
            }
            if is("Product") {
                ld.headline = ld.headline.take().or_else(|| text_of(&v["name"]));
                ld.description = ld.description.take().or_else(|| text_of(&v["description"]));
                ld_images(&v["image"], base, &mut ld.images);
                if ld.price.is_none() {
                    ld.price = offer_price(&v["offers"]);
                }
            }
            if ts.iter().any(|t| t.ends_with("Event")) && ld.event.is_none() {
                let start = v["startDate"].as_str().and_then(parse_date);
                if let Some(start_ms) = start {
                    let loc = &v["location"];
                    let location = [
                        text_of(&loc["name"]),
                        text_of(&loc["address"]["addressLocality"]),
                        loc["address"].as_str().and_then(non_empty),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                    ld.event = Some(Event { start_ms, location });
                    ld.headline = ld.headline.take().or_else(|| text_of(&v["name"]));
                    ld.description = ld.description.take().or_else(|| text_of(&v["description"]));
                    ld_images(&v["image"], base, &mut ld.images);
                }
            }
            if ts.is_empty() || is("WebPage") || is("ItemPage") {
                for key in ["mainEntity", "about"] {
                    if let Some(x) = o.get(key) {
                        walk_ld(x, base, ld, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

fn offer_price(offers: &Value) -> Option<Price> {
    match offers {
        Value::Array(a) => a.iter().find_map(offer_price),
        Value::Object(_) => {
            let amount = ["price", "lowPrice"]
                .iter()
                .find_map(|k| match &offers[*k] {
                    Value::Number(n) => n.as_f64().filter(|v| *v > 0.0),
                    Value::String(s) => parse_amount(s),
                    _ => None,
                })?;
            let currency = offers["priceCurrency"]
                .as_str()
                .unwrap_or("")
                .to_ascii_uppercase();
            Some(Price { amount, currency })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opengraph_twitter_and_icons() {
        let html = r#"<!doctype html><html><head>
<title>Fallback title</title>
<meta property="og:title" content="Big &amp; bright comet">
<meta property="og:description" content="Astronomers expect a show this week.">
<meta property="og:site_name" content="Example Science">
<meta property="og:image" content="/img/comet-1200.jpg">
<meta property="og:image" content="https://cdn.example.com/site-logo.png">
<meta name="twitter:image" content="https://cdn.example.com/comet-tw.jpg">
<meta property="article:published_time" content="2026-10-10T14:36:15Z">
<link rel="icon" href="/favicon-32.png" sizes="32x32">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<link rel="icon" href="/icon.svg">
<link rel="canonical" href="https://example.com/comet">
</head><body><p>Body</p></body></html>"#;
        let m = extract(html, "https://example.com/news/comet?utm_source=x");
        assert_eq!(m.title.as_deref(), Some("Big & bright comet"));
        assert_eq!(
            m.description.as_deref(),
            Some("Astronomers expect a show this week.")
        );
        assert_eq!(m.site_name.as_deref(), Some("Example Science"));
        assert_eq!(
            m.images,
            vec![
                "https://example.com/img/comet-1200.jpg",
                "https://cdn.example.com/comet-tw.jpg"
            ]
        );
        assert_eq!(
            m.favicon.as_deref(),
            Some("https://example.com/apple-touch-icon.png")
        );
        assert_eq!(m.canonical.as_deref(), Some("https://example.com/comet"));
        assert_eq!(m.published_ms, Some(1_791_642_975_000));
        assert!(m.price.is_none() && m.event.is_none());
    }

    #[test]
    fn json_ld_article_product_and_event() {
        let html = r#"<html><head><title>T</title>
<script type="application/ld+json">{"@context":"https://schema.org","@graph":[
 {"@type":"WebSite","name":"Shop"},
 {"@type":"Product","name":"Trail shoe","description":"Light and grippy.","image":["https://shop.example/shoe.jpg"],
  "offers":{"@type":"Offer","price":"119.95","priceCurrency":"usd"}}]}</script>
<script type="application/ld+json">{"@type":"MusicEvent","name":"Band live","startDate":"2026-10-20T20:00:00-07:00",
 "location":{"@type":"Place","name":"The Hall","address":{"addressLocality":"Springfield"}}}</script>
</head></html>"#;
        let m = extract(html, "https://shop.example/p/1");
        assert_eq!(m.title.as_deref(), Some("Trail shoe"));
        assert_eq!(m.description.as_deref(), Some("Light and grippy."));
        assert_eq!(m.images, vec!["https://shop.example/shoe.jpg"]);
        assert_eq!(
            m.price,
            Some(Price {
                amount: 119.95,
                currency: "USD".into()
            })
        );
        let e = m.event.unwrap();
        assert_eq!(e.location, "The Hall, Springfield");
        assert_eq!(e.start_ms, parse_date("2026-10-21T03:00:00Z").unwrap());
        assert_eq!(
            m.favicon.as_deref(),
            Some("https://shop.example/favicon.ico")
        );

        let article = r#"<script type="application/ld+json"><!--
{"@type":["NewsArticle"],"headline":"Rain returns","publisher":{"@type":"Organization","name":"Daily Example"},
 "image":{"@type":"ImageObject","url":"https://news.example/rain.jpg"},"datePublished":"2026-10-09"}
--></script>"#;
        let m = extract(article, "https://news.example/a");
        assert_eq!(m.title.as_deref(), Some("Rain returns"));
        assert_eq!(m.site_name.as_deref(), Some("Daily Example"));
        assert_eq!(m.images, vec!["https://news.example/rain.jpg"]);
        assert!(m.published_ms.is_some());
    }

    #[test]
    fn og_price_tags_and_garbage() {
        let html = r#"<meta property="product:price:amount" content="1,299.00"><meta property="product:price:currency" content="EUR">"#;
        assert_eq!(
            extract(html, "https://s.example/x").price,
            Some(Price {
                amount: 1299.0,
                currency: "EUR".into()
            })
        );
        let m = extract("<<<>>> not html at all <meta", "https://s.example/");
        assert_eq!(m.title, None);
        assert!(m.images.is_empty());
    }
}
