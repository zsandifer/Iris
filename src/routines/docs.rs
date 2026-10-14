//! Reference documents for routines: text extracted once at upload, then
//! pasted into every run's prompt as labelled sections, within a size cap.
//!
//! Only Markdown and plain text are read for now. New types (PDF, Word)
//! plug into [`extract_text`], the one place that dispatches on file type.

use std::path::Path;

/// Most document text pasted into one run, over all documents.
pub const MAX_DOC_CHARS: usize = 40_000;

/// A file type routines can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    Markdown,
    Text,
}

impl DocType {
    pub fn mime(self) -> &'static str {
        match self {
            DocType::Markdown => "text/markdown",
            DocType::Text => "text/plain",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            DocType::Markdown => "md",
            DocType::Text => "txt",
        }
    }
}

/// What the app says it accepts (and the error says).
pub const ACCEPTED: &str = "Markdown (.md) or plain text (.txt)";

/// The type of an uploaded file, from its name.
pub fn doc_type(file_name: &str) -> Option<DocType> {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("md" | "markdown") => Some(DocType::Markdown),
        Some("txt" | "text") => Some(DocType::Text),
        _ => None,
    }
}

/// Extract the text of an uploaded document. The single entry point for
/// every file type; errors are friendly and ready to show.
pub fn extract_text(file_name: &str, bytes: &[u8]) -> Result<(DocType, String), String> {
    let Some(kind) = doc_type(file_name) else {
        return Err(format!(
            "I can only read {ACCEPTED} files for routines right now. Copy the text into a \
             .txt or .md file and upload that instead."
        ));
    };
    let text = match kind {
        DocType::Markdown | DocType::Text => plain_text(bytes)?,
    };
    if text.trim().is_empty() {
        return Err("That file looks empty, so there's nothing for me to read.".into());
    }
    Ok((kind, text))
}

fn plain_text(bytes: &[u8]) -> Result<String, String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "I couldn't read that file as text. Save it as UTF-8 text and try again.")?;
    if text.contains('\0') {
        return Err("That doesn't look like a text file.".into());
    }
    Ok(text.replace("\r\n", "\n"))
}

/// The documents as labelled sections for a run's prompt. Text over the
/// cap is cut with a note saying so.
pub fn prompt_sections(docs: &[(String, String)], cap: usize) -> String {
    let mut out = String::new();
    let mut left = cap;
    for (title, text) in docs {
        let title = title.replace(['"', '<', '>'], "");
        let text = text.trim();
        let total = text.chars().count();
        if left == 0 {
            out.push_str(&format!(
                "<document title=\"{title}\">\n[Left out: the documents above already fill the \
                 space for reference text. Tell the owner if you needed this one.]\n</document>\n\n"
            ));
            continue;
        }
        let body: String = text.chars().take(left).collect();
        left -= body.chars().count();
        out.push_str(&format!("<document title=\"{title}\">\n{body}\n"));
        if total > body.chars().count() {
            out.push_str(&format!(
                "[Truncated: only the first {} of {total} characters are included.]\n",
                body.chars().count()
            ));
        }
        out.push_str("</document>\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_dispatches_on_file_type() {
        let (t, text) =
            extract_text("Takeout SOP.md", b"\xEF\xBB\xBF# Usual\r\n- Pad See Ew\r\n").unwrap();
        assert_eq!(t, DocType::Markdown);
        assert_eq!(text, "# Usual\n- Pad See Ew\n");
        assert_eq!(
            extract_text("notes.TXT", b"hello").unwrap().0,
            DocType::Text
        );
        for bad in ["menu.pdf", "plan.docx", "noext", "image.png"] {
            let e = extract_text(bad, b"%PDF-1.7").unwrap_err();
            assert!(e.contains("Markdown (.md) or plain text (.txt)"), "{e}");
        }
        assert!(extract_text("a.txt", b"\xff\xfe\x00").is_err());
        assert!(extract_text("a.txt", b"bin\0ary").is_err());
        assert!(extract_text("a.md", b"  \n ").is_err());
    }

    #[test]
    fn sections_are_labelled_and_capped() {
        let docs = vec![
            ("SOP.md".to_string(), "a".repeat(30)),
            ("Menu \"v2\".txt".to_string(), "b".repeat(30)),
            ("Late.md".to_string(), "c".repeat(5)),
        ];
        let s = prompt_sections(&docs, 50);
        assert!(s.contains(&format!(
            "<document title=\"SOP.md\">\n{}\n</document>",
            "a".repeat(30)
        )));
        assert!(s.contains("<document title=\"Menu v2.txt\">"));
        assert!(s.contains(&"b".repeat(20)) && !s.contains(&"b".repeat(21)));
        assert!(s.contains("[Truncated: only the first 20 of 30 characters are included.]"));
        assert!(s.contains("<document title=\"Late.md\">\n[Left out"));
        assert!(!s.contains('c'.to_string().repeat(5).as_str()));
        let all = prompt_sections(&docs, MAX_DOC_CHARS);
        assert!(!all.contains("Truncated") && !all.contains("Left out"));
    }
}
