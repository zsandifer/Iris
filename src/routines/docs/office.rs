//! Word (.docx) and OpenDocument text (.odt): zipped XML, read as a stream.
//! Headings become `#` lines, list items `- ` (or `1. `) lines, tables
//! Markdown tables. Also the zip checks every zipped format shares.

use super::table::Table;
use super::{Budget, DocError, DocType, MAX_UNZIPPED_BYTES, MAX_ZIP_ENTRIES, Out};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read};

/// The start of an OLE compound file: what password-protected .docx and
/// .xlsx files are wrapped in (and legacy .doc/.xls files).
pub(crate) const CFB_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// Open a zipped document, refusing archives that would unpack to more
/// than `MAX_UNZIPPED_BYTES` or hold more than `MAX_ZIP_ENTRIES` parts.
/// Each part is also cut off at its declared size while it is read.
pub(crate) fn check_zip<'a>(bytes: &'a [u8], noun: &'static str) -> Result<Zip<'a>, DocError> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| DocError::Unreadable(noun))?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return Err(DocError::TooComplex);
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let f = zip
            .by_index_raw(i)
            .map_err(|_| DocError::Unreadable(noun))?;
        if f.encrypted() {
            return Err(DocError::Encrypted(noun));
        }
        total = total.saturating_add(f.size());
        if total > MAX_UNZIPPED_BYTES {
            return Err(DocError::TooComplex);
        }
    }
    Ok(zip)
}

/// An OpenDocument file encrypted with a password: its manifest lists
/// encryption data for the parts.
pub(crate) fn odf_encrypted(bytes: &[u8]) -> bool {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    read_part(&mut zip, "META-INF/manifest.xml", 4 * 1024 * 1024)
        .ok()
        .flatten()
        .is_some_and(|m| m.windows(15).any(|w| w == b"encryption-data"))
}

/// A whole (small) part, or None when the archive doesn't have it.
fn read_part(zip: &mut Zip<'_>, name: &str, max: u64) -> Result<Option<Vec<u8>>, DocError> {
    let file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(_) => return Err(DocError::Unreadable("document")),
    };
    let mut buf = Vec::new();
    file.take(max)
        .read_to_end(&mut buf)
        .map_err(|_| DocError::Unreadable("document"))?;
    Ok(Some(buf))
}

fn xml_error(_: quick_xml::Error) -> DocError {
    DocError::Unreadable("document")
}

/// A named attribute's value (by local name, so any namespace prefix).
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// The text of an entity or character reference.
fn reference(r: &BytesRef<'_>) -> Option<char> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c);
    }
    match &**r {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// How a paragraph is written.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Block {
    Para,
    Heading(usize),
    Item {
        depth: usize,
        ordered: bool,
    },
    /// A table (it already ends its last line).
    Table,
}

/// A table being read: finished rows, and the row and cell in progress.
#[derive(Default)]
struct TableBuild {
    rows: Vec<Vec<String>>,
    /// Rows left out past the cap.
    dropped: usize,
    row: Vec<String>,
    cell: Option<String>,
    /// Columns the open cell covers (merged cells), and how many times its
    /// content repeats (OpenDocument's repeated cells).
    span: usize,
    repeat: usize,
    /// How many times the open row repeats (OpenDocument's
    /// number-rows-repeated).
    row_repeat: usize,
}

/// Most columns one merged or repeated cell is taken to cover.
const MAX_SPAN: usize = 64;
/// Most cells kept per table row.
const MAX_ROW_CELLS: usize = 1_000;

/// Markdown assembled from paragraphs and tables.
struct Writer {
    out: Out,
    tables: Vec<TableBuild>,
    last: Option<Block>,
}

impl Writer {
    fn new() -> Self {
        Self {
            out: Out::new(),
            tables: Vec::new(),
            last: None,
        }
    }

    fn paragraph(&mut self, block: Block, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(cell) = self.tables.last_mut().and_then(|t| t.cell.as_mut()) {
            if !cell.is_empty() {
                cell.push('\n');
            }
            if let Block::Item { .. } = block {
                cell.push_str("- ");
            }
            cell.push_str(text);
            return;
        }
        let sep = match (self.last, block) {
            (None, _) => "",
            (Some(Block::Item { .. }), Block::Item { .. }) | (Some(Block::Table), _) => "\n",
            _ => "\n\n",
        };
        self.out.push(sep);
        match block {
            Block::Para | Block::Table => self.out.push(text),
            Block::Heading(level) => {
                self.out.push(&"#".repeat(level.clamp(1, 6)));
                self.out.push(" ");
                self.out.push(&text.replace('\n', " "));
            }
            Block::Item { depth, ordered } => {
                self.out.push(&"  ".repeat(depth.min(8)));
                self.out.push(if ordered { "1. " } else { "- " });
                self.out.push(&text.replace('\n', " "));
            }
        }
        self.last = Some(block);
    }

