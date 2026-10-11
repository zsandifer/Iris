//! Polite HTTP for the feed pipeline.
//!
//! - An honest `User-Agent` naming Iris as a personal feed reader.
//! - robots.txt is honored for crawling: every web page fetched for a
//!   preview and every image downloaded. Feeds and documented APIs the user
//!   (or Iris on their behalf) subscribed to are read the way any feed
//!   reader reads them: once a day.
//! - At most one request per host per second (longer when robots.txt asks
//!   for a crawl delay, and for hosts with strict published limits).
//! - Connect/overall timeouts and per-request size caps.
//! - Only http(s), only public addresses: names that resolve to loopback,
//!   private, link-local, or carrier-grade NAT (tailnet) addresses are
//!   refused, on every redirect hop, so a feed cannot point Iris at the
//!   home network.

use super::markup;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const USER_AGENT: &str = concat!(
    "IrisFeed/",
    env!("CARGO_PKG_VERSION"),
    " (self-hosted personal feed reader)"
);
/// Product token matched against robots.txt `User-agent` lines.
const ROBOTS_TOKEN: &str = "irisfeed";
const MAX_REDIRECTS: usize = 5;
const DEFAULT_GAP: Duration = Duration::from_millis(1000);
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(10);
/// Longest wait Iris accepts from a rate-limit header before giving up on
/// a host for this run.
const MAX_RATE_WAIT: Duration = Duration::from_secs(75);
/// How far a host that asked for a longer pause is pushed back (beyond any
/// run).
const BLOCKED_FOR: Duration = Duration::from_secs(24 * 3600);

/// Crawl (robots.txt applies) or Feed (a subscribed feed or API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Crawl,
    Feed,
}

#[derive(Debug)]
pub enum FetchError {
    BadUrl,
    Robots,
    TooLarge,
    Status(u16),
    Net(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::BadUrl => write!(f, "not a public http(s) URL"),
            FetchError::Robots => write!(f, "robots.txt asks crawlers not to fetch this"),
            FetchError::TooLarge => write!(f, "response too large"),
            FetchError::Status(s) => write!(f, "HTTP {s}"),
            FetchError::Net(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub status: u16,
    pub final_url: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    /// Body as text (UTF-8, or Latin-1 when the bytes are not UTF-8).
    pub fn text(&self) -> String {
        match std::str::from_utf8(&self.body) {
            Ok(s) => s.to_string(),
            Err(_) => self.body.iter().map(|&b| b as char).collect(),
        }
    }
}

/// One request.
pub struct Request<'a> {
    pub url: &'a str,
    pub mode: Mode,
    pub accept: &'a str,
    pub max_bytes: u64,
    /// Keep the first `max_bytes` instead of failing (HTML heads).
    pub truncate: bool,
    pub bearer: Option<&'a str>,
}

impl<'a> Request<'a> {
    pub fn new(url: &'a str, mode: Mode, accept: &'a str, max_bytes: u64) -> Self {
        Self {
            url,
            mode,
            accept,
            max_bytes,
            truncate: false,
            bearer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Stats {
    pub requests: u32,
    pub bytes: u64,
    pub robots_blocked: u32,
    pub errors: u32,
}

/// Shared by all fetches of one pipeline run.
pub struct Fetcher {
    agent: ureq::Agent,
    robots: Mutex<HashMap<String, Arc<Robots>>>,
    next_slot: Mutex<HashMap<String, Instant>>,
    stats: Mutex<Stats>,
    /// Minimum spacing between requests to one host.
    gap: Duration,
    /// Reddit refused its JSON API this run; use its feeds instead.
    pub reddit_json_blocked: std::sync::atomic::AtomicBool,
}

/// Resolver that drops non-public addresses (and fails when none remain).
#[derive(Debug, Default)]
struct PublicOnly {
    inner: ureq::unversioned::resolver::DefaultResolver,
    allow_private: bool,
}

impl ureq::unversioned::resolver::Resolver for PublicOnly {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let all = self.inner.resolve(uri, config, timeout)?;
        let mut out = self.empty();
        for a in all.iter() {
            if self.allow_private || is_public(a.ip()) {
                out.push(*a);
            }
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

/// Whether an address is on the public internet.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link-local
                || (seg[0] == 0x64 && seg[1] == 0xff9b) // NAT64
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64/10 CGNAT (tailnets)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        || o[0] >= 240)
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    pub fn new() -> Self {
        Self::build(false)
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::build(true)
    }

    fn build(allow_private: bool) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(8)))
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(0)
            // Never through an ambient proxy: the address check must see
            // the real destination.
            .proxy(None)
            .http_status_as_error(false)
            .max_idle_connections(8)
            .max_idle_connections_per_host(2)
            .build();
        let agent = ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::default(),
            PublicOnly {
                inner: Default::default(),
                allow_private,
            },
        );
        Self {
            agent,
            robots: Mutex::default(),
            next_slot: Mutex::default(),
            stats: Mutex::default(),
            gap: if allow_private {
                Duration::ZERO
            } else {
                DEFAULT_GAP
            },
            reddit_json_blocked: Default::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.stats.lock().unwrap_or_else(|p| p.into_inner()));
    }

