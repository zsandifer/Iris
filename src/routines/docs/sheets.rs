//! Spreadsheets (.xlsx, .xlsm, .xls, .ods): each worksheet becomes a
//! Markdown table under its name. Formulas show their cached values.

use super::office::{CFB_MAGIC, check_zip, odf_encrypted};
use super::table::Table;
use super::{Budget, DocError, DocType, MAX_SHEETS, Out, plural, thousands};
use calamine::{Data, DataRef, ExcelDateTime, Ods, Reader, SheetType, Xls, Xlsx};
use std::io::Cursor;

pub(crate) fn workbook(
    bytes: &[u8],
    kind: DocType,
    budget: &Budget,
) -> Result<(String, String), DocError> {
    let noun = kind.noun();
    let cursor = Cursor::new(bytes);
    let mut tables: Vec<(String, Table)> = Vec::new();
    let mut total_sheets = 0;
    match kind {
        DocType::Xlsx | DocType::Xlsm => {
            // An encrypted Office file is an OLE container, not a zip.
            if bytes.starts_with(CFB_MAGIC) {
                return Err(DocError::Encrypted(noun));
            }
            check_zip(bytes, noun)?;
            let mut wb = Xlsx::new(cursor).map_err(|e| match e {
                calamine::XlsxError::Password => DocError::Encrypted(noun),
                _ => DocError::Unreadable(noun),
            })?;
            let names = worksheet_names(&wb);
            total_sheets = names.len();
            for name in names.into_iter().take(MAX_SHEETS) {
                budget.check()?;
                // Cell by cell, so a sheet with one far-off cell costs
                // nothing (a whole range would be allocated at once).
                let mut cells = wb
                    .worksheet_cells_reader(&name)
                    .map_err(|_| DocError::Unreadable(noun))?;
                let mut table = Table::new();
                let mut row: Vec<(u32, String)> = Vec::new();
                let mut at = None;
                let mut n = 0u32;
                while let Some(cell) = cells.next_cell().map_err(|_| DocError::Unreadable(noun))? {
                    n = n.wrapping_add(1);
                    if n.is_multiple_of(4096) {
                        budget.check()?;
                    }
                    let (r, c) = cell.get_position();
                    if at != Some(r) {
                        table.add_row(std::mem::take(&mut row));
                        at = Some(r);
                    }
                    let v = data_ref_text(cell.get_value());
                    if !v.is_empty() {
                        row.push((c, v));
                    }
                }
                table.add_row(row);
                tables.push((name, table));
            }
        }
        DocType::Xls => {
            let mut wb = Xls::new(cursor).map_err(|e| match e {
                calamine::XlsError::Password => DocError::Encrypted(noun),
                _ => DocError::Unreadable(noun),
            })?;
            ranges(&mut wb, budget, noun, &mut tables, &mut total_sheets)?;
        }
        DocType::Ods => {
            check_zip(bytes, noun)?;
            if odf_encrypted(bytes) {
                return Err(DocError::Encrypted(noun));
            }
            let mut wb = Ods::new(cursor).map_err(|e| match e {
                calamine::OdsError::Password => DocError::Encrypted(noun),
                _ => DocError::Unreadable(noun),
            })?;
            ranges(&mut wb, budget, noun, &mut tables, &mut total_sheets)?;
        }
        _ => unreachable!("not a workbook: {kind:?}"),
    }
    if tables.iter().all(|(_, t)| t.is_empty()) {
        return Err(DocError::Empty);
    }
    let mut out = Out::new();
    let mut rows = 0;
    for (name, table) in &tables {
        rows += table.rows();
        if out.full() {
            continue;
        }
        out.push(&format!("## {}\n\n", name.trim()));
        if table.is_empty() {
            out.push("[This sheet is empty.]\n\n");
        } else {
            table.write(&mut out);
            out.push("\n");
        }
    }
    if total_sheets > tables.len() {
        out.push(&format!(
            "[Showing the first {} of {} sheets.]\n",
            tables.len(),
            thousands(total_sheets)
        ));
    }
    let summary = format!(
        "{} · {}",
        plural(total_sheets, "sheet", "sheets"),
        plural(rows, "row", "rows")
    );
    Ok((out.finish(), summary))
}

