//! Reference documents for routines: text extracted once at upload, then
//! pasted into every run's prompt as labelled sections, within a size cap.
//!
//! [`extract`] is the one place that dispatches on file type. Spreadsheets
//! become Markdown tables (one per sheet), Word and OpenDocument text keeps
//! its headings and lists, PDFs give their text layer (no OCR), and the
//! text formats are read as they are. Uploaded files are untrusted:
//! [`extract_bounded`] reads one file at a time on the blocking pool, with a
//! deadline, and every reader caps what it unpacks and keeps.

use std::path::Path;
use std::time::{Duration, Instant};

pub mod child;
mod office;
mod pdf;
mod sheets;
mod table;
#[cfg(test)]
mod tests;
mod text;

/// Most document text pasted into one run, over all documents.
pub const MAX_DOC_CHARS: usize = 40_000;
/// Most text kept from one document (the rest is cut with a note).
pub const MAX_STORED_CHARS: usize = 200_000;
/// Most rows kept per sheet or CSV file, after the header row.
pub const MAX_TABLE_ROWS: usize = 1_000;
/// Most columns kept per table.
pub const MAX_TABLE_COLS: usize = 50;
/// Longest cell kept, in characters.
pub const MAX_CELL_CHARS: usize = 300;
/// Most sheets read from one workbook.
pub const MAX_SHEETS: usize = 50;
/// Most PDF pages read.
pub const MAX_PDF_PAGES: usize = 500;
/// Most bytes one PDF stream (or one page's content) may inflate to.
pub const MAX_PDF_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Most bytes a zipped document (.xlsx, .docx, .ods, .odt) may unpack to,
/// over all its parts. Guards against zip bombs.
pub const MAX_UNZIPPED_BYTES: u64 = 256 * 1024 * 1024;
/// Most parts in a zipped document.
pub const MAX_ZIP_ENTRIES: usize = 20_000;
/// How long reading one file may take.
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// A file type routines can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocType {
    Markdown,
    Text,
    Csv,
    Tsv,
    Xlsx,
    Xlsm,
    Xls,
    Ods,
    Pdf,
    Docx,
    Odt,
    Json,
    Html,
    Rtf,
}

impl DocType {
    pub fn mime(self) -> &'static str {
        match self {
            DocType::Markdown => "text/markdown",
            DocType::Text => "text/plain",
            DocType::Csv => "text/csv",
            DocType::Tsv => "text/tab-separated-values",
            DocType::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            DocType::Xlsm => "application/vnd.ms-excel.sheet.macroEnabled.12",
            DocType::Xls => "application/vnd.ms-excel",
            DocType::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            DocType::Pdf => "application/pdf",
            DocType::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            DocType::Odt => "application/vnd.oasis.opendocument.text",
            DocType::Json => "application/json",
            DocType::Html => "text/html",
            DocType::Rtf => "application/rtf",
        }
    }
    /// Extension of the stored copy in the Library.
    pub fn ext(self) -> &'static str {
        match self {
            DocType::Markdown => "md",
            DocType::Text => "txt",
            DocType::Csv => "csv",
            DocType::Tsv => "tsv",
            DocType::Xlsx => "xlsx",
            DocType::Xlsm => "xlsm",
            DocType::Xls => "xls",
            DocType::Ods => "ods",
            DocType::Pdf => "pdf",
            DocType::Docx => "docx",
            DocType::Odt => "odt",
            DocType::Json => "json",
            DocType::Html => "html",
            DocType::Rtf => "rtf",
        }
    }
    /// What the owner would call it, for messages.
    pub fn noun(self) -> &'static str {
        match self {
            DocType::Csv | DocType::Tsv | DocType::Xlsx | DocType::Xlsm | DocType::Xls => {
                "spreadsheet"
            }
            DocType::Ods => "spreadsheet",
            DocType::Pdf => "PDF",
            DocType::Docx | DocType::Odt | DocType::Rtf => "document",
            DocType::Markdown | DocType::Text | DocType::Json | DocType::Html => "file",
        }
    }
}

/// What the app says it accepts.
pub const ACCEPTED: &str = "PDFs, Word documents (.docx, .odt), spreadsheets (.xlsx, .xlsm, \
                            .xls, .ods, .csv, .tsv), and text files (.md, .txt, .json, .html, \
                            .rtf)";

/// The type of an uploaded file, from its name.
pub fn doc_type(file_name: &str) -> Option<DocType> {
    Some(match extension(file_name).as_deref()? {
        "md" | "markdown" => DocType::Markdown,
        "txt" | "text" => DocType::Text,
        "csv" => DocType::Csv,
        "tsv" | "tab" => DocType::Tsv,
        "xlsx" => DocType::Xlsx,
        "xlsm" => DocType::Xlsm,
        "xls" => DocType::Xls,
        "ods" => DocType::Ods,
        "pdf" => DocType::Pdf,
        "docx" => DocType::Docx,
        "odt" => DocType::Odt,
        "json" => DocType::Json,
        "html" | "htm" => DocType::Html,
        "rtf" => DocType::Rtf,
        _ => return None,
    })
}

