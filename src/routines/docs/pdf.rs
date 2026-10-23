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

/// Glyph widths of a font, in thousandths of the font size.
enum Widths {
    /// One byte per glyph: `/FirstChar` and `/Widths`.
    Simple {
        first: i64,
        widths: Vec<f32>,
        missing: f32,
    },
    /// Two bytes per glyph (Identity-encoded CID fonts): `/W` and `/DW`.
    Cid { widths: BTreeMap<u32, f32>, dw: f32 },
    /// Not known: where the text ends can't be worked out.
    Unknown,
}

impl Widths {
    fn of(doc: &Document, font: &Dictionary) -> Widths {
        let num =
            |o: &Object| number(o).or_else(|| doc.dereference(o).ok().and_then(|(_, o)| number(o)));
        let array = |o: &Object| -> Option<Vec<Object>> {
            doc.dereference(o)
                .ok()
                .and_then(|(_, o)| o.as_array().ok().cloned())
        };
        if font.get(b"Subtype").ok().and_then(|t| t.as_name().ok()) == Some(b"Type0".as_slice()) {
            let identity = font
                .get(b"Encoding")
                .ok()
                .and_then(|e| e.as_name().ok())
                .is_some_and(|e| e.starts_with(b"Identity"));
            let Some(cid) = font
                .get(b"DescendantFonts")
                .ok()
                .and_then(array)
                .and_then(|d| d.first().and_then(|f| dict(doc, f).cloned()))
            else {
                return Widths::Unknown;
            };
            if !identity {
                return Widths::Unknown;
            }
            let dw = cid.get(b"DW").ok().and_then(num).unwrap_or(1000.0);
            let mut widths = BTreeMap::new();
            let w = cid.get(b"W").ok().and_then(array).unwrap_or_default();
            let mut i = 0;
            // [c [w1 w2 ...]] or [c_first c_last w], repeated.
            while i < w.len() && widths.len() < 65_536 {
                let Some(c) = num(&w[i]) else { break };
                let c = c as u32;
                if let Some(list) = w.get(i + 1).and_then(array) {
                    for (k, v) in list.iter().enumerate().take(65_536) {
                        if let Some(v) = num(v) {
                            widths.insert(c.saturating_add(k as u32), v);
                        }
                    }
                    i += 2;
                } else if let (Some(last), Some(v)) =
                    (w.get(i + 1).and_then(num), w.get(i + 2).and_then(num))
                {
                    for k in c..=(last as u32).min(c.saturating_add(65_535)) {
                        widths.insert(k, v);
                    }
                    i += 3;
                } else {
                    break;
                }
            }
            return Widths::Cid { widths, dw };
        }
        let Some(list) = font.get(b"Widths").ok().and_then(array) else {
            return Widths::Unknown;
        };
        let first = font.get(b"FirstChar").ok().and_then(num).unwrap_or(0.0) as i64;
        let missing = font
            .get(b"FontDescriptor")
            .ok()
            .and_then(|d| dict(doc, d))
            .and_then(|d| d.get(b"MissingWidth").ok().and_then(num))
            .unwrap_or(0.0);
        Widths::Simple {
            first,
            widths: list.iter().map(|v| num(v).unwrap_or(missing)).collect(),
            missing,
        }
    }

    /// The glyph widths of `bytes` (thousandths of the font size) and how
    /// many of them are single-byte spaces (word spacing applies to those).
    fn measure(&self, bytes: &[u8]) -> Option<(f32, usize, usize)> {
        match self {
            Widths::Simple {
                first,
                widths,
                missing,
            } => {
                let w = bytes
                    .iter()
                    .map(|b| {
                        usize::try_from(*b as i64 - first)
                            .ok()
                            .and_then(|i| widths.get(i).copied())
                            .unwrap_or(*missing)
                    })
                    .sum();
                Some((w, bytes.len(), bytes.iter().filter(|b| **b == b' ').count()))
            }
            Widths::Cid { widths, dw } => {
                let w = bytes
                    .chunks(2)
                    .map(|c| {
                        let code = c.iter().fold(0u32, |a, b| (a << 8) | *b as u32);
                        widths.get(&code).copied().unwrap_or(*dw)
                    })
                    .sum();
                Some((w, bytes.len().div_ceil(2), 0))
            }
            Widths::Unknown => None,
        }
    }
}

/// A font a content stream can select.
struct Font<'a> {
    enc: Encoding<'a>,
    widths: Widths,
}