    /// Minimum spacing between requests to one host.
    fn gap_for(&self, host: &str, origin: &str) -> Duration {
        // Reddit is strict with unauthenticated clients; its rate-limit
        // headers (handled in `raw`) set the real pace.
        let base = if host == "reddit.com" || host.ends_with(".reddit.com") {
            Duration::from_secs(6)
        } else {
            self.gap
        };
        let delay = self
            .robots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(origin)
            .and_then(|r| r.crawl_delay)
            .map(|d| d.min(MAX_CRAWL_DELAY))
            .unwrap_or_default();
        base.max(delay)
    }

    /// Wait for this host's next free slot. Re-checks after sleeping, since
    /// a rate-limit header from another request may have moved it. Fails
    /// when the host asked for a longer pause than Iris waits.
    fn wait_turn(&self, host: &str, origin: &str) -> Result<(), FetchError> {
        let gap = self.gap_for(host, origin);
        loop {
            let wait = {
                let mut slots = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
                let now = Instant::now();
                match slots.get(host) {
                    Some(at) if *at > now => *at - now,
                    _ => {
                        slots.insert(host.to_string(), now + gap);
                        return Ok(());
                    }
                }
            };
            if wait > MAX_RATE_WAIT + Duration::from_secs(1) {
                return Err(FetchError::Net(
                    "the site asked Iris to come back later".into(),
                ));
            }
            std::thread::sleep(wait);
        }
    }

    fn robots_for(&self, origin: &str) -> Arc<Robots> {
        if let Some(r) = self
            .robots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(origin)
        {
            return r.clone();
        }
        let url = format!("{origin}/robots.txt");
        // Redirects are followed (RFC 9309, 2.3.1.2); the rules apply to
        // the original origin.
        let rules = match self.get(&Request::new(&url, Mode::Feed, "text/plain", 512 * 1024)) {
            Ok(r) => Robots::parse(&r.text(), ROBOTS_TOKEN),
            // No robots.txt (or access denied to it): everything is allowed.
            Err(FetchError::Status(s)) if (400..500).contains(&s) => Robots::default(),
            // Server errors, too many redirects, network failures: stay
            // away this run.
            _ => Robots::disallow_all(),
        };
        let rules = Arc::new(rules);
        self.robots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(origin.to_string(), rules.clone());
        rules
    }

