//! PDF: the text layer, page by page (no OCR). Streams are inflated with a
//! size limit, so a small file can't unpack into gigabytes.

use super::{Budget, DocError, MAX_PDF_PAGES, MAX_PDF_STREAM_BYTES, Out, plural, thousands};
use lopdf::content::Content;
use lopdf::{Document, Encoding, LoadOptions, Object, ObjectId};
use std::collections::BTreeMap;

const NOUN: &str = "PDF";

pub(crate) fn text(bytes: &[u8], budget: &Budget) -> Result<(String, String), DocError> {
    // The header may follow a little junk, but not much.
    let head = &bytes[..bytes.len().min(1024)];
    if !head.windows(5).any(|w| w == b"%PDF-") {
        return Err(DocError::Unreadable(NOUN));
    }
    let options = LoadOptions {
        max_decompressed_size: Some(MAX_PDF_STREAM_BYTES),
        ..Default::default()
    };
    let doc = Document::load_mem_with_options(bytes, options).map_err(load_error)?;
    // Encrypted, and the empty password (the usual "no password to open")
    // didn't unlock it.
    if doc.is_encrypted() && !doc.was_encrypted() {
        return Err(DocError::Encrypted(NOUN));
    }
    budget.check()?;
    let pages = doc.get_pages();
    if pages.is_empty() {
        return Err(DocError::Unreadable(NOUN));
    }
    let mut out = Out::new();
    let mut read = 0;
    for (number, id) in pages.iter().take(MAX_PDF_PAGES) {
        budget.check()?;
        if out.full() {
            break;
        }
        read += 1;
        match page_text(&doc, *id) {
            Ok(t) if t.trim().is_empty() => {}
            Ok(t) => {
                out.push(&format!("[Page {number}]\n"));
                out.push(super::text::tidy(&t).trim_end());
                out.push("\n\n");
            }
            Err(_) => out.push(&format!("[Page {number}: I couldn't read this page.]\n\n")),
        }
    }
    let mut text = out.finish();
    if !text.trim().is_empty() && pages.len() > read {
        text.push_str(&format!(
            "[Showing the first {} of {} pages.]\n",
            thousands(read),
            thousands(pages.len())
        ));
    }
    Ok((text, plural(pages.len(), "page", "pages")))
}

fn load_error(e: lopdf::Error) -> DocError {
    match e {
        lopdf::Error::InvalidPassword
        | lopdf::Error::Decryption(_)
        | lopdf::Error::UnsupportedSecurityHandler(_) => DocError::Encrypted(NOUN),
        lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. }) => {
            DocError::TooComplex
        }
        _ => DocError::Unreadable(NOUN),
    }
}

fn number(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

/// The text shown on one page, with line breaks where the text moves to a
/// new line and spaces where it jumps along the same line.
fn page_text(doc: &Document, id: ObjectId) -> Result<String, lopdf::Error> {
    let encodings: BTreeMap<Vec<u8>, Encoding> = doc
        .get_page_fonts(id)?
        .into_iter()
        .filter_map(|(name, font)| {
            font.get_font_encoding_with_limit(doc, MAX_PDF_STREAM_BYTES)
                .ok()
                .map(|e| (name, e))
        })
        .collect();
    let content = doc.get_page_content_with_limit(id, MAX_PDF_STREAM_BYTES)?;
    let content = Content::decode(&content)?;
    let mut s = String::new();
    let mut font: Option<&Encoding> = None;
    let mut y: Option<f32> = None;
    for op in &content.operations {
        let args = &op.operands;
        match op.operator.as_str() {
            "Tf" => {
                font = args
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|n| encodings.get(n));
            }
            "ET" | "T*" => newline(&mut s),
            "Td" | "TD" => {
                let dy = args.get(1).and_then(number).unwrap_or(0.0);
                if dy != 0.0 {
                    newline(&mut s);
                } else {
                    space(&mut s);
                }
            }
            "Tm" => {
                let new_y = args.get(5).and_then(number);
                if y.is_some() && new_y != y {
                    newline(&mut s);
                } else {
                    space(&mut s);
                }
                y = new_y;
            }
            "Tj" => show(&mut s, font, args.first()),
            "'" => {
                newline(&mut s);
                show(&mut s, font, args.first());
            }
            "\"" => {
                newline(&mut s);
                show(&mut s, font, args.get(2));
            }
            "TJ" => {
                for item in args
                    .first()
                    .and_then(|a| a.as_array().ok())
                    .into_iter()
                    .flatten()
                {
                    match item {
                        Object::String(..) => show(&mut s, font, Some(item)),
                        // A big jump back in a TJ array is a word gap.
                        o if number(o).is_some_and(|n| n < -200.0) => space(&mut s),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(s)
}

fn show(s: &mut String, font: Option<&Encoding>, text: Option<&Object>) {
    let Some(Object::String(bytes, _)) = text else {
        return;
    };
    match font {
        Some(enc) => {
            if let Ok(t) = enc.bytes_to_string(bytes) {
                s.push_str(&t);
            }
        }
        // No usable font encoding: plain ASCII is still readable.
        None => {
            if bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
                s.push_str(&String::from_utf8_lossy(bytes));
            }
        }
    }
}

fn newline(s: &mut String) {
    let t = s.trim_end_matches(' ').len();
    s.truncate(t);
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
}

fn space(s: &mut String) {
    if !s.is_empty() && !s.ends_with([' ', '\n']) {
        s.push(' ');
    }
}
