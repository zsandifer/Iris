//! Markdown in and out of memory notes.
//!
//! [`parse`] splits a Markdown note file into notes: each top-level bullet
//! (with its nested lines) or paragraph is one note. Headings are context,
//! not notes. [`render`] writes notes back as bullets in a form [`parse`]
//! reads back to the same texts, so an export file that is read, edited and
//! written again changes only the notes that were edited.

use super::{Item, Topic};

/// First line of every file Iris generates in the memory folder.
pub const HEADER: &str =
    "<!-- Iris writes this file from its memory. Changes made to it here are not kept. -->";

/// One note found in a Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    /// The headings it sits under (below the title), outermost first:
    /// `## Alex` then `### Work`.
    pub headings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    /// The first level-one heading.
    pub title: Option<String>,
    pub notes: Vec<Note>,
}

struct Block {
    lines: Vec<String>,
    /// Bullet blocks take indented lines (nested lists, continuations).
    bullet: bool,
    /// Columns to strip from continuation lines.
    indent: usize,
    blank_pending: bool,
    headings: Vec<String>,
}

/// Split Markdown into notes. Front matter and HTML comments are dropped.
pub fn parse(content: &str) -> Parsed {
    let text = strip_comments(&strip_front_matter(&content.replace("\r\n", "\n")));
    let mut out = Parsed::default();
    // Open sections: (level, text).
    let mut sections: Vec<(usize, String)> = Vec::new();
    let headings =
        |sections: &[(usize, String)]| sections.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>();
    let mut block: Option<Block> = None;
    let mut fence: Option<&str> = None;
    let flush = |block: &mut Option<Block>, out: &mut Parsed| {
        if let Some(b) = block.take() {
            let text = normalize(&b.lines.join("\n"));
            if !text.is_empty() {
                out.notes.push(Note {
                    text,
                    headings: b.headings,
                });
            }
        }
    };
    for raw in text.lines() {
        let line = expand_tabs(raw.trim_end());
        // Only ASCII spaces indent (tabs are expanded); other whitespace
        // is text, so slicing by `indent` never splits a character.
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        let trimmed = &line[indent..];
        if let Some(marker) = fence {
            if let Some(b) = block.as_mut() {
                b.lines.push(line.clone());
            }
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.trim().is_empty() {
            if let Some(b) = block.as_mut() {
                b.blank_pending = true;
            }
            continue;
        }
        // Indented lines under a bullet belong to it (nested lists, code,
        // continuation paragraphs), even after a blank line.
        if let Some(b) = block.as_mut().filter(|b| b.bullet && indent >= 2) {
            if std::mem::take(&mut b.blank_pending) {
                b.lines.push(String::new());
            }
            let strip = indent.min(b.indent);
            b.lines.push(line[strip..].to_string());
            continue;
        }
        if indent < 4 && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            fence = Some(if trimmed.starts_with("```") {
                "```"
            } else {
                "~~~"
            });
            match block.as_mut().filter(|b| !b.blank_pending) {
                Some(b) => b.lines.push(trimmed.to_string()),
                None => {
                    flush(&mut block, &mut out);
                    block = Some(paragraph(trimmed, headings(&sections)));
                }
            }
            continue;
        }
        if indent < 4
            && let Some((level, title)) = heading_of(trimmed)
        {
            flush(&mut block, &mut out);
            if level == 1 && out.title.is_none() {
                out.title = Some(title);
                sections.clear();
            } else {
                sections.retain(|(l, _)| *l < level);
                sections.push((level, title));
            }
            continue;
        }
        if indent < 4 && is_break(trimmed) {
            flush(&mut block, &mut out);
            continue;
        }
        if indent < 4
            && let Some(width) = bullet_width(trimmed)
        {
            flush(&mut block, &mut out);
            block = Some(Block {
                lines: vec![trimmed[width..].to_string()],
                bullet: true,
                indent: indent + width,
                blank_pending: false,
                headings: headings(&sections),
            });
            continue;
        }
        match block.as_mut().filter(|b| !b.blank_pending) {
            // A line right after a note continues it.
            Some(b) => b.lines.push(trimmed.to_string()),
            None => {
                flush(&mut block, &mut out);
                block = Some(paragraph(trimmed, headings(&sections)));
            }
        }
    }
    flush(&mut block, &mut out);
    out
}