    fn table_start(&mut self) {
        self.tables.push(TableBuild::default());
    }
    fn row_start(&mut self, repeat: usize) {
        if let Some(t) = self.tables.last_mut() {
            t.row.clear();
            t.row_repeat = repeat.max(1);
        }
    }
    fn cell_start(&mut self) {
        if let Some(t) = self.tables.last_mut() {
            t.cell = Some(String::new());
            (t.span, t.repeat) = (1, 1);
        }
    }
    /// The open cell covers `span` grid columns (Word's gridSpan) or is
    /// repeated `repeat` times (OpenDocument's number-columns-repeated).
    fn cell_size(&mut self, span: usize, repeat: usize) {
        if let Some(t) = self.tables.last_mut() {
            t.span = span.clamp(1, MAX_SPAN);
            t.repeat = repeat.clamp(1, MAX_SPAN);
        }
    }
    fn cell_end(&mut self) {
        if let Some(t) = self.tables.last_mut()
            && let Some(c) = t.cell.take()
        {
            // Keep later cells under their own columns: a merged cell's
            // extra columns are empty.
            for _ in 1..t.repeat {
                if t.row.len() < MAX_ROW_CELLS {
                    t.row.push(c.clone());
                }
            }
            if t.row.len() < MAX_ROW_CELLS {
                t.row.push(c);
            }
            for _ in 1..t.span {
                if t.row.len() < MAX_ROW_CELLS {
                    t.row.push(String::new());
                }
            }
        }
    }
    fn row_end(&mut self) {
        if let Some(t) = self.tables.last_mut() {
            let row = std::mem::take(&mut t.row);
            let repeat = std::mem::replace(&mut t.row_repeat, 1).max(1);
            if row.iter().all(|c| c.trim().is_empty()) {
                // Blank rows are dropped anyway (and are what huge repeat
                // counts are used for).
                if t.rows.len() <= super::MAX_TABLE_ROWS {
                    t.rows.push(row);
                }
                return;
            }
            let room = (super::MAX_TABLE_ROWS + 1).saturating_sub(t.rows.len());
            let keep = repeat.min(room);
            for _ in 0..keep {
                t.rows.push(row.clone());
            }
            t.dropped = t.dropped.saturating_add(repeat - keep);
        }
    }
    fn table_end(&mut self) {
        let Some(t) = self.tables.pop() else { return };
        // A table inside a table cell is flattened into that cell.
        if let Some(cell) = self.tables.last_mut().and_then(|o| o.cell.as_mut()) {
            for row in &t.rows {
                if !cell.is_empty() {
                    cell.push('\n');
                }
                cell.push_str(&row.join(" / "));
            }
            return;
        }
        let mut table = Table::new();
        for row in t.rows {
            table.add_row(
                row.into_iter()
                    .enumerate()
                    .map(|(c, v)| (c as u32, v))
                    .collect(),
            );
        }
        table.add_dropped(t.dropped);
        if table.is_empty() {
            return;
        }
        match self.last {
            None => {}
            Some(Block::Table) => self.out.push("\n"),
            Some(_) => self.out.push("\n\n"),
        }
        table.write(&mut self.out);
        self.last = Some(Block::Table);
    }

    fn finish(mut self) -> String {
        while !self.tables.is_empty() {
            self.cell_end();
            self.row_end();
            self.table_end();
        }
        if self.last != Some(Block::Table) {
            self.out.push("\n");
        }
        self.out.finish()
    }
}

// ------------------------------------------------------------------- docx

/// Heading levels and list numbering of a document's styles.
#[derive(Default)]
struct Styles {
    /// Style id → heading level (1–6).
    headings: HashMap<String, usize>,
    /// Style id → numbering id, for list styles.
    lists: HashMap<String, String>,
    /// (numbering id, level) → numbered (true) or bulleted.
    ordered: HashMap<(String, usize), bool>,
}

