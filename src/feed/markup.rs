//! Lenient markup handling for feeds (XML) and web pages (HTML): a forgiving
//! tokenizer, a tiny element tree for feeds, entity decoding, tag stripping,
//! and URL helpers. Real-world feeds and pages are often malformed, so
//! nothing here ever fails; it does its best and moves on.

/// One markup token. Names are lowercased.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    End {
        name: String,
    },
    /// Decoded text (CDATA is passed through as-is).
    Text(String),
}

/// Longest tag name Iris reads.
const MAX_NAME: usize = 64;
/// Longest URL Iris keeps (anything longer is not a real link).
const MAX_URL: usize = 2048;

/// HTML elements whose content is raw text (never parsed as markup).
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "noscript", "template"];

/// Tokenize `src`. With `html`, the content of `<script>`, `<style>` and
/// similar elements is one raw text token.
pub fn tokenize(src: &str, html: bool) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |out: &mut Vec<Token>, from: usize, to: usize| {
        if to > from {
            let t = &src[from..to];
            if !t.is_empty() {
                out.push(Token::Text(decode_entities(t)));
            }
        }
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            flush(&mut out, text_start, i);
            i = rest.find("-->").map_or(b.len(), |e| i + e + 3);
            text_start = i;
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            flush(&mut out, text_start, i);
            let end = body.find("]]>").unwrap_or(body.len());
            out.push(Token::Text(body[..end].to_string()));
            i += 9 + (end + 3).min(body.len());
            text_start = i;
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            flush(&mut out, text_start, i);
            i = rest.find('>').map_or(b.len(), |e| i + e + 1);
            text_start = i;
        } else if let Some(after) = rest.strip_prefix("</") {
            // A closing tag is a short name, optional spaces, and `>`; look
            // no further than that, so junk like `</</</` stays linear.
            let ab = after.as_bytes();
            let name_len = ab
                .iter()
                .take(MAX_NAME)
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'-' | b'_' | b'.'))
                .count();
            let close = ab[name_len..]
                .iter()
                .take(32)
                .position(|c| *c == b'>')
                .filter(|&p| {
                    ab[name_len..name_len + p]
                        .iter()
                        .all(u8::is_ascii_whitespace)
                });
            match close {
                Some(p) if name_len > 0 && ab[0].is_ascii_alphabetic() => {
                    flush(&mut out, text_start, i);
                    out.push(Token::End {
                        name: after[..name_len].to_ascii_lowercase(),
                    });
                    i += 2 + name_len + p + 1;
                    text_start = i;
                }
                _ => i += 2,
            }
        } else if rest.len() > 1 && rest.as_bytes()[1].is_ascii_alphabetic() {
            // An unterminated tag runs to the end of the input: the rest is
            // text (stopping here keeps malformed input linear).
            let Some((name, attrs, self_closing, len)) = parse_tag(rest) else {
                break;
            };
            flush(&mut out, text_start, i);
            i += len;
            let raw = html && !self_closing && RAW_TEXT.contains(&name.as_str());
            out.push(Token::Start {
                name: name.clone(),
                attrs,
                self_closing,
            });
            if raw {
                let close = format!("</{name}");
                let body = &src[i..];
                let end = find_ci(body, &close).unwrap_or(body.len());
                if end > 0 {
                    out.push(Token::Text(body[..end].to_string()));
                }
                i += end;
                if i < b.len() {
                    i = src[i..].find('>').map_or(b.len(), |e| i + e + 1);
                    out.push(Token::End { name });
                }
            }
            text_start = i;
        } else {
            i += 1;
        }
    }
    flush(&mut out, text_start, b.len());
    out
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// (name, attributes, self-closing, length consumed)
type StartTag = (String, Vec<(String, String)>, bool, usize);