/// The fonts and forms a content stream can use, from its resource
/// dictionaries (the nearest first).
struct Resources<'a> {
    fonts: BTreeMap<Vec<u8>, Font<'a>>,
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
                let Some(font) = dict(doc, font) else {
                    continue;
                };
                if let Ok(enc) = font.get_font_encoding_with_limit(doc, MAX_PDF_STREAM_BYTES) {
                    let widths = Widths::of(doc, font);
                    fonts.insert(name.clone(), Font { enc, widths });
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

/// The text shown on one page. Fragments on one baseline are joined with a
/// space only where there is a visible gap between them (worked out from
/// glyph widths when the font has them), and a new baseline starts a new
/// line.
fn page_text(doc: &Document, id: ObjectId, budget: &Budget) -> Result<String, PageError> {
    let (own, inherited) = doc.get_page_resources(id)?;
    let mut dicts: Vec<&Dictionary> = own.into_iter().collect();
    dicts.extend(inherited.iter().filter_map(|i| doc.get_dictionary(*i).ok()));
    let content = doc.get_page_content_with_limit(id, MAX_PDF_STREAM_BYTES)?;
    let mut page = Page {
        doc,
        budget,
        s: String::new(),
        forms: 0,
        text: TextState::default(),
        pen: None,
        ctm: IDENTITY,
        saved: Vec::new(),
    };
    page.run(&content, &dicts, 0)?;
    Ok(page.s)
}

/// Text state that positions glyphs (PDF 32000-1 §9.3).
#[derive(Clone, Copy)]
struct TextState {
    size: f32,
    char_spacing: f32,
    word_spacing: f32,
    scale: f32,
    leading: f32,
    /// The text line matrix's horizontal scale and origin.
    a: f32,
    d: f32,
    e: f32,
    f: f32,
    /// How far along the line the next glyph goes (text space), if known.
    x: Option<f32>,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            size: 12.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            scale: 1.0,
            leading: 0.0,
            a: 1.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
            x: Some(0.0),
        }
    }
}

struct Page<'a> {
    doc: &'a Document,
    budget: &'a Budget,
    s: String,
    /// Forms drawn so far on this page.
    forms: usize,
    text: TextState,
    /// Where the last fragment ended on the page (x if known, baseline y).
    pen: Option<(Option<f32>, f32)>,
    /// The graphics transform (`cm`), and the ones saved by `q`.
    ctm: Matrix,
    saved: Vec<Matrix>,
}