fn paragraph(line: &str, headings: Vec<String>) -> Block {
    Block {
        lines: vec![line.to_string()],
        bullet: false,
        indent: 0,
        blank_pending: false,
        headings,
    }
}

/// The form a note's text is stored in: trailing spaces removed from every
/// line, and blank lines trimmed from both ends.
pub fn normalize(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    lines.join("\n").trim().to_string()
}

fn expand_tabs(line: &str) -> String {
    let lead = line.len() - line.trim_start_matches(['\t', ' ']).len();
    let (head, rest) = line.split_at(lead);
    format!("{}{rest}", head.replace('\t', "    "))
}

fn strip_front_matter(text: &str) -> String {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return text.to_string();
    }
    let rest: Vec<&str> = lines.collect();
    match rest
        .iter()
        .position(|l| matches!(l.trim_end(), "---" | "..."))
    {
        Some(end) => rest[end + 1..].join("\n"),
        None => text.to_string(),
    }
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn heading_of(line: &str) -> Option<(usize, String)> {
    let level = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    Some((level, title.to_string()))
}

fn is_break(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_')
            || compact.chars().all(|c| c == '='))
}

/// Width of a list marker and the space after it (`- `, `* `, `+ `, `1. `,
/// `2) `), or None when the line is not a list item.
fn bullet_width(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let marker = match bytes.first()? {
        b'-' | b'*' | b'+' => 1,
        b'0'..=b'9' => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    match bytes.get(marker) {
        None => Some(marker),
        Some(b' ') => {
            let spaces = bytes[marker..].iter().take_while(|&&b| b == b' ').count();
            // Five or more spaces start indented code; keep them as text.
            Some(marker + if spaces > 4 { 1 } else { spaces })
        }
        _ => None,
    }
}

/// The section heading `heading` adds to a file.
fn heading_line(heading: &str) -> String {
    format!("\n## {heading}\n\n")
}

/// Characters a section heading takes in `MEMORY.md`.
pub fn heading_chars(heading: &str) -> usize {
    heading_line(heading).chars().count()
}

/// Characters a note takes as a bullet.
pub fn bullet_chars(text: &str) -> usize {
    let mut out = String::new();
    bullet(&mut out, text);
    out.chars().count()
}

/// Write `items` as bullets under `heading` (skipped when empty).
fn section(out: &mut String, heading: &str, items: &[&Item]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&heading_line(heading));
    for item in items {
        bullet(out, &item.text);
    }
}

fn bullet(out: &mut String, text: &str) {
    for (i, line) in text.lines().enumerate() {
        match (i, line.is_empty()) {
            (0, _) => out.push_str("- "),
            (_, true) => {}
            _ => out.push_str("  "),
        }
        out.push_str(line);
        out.push('\n');
    }
}

/// Heading of the pinned notes in `MEMORY.md`.
pub const PINNED_HEADING: &str = "Always in mind";

/// The section of `MEMORY.md` a note is listed under.
pub fn core_heading(item: &Item) -> &'static str {
    if item.pinned {
        PINNED_HEADING
    } else {
        item.topic.title()
    }
}

/// Notes the core has no room for (see [`super::packing`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LeftOut {
    /// Pinned notes that did not fit.
    pub pinned: usize,
    /// Other live notes, listed only in their topic files.
    pub other: usize,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `MEMORY.md`: pinned notes, then the rest of the core by topic, in the
/// order given. Notes left out are counted in comments, which the agent
/// sees in its prompt and which never read back as notes.
pub fn render_core(items: &[&Item], left_out: LeftOut) -> String {
    let mut out = format!("{HEADER}\n# Memory\n");
    if items.is_empty() && left_out == LeftOut::default() {
        out.push_str("\n<!-- Nothing here yet. -->\n");
        return out;
    }
    let pinned: Vec<&Item> = items.iter().copied().filter(|i| i.pinned).collect();
    section(&mut out, PINNED_HEADING, &pinned);
    for topic in Topic::ALL {
        let of: Vec<&Item> = items
            .iter()
            .copied()
            .filter(|i| !i.pinned && i.topic == topic)
            .collect();
        section(&mut out, topic.title(), &of);
    }
    if left_out.pinned > 0 {
        out.push_str(&format!(
            "\n<!-- {} did not fit here; read them in the topic files. -->\n",
            plural(left_out.pinned, "pinned note", "pinned notes")
        ));
    }
    if left_out.other > 0 {
        out.push_str(&format!(
            "\n<!-- {} only in the topic files. -->\n",
            plural(left_out.other, "more note is", "more notes are")
        ));
    }
    out
}

