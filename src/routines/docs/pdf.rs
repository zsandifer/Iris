//! PDF: the text layer, page by page (no OCR), including text inside Form
//! XObjects. Streams are inflated with a size limit, so a small file can't
//! unpack into gigabytes.

use super::{Budget, DocError, MAX_PDF_PAGES, MAX_PDF_STREAM_BYTES, Out, plural, thousands};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Encoding, LoadOptions, Object, ObjectId, Stream};
use std::collections::BTreeMap;

const NOUN: &str = "PDF";
/// Deepest nesting of forms inside forms followed.
const MAX_FORM_DEPTH: usize = 8;
/// Most forms drawn per page (the same form may be drawn many times).
const MAX_FORMS_PER_PAGE: usize = 200;

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
    let (mut read, mut with_text, mut failed) = (0, 0, 0);
    for (number, id) in pages.iter().take(MAX_PDF_PAGES) {
        budget.check()?;
        if out.full() {
            break;
        }
        read += 1;
        match page_text(&doc, *id, budget) {
            Ok(t) if t.trim().is_empty() => {}
            Ok(t) => {
                with_text += 1;
                out.push(&format!("[Page {number}]\n"));
                out.push(super::text::tidy(&t).trim_end());
                out.push("\n\n");
            }
            Err(PageError::Doc(e)) => return Err(e),
            Err(PageError::Damaged) => {
                failed += 1;
                out.push(&format!("[Page {number}: I couldn't read this page.]\n\n"));
            }
        }
    }
    // Nothing but damaged pages: there's no text to keep.
    if with_text == 0 && failed > 0 {
        return Err(DocError::Unreadable(NOUN));
    }
    let mut text = out.finish();
    if with_text > 0 && pages.len() > read {
        text.push_str(&format!(
            "[Showing the first {} of {} pages.]\n",
            thousands(read),
            thousands(pages.len())
        ));
    }
    if with_text == 0 {
        text.clear();
    }
    Ok((text, plural(pages.len(), "page", "pages")))
}

fn load_error(e: lopdf::Error) -> DocError {
    match e {
        lopdf::Error::InvalidPassword
        | lopdf::Error::Decryption(_)
        | lopdf::Error::UnsupportedSecurityHandler(_) => DocError::Encrypted(NOUN),
        e if over_limit(&e) => DocError::TooComplex,
        _ => DocError::Unreadable(NOUN),
    }
}

fn over_limit(e: &lopdf::Error) -> bool {
    matches!(
        e,
        lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { .. })
    )
}

/// Why a page couldn't be read: the whole file stops (over a limit or out
/// of time), or just this page is damaged.
enum PageError {
    Doc(DocError),
    Damaged,
}

impl From<lopdf::Error> for PageError {
    fn from(e: lopdf::Error) -> Self {
        if over_limit(&e) {
            PageError::Doc(DocError::TooComplex)
        } else {
            PageError::Damaged
        }
    }
}

impl From<DocError> for PageError {
    fn from(e: DocError) -> Self {
        PageError::Doc(e)
    }
}

fn number(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

/// A dictionary, directly or through a reference.
fn dict<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Dictionary> {
    doc.dereference(o).ok().and_then(|(_, o)| o.as_dict().ok())
}

/// The fonts and forms a content stream can use, from its resource
/// dictionaries (the nearest first).
struct Resources<'a> {
    fonts: BTreeMap<Vec<u8>, Encoding<'a>>,
    forms: BTreeMap<Vec<u8>, &'a Stream>,
}

impl<'a> Resources<'a> {
    fn new(doc: &'a Document, dicts: &[&'a Dictionary]) -> Self {
        let mut fonts = BTreeMap::new();
        let mut forms = BTreeMap::new();
        for res in dicts {
            for (name, font) in res
                .get(b"Font")
                .ok()
                .and_then(|f| dict(doc, f))
                .into_iter()
                .flatten()
            {
                if fonts.contains_key(name) {
                    continue;
                }
                if let Some(enc) = dict(doc, font).and_then(|f| {
                    f.get_font_encoding_with_limit(doc, MAX_PDF_STREAM_BYTES)
                        .ok()
                }) {
                    fonts.insert(name.clone(), enc);
                }
            }
            for (name, x) in res
                .get(b"XObject")
                .ok()
                .and_then(|x| dict(doc, x))
                .into_iter()
                .flatten()
            {
                let Ok((_, Object::Stream(s))) = doc.dereference(x) else {
                    continue;
                };
                let is_form = s.dict.get(b"Subtype").ok().and_then(|t| t.as_name().ok())
                    == Some(b"Form".as_slice());
                if is_form {
                    forms.entry(name.clone()).or_insert(s);
                }
            }
        }
        Self { fonts, forms }
    }
}

