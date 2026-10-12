//! The approval gate's rules: what kind of action a connector tool call is,
//! who it reaches, and whether it runs freely, waits for the owner, or is
//! refused. Pure functions; the gateway supplies the state (trusted and
//! known recipients, per-connector rules).
//!
//! Everything here is derived from the real tool name, the MCP server's own
//! annotations, and the real arguments, never from text the agent writes
//! about the call, so a confused agent cannot talk its way past it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// What a tool call does, from the gate's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Reading, listing, searching.
    Read,
    /// Writing a draft that is not sent.
    Draft,
    /// Other changes (create an event, edit a file, add a label).
    Write,
    /// Sending email or messages, posting, replying, sharing, inviting.
    Send,
    /// Buying, paying, booking, ordering.
    Purchase,
    /// Deleting or removing.
    Delete,
    /// Exporting or downloading data in bulk.
    Export,
    /// A multi-purpose tool whose operation argument Iris cannot place.
    Unclear,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Read => "read",
            Category::Draft => "draft",
            Category::Write => "write",
            Category::Send => "send",
            Category::Purchase => "purchase",
            Category::Delete => "delete",
            Category::Export => "export",
            Category::Unclear => "unclear",
        }
    }

    /// How much harm a mistaken call could do (for picking the riskier of
    /// two readings).
    fn risk(self) -> u8 {
        match self {
            Category::Read => 0,
            Category::Draft => 1,
            Category::Write => 2,
            Category::Unclear => 3,
            Category::Send => 4,
            // Exports always ask, even when recipients are trusted.
            Category::Export => 5,
            Category::Delete => 6,
            Category::Purchase => 7,
        }
    }

    pub(super) fn riskier(self, other: Category) -> Category {
        if other.risk() > self.risk() {
            other
        } else {
            self
        }
    }
}

/// Per-connector rule, shown in plain language in the app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Reads, searches, drafts, and ordinary edits run freely; sending,
    /// buying, deleting, exporting, and contacting someone new ask first.
    #[default]
    Standard,
    /// Only reading runs freely; every change asks first.
    Careful,
    /// Only reading is allowed; everything else is refused.
    ReadOnly,
    /// Every call asks first.
    AskAlways,
}

impl Mode {
    pub const ALL: [Mode; 4] = [
        Mode::Standard,
        Mode::Careful,
        Mode::ReadOnly,
        Mode::AskAlways,
    ];

    pub fn describe(self) -> &'static str {
        match self {
            Mode::Standard => "Reads freely · asks before sending, buying, or deleting",
            Mode::Careful => "Reads freely · asks before any change",
            Mode::ReadOnly => "Only reads · never changes anything",
            Mode::AskAlways => "Asks every time",
        }
    }
}

/// Owner override for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRule {
    Allow,
    Ask,
    Block,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub mode: Mode,
    /// Tool name -> override. Purchases still ask when set to `allow`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolRule>,
}

impl Policy {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

/// Why a call runs without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedBy {
    /// The connector's rules let this kind of call through.
    Rules,
    /// A message only to trusted recipients.
    Trusted,
    /// The owner set this tool to always run.
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow(AllowedBy),
    /// Hold the call and ask the owner; the reason is shown on the card.
    Ask(String),
    /// Refuse; the reason is told to the agent.
    Block(String),
}

// ------------------------------------------------------------ classifying

/// Split a tool name into lowercase words (`sendEmail`, `send-email`,
/// `gmail.send_email` -> `send`, `email`).
pub fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Delivery/destructive verbs remain risky when an API concatenates them
    // with nouns or other verbs. Conservative substring matches may ask more.
    fn split(word: String) -> Vec<String> {
        let verbs = [
            "unsubscribe",
            "unshare",
            "terminate",
            "dispatch",
            "transmit",
            "broadcast",
            "purchase",
            "download",
            "discard",
            "dispose",
            "deliver",
            "publish",
            "forward",
            "withdraw",
            "destroy",
            "delete",
            "submit",
            "notify",
            "invite",
            "export",
            "mirror",
            "compose",
            "respond",
            "relay",
            "shred",
            "share",
            "reply",
            "draft",
            "send",
            "wire",
            "kill",
            "push",
            "text",
            "ping",
        ];
        let mut parts = Vec::new();
        let mut rest = word.as_str();
        let mut start = 0;
        while !rest.is_empty() {
            if let Some(verb) = verbs.iter().find(|v| rest.starts_with(**v)) {
                if start > 0 {
                    parts.push(
                        word[word.len() - rest.len() - start..word.len() - rest.len()].into(),
                    );
                }
                parts.push((*verb).into());
                rest = &rest[verb.len()..];
                start = 0;
            } else {
                let len = rest.chars().next().unwrap().len_utf8();
                start += len;
                rest = &rest[len..];
            }
        }
        if start > 0 {
            parts.push(word[word.len() - start..].into());
        }
        parts
    }
    out.into_iter().flat_map(split).collect()
}

const READ_VERBS: &[&str] = &[
    "get",
    "list",
    "search",
    "find",
    "read",
    "fetch",
    "query",
    "lookup",
    "look",
    "view",
    "show",
    "describe",
    "count",
    "check",
    "retrieve",
    "browse",
    "preview",
    "inspect",
    "peek",
    "summarize",
    "whoami",
    "status",
    "info",
    "explain",
    "resolve",
    "watch",
    "is",
    "has",
    "can",
];
const SEND_WORDS: &[&str] = &[
    "approve",
    "merge",
    "send",
    "reply",
    "respond",
    "forward",
    "post",
    "publish",
    "share",
    "invite",
    "email",
    "mail",
    "sms",
    "tweet",
    "toot",
    "notify",
    "comment",
    "broadcast",
    "dm",
    "submit",
    "announce",
    "deliver",
    "dispatch",
    "relay",
    "transmit",
    "text",
    "ping",
    "push",
    "remote",
];
/// Verbs that only mean "send" in first position (`message_user`, but not
/// `get_message`).
const SEND_LEADING: &[&str] = &["message", "react", "request"];
const PURCHASE_WORDS: &[&str] = &[
    "buy", "purchase", "pay", "payment", "payments", "checkout", "charge", "donate", "tip",
    "refund", "invoice", "withdraw", "deposit", "wire", "remit", "spend",
];
const PURCHASE_LEADING: &[&str] = &["order", "book", "reserve", "subscribe", "transfer", "rent"];
/// Nouns that make a non-reading call a purchase (`place_order`).
const PURCHASE_NOUNS: &[&str] = &["order", "orders", "booking", "reservation", "subscription"];
const DELETE_WORDS: &[&str] = &[
    "delete",
    "remove",
    "trash",
    "destroy",
    "erase",
    "purge",
    "wipe",
    "drop",
    "revoke",
    "unsubscribe",
    "cancel",
    "decline",
    "uninstall",
    "empty",
    "clear",
    "discard",
    "dispose",
    "shred",
    "kill",
    "terminate",
    "rm",
    "unlink",
    "bin",
    "unshare",
];
const EXPORT_WORDS: &[&str] = &[
    "export",
    "dump",
    "backup",
    "takeout",
    "bulk",
    "exfiltrate",
    "download",
    "mirror",
];
const DRAFT_WORDS: &[&str] = &["draft", "drafts", "compose"];