fn read_styles(zip: &mut Zip<'_>) -> Result<Styles, DocError> {
    let mut styles = Styles::default();
    if let Some(xml) = read_part(zip, "word/styles.xml", 16 * 1024 * 1024)? {
        let mut r = Reader::from_reader(xml.as_slice());
        let mut buf = Vec::new();
        let mut id: Option<String> = None;
        loop {
            match r.read_event_into(&mut buf).map_err(xml_error)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    "style" => id = attr(&e, "styleId"),
                    "name" => {
                        if let (Some(id), Some(name)) = (&id, attr(&e, "val")) {
                            let name = name.to_ascii_lowercase();
                            let level = if name == "title" {
                                Some(1)
                            } else {
                                name.strip_prefix("heading ")
                                    .and_then(|n| n.trim().parse::<usize>().ok())
                            };
                            if let Some(l) = level.filter(|l| (1..=6).contains(l)) {
                                styles.headings.insert(id.clone(), l);
                            }
                        }
                    }
                    "outlineLvl" => {
                        if let (Some(id), Some(l)) = (&id, attr(&e, "val"))
                            && let Ok(l) = l.parse::<usize>()
                            && l < 6
                        {
                            styles.headings.entry(id.clone()).or_insert(l + 1);
                        }
                    }
                    "numId" => {
                        if let (Some(id), Some(n)) = (&id, attr(&e, "val"))
                            && n != "0"
                        {
                            styles.lists.insert(id.clone(), n);
                        }
                    }
                    _ => {}
                },
                Event::End(e) if e.local_name().as_ref() == "style" => id = None,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
    }
    if let Some(xml) = read_part(zip, "word/numbering.xml", 16 * 1024 * 1024)? {
        let mut r = Reader::from_reader(xml.as_slice());
        let mut buf = Vec::new();
        // abstract id → level → numbered
        let mut formats: HashMap<String, HashMap<usize, bool>> = HashMap::new();
        let mut nums: Vec<(String, String)> = Vec::new();
        let (mut abs, mut lvl, mut num): (Option<String>, Option<usize>, Option<String>) =
            (None, None, None);
        loop {
            match r.read_event_into(&mut buf).map_err(xml_error)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    "abstractNum" => abs = attr(&e, "abstractNumId"),
                    "lvl" => lvl = attr(&e, "ilvl").and_then(|l| l.parse().ok()),
                    "numFmt" => {
                        if let (Some(a), Some(l), Some(f)) = (&abs, lvl, attr(&e, "val")) {
                            formats
                                .entry(a.clone())
                                .or_default()
                                .insert(l, f != "bullet" && f != "none");
                        }
                    }
                    "num" => num = attr(&e, "numId"),
                    "abstractNumId" => {
                        if let (Some(n), Some(a)) = (&num, attr(&e, "val")) {
                            nums.push((n.clone(), a));
                        }
                    }
                    _ => {}
                },
                Event::End(e) => match e.local_name().as_ref() {
                    "abstractNum" => abs = None,
                    "lvl" => lvl = None,
                    "num" => num = None,
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        for (n, a) in nums {
            for (l, ordered) in formats.get(&a).into_iter().flatten() {
                styles.ordered.insert((n.clone(), *l), *ordered);
            }
        }
    }
    Ok(styles)
}

/// A Word document's body as Markdown.
pub(crate) fn docx(bytes: &[u8], budget: &Budget) -> Result<String, DocError> {
    let noun = DocType::Docx.noun();
    if bytes.starts_with(CFB_MAGIC) {
        return Err(DocError::Encrypted(noun));
    }
    let mut zip = check_zip(bytes, noun)?;
    let styles = read_styles(&mut zip)?;
    let body = match zip.by_name("word/document.xml") {
        Ok(f) => f,
        Err(_) => return Err(DocError::Unreadable(noun)),
    };
    docx_body(BufReader::new(body), &styles, budget)
}

/// A paragraph being read.
#[derive(Default)]
struct Para {
    style: Option<String>,
    outline: Option<usize>,
    num: Option<String>,
    level: usize,
    text: String,
}

fn docx_body<R: BufRead>(src: R, styles: &Styles, budget: &Budget) -> Result<String, DocError> {
    let mut r = Reader::from_reader(src);
    let mut buf = Vec::new();
    let mut w = Writer::new();
    let mut paras: Vec<Para> = Vec::new();
    // Inside <w:t> (text), <w:pPr> (paragraph properties) or skipped content.
    let (mut in_text, mut in_ppr, mut skip) = (false, 0usize, 0usize);
    let mut n = 0u32;
    loop {
        n = n.wrapping_add(1);
        if n.is_multiple_of(4096) {
            budget.check()?;
            if w.out.full() {
                break;
            }
        }
        let ev = r.read_event_into(&mut buf).map_err(xml_error)?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = e.local_name();
                let name = name.as_ref();
                // Deleted text, field codes, and the duplicate of a text
                // box kept for old readers.
                if matches!(name, "Fallback" | "del" | "instrText" | "delText") {
                    if !empty {
                        skip += 1;
                    }
                    continue;
                }
                if skip > 0 {
                    continue;
                }
                match name {
                    "p" if !empty => paras.push(Para::default()),
                    "pPr" if !empty => in_ppr += 1,
                    "pStyle" if in_ppr > 0 => {
                        if let Some(p) = paras.last_mut() {
                            p.style = attr(e, "val");
                        }
                    }
                    "outlineLvl" if in_ppr > 0 => {
                        if let Some(p) = paras.last_mut() {
                            p.outline = attr(e, "val").and_then(|v| v.parse().ok());
                        }
                    }
                    "numId" if in_ppr > 0 => {
                        if let Some(p) = paras.last_mut() {
                            p.num = attr(e, "val");
                        }
                    }
                    "ilvl" if in_ppr > 0 => {
                        if let Some(p) = paras.last_mut() {
                            p.level = attr(e, "val").and_then(|v| v.parse().ok()).unwrap_or(0);
                        }
                    }
                    "t" if !empty => in_text = true,
                    "tab" if in_ppr == 0 => push_text(&mut paras, "\t"),
                    "br" | "cr" if in_ppr == 0 => push_text(&mut paras, "\n"),
                    "noBreakHyphen" => push_text(&mut paras, "-"),
                    "tbl" if !empty => w.table_start(),
                    "tr" if !empty => w.row_start(1),
                    "tc" if !empty => w.cell_start(),
                    "gridSpan" => {
                        let span = attr(e, "val").and_then(|v| v.parse().ok()).unwrap_or(1);
                        w.cell_size(span, 1);
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(name, "Fallback" | "del" | "instrText" | "delText") {
                    skip = skip.saturating_sub(1);
                    continue;
                }
                if skip > 0 {
                    continue;
                }
                match name {
                    "t" => in_text = false,
                    "pPr" => in_ppr = in_ppr.saturating_sub(1),
                    "p" => {
                        if let Some(p) = paras.pop() {
                            let block = docx_block(&p, styles);
                            w.paragraph(block, &p.text);
                        }
                    }
                    "tc" => w.cell_end(),
                    "tr" => w.row_end(),
                    "tbl" => w.table_end(),
                    _ => {}
                }
            }
            Event::Text(t) if in_text && skip == 0 => push_text(&mut paras, &t.xml10_content()),
            Event::CData(t) if in_text && skip == 0 => push_text(&mut paras, &t),
            Event::GeneralRef(r) if in_text && skip == 0 => {
                if let Some(c) = reference(&r) {
                    push_text(&mut paras, c.encode_utf8(&mut [0; 4]));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(w.finish())
}

fn push_text(paras: &mut [Para], s: &str) {
    if let Some(p) = paras.last_mut()
        && p.text.len() < 1_000_000
    {
        p.text.push_str(s);
    }
}

fn docx_block(p: &Para, styles: &Styles) -> Block {
    let style = p.style.as_deref().unwrap_or("");
    if let Some(l) = styles.headings.get(style) {
        return Block::Heading(*l);
    }
    if let Some(l) = heading_from_id(style) {
        return Block::Heading(l);
    }
    if let Some(o) = p.outline.filter(|o| *o < 6) {
        return Block::Heading(o + 1);
    }
    let num = p
        .num
        .clone()
        .or_else(|| styles.lists.get(style).cloned())
        .filter(|n| n != "0");
    match num {
        Some(n) => Block::Item {
            depth: p.level,
            ordered: styles.ordered.get(&(n, p.level)).copied().unwrap_or(false),
        },
        None => Block::Para,
    }
}

/// Built-in heading style ids ("Heading1", "Title") when styles.xml is
/// missing or doesn't name them.
fn heading_from_id(id: &str) -> Option<usize> {
    if id.eq_ignore_ascii_case("title") {
        return Some(1);
    }
    let rest = id
        .strip_prefix("Heading")
        .or_else(|| id.strip_prefix("heading"))?;
    rest.parse::<usize>().ok().filter(|l| (1..=6).contains(l))
}

// -------------------------------------------------------------------- odt

/// An OpenDocument text file's body as Markdown.
pub(crate) fn odt(bytes: &[u8], budget: &Budget) -> Result<String, DocError> {
    let noun = DocType::Odt.noun();
    let mut zip = check_zip(bytes, noun)?;
    if odf_encrypted(bytes) {
        return Err(DocError::Encrypted(noun));
    }
    let content = match zip.by_name("content.xml") {
        Ok(f) => f,
        Err(_) => return Err(DocError::Unreadable(noun)),
    };
    let mut r = Reader::from_reader(BufReader::new(content));
    let mut buf = Vec::new();
    let mut w = Writer::new();
    // Open paragraphs (text:p / text:h): their block and text.
    let mut paras: Vec<(Block, String)> = Vec::new();
    let mut lists = 0usize;
    let mut skip = 0usize;
    let mut n = 0u32;
    loop {
        n = n.wrapping_add(1);
        if n.is_multiple_of(4096) {
            budget.check()?;
            if w.out.full() {
                break;
            }
        }
        let ev = r.read_event_into(&mut buf).map_err(xml_error)?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(
                    name,
                    "annotation" | "tracked-changes" | "note" | "automatic-styles"
                ) {
                    if !empty {
                        skip += 1;
                    }
                    continue;
                }
                if skip > 0 {
                    continue;
                }
                match name {
                    "h" if !empty => {
                        let level = attr(e, "outline-level")
                            .and_then(|l| l.parse().ok())
                            .unwrap_or(1);
                        paras.push((Block::Heading(level), String::new()));
                    }
                    "p" if !empty => {
                        let block = if lists > 0 {
                            Block::Item {
                                depth: lists - 1,
                                ordered: false,
                            }
                        } else {
                            Block::Para
                        };
                        paras.push((block, String::new()));
                    }
                    "list" if !empty => lists += 1,
                    "s" => {
                        let count = attr(e, "c")
                            .and_then(|c| c.parse::<usize>().ok())
                            .unwrap_or(1)
                            .min(100);
                        odt_text(&mut paras, &" ".repeat(count));
                    }
                    "tab" => odt_text(&mut paras, "\t"),
                    "line-break" => odt_text(&mut paras, "\n"),
                    "table" if !empty => w.table_start(),
                    "table-row" if !empty => w.row_start(
                        attr(e, "number-rows-repeated")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(1),
                    ),
                    "table-cell" | "covered-table-cell" => {
                        w.cell_start();
                        let n = |a: &str| attr(e, a).and_then(|v| v.parse().ok()).unwrap_or(1);
                        // A covered cell is the hidden part of a merged one.
                        if name == "covered-table-cell" {
                            w.cell_size(n("number-columns-repeated"), 1);
                        } else {
                            w.cell_size(1, n("number-columns-repeated"));
                        }
                        if empty {
                            w.cell_end();
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                let name = name.as_ref();
                if matches!(
                    name,
                    "annotation" | "tracked-changes" | "note" | "automatic-styles"
                ) {
                    skip = skip.saturating_sub(1);
                    continue;
                }
                if skip > 0 {
                    continue;
                }
                match name {
                    "h" | "p" => {
                        if let Some((block, text)) = paras.pop() {
                            w.paragraph(block, &text);
                        }
                    }
                    "list" => lists = lists.saturating_sub(1),
                    "table-cell" | "covered-table-cell" => w.cell_end(),
                    "table-row" => w.row_end(),
                    "table" => w.table_end(),
                    _ => {}
                }
            }
            Event::Text(t) if skip == 0 => odt_text(&mut paras, &t.xml10_content()),
            Event::CData(t) if skip == 0 => odt_text(&mut paras, &t),
            Event::GeneralRef(r) if skip == 0 => {
                if let Some(c) = reference(&r) {
                    odt_text(&mut paras, c.encode_utf8(&mut [0; 4]));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(w.finish())
}

fn odt_text(paras: &mut [(Block, String)], s: &str) {
    if let Some((_, text)) = paras.last_mut()
        && text.len() < 1_000_000
    {
        text.push_str(s);
    }
}