/// One topic file (`people.md` ...): every live note of that topic.
pub fn render_topic(topic: Topic, items: &[&Item]) -> String {
    let mut out = format!("{HEADER}\n# {}\n\n", topic.title());
    for item in items {
        bullet(&mut out, &item.text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(md: &str) -> Vec<String> {
        parse(md).notes.into_iter().map(|n| n.text).collect()
    }

    #[test]
    fn bullets_and_paragraphs_become_notes() {
        let md = "---\ntags: [x]\n---\n# Alex\n\nFriend from the climbing gym.\nMet in 2024.\n\n\
                  ## Likes\n\n- Coffee, black\n- Bouldering\n  - mostly on Tuesdays\n\n  Prefers mornings.\n\
                  * Board games\n1. First\n2) Second\n<!-- hidden -->\n---\n\nLast paragraph\n";
        let p = parse(md);
        assert_eq!(p.title.as_deref(), Some("Alex"));
        let t: Vec<&str> = p.notes.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(
            t,
            [
                "Friend from the climbing gym.\nMet in 2024.",
                "Coffee, black",
                "Bouldering\n- mostly on Tuesdays\n\nPrefers mornings.",
                "Board games",
                "First",
                "Second",
                "Last paragraph",
            ]
        );
        assert!(p.notes[0].headings.is_empty());
        assert_eq!(p.notes[1].headings, ["Likes"]);
        assert_eq!(p.notes[6].headings, ["Likes"]);
        let nested =
            parse("# People\n## Alex\n### Work\n- Teaches\n## Sam\n- Bakes\n#### Deep\n- x\n");
        let chains: Vec<Vec<String>> = nested.notes.into_iter().map(|n| n.headings).collect();
        assert_eq!(
            chains,
            [vec!["Alex", "Work"], vec!["Sam"], vec!["Sam", "Deep"]]
        );
    }

    #[test]
    fn markup_that_is_not_a_list_stays_text() {
        assert_eq!(
            texts("*bold* start\n-dash\n#hashtag\n\n```\n# not a heading\n- not a bullet\n```\n"),
            [
                "*bold* start\n-dash\n#hashtag",
                "```\n# not a heading\n- not a bullet\n```"
            ]
        );
        assert_eq!(texts("<!-- open\n- x\n"), Vec::<String>::new());
        // Non-ASCII whitespace is text, never indentation.
        assert_eq!(
            texts("- Fact\n\u{3000}continued\n  \u{3000}nested\n\u{3000}\n- Next"),
            ["Fact\n\u{3000}continued\n\u{3000}nested", "Next"]
        );
        assert_eq!(texts("- \n\n# Title only\n"), Vec::<String>::new());
    }

    #[test]
    fn rendering_reads_back_to_the_same_notes() {
        let items: Vec<Item> = [
            "Plain fact",
            "Line one\nline two",
            "Nested\n- a\n  - b\n\nAfter a blank line",
            "```\ncode with # hash\n- dash\n```",
            "# Looks like a heading",
            "1. starts like a list",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| super::super::tests::item(i as i64 + 1, t, Topic::People))
        .collect();
        let refs: Vec<&Item> = items.iter().collect();
        for md in [
            render_topic(Topic::People, &refs),
            render_core(&refs, LeftOut::default()),
        ] {
            assert!(md.starts_with(HEADER));
            let back = texts(&md);
            let want: Vec<String> = items.iter().map(|i| i.text.clone()).collect();
            assert_eq!(back, want, "{md}");
        }
        assert_eq!(
            texts(&render_core(&[], LeftOut::default())),
            Vec::<String>::new()
        );
        // Counts of notes left out are comments, never notes.
        let left = render_core(
            &refs[..1],
            LeftOut {
                pinned: 2,
                other: 1,
            },
        );
        assert!(left.contains("2 pinned notes did not fit") && left.contains("1 more note is"));
        assert_eq!(texts(&left), ["Plain fact"]);
        // A note's cost is exactly what it adds to the file.
        for item in &items {
            let mut one = String::new();
            section(&mut one, "People", &[item]);
            assert_eq!(
                one.chars().count(),
                heading_chars("People") + bullet_chars(&item.text)
            );
        }
    }
}
