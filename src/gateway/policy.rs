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
    out
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
];
/// Verbs that only mean "send" in first position (`message_user`, but not
/// `get_message`).
const SEND_LEADING: &[&str] = &["message", "react"];
const PURCHASE_WORDS: &[&str] = &[
    "buy", "purchase", "pay", "payment", "payments", "checkout", "charge", "donate", "tip",
    "refund", "invoice", "withdraw", "deposit",
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
];
const EXPORT_WORDS: &[&str] = &["export", "dump", "backup", "takeout", "bulk", "exfiltrate"];
const DRAFT_WORDS: &[&str] = &["draft", "drafts", "compose"];

/// Classify a tool call by its name and the MCP server's annotations
/// (`readOnlyHint`, `destructiveHint`). Explicitly risky words in the name
/// win over annotations, so a mislabeled tool still asks.
pub fn classify(tool: &str, annotations: &Value) -> Category {
    let w = words(tool);
    let has = |set: &[&str]| w.iter().any(|x| set.contains(&x.as_str()));
    let first = w.first().map(String::as_str).unwrap_or("");
    let lead = |set: &[&str]| set.contains(&first);
    let read_first = lead(READ_VERBS);

    if has(PURCHASE_WORDS) && !(read_first && !has(&["pay", "buy", "purchase", "checkout"])) {
        return Category::Purchase;
    }
    if lead(PURCHASE_LEADING) || (!read_first && has(PURCHASE_NOUNS)) {
        return Category::Purchase;
    }
    if has(&["send", "reply", "forward", "publish", "invite", "tweet"])
        || (!read_first && (has(SEND_WORDS) || lead(SEND_LEADING)))
    {
        // `create_draft` and `draft_reply` stay drafts unless they send.
        if has(DRAFT_WORDS) && !has(&["send"]) {
            return Category::Draft;
        }
        return Category::Send;
    }
    if has(&["delete", "trash", "destroy", "erase", "purge", "wipe"])
        || (!read_first && has(DELETE_WORDS))
    {
        return Category::Delete;
    }
    if has(EXPORT_WORDS) || (has(&["download"]) && has(&["all", "everything"])) {
        return Category::Export;
    }
    if has(DRAFT_WORDS) {
        return Category::Draft;
    }
    if read_first || annotations["readOnlyHint"] == true {
        return Category::Read;
    }
    if annotations["destructiveHint"] == true {
        return Category::Delete;
    }
    Category::Write
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
];
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

/// Canonical form of one recipient: the address inside `Name <addr>`,
/// lowercased; phone numbers without punctuation.
pub fn normalize_recipient(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let (Some(a), Some(b)) = (s.rfind('<'), s.rfind('>'))
        && a < b
    {
        s = &s[a + 1..b];
    }
    let s = s.trim().trim_start_matches("mailto:").trim();
    if s.is_empty() || s.len() > 320 {
        return None;
    }
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let phoneish = s
        .chars()
        .all(|c| c.is_ascii_digit() || " +-().".contains(c));
    if phoneish && digits.len() >= 7 {
        let plus = if s.trim_start().starts_with('+') {
            "+"
        } else {
            ""
        };
        return Some(format!("{plus}{digits}"));
    }
    Some(s.to_lowercase())
}

fn push_recipients(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            for part in s.split([',', ';', '\n']) {
                if let Some(r) = normalize_recipient(part) {
                    out.push(r);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| push_recipients(i, out)),
        Value::Object(o) => {
            for (k, v) in o {
                if IDENTITY_FIELDS.contains(&norm_key(k).as_str()) {
                    push_recipients(v, out);
                }
            }
        }
        Value::Number(n) => {
            if let Some(r) = normalize_recipient(&n.to_string()) {
                out.push(r);
            }
        }
        _ => {}
    }
}

fn walk_recipients(v: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 5 {
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
                if RECIPIENT_KEYS.contains(&norm_key(k).as_str()) {
                    push_recipients(val, out);
                } else {
                    walk_recipients(val, depth + 1, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| walk_recipients(i, depth + 1, out)),
        _ => {}
    }
}

/// Everyone a call reaches, as far as its arguments say (deduplicated, in
/// order).
pub fn recipients(args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_recipients(args, 0, &mut out);
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
            ToolRule::Allow => Verdict::Allow(AllowedBy::Override),
        };
    }
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
const MAX_DETAILS: usize = 8000;

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
    let details = cut(
        &reveal_hidden(&serde_json::to_string_pretty(args).unwrap_or_default()),
        MAX_DETAILS,
    );
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
        assert_eq!(c("draft_reply"), Category::Draft);
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
        assert_eq!(c("frobnicate"), Category::Write);
    }

    #[test]
    fn annotations_help_but_risky_names_win() {
        let ro = json!({"readOnlyHint": true});
        assert_eq!(classify("frobnicate", &ro), Category::Read);
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