    /// Whether robots.txt lets Iris crawl `url`.
    pub fn allowed(&self, url: &str) -> bool {
        let (Some(origin), Some((_, _, rest))) = (markup::origin_of(url), markup::split_url(url))
        else {
            return false;
        };
        let path = rest.split('#').next().unwrap_or("");
        let path = if path.is_empty() || path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_string()
        };
        self.robots_for(&origin).allowed(&path)
    }

    /// GET with redirects (each hop re-checked), robots.txt (crawl mode),
    /// per-host spacing, and the size cap. Non-2xx statuses (other than 304)
    /// are errors.
    pub fn get(&self, req: &Request) -> Result<Response, FetchError> {
        let mut url = req.url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            if markup::split_url(&url).is_none() {
                return Err(FetchError::BadUrl);
            }
            if req.mode == Mode::Crawl && !self.allowed(&url) {
                self.bump(|s| s.robots_blocked += 1);
                return Err(FetchError::Robots);
            }
            let hop = Request { url: &url, ..*req };
            let mut res = self
                .raw(&hop)
                .inspect_err(|_| self.bump(|s| s.errors += 1))?;
            if res.status == 429 && res.final_url == "retry" {
                // The host said when to come back (and it is soon): the
                // next slot already waits for it. Try once more.
                res = self
                    .raw(&hop)
                    .inspect_err(|_| self.bump(|s| s.errors += 1))?;
            }
            if (300..400).contains(&res.status) {
                let Some(loc) = res.final_url.strip_prefix("redirect:") else {
                    return Err(FetchError::Status(res.status));
                };
                url = markup::resolve(&url, loc).ok_or(FetchError::BadUrl)?;
                continue;
            }
            if !(200..300).contains(&res.status) {
                self.bump(|s| s.errors += 1);
                return Err(FetchError::Status(res.status));
            }
            return Ok(res);
        }
        Err(FetchError::Status(310))
    }

    /// One request, no redirects. A 3xx response carries its `Location` in
    /// `final_url` as `redirect:<location>`.
    fn raw(&self, req: &Request) -> Result<Response, FetchError> {
        let host = markup::host_of(req.url).ok_or(FetchError::BadUrl)?;
        let origin = markup::origin_of(req.url).ok_or(FetchError::BadUrl)?;
        self.wait_turn(&host, &origin)?;
        let mut r = self.agent.get(req.url).header("Accept", req.accept);
        if let Some(b) = req.bearer {
            r = r.header("Authorization", &format!("Bearer {b}"));
        }
        self.bump(|s| s.requests += 1);
        let mut res = r.call().map_err(|e| FetchError::Net(short_error(&e)))?;
        let status = res.status().as_u16();
        let header = |name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = header("content-type").unwrap_or_default();
        // Honor rate-limit headers (Reddit allows a request a minute):
        // the host's next slot moves to when the server says.
        let reset = header("retry-after")
            .filter(|_| status == 429 || status == 503)
            .or_else(|| {
                header("x-ratelimit-remaining")
                    .and_then(|r| r.parse::<f64>().ok())
                    .filter(|r| *r < 1.0)
                    .and(header("x-ratelimit-reset"))
            })
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s >= 0.0)
            // Clamp before converting: huge values would panic.
            .map(|s| Duration::from_secs_f64(s.min(BLOCKED_FOR.as_secs_f64())));
        let mut retry = false;
        if let Some(wait) = reset {
            retry = status == 429 && wait <= MAX_RATE_WAIT;
            // Longer than Iris will wait: the host is done for this run
            // (`wait_turn` refuses further requests to it).
            let wait = if wait > MAX_RATE_WAIT {
                BLOCKED_FOR
            } else {
                wait
            };
            let mut slots = self.next_slot.lock().unwrap_or_else(|p| p.into_inner());
            let at = Instant::now() + wait + Duration::from_millis(500);
            let cur = slots.entry(host.clone()).or_insert(at);
            if *cur < at {
                *cur = at;
            }
        }
        if (300..400).contains(&status) {
            return Ok(Response {
                status,
                final_url: format!("redirect:{}", header("location").unwrap_or_default()),
                ..Default::default()
            });
        }
        if !(200..300).contains(&status) {
            return Ok(Response {
                status,
                final_url: if retry {
                    "retry".into()
                } else {
                    req.url.to_string()
                },
                content_type,
                ..Default::default()
            });
        }
        if let Some(len) = header("content-length").and_then(|l| l.parse::<u64>().ok())
            && len > req.max_bytes
            && !req.truncate
        {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        res.body_mut()
            .as_reader()
            .take(req.max_bytes + 1)
            .read_to_end(&mut body)
            .map_err(|e| FetchError::Net(e.to_string()))?;
        if body.len() as u64 > req.max_bytes {
            if !req.truncate {
                return Err(FetchError::TooLarge);
            }
            body.truncate(req.max_bytes as usize);
        }
        self.bump(|s| s.bytes += body.len() as u64);
        Ok(Response {
            status,
            final_url: req.url.to_string(),
            content_type,
            body,
        })
    }
}

fn short_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::HostNotFound => "host not found or not public".into(),
        ureq::Error::Timeout(_) => "timed out".into(),
        other => crate::util::truncate_chars(&other.to_string(), 120),
    }
}

// ------------------------------------------------------------------- robots

/// Parsed robots.txt rules for one user agent.
#[derive(Debug, Default, Clone)]
pub struct Robots {
    /// (allow, pattern)
    rules: Vec<(bool, String)>,
    pub crawl_delay: Option<Duration>,
}

impl Robots {
    fn disallow_all() -> Self {
        Self {
            rules: vec![(false, "/".into())],
            crawl_delay: None,
        }
    }

