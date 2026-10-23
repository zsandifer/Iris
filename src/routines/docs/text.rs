//! Text formats: plain text and Markdown (UTF-8, UTF-16 or Windows-1252),
//! JSON (pretty-printed), HTML (readable text with headings and lists) and
//! RTF (plain text).

use super::{Budget, DocError, DocType, Out, plural, words};
use crate::feed::markup::{Token, tokenize};

/// Bytes as text: a byte order mark picks UTF-8 or UTF-16; otherwise UTF-8,
/// or Windows-1252 (what older spreadsheet exports use) when it isn't.
/// Binary files (with NUL bytes) are refused.
pub(crate) fn decode(bytes: &[u8], kind: DocType) -> Result<String, DocError> {
    let text = if let Some((enc, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        enc.decode_without_bom_handling(&bytes[bom..])
            .0
            .into_owned()
    } else if let Ok(s) = std::str::from_utf8(bytes) {
        s.to_string()
    } else {
        encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned()
    };
    if text.contains('\0') {
        return Err(DocError::Unreadable(kind.noun()));
    }
    Ok(text.replace("\r\n", "\n"))
}

/// Markdown or plain text, as written.
pub(crate) fn plain(bytes: &[u8], kind: DocType) -> Result<String, DocError> {
    let mut out = Out::new();
    out.push(&decode(bytes, kind)?);
    Ok(out.finish())
}

/// JSON, checked and pretty-printed in a code block.
pub(crate) fn json(bytes: &[u8]) -> Result<(String, String), DocError> {
    let src = decode(bytes, DocType::Json)?;
    let value: serde_json::Value =
        serde_json::from_str(&src).map_err(|_| DocError::Unreadable(DocType::Json.noun()))?;
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|_| DocError::Unreadable(DocType::Json.noun()))?;
    let summary = match &value {
        serde_json::Value::Array(a) => plural(a.len(), "item", "items"),
        serde_json::Value::Object(o) => plural(o.len(), "field", "fields"),
        _ => words(&pretty),
    };
    let mut out = Out::new();
    out.push("```json\n");
    out.push(&pretty);
    out.push("\n```\n");
    Ok((out.finish(), summary))
}

/// Elements whose content is never shown.
const HIDDEN: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "iframe", "object", "textarea",
];

/// HTML as readable text: headings become `#` lines, list items `- `
/// lines, paragraphs and table rows their own lines.
pub(crate) fn html(bytes: &[u8], kind: DocType) -> Result<String, DocError> {
    let src = decode(bytes, kind)?;
    let mut out = String::new();
    let mut hidden = 0usize;
    let mut lists = 0usize;
    let mut pre = 0usize;
    // Cells so far in the current table row.
    let mut cells = 0usize;
    for tok in tokenize(&src, true) {
        match tok {
            Token::Start {
                name, self_closing, ..
            } => {
                if HIDDEN.contains(&name.as_str()) {
                    if !self_closing {
                        hidden += 1;
                    }
                    continue;
                }
                if hidden > 0 {
                    continue;
                }
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse::<usize>().unwrap_or(1);
                        block(&mut out);
                        out.push_str(&"#".repeat(level));
                        out.push(' ');
                    }
                    "ul" | "ol" => {
                        // A nested list continues its item's lines.
                        if lists > 0 {
                            line(&mut out)
                        } else {
                            block(&mut out)
                        }
                        lists += 1;
                    }
                    "li" => {
                        line(&mut out);
                        out.push_str(&"  ".repeat(lists.saturating_sub(1)));
                        out.push_str("- ");
                    }
                    "br" => out.push('\n'),
                    "pre" => {
                        pre += 1;
                        block(&mut out);
                    }
                    "td" | "th" => {
                        if cells > 0 {
                            out.push_str(" | ");
                        }
                        cells += 1;
                    }
                    "tr" => {
                        line(&mut out);
                        cells = 0;
                    }
                    n if is_block(n) => block(&mut out),
                    _ => {}
                }
            }
            Token::End { name } => {
                if HIDDEN.contains(&name.as_str()) {
                    hidden = hidden.saturating_sub(1);
                    continue;
                }
                if hidden > 0 {
                    continue;
                }
                match name.as_str() {
                    "ul" | "ol" => {
                        lists = lists.saturating_sub(1);
                        if lists > 0 {
                            line(&mut out)
                        } else {
                            block(&mut out)
                        }
                    }
                    "pre" => {
                        pre = pre.saturating_sub(1);
                        block(&mut out);
                    }
                    "li" | "tr" => line(&mut out),
                    n if n.len() == 2 && n.starts_with('h') || is_block(n) => block(&mut out),
                    _ => {}
                }
            }
            Token::Text(t) if hidden == 0 => {
                if pre > 0 {
                    out.push_str(&t);
                } else {
                    let collapsed = crate::feed::markup::collapse_ws(&t);
                    if collapsed.is_empty() {
                        continue;
                    }
                    if t.starts_with(char::is_whitespace) && !out.ends_with([' ', '\n']) {
                        out.push(' ');
                    }
                    out.push_str(&collapsed);
                    if t.ends_with(char::is_whitespace) {
                        out.push(' ');
                    }
                }
            }
            Token::Text(_) => {}
        }
    }
    let mut capped = Out::new();
    capped.push(&tidy(&out));
    Ok(capped.finish())
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "blockquote"
            | "table"
            | "figure"
            | "figcaption"
            | "hr"
            | "dl"
            | "dt"
            | "dd"
            | "form"
            | "address"
    )
}