/// Ordinary changes (recognized, so an operation argument using them is
/// not "unclear").
const WRITE_VERBS: &[&str] = &[
    "create",
    "update",
    "add",
    "edit",
    "set",
    "modify",
    "insert",
    "upload",
    "write",
    "move",
    "rename",
    "label",
    "mark",
    "apply",
    "star",
    "pin",
    "schedule",
    "save",
    "assign",
    "patch",
    "put",
    "append",
    "replace",
    "change",
    "enable",
    "disable",
    "lock",
    "unlock",
    "archive",
    "copy",
    "duplicate",
    "tag",
    "unpin",
    "unstar",
    "unlabel",
];
/// Things that others see once created or changed (`create_pull_request`,
/// `update_issue`, `create_release`).
const PUBLISH_NOUNS: &[&str] = &[
    "pull",
    "pr",
    "issue",
    "issues",
    "release",
    "gist",
    "discussion",
    "comment",
    "comments",
    "post",
    "tweet",
    "story",
    "announcement",
    "wiki",
];
/// Arguments that choose what a multi-purpose tool does
/// (`pull_request_review_write(method: "submit_pending")`).
const SELECTOR_KEYS: &[&str] = &[
    "method",
    "action",
    "operation",
    "op",
    "verb",
    "command",
    "event",
];

/// What explicit words say, or None when no rule matches.
fn classify_words(w: &[String]) -> Option<Category> {
    let has = |set: &[&str]| w.iter().any(|x| set.contains(&x.as_str()));
    let first = w.first().map(String::as_str).unwrap_or("");
    let lead = |set: &[&str]| set.contains(&first);
    let read_first = lead(READ_VERBS);

    // These actions always ask. Consider all of them before email and
    // draft words, which must never hide a destructive or bulk operation.
    let mut risky = Category::Read;
    if (has(PURCHASE_WORDS) && !(read_first && !has(&["pay", "buy", "purchase", "checkout"])))
        || lead(PURCHASE_LEADING)
        || (!read_first && has(PURCHASE_NOUNS))
    {
        risky = risky.riskier(Category::Purchase);
    }
    if has(&["delete", "trash", "destroy", "erase", "purge", "wipe"])
        || (!read_first && has(DELETE_WORDS))
    {
        risky = risky.riskier(Category::Delete);
    }
    if has(EXPORT_WORDS) || (has(&["download"]) && has(&["all", "everything"])) {
        risky = risky.riskier(Category::Export);
    }
    if risky != Category::Read {
        return Some(risky);
    }
    // Nouns such as email/reply may describe a draft; delivery verbs may not.
    let delivery = has(&[
        "send",
        "reply",
        "respond",
        "forward",
        "publish",
        "share",
        "invite",
        "tweet",
        "post",
        "submit",
        "deliver",
        "dispatch",
        "relay",
        "transmit",
        "text",
        "ping",
        "push",
        "remote",
        "broadcast",
        "notify",
        "announce",
    ]);
    if !delivery && (lead(DRAFT_WORDS) || (first == "create" && has(DRAFT_WORDS))) {
        return Some(Category::Draft);
    }
    if delivery || (!read_first && (has(SEND_WORDS) || lead(SEND_LEADING))) {
        return Some(Category::Send);
    }
    if read_first {
        return Some(Category::Read);
    }
    // Creating or changing something other people see is publishing.
    if has(WRITE_VERBS) && has(PUBLISH_NOUNS) {
        return Some(Category::Send);
    }
    if has(WRITE_VERBS) {
        return Some(Category::Write);
    }
    None
}

/// Classify a tool by its name and the MCP server's annotations
/// (`readOnlyHint`, `destructiveHint`). Annotations may raise a name's risk,
/// but never lower it. Unrecognized names ask even with a read-only hint.
pub fn classify(tool: &str, annotations: &Value) -> Category {
    let mut category = classify_words(&words(tool)).unwrap_or(Category::Unclear);
    if annotations["destructiveHint"] == true {
        category = category.riskier(Category::Delete);
    }
    if category == Category::Read && annotations["readOnlyHint"] == false {
        // The name and annotation disagree: approval must settle it.
        category = category.riskier(Category::Unclear);
    }
    category
}

/// Classify one call: the tool, plus any operation argument that chooses
/// what it does. The riskier reading wins; an operation on a tool that
/// changes things that Iris cannot place is `Unclear` (it asks).
pub fn classify_call(tool: &str, annotations: &Value, args: &Value) -> Category {
    let base = classify(tool, annotations);
    let mut cat = base;
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            if !SELECTOR_KEYS.contains(&norm_key(k).as_str()) {
                continue;
            }
            let Some(text) = v.as_str() else { continue };
            match classify_words(&words(text)) {
                Some(c) => cat = cat.riskier(c),
                None if base != Category::Read => cat = cat.riskier(Category::Unclear),
                None => {}
            }
        }
    }
    cat
}

// -------------------------------------------------------------- recipients

/// Argument keys whose values name the people or places a call reaches.
const RECIPIENT_KEYS: &[&str] = &[
    "to",
    "cc",
    "bcc",
    "recipient",
    "recipients",
    "recipientid",
    "toaddress",
    "toaddresses",
    "attendee",
    "attendees",
    "invitee",
    "invitees",
    "guest",
    "guests",
    "participant",
    "participants",
    "email",
    "emails",
    "emailaddress",
    "emailaddresses",
    "sharewith",
    "sharedwith",
    "member",
    "members",
    "phone",
    "phonenumber",
    "phonenumbers",
    "user",
    "users",
    "userid",
    "username",
    "usernames",
    "handle",
    "channel",
    "channelid",
    "chatid",
    "assignee",
    "assignees",
    "reviewer",
    "reviewers",
    "collaborator",
    "grantee",
    "guestemails",
    "invite",
    "invites",
    "sendto",
    "destination",
    "bcclist",
    "recipientemail",
];
/// Flags that make a server add recipients Iris cannot see in the
/// arguments (reply-all fills in everyone on the thread).
const IMPLICIT_RECIPIENT_FLAGS: &[&str] = &[
    "replyall",
    "replytoall",
    "toall",
    "includeall",
    "allrecipients",
    "replyalltosender",
];
/// Stands for recipients the server will add on its own; never trusted.
pub const IMPLICIT_RECIPIENTS: &str = "everyone else on the thread";
/// Stands for everyone who can see what a call publishes (a pull request,
/// issue, comment, or post); never trusted.
pub const PUBLIC_AUDIENCE: &str = "everyone who can see it";