/// The text shown on one page, with line breaks where the text moves to a
/// new line and spaces where it jumps along the same line.
fn page_text(doc: &Document, id: ObjectId, budget: &Budget) -> Result<String, PageError> {
    let (own, inherited) = doc.get_page_resources(id)?;
    let mut dicts: Vec<&Dictionary> = own.into_iter().collect();
    dicts.extend(inherited.iter().filter_map(|i| doc.get_dictionary(*i).ok()));
    let content = doc.get_page_content_with_limit(id, MAX_PDF_STREAM_BYTES)?;
    let mut page = Page {
        doc,
        budget,
        s: String::new(),
        y: None,
        forms: 0,
    };
    page.run(&content, &dicts, 0)?;
    Ok(page.s)
}

struct Page<'a> {
    doc: &'a Document,
    budget: &'a Budget,
    s: String,
    y: Option<f32>,
    /// Forms drawn so far on this page.
    forms: usize,
}

impl<'a> Page<'a> {
    /// Show one content stream (the page's, or a form's).
    fn run(
        &mut self,
        content: &[u8],
        dicts: &[&'a Dictionary],
        depth: usize,
    ) -> Result<(), PageError> {
        let res = Resources::new(self.doc, dicts);
        let content = Content::decode(content)?;
        let mut font: Option<&Encoding> = None;
        for op in &content.operations {
            let args = &op.operands;
            let s = &mut self.s;
            match op.operator.as_str() {
                "Tf" => {
                    font = args
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|n| res.fonts.get(n));
                }
                "ET" | "T*" => newline(s),
                "Td" | "TD" => {
                    let dy = args.get(1).and_then(number).unwrap_or(0.0);
                    if dy != 0.0 {
                        newline(s);
                    } else {
                        space(s);
                    }
                }
                "Tm" => {
                    let new_y = args.get(5).and_then(number);
                    if self.y.is_some() && new_y != self.y {
                        newline(s);
                    } else {
                        space(s);
                    }
                    self.y = new_y;
                }
                "Tj" => show(s, font, args.first()),
                "'" => {
                    newline(s);
                    show(s, font, args.first());
                }
                "\"" => {
                    newline(s);
                    show(s, font, args.get(2));
                }
                "TJ" => {
                    for item in args
                        .first()
                        .and_then(|a| a.as_array().ok())
                        .into_iter()
                        .flatten()
                    {
                        match item {
                            Object::String(..) => show(s, font, Some(item)),
                            // A big jump back in a TJ array is a word gap.
                            o if number(o).is_some_and(|n| n < -200.0) => space(s),
                            _ => {}
                        }
                    }
                }
                // Draw a form: its own content, with its own resources
                // (falling back to the page's).
                "Do" => {
                    let Some(form) = args
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|n| res.forms.get(n))
                    else {
                        continue;
                    };
                    if depth >= MAX_FORM_DEPTH || self.forms >= MAX_FORMS_PER_PAGE {
                        continue;
                    }
                    self.forms += 1;
                    self.budget.check()?;
                    let body = form.get_plain_content_with_limit(MAX_PDF_STREAM_BYTES)?;
                    let mut inner: Vec<&'a Dictionary> = form
                        .dict
                        .get(b"Resources")
                        .ok()
                        .and_then(|r| dict(self.doc, r))
                        .into_iter()
                        .collect();
                    inner.extend_from_slice(dicts);
                    newline(&mut self.s);
                    self.run(&body, &inner, depth + 1)?;
                    newline(&mut self.s);
                }
                _ => {}
            }
        }
        Ok(())
    }
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