/// Parse a start tag at the beginning of `s` (which starts with `<`).
fn parse_tag(s: &str) -> Option<StartTag> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = s[1..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while i < b.len()
            && (b[i].is_ascii_whitespace() || (b[i] == b'/' && b.get(i + 1) != Some(&b'>')))
        {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        if b[i] == b'>' {
            return Some((name, attrs, false, i + 1));
        }
        if b[i] == b'/' {
            // "/>"
            return Some((name, attrs, true, i + 2));
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = s[start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = s[vs..]
                    .bytes()
                    .position(|c| c == q)
                    .map_or(b.len(), |p| vs + p);
                value = decode_entities(&s[vs..ve]);
                i = (ve + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = decode_entities(&s[vs..i]);
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
}

/// Attribute value by (lowercase) name.
pub fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ------------------------------------------------------------- element tree

/// A minimal element tree for feeds.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    /// Direct text content (concatenated). The same text also appears,
    /// in document order, as `#text` children.
    pub text: String,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        attr(&self.attrs, name)
    }
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    /// Trimmed direct text of a child, if non-empty.
    pub fn child_text(&self, name: &str) -> Option<String> {
        self.child(name)
            .map(|c| c.all_text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }
    /// Whether the node has element children (not just text).
    pub fn has_elements(&self) -> bool {
        self.children.iter().any(|c| c.name != "#text")
    }
    /// Text of this node and all descendants, in document order. Block
    /// elements are separated by a space.
    pub fn all_text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            if c.name == "#text" {
                s.push_str(&c.text);
            } else {
                s.push_str(&c.all_text());
                if is_block(&c.name) {
                    s.push(' ');
                }
            }
        }
        s
    }
}

/// Build an element tree from XML. Unclosed elements are closed at the
/// end; stray end tags are ignored. Returns a synthetic root.
pub fn parse_xml(src: &str) -> Node {
    let mut stack: Vec<Node> = vec![Node {
        name: "#root".into(),
        ..Default::default()
    }];
    const MAX_DEPTH: usize = 64;
    for tok in tokenize(src, false) {
        match tok {
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                let node = Node {
                    name,
                    attrs,
                    ..Default::default()
                };
                if self_closing || stack.len() >= MAX_DEPTH {
                    stack.last_mut().expect("root").children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Token::End { name } => {
                if let Some(pos) = stack.iter().rposition(|n| n.name == name)
                    && pos > 0
                {
                    while stack.len() > pos {
                        let n = stack.pop().expect("non-empty");
                        stack.last_mut().expect("root").children.push(n);
                    }
                }
            }
            Token::Text(t) => {
                let top = stack.last_mut().expect("root");
                top.text.push_str(&t);
                top.children.push(Node {
                    name: "#text".into(),
                    text: t,
                    ..Default::default()
                });
            }
        }
    }
    while stack.len() > 1 {
        let n = stack.pop().expect("non-empty");
        stack.last_mut().expect("root").children.push(n);
    }
    stack.pop().expect("root")
}

// ----------------------------------------------------------------- entities

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "middot" => "·",
        "bull" => "•",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "times" => "×",
        "eacute" => "é",
        "egrave" => "è",
        "aacute" => "á",
        "agrave" => "à",
        "iacute" => "í",
        "oacute" => "ó",
        "uacute" => "ú",
        "ntilde" => "ñ",
        "ouml" => "ö",
        "uuml" => "ü",
        "auml" => "ä",
        "szlig" => "ß",
        "ccedil" => "ç",
        "shy" => "",
        "zwj" => "\u{200d}",
        "zwnj" => "\u{200c}",
        _ => return None,
    })
}