/// Placeholders for audiences Iris cannot list (never trusted or known).
pub fn is_placeholder(r: &str) -> bool {
    r == IMPLICIT_RECIPIENTS || r == PUBLIC_AUDIENCE || r == UNREADABLE_RECIPIENTS
}

/// The call publishes something to an audience beyond the people its
/// arguments name (assignees and reviewers are not the whole audience).
fn publishes(tool: &str, args: &Value) -> bool {
    let check = |w: &[String]| {
        let has = |set: &[&str]| w.iter().any(|x| set.contains(&x.as_str()));
        has(PUBLISH_NOUNS) || has(&["publish", "approve", "merge", "tweet", "broadcast"])
    };
    if check(&words(tool)) {
        return true;
    }
    args.as_object().is_some_and(|o| {
        o.iter().any(|(k, v)| {
            SELECTOR_KEYS.contains(&norm_key(k).as_str())
                && v.as_str().is_some_and(|t| check(&words(t)))
        })
    })
}

/// Everyone a call reaches: the recipients its arguments name, plus a
/// placeholder for an audience they cannot list (reply-all, publishing).
pub fn audience(tool: &str, args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let scan = matches!(
        classify_call(tool, &Value::Null, args),
        Category::Send | Category::Write | Category::Unclear
    );
    walk_recipients(args, 0, scan, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.clone()));
    if publishes(tool, args) && !out.iter().any(|r| r == PUBLIC_AUDIENCE) {
        out.push(PUBLIC_AUDIENCE.to_string());
    }
    out
}

/// Fields of an object inside a recipient list that identify the person.
const IDENTITY_FIELDS: &[&str] = &[
    "email",
    "emailaddress",
    "address",
    "id",
    "handle",
    "username",
    "login",
    "phone",
    "number",
];

fn norm_key(k: &str) -> String {
    k.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// An unparseable audience is never trusted or remembered after approval.
const UNREADABLE_RECIPIENTS: &str = "recipients Iris cannot read unambiguously";

fn atext(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&c)
}

/// RFC 5322 tokens: comments are CFWS, not display names or addresses.
/// Keep quoted strings intact, including escapes and punctuation. Reject
/// malformed input rather than recovering a possibly different audience.
fn address_tokens(raw: &str) -> Option<Vec<&str>> {
    let bytes = raw.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b' ' | b'\t' => i += 1,
            b'\r'
                if bytes.get(i + 1) == Some(&b'\n')
                    && bytes.get(i + 2).is_some_and(|c| matches!(c, b' ' | b'\t')) =>
            {
                i += 2;
            }
            b'(' => {
                let mut depth = 1;
                i += 1;
                while depth > 0 {
                    match *bytes.get(i)? {
                        b'\\' => {
                            i += 1;
                            if !matches!(*bytes.get(i)?, b' '..=b'~' | b'\t') {
                                return None;
                            }
                        }
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        b' '..=b'~' | b'\t' => {}
                        _ => return None,
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                loop {
                    match *bytes.get(i)? {
                        b'"' => break,
                        b'\\' => {
                            i += 1;
                            if !matches!(*bytes.get(i)?, b' '..=b'~' | b'\t') {
                                return None;
                            }
                        }
                        b' '..=b'~' | b'\t' => {}
                        _ => return None,
                    }
                    i += 1;
                }
                i += 1;
                tokens.push(&raw[start..i]);
            }
            b'@' | b'.' | b'<' | b'>' | b',' | b':' | b';' => {
                i += 1;
                tokens.push(&raw[start..i]);
            }
            c if atext(c) => {
                i += 1;
                while bytes.get(i).is_some_and(|c| atext(*c)) {
                    i += 1;
                }
                tokens.push(&raw[start..i]);
            }
            // Domain literals, obsolete syntax, and internationalized
            // addresses need approval until Iris can parse them safely.
            _ => return None,
        }
    }
    Some(tokens)
}

fn atom(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(atext)
}

fn phrase(tokens: &[&str]) -> bool {
    !tokens.is_empty() && tokens.iter().all(|s| atom(s) || s.starts_with('"'))
}

fn dot_atom(tokens: &[&str]) -> bool {
    !tokens.is_empty()
        && tokens.len() % 2 == 1
        && tokens
            .iter()
            .enumerate()
            .all(|(i, s)| if i % 2 == 0 { atom(s) } else { *s == "." })
}

fn mailbox(tokens: &[&str]) -> Option<String> {
    let address = if let Some(lt) = tokens.iter().position(|s| *s == "<") {
        if tokens.last() != Some(&">") || (lt > 0 && !phrase(&tokens[..lt])) {
            return None;
        }
        &tokens[lt + 1..tokens.len() - 1]
    } else {
        tokens
    };
    let at = address.iter().position(|s| *s == "@")?;
    let local = &address[..at];
    let domain = &address[at + 1..];
    if !(dot_atom(local) || local.len() == 1 && local[0].starts_with('"')) || !dot_atom(domain) {
        return None;
    }
    let out = address.concat();
    (out.len() <= 320).then(|| out.to_ascii_lowercase())
}

/// Parse the supported RFC 5322 address-list grammar, including groups. A partial
/// parse is unsafe: an ignored suffix may name an additional recipient.
fn email_recipients(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    let raw = if raw
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("mailto:"))
    {
        &raw[7..]
    } else {
        raw
    };
    let tokens = address_tokens(raw)?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let start = i;
        while i < tokens.len() && ![",", ":", ";"].contains(&tokens[i]) {
            i += 1;
        }
        if tokens.get(i) == Some(&":") {
            if !phrase(&tokens[start..i]) {
                return None;
            }
            i += 1;
            if tokens.get(i) != Some(&";") {
                loop {
                    let start = i;
                    while i < tokens.len() && ![",", ":", ";"].contains(&tokens[i]) {
                        i += 1;
                    }
                    out.push(mailbox(&tokens[start..i])?);
                    match tokens.get(i) {
                        Some(&",") => i += 1,
                        Some(&";") => break,
                        _ => return None,
                    }
                }
            }
            i += 1; // group terminator
        } else {
            out.push(mailbox(&tokens[start..i])?);
        }
        if i == tokens.len() {
            break;
        }
        if tokens[i] != "," || i + 1 == tokens.len() {
            return None;
        }
        i += 1;
    }
    // Empty groups do not identify a verifiable audience.
    (!out.is_empty()).then_some(out)
}

