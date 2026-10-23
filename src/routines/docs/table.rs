//! Tables as Markdown: the builder sheets and CSV files share (empty rows
//! and columns dropped, rows and columns capped with a note), and the CSV
//! and TSV reader.

use super::{
    Budget, DocError, DocType, MAX_CELL_CHARS, MAX_TABLE_COLS, MAX_TABLE_ROWS, Out, plural, text,
    thousands,
};
use std::collections::BTreeSet;

/// Most non-empty cells kept from one row (enough to find the first
/// `MAX_TABLE_COLS` used columns of all but freakishly sparse sheets).
const MAX_ROW_CELLS: usize = MAX_TABLE_COLS * 4;

/// Rows of one table, added in order. Only non-empty cells are given, as
/// (column, text); the first non-empty row is the header.
pub(crate) struct Table {
    /// Kept rows: the header and up to `MAX_TABLE_ROWS` more.
    rows: Vec<Vec<(u32, String)>>,
    /// Every non-empty row seen, kept or not.
    total: usize,
}

impl Table {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            total: 0,
        }
    }

    /// Add a row; blank cells and blank rows are dropped.
    pub(crate) fn add_row(&mut self, cells: Vec<(u32, String)>) {
        let mut cells: Vec<(u32, String)> = cells
            .into_iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .collect();
        if cells.is_empty() {
            return;
        }
        self.total += 1;
        if self.rows.len() <= MAX_TABLE_ROWS {
            // Bound what one very wide row can keep.
            cells.sort_by_key(|(c, _)| *c);
            cells.truncate(MAX_ROW_CELLS);
            self.rows.push(
                cells
                    .into_iter()
                    .map(|(c, v)| (c, cell(v.trim())))
                    .collect(),
            );
        }
    }

    /// Count non-empty rows a reader left out before they got here.
    pub(crate) fn add_dropped(&mut self, n: usize) {
        if self.total > 0 {
            self.total += n;
        }
    }

    /// Every non-empty row, the header included.
    pub(crate) fn rows(&self) -> usize {
        self.total
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Columns with something in them, among the kept rows.
    fn columns(&self) -> Vec<u32> {
        self.rows
            .iter()
            .flat_map(|r| r.iter().map(|(c, _)| *c))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn column_count(&self) -> usize {
        self.columns().len()
    }

    /// The table as Markdown, with notes for rows and columns left out.
    pub(crate) fn write(&self, out: &mut Out) {
        let all = self.columns();
        let cols = &all[..all.len().min(MAX_TABLE_COLS)];
        let line = |row: &[(u32, String)]| {
            let mut s = String::from("|");
            for c in cols {
                let v = row
                    .iter()
                    .find(|(k, _)| k == c)
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                s.push(' ');
                s.push_str(v);
                s.push_str(" |");
            }
            s.push('\n');
            s
        };
        if all.len() > cols.len() {
            out.push(&format!(
                "[Showing the first {} of {} columns.]\n\n",
                cols.len(),
                all.len()
            ));
        }
        let Some((header, body)) = self.rows.split_first() else {
            return;
        };
        out.push(&line(header));
        out.push(&format!("|{}\n", " --- |".repeat(cols.len())));
        for row in body {
            if out.full() {
                return;
            }
            out.push(&line(row));
        }
        let data_rows = self.total - 1;
        if data_rows > body.len() {
            out.push(&format!(
                "\n[Showing the first {} of {} rows.]\n",
                thousands(body.len()),
                thousands(data_rows)
            ));
        }
    }
}

/// One cell's text, safe inside a Markdown table row.
fn cell(v: &str) -> String {
    let mut s: String = v.chars().take(MAX_CELL_CHARS).collect();
    if v.chars().count() > MAX_CELL_CHARS {
        s.push('…');
    }
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('|', "\\|")
        .replace('\n', "<br>")
}

/// A CSV or TSV file as one Markdown table.
pub(crate) fn delimited(
    bytes: &[u8],
    kind: DocType,
    budget: &Budget,
) -> Result<(String, String), DocError> {
    let src = text::decode(bytes, kind)?;
    let delim = if kind == DocType::Tsv {
        '\t'
    } else {
        sniff_delimiter(&src)
    };
    let mut table = Table::new();
    for (i, row) in Records::new(&src, delim).enumerate() {
        if i % 1024 == 0 {
            budget.check()?;
        }
        table.add_row(
            row.into_iter()
                .enumerate()
                .map(|(c, v)| (c as u32, v))
                .collect(),
        );
    }
    if table.is_empty() {
        return Err(DocError::Empty);
    }
    let mut out = Out::new();
    table.write(&mut out);
    let summary = format!(
        "{} · {}",
        plural(table.rows(), "row", "rows"),
        plural(table.column_count(), "column", "columns")
    );
    Ok((out.finish(), summary))
}

/// Comma, or the semicolon or tab some regions' spreadsheets export, by
/// which appears most in the first line (outside quotes).
fn sniff_delimiter(src: &str) -> char {
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for c in src.chars() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => break,
            _ if !quoted => {
                for (d, n) in counts.iter_mut() {
                    if *d == c {
                        *n += 1;
                    }
                }
            }
            _ => {}
        }
    }
    counts
        .iter()
        .fold(
            (',', 0),
            |best, &(d, n)| if n > best.1 { (d, n) } else { best },
        )
        .0
}

/// RFC 4180 records: quoted fields may hold the delimiter, newlines and
/// doubled quotes. A quote inside an unquoted field is kept as written.
pub(crate) struct Records<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    delim: char,
}

impl<'a> Records<'a> {
    pub(crate) fn new(src: &'a str, delim: char) -> Self {
        Self {
            chars: src.chars().peekable(),
            delim,
        }
    }
}

impl Iterator for Records<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        self.chars.peek()?;
        let mut row = Vec::new();
        let mut field = String::new();
        // At the start of a field (where an opening quote counts).
        let mut start = true;
        let mut quoted = false;
        while let Some(c) = self.chars.next() {
            if quoted {
                if c == '"' {
                    if self.chars.peek() == Some(&'"') {
                        self.chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(c);
                }
                continue;
            }
            match c {
                '"' if start => {
                    quoted = true;
                    start = false;
                }
                c if c == self.delim => {
                    row.push(std::mem::take(&mut field));
                    start = true;
                }
                '\r' if self.chars.peek() == Some(&'\n') => {}
                '\n' | '\r' => {
                    row.push(field);
                    return Some(row);
                }
                c => {
                    field.push(c);
                    start = false;
                }
            }
        }
        row.push(field);
        Some(row)
    }
}