    /// Parse robots.txt (RFC 9309), keeping the group for `token` (or `*`).
    pub fn parse(text: &str, token: &str) -> Self {
        struct Group {
            agents: Vec<String>,
            rules: Vec<(bool, String)>,
            delay: Option<Duration>,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut in_agents = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if !in_agents {
                        groups.push(Group {
                            agents: Vec::new(),
                            rules: Vec::new(),
                            delay: None,
                        });
                        in_agents = true;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.agents.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" => {
                    in_agents = false;
                    if let Some(g) = groups.last_mut()
                        && !value.is_empty()
                    {
                        g.rules.push((key == "allow", normalize_path(value)));
                    }
                }
                "crawl-delay" => {
                    in_agents = false;
                    if let Some(g) = groups.last_mut() {
                        g.delay = value
                            .parse::<f64>()
                            .ok()
                            .filter(|d| d.is_finite() && *d >= 0.0)
                            .map(|d| Duration::from_secs_f64(d.min(MAX_CRAWL_DELAY.as_secs_f64())));
                    }
                }
                _ => in_agents = false,
            }
        }
        let specific: Vec<&Group> = groups
            .iter()
            .filter(|g| {
                g.agents
                    .iter()
                    .any(|a| a != "*" && token.starts_with(a.as_str()))
            })
            .collect();
        let chosen: Vec<&Group> = if specific.is_empty() {
            groups
                .iter()
                .filter(|g| g.agents.iter().any(|a| a == "*"))
                .collect()
        } else {
            specific
        };
        Robots {
            rules: chosen
                .iter()
                .flat_map(|g| g.rules.iter().cloned())
                .collect(),
            crawl_delay: chosen.iter().find_map(|g| g.delay),
        }
    }

    /// Longest matching rule wins; allow wins ties; no match allows.
    pub fn allowed(&self, path: &str) -> bool {
        let path = normalize_path(path);
        let mut best: Option<(usize, bool)> = None;
        for (allow, pat) in &self.rules {
            if pattern_matches(pat, &path) {
                let len = pat.len();
                match best {
                    Some((l, a)) if l > len || (l == len && a) => {}
                    _ => best = Some((len, *allow)),
                }
            }
        }
        best.is_none_or(|(_, a)| a)
    }
}

/// One spelling for paths and rules (RFC 9309, 2.2.2): `%XX` escapes of
/// unreserved characters are decoded, other escapes get uppercase hex, and
/// non-ASCII characters are percent-encoded as UTF-8.
fn normalize_path(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            )
        {
            let v = h * 16 + l;
            if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                out.push(v as char);
            } else {
                out.push_str(&format!("%{v:02X}"));
            }
            i += 3;
        } else if c.is_ascii() {
            out.push(c as char);
            i += 1;
        } else {
            out.push_str(&format!("%{c:02X}"));
            i += 1;
        }
    }
    out
}