/// Phone numbers, handles, and channels retain their existing canonical
/// forms. Email syntax must go through the complete address-list parser.
fn other_recipient(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 320 || s.chars().any(char::is_control) {
        return None;
    }
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 7
        && s.chars()
            .all(|c| c.is_ascii_digit() || " +-().".contains(c))
    {
        let plus = if s.starts_with('+') { "+" } else { "" };
        return Some(format!("{plus}{digits}"));
    }
    if s.contains(['<', '>', '(', ')', '"', '\\', ':', '[', ']'])
        || s.contains('@') && !(s.starts_with('@') && s[1..].bytes().all(atext))
    {
        return None;
    }
    Some(s.to_lowercase())
}

fn parse_recipients(raw: &str) -> Option<Vec<String>> {
    if raw.contains('@') && !raw.trim().starts_with('@') || raw.contains(['<', '>', '"', ':']) {
        email_recipients(raw)
    } else {
        raw.split([',', ';', '\n'])
            .filter(|s| !s.trim().is_empty())
            .map(other_recipient)
            .collect()
    }
}

/// Canonical form of exactly one unambiguous recipient.
pub fn normalize_recipient(raw: &str) -> Option<String> {
    let mut recipients = parse_recipients(raw)?;
    (recipients.len() == 1).then(|| recipients.remove(0))
}

fn push_recipients(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 5 {
        out.push(UNREADABLE_RECIPIENTS.into());
        return;
    }
    let before = out.len();
    match v {
        Value::String(s) => match parse_recipients(s) {
            Some(recipients) => out.extend(recipients),
            None => out.push(UNREADABLE_RECIPIENTS.into()),
        },
        Value::Array(items) => items
            .iter()
            .for_each(|i| push_recipients(i, depth + 1, out)),
        Value::Object(o) => {
            for (k, v) in o {
                if IDENTITY_FIELDS.contains(&norm_key(k).as_str()) {
                    push_recipients(v, depth + 1, out);
                }
            }
        }
        Value::Number(n) => {
            if let Some(r) = other_recipient(&n.to_string()) {
                out.push(r);
            }
        }
        _ => {}
    }
    if out.len() == before {
        out.push(UNREADABLE_RECIPIENTS.into());
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "on"
        ),
        _ => false,
    }
}