fn extension(file_name: &str) -> Option<String> {
    Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Why a document could not be read. Every variant has a friendly,
/// first-person message ready to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocError {
    /// Not a type routines read (the message says what to do instead).
    Unsupported(String),
    /// Password-protected or encrypted.
    Encrypted(&'static str),
    /// Damaged, or not what its name says.
    Unreadable(&'static str),
    /// Nothing to read.
    Empty,
    /// A PDF without a text layer (a scan).
    NoText,
    /// Over a size or time limit while reading.
    TooComplex,
    /// Another file is still being read.
    Busy,
    /// The reader process couldn't start.
    Unavailable,
}

impl DocError {
    /// The API error code.
    pub fn code(&self) -> &'static str {
        match self {
            DocError::Unsupported(_) => "unsupported_media_type",
            DocError::Encrypted(_) => "encrypted",
            DocError::Unreadable(_) => "unreadable",
            DocError::Empty => "empty",
            DocError::NoText => "no_text",
            DocError::TooComplex => "too_complex",
            DocError::Busy => "busy",
            DocError::Unavailable => "unavailable",
        }
    }

    pub fn message(&self) -> String {
        match self {
            DocError::Unsupported(m) => m.clone(),
            DocError::Encrypted(noun) => format!(
                "That {noun} is password-protected, so I can't open it. Save a copy without \
                 the password and upload that."
            ),
            DocError::Unreadable(noun) => format!(
                "I couldn't read that {noun}. It may be damaged, or saved in a different format \
                 than its name says. Open it, save it again, and upload the new copy."
            ),
            DocError::Empty => "That file looks empty, so there's nothing for me to read.".into(),
            DocError::NoText => "That PDF has no text I can read. It looks like a scan or \
                                 pictures of pages, and I can't read those yet. If you have the \
                                 original document, upload that instead."
                .into(),
            DocError::TooComplex => "That file is too big or complicated for me to read in one \
                                     go. Try splitting it up, or save just the part I need as \
                                     its own file."
                .into(),
            DocError::Busy => "I'm still reading another file. Try again in a minute.".into(),
            DocError::Unavailable => {
                "I couldn't start reading that file just now. Try again in a minute.".into()
            }
        }
    }
}

/// A document read at upload: its text and a short summary for the
/// documents list ("3 sheets · 120 rows", "12 pages", "1,240 words").
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub kind: DocType,
    pub text: String,
    pub summary: String,
}

/// The time left to read one file. Readers check it in their loops so a
/// file that takes too long stops on its own.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    deadline: Instant,
}

impl Budget {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
        }
    }
    pub(crate) fn check(&self) -> Result<(), DocError> {
        if Instant::now() > self.deadline {
            Err(DocError::TooComplex)
        } else {
            Ok(())
        }
    }
}

/// Extracted text under construction, capped at `MAX_STORED_CHARS`.
pub(crate) struct Out {
    text: String,
    chars: usize,
    max: usize,
    /// Characters left out once the cap was reached.
    dropped: bool,
}

impl Out {
    pub(crate) fn new() -> Self {
        Self::with_cap(MAX_STORED_CHARS)
    }
    pub(crate) fn with_cap(max: usize) -> Self {
        Self {
            text: String::new(),
            chars: 0,
            max,
            dropped: false,
        }
    }
    pub(crate) fn push(&mut self, s: &str) {
        if self.dropped {
            return;
        }
        let n = s.chars().count();
        if self.chars + n <= self.max {
            self.text.push_str(s);
            self.chars += n;
        } else {
            let keep = self.max - self.chars;
            self.text.extend(s.chars().take(keep));
            self.chars += keep;
            self.dropped = true;
        }
    }
    /// The cap was reached: further text is dropped.
    pub(crate) fn full(&self) -> bool {
        self.dropped
    }
    pub(crate) fn finish(self) -> String {
        let mut text = self.text;
        if self.dropped {
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
            text.push_str(&format!(
                "\n\n[Cut here: this document is longer than the {} characters I keep, so the \
                 rest is missing.]",
                thousands(self.max)
            ));
        }
        text
    }
}

/// Extract the text of an uploaded document. The single entry point for
/// every file type. Runs on the caller's thread; see [`extract_bounded`].
#[cfg(test)]
pub fn extract(file_name: &str, bytes: &[u8]) -> Result<Extracted, DocError> {
    extract_with(file_name, bytes, Budget::new(EXTRACT_TIMEOUT))
}