/// robots.txt path pattern: prefix match with `*` wildcards and `$` end.
fn pattern_matches(pat: &str, path: &str) -> bool {
    let (pat, anchored) = match pat.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pat, false),
    };
    let parts: Vec<&str> = pat.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if let Some(found) = path[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    if anchored {
        if parts.len() > 1 {
            // The last literal must end the path.
            let last = parts.last().copied().unwrap_or("");
            return path.ends_with(last) && path.len() >= pos;
        }
        return pos == path.len();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_address_filter() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "224.0.0.1",
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad}");
        }
        for good in ["93.184.216.34", "1.1.1.1", "2606:4700::1111", "100.200.1.1"] {
            assert!(is_public(good.parse().unwrap()), "{good}");
        }
    }

    #[test]
    fn robots_rules() {
        let txt = "User-agent: *\nDisallow: /private/\nAllow: /private/ok\nDisallow: /*.pdf$\nCrawl-delay: 2\n\n\
                   User-agent: OtherBot\nDisallow: /\n";
        let r = Robots::parse(txt, "irisfeed");
        assert!(r.allowed("/"));
        assert!(r.allowed("/news/story"));
        assert!(!r.allowed("/private/x"));
        assert!(r.allowed("/private/ok/1"));
        assert!(!r.allowed("/files/a.pdf"));
        assert!(r.allowed("/files/a.pdf?x=1"));
        assert_eq!(r.crawl_delay, Some(Duration::from_secs(2)));
        let huge = Robots::parse("User-agent: *\nCrawl-delay: 1e300\n", "irisfeed");
        assert_eq!(huge.crawl_delay, Some(MAX_CRAWL_DELAY), "clamped, no panic");

        let specific = Robots::parse(
            "User-agent: *\nDisallow: /\n\nUser-agent: IrisFeed\nUser-agent: Another\nAllow: /\nDisallow: /search\n",
            "irisfeed",
        );
        assert!(specific.allowed("/story"));
        assert!(!specific.allowed("/search?q=1"));
        // Escapes and non-ASCII paths match their rules (RFC 9309, 2.2.2).
        let enc = Robots::parse(
            "User-agent: *\nDisallow: /private/\nDisallow: /café\n",
            "irisfeed",
        );
        assert!(!enc.allowed("/%70rivate/photo.jpg"));
        assert!(!enc.allowed("/caf%C3%A9/menu"));
        assert!(!enc.allowed("/caf%c3%a9"));
        assert!(enc.allowed("/public"));
        let all = Robots::parse("User-agent: *\nDisallow: /", "irisfeed");
        assert!(!all.allowed("/anything"));
        assert!(Robots::parse("", "irisfeed").allowed("/x"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n", "irisfeed").allowed("/x"));
    }

    /// A tiny local HTTP server (tests only reach it through the
    /// private-address escape hatch).
    fn serve(routes: Vec<(&'static str, u16, &'static str, String)>) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(32) {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, extra, body) = routes
                    .iter()
                    .find(|(p, ..)| *p == path)
                    .map(|(_, st, ex, b)| (*st, *ex, b.clone()))
                    .unwrap_or((404, "", String::new()));
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn robots_redirects_are_followed() {
        let base = serve(vec![
            (
                "/robots.txt",
                301,
                "Location: /real-robots.txt\r\n",
                String::new(),
            ),
            (
                "/real-robots.txt",
                200,
                "",
                "User-agent: *\nDisallow: /secret\n".into(),
            ),
        ]);
        let f = Fetcher::for_tests();
        assert!(f.allowed(&format!("{base}/news")));
        assert!(!f.allowed(&format!("{base}/secret/1")));
    }

    #[test]
    fn fetch_redirects_robots_and_caps() {
        let base = serve(vec![
            (
                "/robots.txt",
                200,
                "",
                "User-agent: *\nDisallow: /secret\n".into(),
            ),
            ("/a", 301, "Location: /b\r\n", String::new()),
            (
                "/b",
                200,
                "Content-Type: text/html\r\n",
                "<html>hello</html>".into(),
            ),
            ("/secret", 200, "", "no".into()),
            ("/big", 200, "", "x".repeat(5000)),
            (
                "/limited",
                200,
                "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 1\r\n",
                "ok".into(),
            ),
            (
                "/huge",
                429,
                "Retry-After: 99999999999999999999999999\r\n",
                String::new(),
            ),
        ]);
        let f = Fetcher::for_tests();
        let url = format!("{base}/a");
        let r = f
            .get(&Request::new(&url, Mode::Crawl, "text/html", 1000))
            .unwrap();
        assert_eq!(r.text(), "<html>hello</html>");
        assert_eq!(r.final_url, format!("{base}/b"));
        let secret = format!("{base}/secret");
        assert!(matches!(
            f.get(&Request::new(&secret, Mode::Crawl, "*/*", 1000)),
            Err(FetchError::Robots)
        ));
        // Feeds and APIs are not crawled, so robots.txt does not apply.
        assert!(
            f.get(&Request::new(&secret, Mode::Feed, "*/*", 1000))
                .is_ok()
        );
        let big = format!("{base}/big");
        assert!(matches!(
            f.get(&Request::new(&big, Mode::Feed, "*/*", 1000)),
            Err(FetchError::TooLarge)
        ));
        let mut head = Request::new(&big, Mode::Feed, "*/*", 1000);
        head.truncate = true;
        let r = f.get(&head).unwrap();
        assert_eq!(r.body.len(), 1000);
        assert!(f.stats().robots_blocked >= 1);
        // Rate-limit headers hold the next request to the host.
        let limited = format!("{base}/limited");
        f.get(&Request::new(&limited, Mode::Feed, "*/*", 1000))
            .unwrap();
        let t0 = Instant::now();
        f.get(&Request::new(&limited, Mode::Feed, "*/*", 1000))
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_secs(1));
        // An absurd Retry-After is clamped (no panic) and not waited for.
        let huge = format!("{base}/huge");
        let t0 = Instant::now();
        assert!(matches!(
            f.get(&Request::new(&huge, Mode::Feed, "*/*", 1000)),
            Err(FetchError::Status(429))
        ));
        assert!(t0.elapsed() < Duration::from_secs(5));
        // ...and the host is left alone for the rest of the run.
        let t0 = Instant::now();
        assert!(matches!(
            f.get(&Request::new(&url, Mode::Feed, "*/*", 1000)),
            Err(FetchError::Net(_))
        ));
        assert!(t0.elapsed() < Duration::from_secs(1));
        // The real resolver refuses loopback.
        let real = Fetcher::new();
        assert!(
            real.get(&Request::new(&url, Mode::Feed, "*/*", 1000))
                .is_err()
        );
    }
}