fn walk_recipients(v: &Value, depth: usize, scan: bool, out: &mut Vec<String>) {
    if depth > 5 {
        out.push(UNREADABLE_RECIPIENTS.into());
        return;
    }
    match v {
        Value::Object(o) => {
            // Main recipients first (`to`), then copies, then the rest.
            let rank = |k: &str| match norm_key(k).as_str() {
                "to" | "recipient" | "recipients" | "toaddress" | "toaddresses" => 0,
                "cc" => 1,
                "bcc" => 2,
                _ => 3,
            };
            let mut entries: Vec<(&String, &Value)> = o.iter().collect();
            entries.sort_by_key(|(k, _)| rank(k));
            for (k, val) in entries {
                let key = norm_key(k);
                if IMPLICIT_RECIPIENT_FLAGS.contains(&key.as_str()) && truthy(val) {
                    out.push(IMPLICIT_RECIPIENTS.into());
                }
                if ([
                    "visibility",
                    "public",
                    "anyone",
                    "linksharing",
                    "anyonewithlink",
                ]
                .contains(&key.as_str())
                    && truthy(val))
                    || (["visibility", "audience", "access", "sharing", "linksharing"]
                        .contains(&key.as_str())
                        && val.as_str().is_some_and(|s| {
                            ["public", "anyone", "anyonewithlink", "link", "true"]
                                .contains(&norm_key(s).as_str())
                        }))
                {
                    out.push(PUBLIC_AUDIENCE.into());
                }
                if RECIPIENT_KEYS.contains(&key.as_str()) {
                    push_recipients(val, depth + 1, out);
                } else {
                    walk_recipients(val, depth + 1, scan, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| walk_recipients(i, depth + 1, scan, out)),
        Value::String(s) if scan => {
            // Unknown fields can carry an address embedded in prose. Ambiguous
            // address-like text fails closed instead of silently dropping it.
            let digits = s.chars().filter(char::is_ascii_digit).count();
            if digits >= 7
                && s.chars()
                    .all(|c| c.is_ascii_digit() || c.is_whitespace() || "+()-./".contains(c))
            {
                push_recipients(v, depth, out);
                return;
            }
            for token in s.split_whitespace() {
                let token = token.trim_matches(['<', '>', ',', ';', '"', '(', ')']);
                if token.contains('@')
                    || token.starts_with('+')
                    || token.starts_with('#')
                    || (token.chars().filter(char::is_ascii_digit).count() >= 7)
                {
                    push_recipients(&Value::String(token.into()), depth, out);
                }
            }
        }
        _ => {}
    }
}

/// Everyone a call reaches, as far as its arguments say (deduplicated, in
/// order).
pub fn recipients(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_recipients(args, 0, false, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.clone()));
    out
}

// ---------------------------------------------------------------- deciding

fn ask_reason(category: Category) -> &'static str {
    match category {
        Category::Send => "Sends a message for you",
        Category::Purchase => "Spends money or commits you to something",
        Category::Delete => "Deletes or removes something",
        Category::Export => "Exports a lot of your data",
        Category::Unclear => "Iris can't tell exactly what this does",
        Category::Write => "Changes something in your account",
        Category::Draft => "Writes a draft",
        Category::Read => "Reads from your account",
    }
}

/// Decide what happens to one call. `trusted` and `known` say whether a
/// (normalized) recipient is on the owner's trusted list or was approved
/// before.
pub fn decide(
    policy: &Policy,
    tool: &str,
    category: Category,
    recipients: &[String],
    trusted: &dyn Fn(&str) -> bool,
    known: &dyn Fn(&str) -> bool,
) -> Verdict {
    if let Some(rule) = policy.tools.get(tool) {
        return match rule {
            ToolRule::Block => Verdict::Block("the owner turned this action off".into()),
            ToolRule::Ask => Verdict::Ask(ask_reason(category).into()),
            ToolRule::Allow if category == Category::Purchase => {
                Verdict::Ask("Purchases always ask first".into())
            }
            ToolRule::Allow if category == Category::Unclear => {
                Verdict::Ask(ask_reason(category).into())
            }
            ToolRule::Allow => Verdict::Allow(AllowedBy::Override),
        };
    }
    let trusted = |r: &str| !is_placeholder(r) && trusted(r);
    let known = |r: &str| !is_placeholder(r) && known(r);
    let all_trusted = !recipients.is_empty() && recipients.iter().all(|r| trusted(r));
    let new: Vec<&String> = recipients
        .iter()
        .filter(|r| !trusted(r) && !known(r))
        .collect();
    match policy.mode {
        Mode::ReadOnly => match category {
            Category::Read => Verdict::Allow(AllowedBy::Rules),
            _ => Verdict::Block("this connection is set to read only".into()),
        },
        Mode::AskAlways => Verdict::Ask(ask_reason(category).into()),
        Mode::Standard | Mode::Careful => match category {
            Category::Read => Verdict::Allow(AllowedBy::Rules),
            Category::Draft | Category::Write if policy.mode == Mode::Careful => {
                Verdict::Ask(ask_reason(category).into())
            }
            Category::Draft => Verdict::Allow(AllowedBy::Rules),
            Category::Write if !new.is_empty() => Verdict::Ask(format!(
                "Reaches someone new: {}",
                new.iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Category::Write => Verdict::Allow(AllowedBy::Rules),
            Category::Send if all_trusted => Verdict::Allow(AllowedBy::Trusted),
            Category::Send if recipients.iter().any(|r| r == PUBLIC_AUDIENCE) => {
                Verdict::Ask("Publishes something others can see".into())
            }
            Category::Send if !new.is_empty() => Verdict::Ask(format!(
                "Sends a message to someone new: {}",
                new.iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            _ => Verdict::Ask(ask_reason(category).into()),
        },
    }
}

// ----------------------------------------------------------------- preview

/// What the approval card shows, built from the real arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub title: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    /// Other short arguments worth seeing (amount, date, file name...).
    pub fields: Vec<(String, String)>,
    /// The full arguments, pretty-printed (cut when very long).
    pub details: String,
    pub action_label: String,
}

const SUBJECT_KEYS: &[&str] = &["subject", "title", "summary", "name", "topic"];
const BODY_KEYS: &[&str] = &[
    "body",
    "text",
    "message",
    "content",
    "description",
    "comment",
    "html",
    "markdown",
    "note",
    "notes",
    "caption",
    "status",
];
pub const MAX_PREVIEW_BODY: usize = 4000;
/// The longest request an approval card can show in full; larger ones are
/// refused rather than approved unseen.
pub const MAX_DETAILS: usize = 64_000;

/// Make characters that could hide or reorder text visible, so the preview
/// cannot look different from what is sent.
pub fn reveal_hidden(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let hidden = matches!(c as u32,
            0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF)
            || (c.is_control() && c != '\n' && c != '\t');
        if hidden {
            out.push_str(&format!("[U+{:04X}]", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// The full request as the owner sees it (pretty JSON, hidden characters
/// made visible).
pub fn request_text(args: &Value) -> String {
    reveal_hidden(&serde_json::to_string_pretty(args).unwrap_or_default())
}

fn cut(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n… ({} more characters, see details)", n - max)
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Human label for a tool name: `send_message` -> "Send message".
pub fn humanize(tool: &str) -> String {
    let w = words(tool).join(" ");
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => tool.to_string(),
    }
}

pub fn preview(service_label: &str, tool: &str, category: Category, args: &Value) -> Preview {
    let obj: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    let find = |keys: &[&str]| -> Option<(String, String)> {
        keys.iter().find_map(|want| {
            obj.iter()
                .find(|(k, _)| norm_key(k) == *want)
                .and_then(|(k, v)| scalar_text(v).map(|t| (k.clone(), t)))
        })
    };
    let to = recipients(args).join(", ");
    let subject = find(SUBJECT_KEYS);
    let body = find(BODY_KEYS);
    let used: Vec<String> = [&subject, &body]
        .iter()
        .filter_map(|x| x.as_ref().map(|(k, _)| k.clone()))
        .collect();
    let fields: Vec<(String, String)> = obj
        .iter()
        .filter(|(k, _)| !used.contains(k) && !RECIPIENT_KEYS.contains(&norm_key(k).as_str()))
        .filter_map(|(k, v)| scalar_text(v).map(|t| (k.clone(), t)))
        .filter(|(_, t)| t.chars().count() <= 200)
        .take(8)
        .map(|(k, t)| (humanize(&k), reveal_hidden(&t)))
        .collect();
    let details = cut(&request_text(args), MAX_DETAILS);
    let action_label = match category {
        Category::Send => "Send",
        Category::Purchase => "Pay",
        Category::Delete => "Delete",
        _ => "Allow",
    };
    Preview {
        title: format!("{} · {service_label}", humanize(tool)),
        to: reveal_hidden(&to),
        subject: subject.map(|(_, s)| reveal_hidden(&s)).unwrap_or_default(),
        body: body
            .map(|(_, b)| cut(&reveal_hidden(&b), MAX_PREVIEW_BODY))
            .unwrap_or_default(),
        fields,
        details,
        action_label: action_label.into(),
    }
}

impl Preview {
    pub fn fields_json(&self) -> Value {
        Value::Array(
            self.fields
                .iter()
                .map(|(k, v)| json!({"label": k, "value": v}))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_disguised_delivery_tools_ask() {
        let policy = Policy::default();
        for tool in [
            "frobnicate",
            "sendemail",
            "sendmail",
            "emailsend",
            "dispatch_message",
            "relay_message",
            "transmit_message",
            "text_user",
            "ping_user",
            "push_to_remote",
            "sync_to_remote",
            "submit_draft",
            "share_draft",
            "deliver_draft",
            "wire_money",
            "discard",
            "dispose",
            "shred",
            "kill",
            "terminate",
            "rm",
            "unlink",
            "bin",
            "unshare",
            "download_all",
            "mirror",
        ] {
            let c = classify(tool, &json!({"readOnlyHint": true}));
            assert!(
                matches!(
                    decide(&policy, tool, c, &[], &|_| false, &|_| false),
                    Verdict::Ask(_)
                ),
                "{tool}: {c:?}"
            );
        }
        let mut override_policy = Policy::default();
        override_policy
            .tools
            .insert("frobnicate".into(), ToolRule::Allow);
        assert!(matches!(
            decide(
                &override_policy,
                "frobnicate",
                classify("frobnicate", &Value::Null),
                &[],
                &|_| true,
                &|_| true
            ),
            Verdict::Ask(_)
        ));
        assert_eq!(classify("sendemail", &Value::Null), Category::Send);
        assert_eq!(classify("emailsend", &Value::Null), Category::Send);
        for tool in [
            "submit_draft",
            "share_draft",
            "compose_and_dispatch",
            "draft_sendmail",
        ] {
            assert_eq!(classify(tool, &Value::Null), Category::Send);
        }
        assert_eq!(classify("update_draft", &Value::Null), Category::Write);
        assert_eq!(classify("compose_email", &Value::Null), Category::Draft);
    }

    #[test]
    fn hidden_recipients_and_implicit_audiences_ask() {
        let p = Policy::default();
        for args in [
            json!({"destination": "stranger@example.test"}),
            json!({"odd": "send to stranger@example.test now"}),
            json!({"odd": "+15550102000"}),
            json!({"odd": "@stranger"}),
            json!({"guestEmails": ["stranger@example.test"]}),
            json!({"invite": ["stranger@example.test"]}),
            json!({"a":{"b":{"c":{"d":{"e":{"f":{"to":"stranger@example.test"}}}}}}}),
        ] {
            let r = audience("update_record", &args);
            assert!(!r.is_empty(), "{args}");
            assert!(
                matches!(
                    decide(
                        &p,
                        "update_record",
                        Category::Write,
                        &r,
                        &|_| false,
                        &|_| false
                    ),
                    Verdict::Ask(_)
                ),
                "{args}"
            );
        }
        for flag in [
            json!("yes"),
            json!("1"),
            json!(1),
            json!("TRUE"),
            json!(" true"),
        ] {
            assert!(
                audience("send_email", &json!({"nested": {"reply_all": flag}}))
                    .iter()
                    .any(|r| r == IMPLICIT_RECIPIENTS)
            );
        }
        for args in [
            json!({"visibility":"public"}),
            json!({"visibility":true}),
            json!({"visibility":" yes"}),
            json!({"anyone":true}),
            json!({"public":"yes"}),
            json!({"link-sharing":"TRUE"}),
        ] {
            let r = audience("share_file", &args);
            assert!(r.iter().any(|r| r == PUBLIC_AUDIENCE), "{args}");
            assert!(matches!(
                decide(&p, "share_file", Category::Send, &r, &|_| true, &|_| true),
                Verdict::Ask(_)
            ));
        }
    }

    #[test]
    fn parses_recipient_comments_quotes_and_groups() {
        for (raw, expected) in [
            (
                "evil@example.com (Friend <sam@example.com>)",
                vec!["evil@example.com"],
            ),
            (
                "evil@example.com (nested (Friend <sam@example.com>))",
                vec!["evil@example.com"],
            ),
            (
                "Sam (evil@example.com) <sam@example.com>",
                vec!["sam@example.com"],
            ),
            (
                "\"Doe, Sam\" <sam@example.com>, riley@example.com",
                vec!["sam@example.com", "riley@example.com"],
            ),
            (
                "\"Friend \\\"<sam@example.com>\\\"\" <evil@example.com>",
                vec!["evil@example.com"],
            ),
            (
                "Team: sam@example.com, Evil <evil@example.com>;",
                vec!["sam@example.com", "evil@example.com"],
            ),
            (
                "\"Team: Friends\": sam@example.com; , evil@example.com",
                vec!["sam@example.com", "evil@example.com"],
            ),
            (
                "\"evil,sam@example.com\"@example.net",
                vec!["\"evil,sam@example.com\"@example.net"],
            ),
            ("<sam (comment) @ example.com>", vec!["sam@example.com"]),
            ("Sam\r\n <SAM@Example.com>", vec!["sam@example.com"]),
            ("MAILTO:SAM@Example.com", vec!["sam@example.com"]),
        ] {
            assert_eq!(recipients(&json!({"to": raw})), expected, "{raw}");
        }
        assert_eq!(
            normalize_recipient("+1 (555) 010-9999").as_deref(),
            Some("+15550109999")
        );
        assert_eq!(normalize_recipient("@Sam").as_deref(), Some("@sam"));
        assert_eq!(normalize_recipient("#General").as_deref(), Some("#general"));
        assert_eq!(
            normalize_recipient("sam@example.com, riley@example.com"),
            None
        );
    }

    #[test]
    fn malformed_recipient_lists_cannot_be_trusted_or_known() {
        for raw in [
            "evil@example.com Friend <sam@example.com>",
            "\"a\" <evil@example.com> <sam@example.com>",
            "Team: sam@example.com",
            "Team: sam@example.com; evil@example.com",
            "Team: Team: sam@example.com;;",
            "<sam@example.com> evil@example.com",
            "Sam <sam@example.com",
            "\"unterminated <sam@example.com>",
            "sam@example.com (unterminated",
            "sam@example.com )",
            "sam@example.com (escaped end \\)",
            "mailto:sam@example.com?cc=evil@example.com",
            "\"sam@example.com\" <evil@example.com>x",
            "sam@example.com,",
            "sam@example.com,,evil@example.com",
            "sam@example.com\r\nBcc: evil@example.com",
            "sam..lee@example.com",
            "sam@example..com",
            "sam@example.com@evil.example.com",
            "sam(comment)lee@example.com",
            "<@evil.example.com:sam@example.com>",
            "Undisclosed:;",
        ] {
            let out = recipients(&json!({"to": raw}));
            assert_eq!(out, [UNREADABLE_RECIPIENTS], "{raw}");
            // Even a stored/trusted string matching the placeholder must
            // never let a malformed audience run automatically.
            assert!(
                matches!(
                    decide(
                        &Policy::default(),
                        "send_email",
                        Category::Send,
                        &out,
                        &|_| true,
                        &|_| true
                    ),
                    Verdict::Ask(_)
                ),
                "{raw}"
            );
            assert_eq!(normalize_recipient(raw), None, "{raw}");
        }
        for raw in [
            "evil@example.com (Friend <sam@example.com>)",
            "\"sam@example.com\" <evil@example.com>",
            "Team: sam@example.com, evil@example.com;",
        ] {
            let out = recipients(&json!({"to": raw}));
            assert!(
                matches!(
                    decide(
                        &Policy::default(),
                        "send_email",
                        Category::Send,
                        &out,
                        &|r| r == "sam@example.com",
                        &|_| false
                    ),
                    Verdict::Ask(_)
                ),
                "{raw}: {out:?}"
            );
        }
    }

    fn c(name: &str) -> Category {
        classify(name, &Value::Null)
    }

    #[test]
    fn splits_tool_names() {
        assert_eq!(words("sendEmail"), ["send", "email"]);
        assert_eq!(words("gmail.send_email"), ["gmail", "send", "email"]);
        assert_eq!(words("list-ToDos"), ["list", "to", "dos"]);
    }

    #[test]
    fn classifies_common_tools() {
        assert_eq!(c("send_message"), Category::Send);
        assert_eq!(c("sendEmail"), Category::Send);
        assert_eq!(c("reply_to_thread"), Category::Send);
        assert_eq!(c("post_to_channel"), Category::Send);
        assert_eq!(c("message_user"), Category::Send);
        assert_eq!(c("get_message"), Category::Read);
        assert_eq!(c("search_threads"), Category::Read);
        assert_eq!(c("list_events"), Category::Read);
        assert_eq!(c("create_draft"), Category::Draft);
        assert_eq!(c("draft_reply"), Category::Send);
        assert_eq!(c("send_draft"), Category::Send);
        assert_eq!(c("create_event"), Category::Write);
        assert_eq!(c("update_file"), Category::Write);
        assert_eq!(c("delete_event"), Category::Delete);
        assert_eq!(c("trash_message"), Category::Delete);
        assert_eq!(c("buy_item"), Category::Purchase);
        assert_eq!(c("place_order"), Category::Purchase);
        assert_eq!(c("order_pizza"), Category::Purchase);
        assert_eq!(c("list_orders"), Category::Read);
        assert_eq!(c("get_payment_status"), Category::Read);
        assert_eq!(c("send_payment"), Category::Purchase);
        assert_eq!(c("export_all_contacts"), Category::Export);
        assert_eq!(c("share_file"), Category::Send);
        assert_eq!(c("frobnicate"), Category::Unclear);
    }

    #[test]
    fn security_regression_risky_verbs_win_over_email_and_drafts() {
        for (tool, expected) in [
            ("delete_email_draft", Category::Delete),
            ("trash_mail_draft", Category::Delete),
            ("export_email_drafts", Category::Export),
            ("delete_email", Category::Delete),
            ("export_email", Category::Export),
            ("remove_email_draft", Category::Delete),
            ("purge_mail_drafts", Category::Delete),
            ("send_export_email", Category::Export),
            ("delete_export_email_drafts", Category::Delete),
            ("delete_payment", Category::Purchase),
        ] {
            let category = c(tool);
            assert_eq!(category, expected, "{tool}");
            assert!(
                matches!(
                    decide(
                        &Policy::default(),
                        tool,
                        category,
                        &["sam@example.com".into()],
                        &|_| true,
                        &|_| true,
                    ),
                    Verdict::Ask(_)
                ),
                "{tool} must ask even with trusted recipients"
            );
        }
        assert_eq!(
            classify_call("send_email", &Value::Null, &json!({"action": "export"})),
            Category::Export
        );
    }

    #[test]
    fn security_regression_annotations_can_only_raise_named_risk() {
        for hints in [
            json!({"readOnlyHint": false, "destructiveHint": true}),
            json!({"readOnlyHint": true, "destructiveHint": true}),
            json!({"destructiveHint": true}),
            json!({"readOnlyHint": false}),
        ] {
            let category = classify("query", &hints);
            if hints["destructiveHint"] == true {
                assert_eq!(category, Category::Delete);
            }
            assert!(matches!(
                decide(
                    &Policy::default(),
                    "query",
                    category,
                    &[],
                    &|_| false,
                    &|_| false
                ),
                Verdict::Ask(_)
            ));
            let read_only = Policy {
                mode: Mode::ReadOnly,
                ..Policy::default()
            };
            assert!(matches!(
                decide(&read_only, "query", category, &[], &|_| false, &|_| false),
                Verdict::Block(_)
            ));
        }
        let ro = json!({"readOnlyHint": true});
        for (tool, expected) in [
            ("create_event", Category::Write),
            ("create_draft", Category::Draft),
            ("export_email", Category::Export),
            ("delete_email", Category::Delete),
            ("send_email", Category::Send),
            ("buy_item", Category::Purchase),
        ] {
            assert_eq!(classify(tool, &ro), expected, "{tool}");
        }
        assert_eq!(
            classify("buy_item", &json!({"destructiveHint": true})),
            Category::Purchase
        );
    }

    #[test]
    fn security_regression_unreadable_recipient_elements_require_approval() {
        for element in [
            json!({"recipient_id": "new-person"}),
            json!({"displayName": "New person"}),
            json!({}),
            json!({"email": []}),
            json!(true),
            Value::Null,
            json!(""),
            json!([]),
        ] {
            let out = recipients(&json!({"to": [{"email": "sam@example.com"}, element]}));
            assert_eq!(out, ["sam@example.com", UNREADABLE_RECIPIENTS], "{element}");
            assert!(is_placeholder(&out[1]));
            assert!(matches!(
                decide(
                    &Policy::default(),
                    "send_email",
                    Category::Send,
                    &out,
                    &|_| true,
                    &|_| true
                ),
                Verdict::Ask(_)
            ));
        }
    }

    #[test]
    fn operation_arguments_count() {
        let none = Value::Null;
        let review = |method: &str| {
            classify_call(
                "pull_request_review_write",
                &json!({"readOnlyHint": false}),
                &json!({"method": method, "owner": "o", "repo": "r", "pullNumber": 1}),
            )
        };
        // Reviews are seen by others: every operation on them asks.
        assert_eq!(review("create"), Category::Send);
        assert_eq!(review("submit_pending"), Category::Send);
        assert_eq!(review("delete_pending"), Category::Delete);
        assert_eq!(review("frobnicate"), Category::Send);
        assert_eq!(
            classify_call("repo_admin_write", &none, &json!({"method": "frobnicate"})),
            Category::Unclear
        );
        assert_eq!(
            classify_call(
                "pull_request_review_write",
                &none,
                &json!({"method": "create", "event": "REQUEST_CHANGES"})
            ),
            Category::Send
        );
        assert_eq!(
            classify_call(
                "pull_request_review_write",
                &none,
                &json!({"method": "create", "event": "APPROVE"})
            ),
            Category::Send
        );
        // Reads stay reads whatever their options say, unless they name a
        // risky operation.
        assert_eq!(
            classify_call(
                "search_issues",
                &none,
                &json!({"mode": "fuzzy", "method": "bm25"})
            ),
            Category::Read
        );
        assert_eq!(
            classify_call("list_things", &none, &json!({"action": "delete"})),
            Category::Delete
        );
        assert!(matches!(
            decide(
                &Policy::default(),
                "x",
                Category::Unclear,
                &[],
                &|_| false,
                &|_| false
            ),
            Verdict::Ask(_)
        ));
    }

    #[test]
    fn annotations_help_but_risky_names_win() {
        let ro = json!({"readOnlyHint": true});
        assert_eq!(classify("frobnicate", &ro), Category::Unclear);
        assert_eq!(classify("send_message", &ro), Category::Send);
        let destructive = json!({"destructiveHint": true});
        assert_eq!(classify("frobnicate", &destructive), Category::Delete);
    }

    #[test]
    fn finds_recipients_in_real_shapes() {
        let r = recipients(&json!({
            "to": "Sam Lee <Sam@Example.com>, riley@example.com",
            "cc": ["kim@example.com"],
            "subject": "hi",
            "body": "write to nobody@example.com",
        }));
        assert_eq!(
            r,
            ["sam@example.com", "riley@example.com", "kim@example.com"]
        );
        let r = recipients(
            &json!({"event": {"attendees": [{"email": "a@x.org", "displayName": "A"}]}}),
        );
        assert_eq!(r, ["a@x.org"]);
        let r = recipients(&json!({"phone_number": "+1 (555) 010-2000"}));
        assert_eq!(r, ["+15550102000"]);
        assert!(recipients(&json!({"query": "from:x@y.com"})).is_empty());
    }

    #[test]
    fn publications_are_never_only_to_trusted_people() {
        let none = Value::Null;
        // An issue assigned to a trusted teammate still reaches everyone
        // who can read the repository.
        let args = json!({"method": "create", "owner": "o", "repo": "r",
                          "title": "Bug", "assignees": ["sam@example.com"]});
        let cat = classify_call("issue_write", &none, &args);
        assert_eq!(cat, Category::Send);
        let who = audience("issue_write", &args);
        assert!(who.contains(&PUBLIC_AUDIENCE.to_string()), "{who:?}");
        let everyone = |_: &str| true;
        assert!(matches!(
            decide(
                &Policy::default(),
                "issue_write",
                cat,
                &who,
                &everyone,
                &everyone
            ),
            Verdict::Ask(_)
        ));
        // A plain message to trusted people still skips approval.
        let args = json!({"to": "sam@example.com", "text": "hi"});
        let who = audience("send_message", &args);
        assert_eq!(who, ["sam@example.com"]);
        assert!(matches!(
            decide(
                &Policy::default(),
                "send_message",
                Category::Send,
                &who,
                &everyone,
                &everyone
            ),
            Verdict::Allow(AllowedBy::Trusted)
        ));
        // Delivering an existing draft is sending; composing one is not.
        assert_eq!(c("publish_draft"), Category::Send);
        assert_eq!(c("forward_draft"), Category::Send);
        assert_eq!(c("send_draft"), Category::Send);
        assert_eq!(c("create_draft"), Category::Draft);
        assert_eq!(c("draft_reply"), Category::Send);
    }

    #[test]
    fn publishing_and_reply_all() {
        let none = Value::Null;
        assert_eq!(c("create_pull_request"), Category::Send);
        assert_eq!(c("update_issue"), Category::Send);
        assert_eq!(c("create_branch"), Category::Write);
        assert_eq!(c("list_issues"), Category::Read);
        // Reply-all reaches people Iris cannot see: never "all trusted".
        let args =
            json!({"to": "sam@example.com", "thread_id": "t1", "reply_all": true, "body": "ok"});
        let r = recipients(&args);
        assert!(r.contains(&IMPLICIT_RECIPIENTS.to_string()));
        let tr = |x: &str| x == "sam@example.com";
        assert!(matches!(
            decide(
                &Policy::default(),
                "send_gmail_message",
                classify_call("send_gmail_message", &none, &args),
                &r,
                &tr,
                &tr
            ),
            Verdict::Ask(_)
        ));
    }

    #[test]
    fn standard_rules() {
        let p = Policy::default();
        let none = |_: &str| false;
        let tr = |r: &str| r == "sam@example.com";
        let sam = vec!["sam@example.com".to_string()];
        let kim = vec!["kim@example.com".to_string()];
        assert_eq!(
            decide(&p, "search", Category::Read, &[], &none, &none),
            Verdict::Allow(AllowedBy::Rules)
        );
        assert_eq!(
            decide(&p, "create_draft", Category::Draft, &kim, &none, &none),
            Verdict::Allow(AllowedBy::Rules)
        );
        assert!(matches!(
            decide(&p, "send", Category::Send, &kim, &none, &none),
            Verdict::Ask(r) if r.contains("someone new")
        ));
        assert_eq!(
            decide(&p, "send", Category::Send, &sam, &tr, &none),
            Verdict::Allow(AllowedBy::Trusted)
        );
        // Known (approved before) is not the same as trusted for sends.
        assert!(matches!(
            decide(&p, "send", Category::Send, &kim, &none, &|_| true),
            Verdict::Ask(_)
        ));
        // A send with no recipient we can check still asks.
        assert!(matches!(
            decide(&p, "send", Category::Send, &[], &tr, &none),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            decide(&p, "create_event", Category::Write, &kim, &none, &none),
            Verdict::Ask(r) if r.contains("kim@example.com")
        ));
        assert_eq!(
            decide(&p, "create_event", Category::Write, &kim, &none, &|_| true),
            Verdict::Allow(AllowedBy::Rules)
        );
        for cat in [Category::Purchase, Category::Delete, Category::Export] {
            assert!(matches!(
                decide(&p, "x", cat, &sam, &tr, &tr),
                Verdict::Ask(_)
            ));
        }
    }

    #[test]
    fn modes_and_overrides() {
        let none = |_: &str| false;
        let ro = Policy {
            mode: Mode::ReadOnly,
            ..Policy::default()
        };
        assert!(matches!(
            decide(&ro, "x", Category::Draft, &[], &none, &none),
            Verdict::Block(_)
        ));
        let careful = Policy {
            mode: Mode::Careful,
            ..Policy::default()
        };
        assert!(matches!(
            decide(&careful, "x", Category::Write, &[], &none, &none),
            Verdict::Ask(_)
        ));
        let all = Policy {
            mode: Mode::AskAlways,
            ..Policy::default()
        };
        assert!(matches!(
            decide(&all, "x", Category::Read, &[], &none, &none),
            Verdict::Ask(_)
        ));
        let mut p = Policy::default();
        p.tools.insert("send_note".into(), ToolRule::Allow);
        p.tools.insert("buy".into(), ToolRule::Allow);
        p.tools.insert("search".into(), ToolRule::Block);
        assert_eq!(
            decide(&p, "send_note", Category::Send, &[], &none, &none),
            Verdict::Allow(AllowedBy::Override)
        );
        assert!(matches!(
            decide(&p, "buy", Category::Purchase, &[], &none, &none),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            decide(&p, "search", Category::Read, &[], &none, &none),
            Verdict::Block(_)
        ));
        assert_eq!(Policy::parse("garbage"), Policy::default());
        let raw = serde_json::to_string(&p).unwrap();
        assert_eq!(Policy::parse(&raw), p);
    }

    #[test]
    fn preview_comes_from_the_arguments() {
        let args = json!({
            "to": "sam@example.com",
            "subject": "Dinner",
            "body": "Friday at 7?\u{202E}gnp.exe",
            "send_at": "2026-10-12T19:00",
        });
        let p = preview("Mail", "send_email", Category::Send, &args);
        assert_eq!(p.title, "Send email · Mail");
        assert_eq!(p.to, "sam@example.com");
        assert_eq!(p.subject, "Dinner");
        assert!(p.body.contains("[U+202E]"), "{}", p.body);
        assert_eq!(
            p.fields,
            [("Send at".to_string(), "2026-10-12T19:00".to_string())]
        );
        assert!(p.details.contains("send_at"));
        assert_eq!(p.action_label, "Send");
        let long = "x".repeat(MAX_PREVIEW_BODY + 10);
        let p = preview("Mail", "send", Category::Send, &json!({"text": long}));
        assert!(p.body.contains("10 more characters"));
    }
}