/// End the current line.
fn line(out: &mut String) {
    let trimmed = out.trim_end_matches(' ').len();
    out.truncate(trimmed);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// End the current paragraph.
fn block(out: &mut String) {
    line(out);
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
}

/// Trim each line's trailing spaces and collapse runs of blank lines.
pub(crate) fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for l in s.lines() {
        let l = l.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(l);
    }
    out.push('\n');
    out
}

/// Groups whose content is not body text.
const RTF_SKIP: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "themedata",
    "colorschememapping",
    "datastore",
    "latentstyles",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "xmlnstbl",
    "mmathPr",
    "filetbl",
    "revtbl",
    "header",
    "headerl",
    "headerr",
    "headerf",
    "footer",
    "footerl",
    "footerr",
    "footerf",
    "fldinst",
    "nonshppict",
    "shppict",
    "userprops",
    "pgdsctbl",
    "bkmkstart",
    "bkmkend",
    "footnote",
    "annotation",
];

/// Deepest RTF group nesting followed.
const MAX_RTF_DEPTH: usize = 1_000;

/// RTF as plain text: control words for paragraphs, tabs and special
/// characters are honoured; fonts, pictures and other metadata are skipped.
pub(crate) fn rtf(bytes: &[u8], budget: &Budget) -> Result<String, DocError> {
    let noun = DocType::Rtf.noun();
    if !bytes.starts_with(b"{\\rtf") {
        return Err(DocError::Unreadable(noun));
    }
    let mut w = RtfText::new();
    // (skipping, unicode fallback length) per open group.
    let mut stack: Vec<(bool, usize)> = Vec::new();
    let (mut skip, mut uc) = (false, 1usize);
    // Fallback characters still to drop after a \u escape.
    let mut drop = 0usize;
    let mut i = 0;
    let mut steps = 0u32;
    while i < bytes.len() {
        steps = steps.wrapping_add(1);
        if steps.is_multiple_of(65_536) {
            budget.check()?;
        }
        let b = bytes[i];
        match b {
            b'{' => {
                if stack.len() >= MAX_RTF_DEPTH {
                    return Err(DocError::TooComplex);
                }
                stack.push((skip, uc));
                drop = 0;
                i += 1;
                // `{\*\dest ...}` is an optional destination: skip it.
                if bytes[i..].starts_with(b"\\*") {
                    skip = true;
                }
            }
            b'}' => {
                (skip, uc) = stack.pop().unwrap_or((true, 1));
                drop = 0;
                i += 1;
            }
            b'\\' => {
                i += 1;
                let Some(&c) = bytes.get(i) else { break };
                if c.is_ascii_alphabetic() {
                    let start = i;
                    while i < bytes.len() && bytes[i].is_ascii_alphabetic() && i - start < 32 {
                        i += 1;
                    }
                    let word = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
                    let num_start = i;
                    if bytes.get(i) == Some(&b'-') {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() && i - num_start < 10 {
                        i += 1;
                    }
                    let param = std::str::from_utf8(&bytes[num_start..i])
                        .ok()
                        .and_then(|n| n.parse::<i64>().ok());
                    if bytes.get(i) == Some(&b' ') {
                        i += 1;
                    }
                    if word == "bin" {
                        // Raw binary data follows.
                        i = i.saturating_add(param.unwrap_or(0).max(0) as usize);
                        continue;
                    }
                    if RTF_SKIP.contains(&word) {
                        skip = true;
                        continue;
                    }
                    match word {
                        "ansicpg" => {
                            if let Some(n) = param {
                                w.set_codepage(n);
                            }
                        }
                        "uc" => uc = param.unwrap_or(1).clamp(0, 10) as usize,
                        "u" if !skip => {
                            let n = param.unwrap_or(0);
                            let unit = if n < 0 { n + 65_536 } else { n };
                            w.unit(unit.clamp(0, 0xFFFF) as u16);
                            drop = uc;
                        }
                        _ if skip => {}
                        "par" | "line" | "sect" | "page" | "row" => w.text("\n"),
                        "tab" => w.text("\t"),
                        "cell" => w.text(" | "),
                        "emdash" => w.text("—"),
                        "endash" => w.text("–"),
                        "bullet" => w.text("•"),
                        "lquote" => w.text("‘"),
                        "rquote" => w.text("’"),
                        "ldblquote" => w.text("“"),
                        "rdblquote" => w.text("”"),
                        _ => {}
                    }
                } else {
                    i += 1;
                    match c {
                        b'\'' => {
                            let hex = bytes
                                .get(i..i + 2)
                                .and_then(|h| std::str::from_utf8(h).ok());
                            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                                i += 2;
                                if drop > 0 {
                                    drop -= 1;
                                } else if !skip {
                                    w.byte(v);
                                }
                            }
                        }
                        b'\\' | b'{' | b'}' if !skip => {
                            if drop > 0 {
                                drop -= 1;
                            } else {
                                w.byte(c);
                            }
                        }
                        b'~' if !skip => w.text(" "),
                        b'_' if !skip => w.text("-"),
                        b'\n' | b'\r' if !skip => w.text("\n"),
                        _ => {}
                    }
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                i += 1;
                if drop > 0 {
                    drop -= 1;
                } else if !skip {
                    w.byte(b);
                }
            }
        }
    }
    let mut out = Out::new();
    out.push(&tidy(&w.finish()));
    Ok(out.finish())
}

/// RTF text being assembled from code-page bytes and UTF-16 units.
struct RtfText {
    out: String,
    bytes: Vec<u8>,
    units: Vec<u16>,
    codepage: &'static encoding_rs::Encoding,
}

impl RtfText {
    fn new() -> Self {
        Self {
            out: String::new(),
            bytes: Vec::new(),
            units: Vec::new(),
            codepage: encoding_rs::WINDOWS_1252,
        }
    }
    fn set_codepage(&mut self, n: i64) {
        self.flush();
        let label = match n {
            65001 => "utf-8".to_string(),
            n => format!("windows-{n}"),
        };
        if let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            self.codepage = enc;
        }
    }
    fn flush(&mut self) {
        if !self.bytes.is_empty() {
            let (s, _) = self.codepage.decode_without_bom_handling(&self.bytes);
            self.out.push_str(&s);
            self.bytes.clear();
        }
        if !self.units.is_empty() {
            self.out.push_str(&String::from_utf16_lossy(&self.units));
            self.units.clear();
        }
    }
    fn byte(&mut self, b: u8) {
        if !self.units.is_empty() {
            self.flush();
        }
        self.bytes.push(b);
    }
    fn unit(&mut self, u: u16) {
        if !self.bytes.is_empty() {
            self.flush();
        }
        self.units.push(u);
    }
    fn text(&mut self, s: &str) {
        self.flush();
        self.out.push_str(s);
    }
    fn finish(mut self) -> String {
        self.flush();
        self.out
    }
}