/// Decode character references (`&amp;`, `&#39;`, `&#x27;`, common named
/// entities). Unknown references are kept as written.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let end = rest[1..]
            .find(|c: char| c == ';' || c == '&' || c.is_whitespace() || c == '<')
            .map(|e| e + 1);
        let decoded = end
            .filter(|&e| rest.as_bytes().get(e) == Some(&b';'))
            .and_then(|e| {
                let name = &rest[1..e];
                let ch = if let Some(num) = name.strip_prefix('#') {
                    let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                    code.and_then(char::from_u32).map(String::from)
                } else {
                    named_entity(name).map(String::from)
                };
                ch.map(|c| (c, e + 1))
            });
        match decoded {
            Some((c, len)) => {
                out.push_str(&c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Plain text from an HTML fragment: tags removed, block elements become
/// spaces, whitespace collapsed. Scripts and styles are dropped.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(4096));
    let mut skip = 0usize;
    for tok in tokenize(html, true) {
        match tok {
            Token::Start {
                name, self_closing, ..
            } => {
                if RAW_TEXT.contains(&name.as_str()) && !self_closing {
                    skip += 1;
                }
                if is_block(&name) {
                    out.push(' ');
                }
            }
            Token::End { name } => {
                if RAW_TEXT.contains(&name.as_str()) {
                    skip = skip.saturating_sub(1);
                }
                if is_block(&name) {
                    out.push(' ');
                }
            }
            Token::Text(t) if skip == 0 => out.push_str(&t),
            Token::Text(_) => {}
        }
    }
    collapse_ws(&out)
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "br"
            | "div"
            | "li"
            | "ul"
            | "ol"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "tr"
            | "td"
            | "th"
            | "table"
            | "blockquote"
            | "section"
            | "article"
            | "figure"
            | "figcaption"
            | "hr"
    )
}

/// Collapse runs of whitespace (including NBSP) into single spaces.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{a0}' {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// `src` of every `<img>` in an HTML fragment, in order.
pub fn img_srcs(html: &str) -> Vec<String> {
    tokenize(html, true)
        .into_iter()
        .filter_map(|t| match t {
            Token::Start { name, attrs, .. } if name == "img" => attr(&attrs, "src")
                .or_else(|| attr(&attrs, "data-src"))
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// `(href, text)` of every link in an HTML fragment.
pub fn links(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut open: Option<(String, String)> = None;
    for t in tokenize(html, true) {
        match t {
            Token::Start { name, attrs, .. } if name == "a" => {
                if let Some(o) = open.take() {
                    out.push(o);
                }
                open = attr(&attrs, "href").map(|h| (h.to_string(), String::new()));
            }
            Token::End { name } if name == "a" => {
                if let Some((h, text)) = open.take() {
                    out.push((h, collapse_ws(&text)));
                }
            }
            Token::Text(s) => {
                if let Some((_, text)) = open.as_mut() {
                    text.push_str(&s);
                }
            }
            _ => {}
        }
    }
    if let Some(o) = open {
        out.push(o);
    }
    out
}

// --------------------------------------------------------------------- URLs

/// (scheme, authority, rest) of an absolute http(s) URL. `rest` starts with
/// `/`, `?`, `#`, or is empty.
pub fn split_url(url: &str) -> Option<(String, &str, &str)> {
    let (scheme, after) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..end];
    if authority.is_empty() || authority.contains(char::is_whitespace) {
        return None;
    }
    Some((scheme, authority, &after[end..]))
}

/// Lowercase host of an http(s) URL (no port, no credentials).
pub fn host_of(url: &str) -> Option<String> {
    let (_, authority, _) = split_url(url)?;
    let host = authority.rsplit('@').next()?;
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]"))?
    } else {
        host.split(':').next()?.to_string()
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `scheme://authority` of an http(s) URL.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, authority, _) = split_url(url)?;
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// Resolve `href` against `base`. Only http(s) results are returned.
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.len() > MAX_URL || href.contains(['\n', '\r', '\t']) {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return split_url(href).map(|_| href.to_string());
    }
    if lower.contains(':')
        && !lower.starts_with('/')
        && !lower.starts_with('?')
        && !lower.starts_with('#')
    {
        // Another scheme (javascript:, data:, mailto:) unless the colon is
        // inside a path segment after a slash.
        let colon = lower.find(':').unwrap_or(0);
        if lower.find('/').is_none_or(|s| colon < s) {
            return None;
        }
    }
    let (scheme, authority, rest) = split_url(base)?;
    if let Some(r) = href.strip_prefix("//") {
        return split_url(&format!("{scheme}://{r}")).map(|_| format!("{scheme}://{r}"));
    }
    if href.starts_with('#') {
        return None;
    }
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let base_path = if rest[..path_end].is_empty() {
        "/"
    } else {
        &rest[..path_end]
    };
    if href.starts_with('?') {
        return Some(format!("{scheme}://{authority}{base_path}{href}"));
    }
    let (path_part, suffix) = match href.find(['?', '#']) {
        Some(p) => (&href[..p], &href[p..]),
        None => (href, ""),
    };
    let joined = if path_part.starts_with('/') {
        path_part.to_string()
    } else {
        let dir = &base_path[..base_path.rfind('/').map_or(0, |p| p + 1)];
        format!("{dir}{path_part}")
    };
    Some(format!(
        "{scheme}://{authority}{}{suffix}",
        normalize_path(&joined)
    ))
}

fn normalize_path(path: &str) -> String {
    let mut segs: Vec<&str> = Vec::new();
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    let mut out = format!("/{}", segs.join("/"));
    if trailing && out.len() > 1 {
        out.push('/');
    }
    out
}

/// Percent-encode a query value.
pub fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_messy_markup() {
        let toks = tokenize(
            "<p class=a data-x='1'>Hi &amp; bye<br/><!-- c --><![CDATA[<raw>]]></P><x",
            false,
        );
        assert_eq!(
            toks[0],
            Token::Start {
                name: "p".into(),
                attrs: vec![("class".into(), "a".into()), ("data-x".into(), "1".into())],
                self_closing: false
            }
        );
        assert_eq!(toks[1], Token::Text("Hi & bye".into()));
        assert!(matches!(&toks[2], Token::Start { name, self_closing: true, .. } if name == "br"));
        assert_eq!(toks[3], Token::Text("<raw>".into()));
        assert_eq!(toks[4], Token::End { name: "p".into() });
    }

    #[test]
    fn html_raw_text_elements() {
        let toks = tokenize("<script>if (a<b) {}</script><style>p{}</style>ok", true);
        assert_eq!(toks[1], Token::Text("if (a<b) {}".into()));
        assert_eq!(
            toks[2],
            Token::End {
                name: "script".into()
            }
        );
        assert_eq!(toks.last(), Some(&Token::Text("ok".into())));
        assert_eq!(
            strip_tags("<p>One</p><script>x()</script><p>Two&nbsp; three</p>"),
            "One Two three"
        );
    }

    #[test]
    fn xml_tree_is_lenient() {
        let root = parse_xml(
            "<?xml version='1.0'?><rss><channel><title>T</title><item><title>A</title><link>u</link></item><item><title>B</channel></rss>",
        );
        let ch = root.child("rss").and_then(|r| r.child("channel")).unwrap();
        assert_eq!(ch.child_text("title").as_deref(), Some("T"));
        let items: Vec<_> = ch.children_named("item").collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].child_text("title").as_deref(), Some("B"));
    }

    #[test]
    fn mixed_content_keeps_order_and_bad_markup_stays_linear() {
        let root = parse_xml("<content><p>A <b>bright</b> comet</p><p>tonight</p></content>");
        assert_eq!(
            collapse_ws(&root.child("content").unwrap().all_text()),
            "A bright comet tonight"
        );
        // 200k unterminated tags, or broken closing tags: must not rescan
        // the rest at every `<`.
        for junk in ["<a".repeat(200_000), format!("{}>", "</".repeat(200_000))] {
            let t0 = std::time::Instant::now();
            let toks = tokenize(&junk, true);
            assert!(t0.elapsed() < std::time::Duration::from_secs(2));
            assert_eq!(toks.len(), 1);
        }
        assert_eq!(
            resolve("https://a.example/", &format!("/{}", "x".repeat(3000))),
            None
        );
    }

    #[test]
    fn entities() {
        assert_eq!(
            decode_entities("a &lt;b&gt; &#39;c&#x27; &rsquo; &bogus; & d"),
            "a <b> 'c' ’ &bogus; & d"
        );
        assert_eq!(decode_entities("AT&T"), "AT&T");
    }

    #[test]
    fn url_resolution() {
        let base = "https://Example.com/a/b/page.html?q=1";
        assert_eq!(
            resolve(base, "img.png").as_deref(),
            Some("https://Example.com/a/b/img.png")
        );
        assert_eq!(
            resolve(base, "../c.png").as_deref(),
            Some("https://Example.com/a/c.png")
        );
        assert_eq!(
            resolve(base, "/x?y=1").as_deref(),
            Some("https://Example.com/x?y=1")
        );
        assert_eq!(
            resolve(base, "//cdn.example.org/i.jpg").as_deref(),
            Some("https://cdn.example.org/i.jpg")
        );
        assert_eq!(
            resolve(base, "?p=2").as_deref(),
            Some("https://Example.com/a/b/page.html?p=2")
        );
        assert_eq!(resolve(base, "javascript:alert(1)"), None);
        assert_eq!(resolve(base, "data:image/png;base64,xx"), None);
        assert_eq!(resolve(base, "#top"), None);
        assert_eq!(
            resolve(base, "HTTPS://other.example/x").as_deref(),
            Some("HTTPS://other.example/x")
        );
        assert_eq!(
            host_of("https://user@WWW.Example.com:8443/x").as_deref(),
            Some("www.example.com")
        );
        assert_eq!(
            origin_of("https://Example.com:8443/x").as_deref(),
            Some("https://example.com:8443")
        );
        assert_eq!(host_of("ftp://example.com/"), None);
    }

    #[test]
    fn images_and_links_in_fragments() {
        let html = r#"<a href="https://r.example/c">[comments]</a> <a href="https://site.example/story">[link]</a><img src="/i.jpg"><img data-src="https://x/y.png">"#;
        assert_eq!(img_srcs(html), vec!["/i.jpg", "https://x/y.png"]);
        let l = links(html);
        assert_eq!(
            l[1],
            (
                "https://site.example/story".to_string(),
                "[link]".to_string()
            )
        );
        assert_eq!(encode_query("a b&c/é"), "a%20b%26c%2F%C3%A9");
    }
}