type Matrix = [f32; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
/// Deepest `q` nesting followed.
const MAX_SAVED: usize = 256;

/// `m` then `n` (PDF's row-vector order: a point goes through `m` first).
fn concat(m: Matrix, n: Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn matrix(args: &[Object]) -> Option<Matrix> {
    let v: Vec<f32> = args.iter().filter_map(number).collect();
    (v.len() == 6).then(|| [v[0], v[1], v[2], v[3], v[4], v[5]])
}

impl<'a> Page<'a> {
    /// Move the text line origin to (e, f), as `Td` and `Tm` do.
    fn line_at(&mut self, e: f32, f: f32) {
        self.text.e = e;
        self.text.f = f;
        self.text.x = Some(0.0);
    }

    /// Show one string: a separator first if it doesn't continue the last
    /// fragment, then its text; then move along by its width. `joined`:
    /// the next piece of one `TJ` array, which continues the last one unless
    /// widths say otherwise.
    fn show(&mut self, font: Option<&Font>, text: Option<&Object>, joined: bool) {
        let Some(Object::String(bytes, _)) = text else {
            return;
        };
        let t = self.text;
        // Positions on the page: text space through the text line matrix,
        // then the graphics transform.
        let m = self.ctm;
        let page = |x: f32| {
            let (ux, uy) = (t.e + x * t.a, t.f);
            (m[0] * ux + m[2] * uy + m[4], m[1] * ux + m[3] * uy + m[5])
        };
        let (sx, sy) = (m[0].hypot(m[1]), m[2].hypot(m[3]));
        let start = page(t.x.unwrap_or(0.0));
        let start_x = t.x.map(|_| start.0);
        let em = (t.size * t.d * sy).abs().max(0.1);
        if let Some((end_x, y)) = self.pen {
            if (start.1 - y).abs() > em * 0.3 {
                newline(&mut self.s);
            } else {
                match (end_x, start_x) {
                    // A gap wider than a thin space, or a jump back.
                    (Some(end), Some(start)) => {
                        let gap = (start - end) / (t.size * t.a * sx).abs().max(0.1);
                        if !(-1.0..=0.15).contains(&gap) {
                            space(&mut self.s);
                        }
                    }
                    _ if !joined => space(&mut self.s),
                    _ => {}
                }
            }
        }
        let decoded = match font {
            Some(f) => f.enc.bytes_to_string(bytes).ok(),
            // No usable font encoding: plain ASCII is still readable.
            None => bytes
                .iter()
                .all(|b| (0x20..0x7f).contains(b))
                .then(|| String::from_utf8_lossy(bytes).into_owned()),
        };
        if let Some(d) = decoded {
            self.s.push_str(&d);
        }
        let advance = font
            .and_then(|f| f.widths.measure(bytes))
            .map(|(w, glyphs, spaces)| {
                (w / 1000.0 * t.size
                    + glyphs as f32 * t.char_spacing
                    + spaces as f32 * t.word_spacing)
                    * t.scale
            });
        self.text.x = t.x.zip(advance).map(|(x, a)| x + a);
        self.pen = Some((self.text.x.map(|x| page(x).0), start.1));
    }

    /// Next line, as `T*` does.
    fn next_line(&mut self) {
        let t = self.text;
        self.line_at(t.e, t.f - t.leading * t.d);
        newline(&mut self.s);
        self.pen = None;
    }

    /// Show one content stream (the page's, or a form's).
    fn run(
        &mut self,
        content: &[u8],
        dicts: &[&'a Dictionary],
        depth: usize,
    ) -> Result<(), PageError> {
        let res = Resources::new(self.doc, dicts);
        let content = Content::decode(content)?;
        let mut font: Option<&Font> = None;
        for op in &content.operations {
            let args = &op.operands;
            let n = |i: usize| args.get(i).and_then(number);
            match op.operator.as_str() {
                "q" if self.saved.len() < MAX_SAVED => self.saved.push(self.ctm),
                "Q" => {
                    if let Some(m) = self.saved.pop() {
                        self.ctm = m;
                    }
                }
                "cm" => {
                    if let Some(m) = matrix(args) {
                        self.ctm = concat(m, self.ctm);
                    }
                }
                "BT" => {
                    let t = &mut self.text;
                    (t.a, t.d) = (1.0, 1.0);
                    self.line_at(0.0, 0.0);
                }
                "Tf" => {
                    font = args
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|n| res.fonts.get(n));
                    if let Some(size) = n(1) {
                        self.text.size = size;
                    }
                }
                "Tc" => self.text.char_spacing = n(0).unwrap_or(0.0),
                "Tw" => self.text.word_spacing = n(0).unwrap_or(0.0),
                "Tz" => self.text.scale = n(0).unwrap_or(100.0) / 100.0,
                "TL" => self.text.leading = n(0).unwrap_or(0.0),
                "Td" | "TD" => {
                    let (tx, ty) = (n(0).unwrap_or(0.0), n(1).unwrap_or(0.0));
                    if op.operator == "TD" {
                        self.text.leading = -ty;
                    }
                    let t = self.text;
                    self.line_at(t.e + tx * t.a, t.f + ty * t.d);
                }
                "Tm" => {
                    self.text.a = n(0).unwrap_or(1.0);
                    self.text.d = n(3).unwrap_or(1.0);
                    self.line_at(n(4).unwrap_or(0.0), n(5).unwrap_or(0.0));
                }
                "T*" => self.next_line(),
                "Tj" => self.show(font, args.first(), false),
                "'" => {
                    self.next_line();
                    self.show(font, args.first(), false);
                }
                "\"" => {
                    self.text.word_spacing = n(0).unwrap_or(0.0);
                    self.text.char_spacing = n(1).unwrap_or(0.0);
                    self.next_line();
                    self.show(font, args.get(2), false);
                }
                "TJ" => {
                    let mut joined = false;
                    for item in args
                        .first()
                        .and_then(|a| a.as_array().ok())
                        .into_iter()
                        .flatten()
                    {
                        match item {
                            Object::String(..) => {
                                self.show(font, Some(item), joined);
                                joined = true;
                            }
                            o => {
                                // Moves left by n thousandths of the font size.
                                if let Some(adj) = number(o) {
                                    let t = self.text;
                                    self.text.x = t.x.map(|x| x - adj / 1000.0 * t.size * t.scale);
                                    // Without widths, only a big jump is a gap.
                                    if t.x.is_none() && adj < -200.0 {
                                        space(&mut self.s);
                                    }
                                }
                            }
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
                    let (text, ctm, depth_saved) = (self.text, self.ctm, self.saved.len());
                    if let Some(m) = form
                        .dict
                        .get(b"Matrix")
                        .ok()
                        .and_then(|m| m.as_array().ok())
                        .and_then(|m| matrix(m))
                    {
                        self.ctm = concat(m, self.ctm);
                    }
                    self.pen = None;
                    self.run(&body, &inner, depth + 1)?;
                    (self.text, self.ctm) = (text, ctm);
                    self.saved.truncate(depth_saved);
                    self.pen = None;
                    newline(&mut self.s);
                }
                _ => {}
            }
        }
        Ok(())
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