/// Worksheets only (not chart or macro sheets), in workbook order.
fn worksheet_names<RS: std::io::Read + std::io::Seek, R: Reader<RS>>(wb: &R) -> Vec<String> {
    wb.sheets_metadata()
        .iter()
        .filter(|s| s.typ == SheetType::WorkSheet)
        .map(|s| s.name.clone())
        .collect()
}

/// Read each worksheet as a whole range (.xls and .ods).
fn ranges<RS: std::io::Read + std::io::Seek, R: Reader<RS>>(
    wb: &mut R,
    budget: &Budget,
    noun: &'static str,
    tables: &mut Vec<(String, Table)>,
    total: &mut usize,
) -> Result<(), DocError> {
    let names = worksheet_names(wb);
    *total = names.len();
    for name in names.into_iter().take(MAX_SHEETS) {
        budget.check()?;
        let range = wb
            .worksheet_range(&name)
            .map_err(|_| DocError::Unreadable(noun))?;
        let mut table = Table::new();
        let mut row: Vec<(u32, String)> = Vec::new();
        let mut at = None;
        for (n, (r, c, v)) in range.used_cells().enumerate() {
            if n % 4096 == 4095 {
                budget.check()?;
            }
            if at != Some(r) {
                table.add_row(std::mem::take(&mut row));
                at = Some(r);
            }
            let v = data_text(v);
            if !v.is_empty() {
                row.push((c as u32, v));
            }
        }
        table.add_row(row);
        tables.push((name, table));
    }
    Ok(())
}

fn data_text(v: &Data) -> String {
    match v {
        Data::Empty => String::new(),
        Data::Float(f) => number(*f),
        Data::DateTime(d) => date(d),
        other => other.to_string(),
    }
}

fn data_ref_text(v: &DataRef<'_>) -> String {
    match v {
        DataRef::Empty => String::new(),
        DataRef::Int(i) => i.to_string(),
        DataRef::Float(f) => number(*f),
        DataRef::String(s) => s.clone(),
        DataRef::SharedString(s) => s.to_string(),
        DataRef::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        DataRef::DateTime(d) => date(d),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.clone(),
        DataRef::Error(e) => e.to_string(),
    }
}

/// A number the way a spreadsheet shows it by default: up to 15
/// significant digits, no float noise (0.1 + 0.2 is 0.3).
pub(crate) fn number(f: f64) -> String {
    if !f.is_finite() {
        return f.to_string();
    }
    if f.fract() == 0.0 && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    let rounded: f64 = format!("{f:.14e}").parse().unwrap_or(f);
    rounded.to_string()
}

/// A date, time or both, as `YYYY-MM-DD HH:MM`.
fn date(d: &ExcelDateTime) -> String {
    if d.is_duration() {
        let secs = (d.as_f64() * 86_400.0).round() as i64;
        let sign = if secs < 0 { "-" } else { "" };
        let secs = secs.abs();
        return format!(
            "{sign}{}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
    }
    let (y, mo, day, h, mi, s, _) = d.to_ymd_hms_milli();
    let has_date = d.as_f64() >= 1.0;
    let has_time = d.as_f64().fract() != 0.0;
    let date = format!("{y:04}-{mo:02}-{day:02}");
    let time = if s != 0 {
        format!("{h:02}:{mi:02}:{s:02}")
    } else {
        format!("{h:02}:{mi:02}")
    };
    match (has_date, has_time) {
        (true, true) => format!("{date} {time}"),
        (true, false) => date,
        (false, _) => time,
    }
}