pub(crate) fn extract_with(
    file_name: &str,
    bytes: &[u8],
    budget: Budget,
) -> Result<Extracted, DocError> {
    let kind = doc_type(file_name).ok_or_else(|| unsupported(file_name))?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(DocError::Empty);
    }
    let (text, summary) = match kind {
        DocType::Markdown | DocType::Text => {
            let t = text::plain(bytes, kind)?;
            let s = words(&t);
            (t, s)
        }
        DocType::Csv | DocType::Tsv => table::delimited(bytes, kind, &budget)?,
        DocType::Xlsx | DocType::Xlsm | DocType::Xls | DocType::Ods => {
            sheets::workbook(bytes, kind, &budget)?
        }
        DocType::Pdf => pdf::text(bytes, &budget)?,
        DocType::Docx => {
            let t = office::docx(bytes, &budget)?;
            let s = words(&t);
            (t, s)
        }
        DocType::Odt => {
            let t = office::odt(bytes, &budget)?;
            let s = words(&t);
            (t, s)
        }
        DocType::Json => text::json(bytes)?,
        DocType::Html => {
            let t = text::html(bytes, kind, &budget)?;
            let s = words(&t);
            (t, s)
        }
        DocType::Rtf => {
            let t = text::rtf(bytes, &budget)?;
            let s = words(&t);
            (t, s)
        }
    };
    if text.trim().is_empty() {
        return Err(if kind == DocType::Pdf {
            DocError::NoText
        } else {
            DocError::Empty
        });
    }
    Ok(Extracted {
        kind,
        text,
        summary,
    })
}

/// One file is read at a time.
static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Read an upload within [`EXTRACT_TIMEOUT`], one file at a time. With
/// `isolate` (the daemon), the file is read in a child process with memory
/// and CPU limits that is killed at the deadline, so no file can take the
/// daemon down; if that can't start, the file isn't read. Without it
/// (tests), it is read on the blocking pool, where a file past its
/// deadline holds the slot until its reader notices and stops.
pub async fn extract_bounded(
    file_name: String,
    bytes: Vec<u8>,
    isolate: bool,
) -> Result<Extracted, DocError> {
    let kind = doc_type(&file_name).ok_or_else(|| unsupported(&file_name))?;
    let permit = take_slot(&SLOT, EXTRACT_TIMEOUT).await?;
    if isolate {
        let result = child::run(kind, bytes).await;
        drop(permit);
        return result;
    }
    blocking(permit, EXTRACT_TIMEOUT, kind.noun(), move |budget| {
        extract_with(&file_name, &bytes, budget)
    })
    .await
}

/// Wait up to `wait` for the reading slot.
async fn take_slot(
    slot: &'static tokio::sync::Semaphore,
    wait: Duration,
) -> Result<tokio::sync::SemaphorePermit<'static>, DocError> {
    match tokio::time::timeout(wait, slot.acquire()).await {
        Ok(Ok(p)) => Ok(p),
        _ => Err(DocError::Busy),
    }
}

/// Run `read` on the blocking pool with a deadline, holding `permit` until
/// it returns (even after the caller has stopped waiting).
async fn blocking<T: Send + 'static>(
    permit: tokio::sync::SemaphorePermit<'static>,
    timeout: Duration,
    noun: &'static str,
    read: impl FnOnce(Budget) -> Result<T, DocError> + Send + 'static,
) -> Result<T, DocError> {
    let budget = Budget::new(timeout);
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        read(budget)
    });
    // A little grace past the deadline for a reader that is about to stop.
    match tokio::time::timeout(timeout + timeout / 10, task).await {
        Ok(Ok(result)) => result,
        // A reader panicked on a malformed file.
        Ok(Err(_)) => Err(DocError::Unreadable(noun)),
        Err(_) => Err(DocError::TooComplex),
    }
}

fn unsupported(file_name: &str) -> DocError {
    let ext = extension(file_name);
    let tip = match ext.as_deref() {
        Some("doc") => "That's an older Word file (.doc). Open it in Word, save it as .docx, and \
                        upload that."
            .to_string(),
        Some("xlsb") => "That's a binary Excel workbook (.xlsb). Open it in Excel, save it as \
                         .xlsx, and upload that."
            .to_string(),
        Some("pages" | "numbers" | "key") => {
            "I can't read Apple iWork files. Export it as PDF, Word or Excel, and upload that."
                .to_string()
        }
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "tif" | "tiff" | "bmp") => {
            "I can't read pictures or scans in routine documents yet. If there's a text \
             version, upload that instead."
                .to_string()
        }
        Some(e) => format!(
            "I can't read .{e} files for routines. I can read {ACCEPTED}. Save it as one of \
             those and upload that."
        ),
        None => format!(
            "I couldn't tell what kind of file that is, because its name has no extension. I \
             can read {ACCEPTED}."
        ),
    };
    DocError::Unsupported(tip)
}

/// "1,240 words" (Markdown marks like `#` and `-` don't count).
pub(crate) fn words(text: &str) -> String {
    let n = text
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count();
    plural(n, "word", "words")
}

/// "1 row", "1,200 rows".
pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

pub(crate) fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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
                "[Truncated: only the first {} of {total} characters are included, so this \
                 document is partial. Tell the owner if you needed the rest.]\n",
                body.chars().count()
            ));
        }
        out.push_str("</document>\n\n");
    }
    out
}
