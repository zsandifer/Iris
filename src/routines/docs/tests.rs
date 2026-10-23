//! Extraction tests. Every fixture is synthetic and built here: zipped
//! Office and OpenDocument files from hand-written XML, PDFs from
//! hand-written objects.

use super::*;
use std::io::Write;

// ---------------------------------------------------------------- fixtures

fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in files {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn col_name(c: usize) -> String {
    let mut c = c + 1;
    let mut s = String::new();
    while c > 0 {
        s.insert(0, (b'A' + ((c - 1) % 26) as u8) as char);
        c = (c - 1) / 26;
    }
    s
}

/// A cell: numbers as numbers, `=formula|cached` as a formula with its
/// cached value, anything else as an inline string.
fn xlsx_cell(r: usize, c: usize, v: &str) -> String {
    let at = format!("{}{}", col_name(c), r + 1);
    if v.is_empty() {
        return String::new();
    }
    if let Some(f) = v.strip_prefix('=') {
        let (formula, cached) = f.split_once('|').unwrap();
        return format!("<c r=\"{at}\"><f>{formula}</f><v>{cached}</v></c>");
    }
    if v.parse::<f64>().is_ok() {
        return format!("<c r=\"{at}\"><v>{v}</v></c>");
    }
    let v = v.replace('&', "&amp;").replace('<', "&lt;");
    format!("<c r=\"{at}\" t=\"inlineStr\"><is><t>{v}</t></is></c>")
}

/// A workbook of named sheets, each a grid of cells ("" = no cell).
fn xlsx(sheets: &[(&str, Vec<Vec<String>>)]) -> Vec<u8> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut wb_sheets = String::new();
    let mut rels = String::new();
    let mut types = String::new();
    for (i, (name, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb_sheets.push_str(&format!(
            "<sheet name=\"{name}\" sheetId=\"{n}\" r:id=\"rId{n}\"/>"
        ));
        rels.push_str(&format!(
            "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
        ));
        types.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        ));
        let mut data = String::new();
        for (r, row) in rows.iter().enumerate() {
            let cells: String = row
                .iter()
                .enumerate()
                .map(|(c, v)| xlsx_cell(r, c, v))
                .collect();
            if !cells.is_empty() {
                data.push_str(&format!("<row r=\"{}\">{cells}</row>", r + 1));
            }
        }
        files.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>{data}</sheetData></worksheet>"
            ),
        ));
    }
    files.push((
        "[Content_Types].xml".into(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>{types}</Types>"
        ),
    ));
    files.push((
        "_rels/.rels".into(),
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>".into(),
    ));
    files.push((
        "xl/workbook.xml".into(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>{wb_sheets}</sheets></workbook>"
        ),
    ));
    files.push((
        "xl/_rels/workbook.xml.rels".into(),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{rels}</Relationships>"
        ),
    ));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    zip_of(&refs)
}

fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect()
}

const W: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";

fn docx(body: &str, styles: &str, numbering: &str) -> Vec<u8> {
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><w:document {W}><w:body>{body}</w:body></w:document>"
    );
    let styles = format!("<?xml version=\"1.0\"?><w:styles {W}>{styles}</w:styles>");
    let numbering = format!("<?xml version=\"1.0\"?><w:numbering {W}>{numbering}</w:numbering>");
    zip_of(&[
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"/>",
        ),
        ("word/document.xml", &document),
        ("word/styles.xml", &styles),
        ("word/numbering.xml", &numbering),
    ])
}

fn para(style: Option<&str>, num: Option<(u32, u32)>, text: &str) -> String {
    let mut ppr = String::new();
    if let Some(s) = style {
        ppr.push_str(&format!("<w:pStyle w:val=\"{s}\"/>"));
    }
    if let Some((id, lvl)) = num {
        ppr.push_str(&format!(
            "<w:numPr><w:ilvl w:val=\"{lvl}\"/><w:numId w:val=\"{id}\"/></w:numPr>"
        ));
    }
    format!(
        "<w:p><w:pPr>{ppr}<w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr><w:r><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>"
    )
}

/// A PDF from object bodies (numbered from 1; object 1 is the catalog),
/// with a correct cross-reference table.
fn pdf(objects: &[String], trailer: &str) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R {trailer} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn stream(content: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

/// A PDF whose pages show the given content streams in Helvetica.
fn text_pdf(pages: &[&str]) -> Vec<u8> {
    let n = pages.len();
    // 1 catalog, 2 pages, 3 font, then (page, content) pairs.
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
    ];
    for (i, content) in pages.iter().enumerate() {
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + 2 * i
        ));
        objects.push(stream(content));
    }
    pdf(&objects, "")
}

// ------------------------------------------------------------------- types

#[test]
fn file_types_come_from_the_name() {
    for (name, kind) in [
        ("SOP.md", DocType::Markdown),
        ("notes.TXT", DocType::Text),
        ("orders.csv", DocType::Csv),
        ("orders.tsv", DocType::Tsv),
        ("Budget.XLSX", DocType::Xlsx),
        ("macros.xlsm", DocType::Xlsm),
        ("old.xls", DocType::Xls),
        ("calc.ods", DocType::Ods),
        ("menu.pdf", DocType::Pdf),
        ("plan.docx", DocType::Docx),
        ("plan.odt", DocType::Odt),
        ("data.json", DocType::Json),
        ("page.htm", DocType::Html),
        ("letter.rtf", DocType::Rtf),
    ] {
        assert_eq!(doc_type(name), Some(kind), "{name}");
        assert!(!kind.mime().is_empty() && !kind.ext().is_empty());
    }
    for bad in ["image.png", "deck.pptx", "noext", "old.doc"] {
        assert_eq!(doc_type(bad), None, "{bad}");
    }
}

#[test]
fn unsupported_files_get_a_tip() {
    let msg = |name: &str| match extract(name, b"data") {
        Err(DocError::Unsupported(m)) => m,
        other => panic!("{name}: {other:?}"),
    };
    assert!(msg("old.doc").contains("save it as .docx"));
    assert!(msg("book.xlsb").contains("save it as .xlsx"));
    assert!(msg("photo.JPG").contains("pictures or scans"));
    assert!(msg("plan.pages").contains("iWork"));
    assert!(msg("deck.pptx").contains("I can't read .pptx files"));
    assert!(msg("deck.pptx").contains(ACCEPTED));
    assert!(msg("README").contains("no extension"));
    let e = extract("deck.pptx", b"x").unwrap_err();
    assert_eq!(e.code(), "unsupported_media_type");
}

#[test]
fn every_error_has_a_code_and_a_friendly_message() {
    for e in [
        unsupported("a.zzz"),
        unsupported("noext"),
        DocError::Encrypted("PDF"),
        DocError::Unreadable("spreadsheet"),
        DocError::Empty,
        DocError::NoText,
        DocError::TooComplex,
        DocError::Busy,
    ] {
        assert!(!e.code().is_empty());
        let m = e.message();
        assert!(m.ends_with('.'), "{m}");
        assert!(!m.to_lowercase().contains("log"), "{m}");
    }
    assert!(
        DocError::Encrypted("PDF")
            .message()
            .contains("password-protected")
    );
    assert!(DocError::NoText.message().contains("scan"));
}

// -------------------------------------------------------------------- text

#[test]
fn text_files_are_read_in_their_encoding() {
    let d = extract("Takeout SOP.md", b"\xEF\xBB\xBF# Usual\r\n- Pad See Ew\r\n").unwrap();
    assert_eq!(d.kind, DocType::Markdown);
    assert_eq!(d.text, "# Usual\n- Pad See Ew\n");
    assert_eq!(d.summary, "4 words");
    // Older exports in Windows-1252, and UTF-16 with a byte order mark.
    assert_eq!(extract("a.txt", b"caf\xe9").unwrap().text, "café");
    let utf16: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain("hi there".encode_utf16().flat_map(|u| u.to_le_bytes()))
        .collect();
    assert_eq!(extract("a.txt", &utf16).unwrap().text, "hi there");
    assert_eq!(extract("one.txt", b"word").unwrap().summary, "1 word");
    // Binary data renamed .txt, and empty files.
    assert_eq!(
        extract("a.txt", b"bin\0ary").unwrap_err(),
        DocError::Unreadable("file")
    );
    assert_eq!(extract("a.md", b"  \n ").unwrap_err(), DocError::Empty);
    assert_eq!(extract("a.md", b"").unwrap_err(), DocError::Empty);
}

#[test]
fn stored_text_is_capped_with_a_note() {
    let big = "word ".repeat(MAX_STORED_CHARS / 4);
    let d = extract("big.txt", big.as_bytes()).unwrap();
    assert!(d.text.chars().count() < MAX_STORED_CHARS + 200);
    assert!(
        d.text.contains("[Cut here:"),
        "{}",
        &d.text[d.text.len() - 200..]
    );
}

#[test]
fn json_is_pretty_printed() {
    let d = extract(
        "orders.json",
        br#"[{"item":"Pad See Ew","qty":2},{"item":"Tea"}]"#,
    )
    .unwrap();
    assert!(d.text.starts_with("```json\n[\n  {\n"));
    assert!(d.text.contains("\"item\": \"Pad See Ew\""));
    assert_eq!(d.summary, "2 items");
    assert_eq!(extract("o.json", br#"{"a":1}"#).unwrap().summary, "1 field");
    assert_eq!(
        extract("bad.json", b"{\"a\": ").unwrap_err(),
        DocError::Unreadable("file")
    );
}

#[test]
fn html_keeps_headings_and_lists() {
    let page = b"<!doctype html><html><head><title>T</title><style>p{}</style></head>\
        <body><nav>Home</nav><h1>Weekly  order</h1><p>Order by <b>Friday</b>.</p>\
        <ul><li>Rice</li><li>Tea<ul><li>Green</li></ul></li></ul>\
        <script>var x = 1;</script><table><tr><th>Item</th><th>Qty</th></tr>\
        <tr><td>Rice</td><td>2</td></tr></table><p>Fish &amp; chips</p></body></html>";
    let d = extract("page.html", page).unwrap();
    assert!(d.text.contains("# Weekly order\n"), "{}", d.text);
    assert!(d.text.contains("Order by Friday."), "{}", d.text);
    assert!(d.text.contains("- Rice\n- Tea\n  - Green"), "{}", d.text);
    assert!(d.text.contains("Fish & chips"));
    assert!(d.text.contains("Item | Qty"), "{}", d.text);
    assert!(!d.text.contains("var x") && !d.text.contains("p{}"));
    assert!(!d.text.contains("<"), "{}", d.text);
}

#[test]
fn rtf_becomes_plain_text() {
    let rtf = br"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}
{\*\generator Synthetic 1.0;}{\info{\title Secret title}}
\pard\b Weekly order\b0\par
Caf\'e9 latte\tab x2\par
Price: \u8364? 4\par
{\pict\pngblip 89504e47}
Braces \{ and \} and back\\slash\line done}";
    let d = extract("order.rtf", rtf).unwrap();
    assert!(d.text.contains("Weekly order\n"), "{}", d.text);
    assert!(d.text.contains("Café latte\tx2"), "{}", d.text);
    assert!(d.text.contains("Price: € 4"), "{}", d.text);
    assert!(
        d.text.contains("Braces { and } and back\\slash\ndone"),
        "{}",
        d.text
    );
    assert!(!d.text.contains("Arial") && !d.text.contains("Synthetic"));
    assert!(!d.text.contains("Secret") && !d.text.contains("89504e47"));
    assert_eq!(
        extract("x.rtf", b"not rtf").unwrap_err(),
        DocError::Unreadable("document")
    );
}

// ------------------------------------------------------------------ tables

#[test]
fn csv_quoting_and_delimiters() {
    let csv =
        "name,notes,qty\r\n\"Smith, J.\",\"said \"\"hi\"\"\nthen left\",3\r\n,,\r\nLee,a|b,\r\n";
    let d = extract("people.csv", csv.as_bytes()).unwrap();
    assert_eq!(
        d.text,
        "| name | notes | qty |\n| --- | --- | --- |\n| Smith, J. | said \"hi\"<br>then left | 3 |\n| Lee | a\\|b |  |\n"
    );
    assert_eq!(d.summary, "3 rows · 3 columns");
    // Semicolons (some regions' exports) and tabs.
    let d = extract("eu.csv", "a;b\n1,5;2\n".as_bytes()).unwrap();
    assert!(d.text.contains("| 1,5 | 2 |"), "{}", d.text);
    let d = extract("t.tsv", "a\tb\n1\t\"x, y\"\n".as_bytes()).unwrap();
    assert!(d.text.contains("| 1 | x, y |"), "{}", d.text);
    // Empty columns are dropped; a trailing quote inside a field is kept.
    let d = extract("gap.csv", "a,,c\n1,,2 \"in\"\n".as_bytes()).unwrap();
    assert!(d.text.starts_with("| a | c |\n"), "{}", d.text);
    assert!(d.text.contains("| 1 | 2 \"in\" |"), "{}", d.text);
    assert_eq!(
        extract("blank.csv", b",,\n,,\n").unwrap_err(),
        DocError::Empty
    );
}

#[test]
fn big_tables_are_capped_with_notes() {
    let table = |rows: usize, cols: usize| {
        let mut csv = String::new();
        for r in 0..rows {
            let row: Vec<String> = (0..cols).map(|c| format!("r{r}c{c}")).collect();
            csv.push_str(&row.join(","));
            csv.push('\n');
        }
        csv
    };
    let d = extract("wide.csv", table(20, 60).as_bytes()).unwrap();
    assert!(
        d.text
            .starts_with("[Showing the first 50 of 60 columns.]\n\n| r0c0 |")
    );
    assert!(d.text.contains("r19c49") && !d.text.contains("c50"));
    assert_eq!(d.summary, "20 rows · 60 columns");
    let d = extract("long.csv", table(1_500, 3).as_bytes()).unwrap();
    assert!(
        d.text
            .ends_with("| r1000c2 |\n\n[Showing the first 1,000 of 1,499 rows.]\n")
    );
    assert!(!d.text.contains("r1001c0"));
    assert_eq!(d.summary, "1,500 rows · 3 columns");
    // Both: the text stops at the stored-text cap, saying so.
    let d = extract("huge.csv", table(1_500, 60).as_bytes()).unwrap();
    assert!(d.text.chars().count() < MAX_STORED_CHARS + 200);
    assert!(
        d.text.ends_with("so the rest is missing.]"),
        "{}",
        &d.text[d.text.len() - 100..]
    );
    // Long cells are cut.
    let long = format!("h\n{}\n", "x".repeat(MAX_CELL_CHARS + 50));
    let d = extract("long.csv", long.as_bytes()).unwrap();
    assert!(d.text.contains(&format!("{}…", "x".repeat(MAX_CELL_CHARS))));
}

// ------------------------------------------------------------ spreadsheets

#[test]
fn workbook_sheets_become_tables() {
    let book = xlsx(&[
        (
            "Orders",
            grid(&[
                &["Item", "", "Qty", "Price", "Total"],
                &["Pad See Ew", "", "2", "7.25", "=C2*D2|14.5"],
                &["", "", "", "", ""],
                &[
                    "Spring rolls",
                    "",
                    "1",
                    "0.1",
                    "=D3+0.2|0.30000000000000004",
                ],
                &["Tea & milk", "", "3", "1", "=C4*D4|3"],
            ]),
        ),
        ("Notes", grid(&[&["Order before 5pm on Friday."]])),
        ("Empty", Vec::new()),
    ]);
    let d = extract("Orders.xlsx", &book).unwrap();
    assert_eq!(d.kind, DocType::Xlsx);
    assert!(
        d.text.starts_with(
            "## Orders\n\n| Item | Qty | Price | Total |\n| --- | --- | --- | --- |\n\
             | Pad See Ew | 2 | 7.25 | 14.5 |\n| Spring rolls | 1 | 0.1 | 0.3 |\n\
             | Tea & milk | 3 | 1 | 3 |\n"
        ),
        "{}",
        d.text
    );
    assert!(
        d.text
            .contains("## Notes\n\n| Order before 5pm on Friday. |")
    );
    assert!(d.text.contains("## Empty\n\n[This sheet is empty.]"));
    assert_eq!(d.summary, "3 sheets · 5 rows");
    // The same workbook named .xlsm reads the same.
    assert_eq!(extract("Orders.xlsm", &book).unwrap().text, d.text);
}

#[test]
fn huge_sheets_are_capped() {
    let mut rows = vec![vec!["Day".to_string(), "Amount".to_string()]];
    for i in 0..1_500 {
        rows.push(vec![format!("Day {i}"), i.to_string()]);
    }
    let d = extract("big.xlsx", &xlsx(&[("Log", rows)])).unwrap();
    assert!(d.text.contains("| Day 999 | 999 |"));
    assert!(!d.text.contains("Day 1000"));
    assert!(d.text.contains("[Showing the first 1,000 of 1,500 rows.]"));
    assert_eq!(d.summary, "1 sheet · 1,501 rows");
}

#[test]
fn a_far_off_cell_does_not_blow_up_a_sheet() {
    // One value in the last row and column Excel allows: reading cell by
    // cell keeps this tiny (a whole range would be 17 billion cells).
    let book = xlsx(&[("Far", Vec::new())]);
    let mut files = unzip(&book);
    files.insert(
        "xl/worksheets/sheet1.xml".into(),
        "<?xml version=\"1.0\"?><worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>top</t></is></c></row><row r=\"1048576\"><c r=\"XFD1048576\"><v>1</v></c></row></sheetData></worksheet>".into(),
    );
    let d = extract("far.xlsx", &rezip(&files)).unwrap();
    assert!(
        d.text.contains("| top |  |\n| --- | --- |\n|  | 1 |"),
        "{}",
        d.text
    );
}

fn unzip(bytes: &[u8]) -> std::collections::BTreeMap<String, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        out.insert(f.name().unwrap().to_string(), s);
    }
    out
}

fn rezip(files: &std::collections::BTreeMap<String, String>) -> Vec<u8> {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    zip_of(&refs)
}

#[test]
fn opendocument_spreadsheets_are_read() {
    let content = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" office:version=\"1.2\">\
<office:body><office:spreadsheet>\
<table:table table:name=\"Budget\">\
<table:table-row><table:table-cell office:value-type=\"string\"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type=\"string\"><text:p>Cost</text:p></table:table-cell></table:table-row>\
<table:table-row><table:table-cell office:value-type=\"string\"><text:p>Rent</text:p></table:table-cell><table:table-cell office:value-type=\"float\" office:value=\"1200\"><text:p>1200</text:p></table:table-cell></table:table-row>\
<table:table-row table:number-rows-repeated=\"1048000\"><table:table-cell table:number-columns-repeated=\"1024\"/></table:table-row>\
</table:table>\
<table:table table:name=\"Blank\"><table:table-row><table:table-cell/></table:table-row></table:table>\
</office:spreadsheet></office:body></office:document-content>";
    let ods = zip_of(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        (
            "META-INF/manifest.xml",
            "<?xml version=\"1.0\"?><manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\"><manifest:file-entry manifest:full-path=\"/\" manifest:media-type=\"application/vnd.oasis.opendocument.spreadsheet\"/></manifest:manifest>",
        ),
        ("content.xml", content),
    ]);
    let d = extract("budget.ods", &ods).unwrap();
    assert!(
        d.text
            .contains("## Budget\n\n| Item | Cost |\n| --- | --- |\n| Rent | 1200 |"),
        "{}",
        d.text
    );
    assert!(d.text.contains("## Blank\n\n[This sheet is empty.]"));
    assert_eq!(d.summary, "2 sheets · 2 rows");
}

#[test]
fn encrypted_files_say_so() {
    // Password-protected Office files are OLE containers, not zips.
    let mut ole = CFB_MAGIC_FOR_TESTS.to_vec();
    ole.extend_from_slice(&[0u8; 600]);
    assert_eq!(
        extract("secret.xlsx", &ole).unwrap_err(),
        DocError::Encrypted("spreadsheet")
    );
    assert_eq!(
        extract("secret.docx", &ole).unwrap_err(),
        DocError::Encrypted("document")
    );
    // OpenDocument encryption is listed in the manifest.
    let odt = zip_of(&[
        ("mimetype", "application/vnd.oasis.opendocument.text"),
        (
            "META-INF/manifest.xml",
            "<?xml version=\"1.0\"?><manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\"><manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"><manifest:encryption-data manifest:checksum-type=\"SHA1/1K\"/></manifest:file-entry></manifest:manifest>",
        ),
        ("content.xml", "\u{1}\u{2}binary"),
    ]);
    assert_eq!(
        extract("secret.odt", &odt).unwrap_err(),
        DocError::Encrypted("document")
    );
    // A PDF that needs a password to open (the empty one doesn't work).
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>".to_string(),
        format!(
            "<< /Filter /Standard /V 1 /R 2 /Length 40 /P -4 /O <{}> /U <{}> >>",
            "6b".repeat(32),
            "a5".repeat(32)
        ),
    ];
    let locked = pdf(
        &objects,
        &format!(
            "/Encrypt 4 0 R /ID [<{}> <{}>]",
            "11".repeat(16),
            "11".repeat(16)
        ),
    );
    assert_eq!(
        extract("locked.pdf", &locked).unwrap_err(),
        DocError::Encrypted("PDF")
    );
    // Real RC4 encryption: with only an owner password (the common "no
    // password to open, but no editing" PDF) it reads; with a password to
    // open, it doesn't.
    let encrypted = |user: &str| {
        let mut doc =
            lopdf::Document::load_mem(&text_pdf(&["BT /F1 12 Tf (Secret menu) Tj ET"])).unwrap();
        let id = lopdf::Object::String(vec![7; 16], lopdf::StringFormat::Hexadecimal);
        doc.trailer
            .set("ID", lopdf::Object::Array(vec![id.clone(), id]));
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner-pass",
            user_password: user,
            key_length: 128,
            permissions: lopdf::Permissions::all(),
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    };
    let open = encrypted("");
    assert!(!open.windows(11).any(|w| w == b"Secret menu"));
    assert_eq!(
        extract("open.pdf", &open).unwrap().text,
        "[Page 1]\nSecret menu\n\n"
    );
    assert_eq!(
        extract("locked.pdf", &encrypted("user-pass")).unwrap_err(),
        DocError::Encrypted("PDF")
    );
}

const CFB_MAGIC_FOR_TESTS: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

#[test]
fn corrupt_files_are_unreadable() {
    let book = xlsx(&[("S", grid(&[&["a"]]))]);
    for (name, bytes, noun) in [
        ("cut.xlsx", book[..book.len() / 2].to_vec(), "spreadsheet"),
        (
            "junk.xlsx",
            b"PK\x03\x04 not really".to_vec(),
            "spreadsheet",
        ),
        (
            "junk.xls",
            b"not an excel file at all".to_vec(),
            "spreadsheet",
        ),
        ("junk.ods", b"plain text".to_vec(), "spreadsheet"),
        ("junk.docx", b"plain text".to_vec(), "document"),
        ("junk.odt", b"plain text".to_vec(), "document"),
        ("junk.pdf", b"%PDF-1.7 and then nothing".to_vec(), "PDF"),
        ("notpdf.pdf", b"hello".to_vec(), "PDF"),
    ] {
        assert_eq!(
            extract(name, &bytes).unwrap_err(),
            DocError::Unreadable(noun),
            "{name}"
        );
    }
    // A zip that isn't a workbook.
    let other = zip_of(&[("readme.txt", "hello")]);
    assert_eq!(
        extract("other.xlsx", &other).unwrap_err(),
        DocError::Unreadable("spreadsheet")
    );
    assert_eq!(
        extract("other.docx", &other).unwrap_err(),
        DocError::Unreadable("document")
    );
}

#[test]
fn zip_bombs_are_refused_before_unpacking() {
    // A part that says it unpacks to almost 4 GB: patch the central
    // directory's uncompressed size (offset 24 in its header).
    let mut z = zip_of(&[("word/document.xml", "<w:document/>")]);
    let cd = z
        .windows(4)
        .rposition(|w| w == b"PK\x01\x02")
        .expect("central directory");
    z[cd + 24..cd + 28].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    assert_eq!(extract("bomb.docx", &z).unwrap_err(), DocError::TooComplex);
    assert_eq!(extract("bomb.xlsx", &z).unwrap_err(), DocError::TooComplex);
    // Too many parts.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..=MAX_ZIP_ENTRIES {
        w.start_file(format!("{i}"), opts).unwrap();
    }
    let many = w.finish().unwrap().into_inner();
    assert_eq!(
        extract("many.odt", &many).unwrap_err(),
        DocError::TooComplex
    );
}

// -------------------------------------------------------------------- word

#[test]
fn docx_keeps_headings_lists_and_tables() {
    let styles = "<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/></w:style>\
        <w:style w:type=\"paragraph\" w:styleId=\"Ueberschrift2\"><w:name w:val=\"heading 2\"/></w:style>\
        <w:style w:type=\"paragraph\" w:styleId=\"Bullets\"><w:name w:val=\"My Bullets\"/><w:pPr><w:numPr><w:numId w:val=\"1\"/></w:numPr></w:pPr></w:style>";
    let numbering = "<w:abstractNum w:abstractNumId=\"10\"><w:lvl w:ilvl=\"0\"><w:numFmt w:val=\"bullet\"/></w:lvl><w:lvl w:ilvl=\"1\"><w:numFmt w:val=\"bullet\"/></w:lvl></w:abstractNum>\
        <w:abstractNum w:abstractNumId=\"20\"><w:lvl w:ilvl=\"0\"><w:numFmt w:val=\"decimal\"/></w:lvl></w:abstractNum>\
        <w:num w:numId=\"1\"><w:abstractNumId w:val=\"10\"/></w:num><w:num w:numId=\"2\"><w:abstractNumId w:val=\"20\"/></w:num>";
    let body = [
        para(Some("Heading1"), None, "Saturday dinner"),
        para(None, None, "Order from a Thai place that&apos;s open."),
        para(Some("Ueberschrift2"), None, "Usual order"),
        para(None, Some((1, 0)), "Pad See Ew"),
        para(None, Some((1, 1)), "extra egg"),
        para(Some("Bullets"), None, "Spring rolls"),
        para(None, None, "Steps"),
        para(None, Some((2, 0)), "Check the menu"),
        para(None, Some((2, 0)), "Pay with the saved card"),
        // Tracked deletions and field codes are left out; tabs and breaks kept.
        "<w:p><w:r><w:t>Tip</w:t></w:r><w:r><w:tab/><w:t>15%</w:t></w:r><w:del><w:r><w:delText>20%</w:delText></w:r></w:del><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:br/><w:t>max</w:t></w:r></w:p>".to_string(),
        "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Dish</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>\
         <w:tr><w:tc><w:p><w:r><w:t>Curry | hot</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>1</w:t></w:r></w:p><w:p><w:r><w:t>or 2</w:t></w:r></w:p></w:tc></w:tr></w:tbl>".to_string(),
        "<w:p/>".to_string(),
        para(None, None, "The end."),
    ]
    .concat();
    let d = extract("plan.docx", &docx(&body, styles, numbering)).unwrap();
    assert_eq!(
        d.text,
        "# Saturday dinner\n\n\
         Order from a Thai place that's open.\n\n\
         ## Usual order\n\n\
         - Pad See Ew\n  - extra egg\n- Spring rolls\n\n\
         Steps\n\n\
         1. Check the menu\n1. Pay with the saved card\n\n\
         Tip\t15%\nmax\n\n\
         | Dish | Qty |\n| --- | --- |\n| Curry \\| hot | 1<br>or 2 |\n\n\
         The end.\n"
    );
    assert_eq!(d.summary, "40 words");
    // Without styles.xml, built-in heading ids still count.
    let bare = zip_of(&[(
        "word/document.xml",
        &format!(
            "<w:document {W}><w:body>{}</w:body></w:document>",
            para(Some("Heading2"), None, "Plain")
        ),
    )]);
    assert_eq!(extract("bare.docx", &bare).unwrap().text, "## Plain\n");
    // A document with no text.
    let empty = docx("<w:p/>", "", "");
    assert_eq!(extract("e.docx", &empty).unwrap_err(), DocError::Empty);
}

#[test]
fn odt_keeps_headings_lists_and_tables() {
    let content = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\">\
<office:automatic-styles><style:style xmlns:style=\"s\" style:name=\"P1\">hidden</style:style></office:automatic-styles>\
<office:body><office:text>\
<text:h text:outline-level=\"1\">Packing list</text:h>\
<text:p>Pack the night before.<office:annotation><text:p>a comment</text:p></office:annotation></text:p>\
<text:list><text:list-item><text:p>Passport</text:p></text:list-item>\
<text:list-item><text:p>Chargers</text:p><text:list><text:list-item><text:p>Phone</text:p></text:list-item></text:list></text:list-item></text:list>\
<table:table><table:table-row><table:table-cell><text:p>Day</text:p></table:table-cell><table:table-cell><text:p>Bag</text:p></table:table-cell></table:table-row>\
<table:table-row><table:table-cell><text:p>Mon</text:p></table:table-cell><table:table-cell><text:p>Blue</text:p></table:table-cell></table:table-row></table:table>\
<text:p>Two<text:s text:c=\"2\"/>spaces<text:tab/>tab<text:line-break/>line &amp; more</text:p>\
</office:text></office:body></office:document-content>";
    let odt = zip_of(&[
        ("mimetype", "application/vnd.oasis.opendocument.text"),
        ("content.xml", content),
    ]);
    let d = extract("pack.odt", &odt).unwrap();
    assert_eq!(
        d.text,
        "# Packing list\n\n\
         Pack the night before.\n\n\
         - Passport\n- Chargers\n  - Phone\n\n\
         | Day | Bag |\n| --- | --- |\n| Mon | Blue |\n\n\
         Two  spaces\ttab\nline & more\n"
    );
}

// --------------------------------------------------------------------- pdf

#[test]
fn pdf_text_layer_is_read_page_by_page() {
    let d = extract(
        "menu.pdf",
        &text_pdf(&[
            "BT /F1 12 Tf 72 720 Td (Weekly order list) Tj 0 -14 Td (Pad See Ew x2) Tj ET",
            "BT /F1 12 Tf 1 0 0 1 72 720 Tm [(Spr) 20 (ing) -300 (rolls)] TJ \
             1 0 0 1 72 700 Tm (Total \\(with tip\\): 31) Tj T* (Thanks) Tj ET",
        ]),
    )
    .unwrap();
    assert_eq!(
        d.text,
        "[Page 1]\nWeekly order list\nPad See Ew x2\n\n\
         [Page 2]\nSpring rolls\nTotal (with tip): 31\nThanks\n\n"
    );
    assert_eq!(d.summary, "2 pages");
    // Pages without text are skipped; a PDF without any is a scan.
    let d = extract("mixed.pdf", &text_pdf(&["", "BT /F1 9 Tf (Hi) Tj ET"])).unwrap();
    assert_eq!(d.text, "[Page 2]\nHi\n\n");
    assert_eq!(d.summary, "2 pages");
    assert_eq!(
        extract("scan.pdf", &text_pdf(&["0 0 m 100 100 l S"])).unwrap_err(),
        DocError::NoText
    );
}

#[test]
fn many_pdf_pages_are_capped() {
    let pages: Vec<String> = (0..MAX_PDF_PAGES + 3)
        .map(|i| format!("BT /F1 9 Tf (p{i}) Tj ET"))
        .collect();
    let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
    let d = extract("long.pdf", &text_pdf(&refs)).unwrap();
    assert!(d.text.contains(&format!("p{}", MAX_PDF_PAGES - 1)));
    assert!(!d.text.contains(&format!("p{}", MAX_PDF_PAGES)));
    assert!(d.text.contains("[Showing the first 500 of 503 pages.]"));
    assert_eq!(d.summary, "503 pages");
}

// ------------------------------------------------------------------ bounds

#[test]
fn a_past_deadline_stops_every_reader() {
    let late = || Budget {
        deadline: std::time::Instant::now() - std::time::Duration::from_secs(1),
    };
    let many_paras: String = (0..3_000)
        .map(|i| para(None, None, &format!("p{i}")))
        .collect();
    let odt_body: String = (0..3_000)
        .map(|i| format!("<text:p>p{i}</text:p>"))
        .collect();
    let odt = zip_of(&[(
        "content.xml",
        &format!(
            "<office:document-content xmlns:office=\"o\" xmlns:text=\"t\"><office:body><office:text>{odt_body}</office:text></office:body></office:document-content>"
        ),
    )]);
    let rtf = format!("{{\\rtf1 {}}}", "word ".repeat(20_000));
    let html = "<p>word</p>".repeat(5_000);
    for (name, bytes) in [
        ("late.csv", "a,b\n".repeat(10).into_bytes()),
        ("late.xlsx", xlsx(&[("S", grid(&[&["a"]]))])),
        ("late.pdf", text_pdf(&["BT /F1 9 Tf (x) Tj ET"])),
        ("late.docx", docx(&many_paras, "", "")),
        ("late.odt", odt),
        ("late.rtf", rtf.into_bytes()),
        ("late.html", html.into_bytes()),
    ] {
        assert_eq!(
            extract_with(name, &bytes, late()).unwrap_err(),
            DocError::TooComplex,
            "{name}"
        );
        assert!(extract(name, &bytes).is_ok(), "{name}");
    }
}

#[tokio::test]
async fn bounded_extraction_reads_in_process_without_isolation() {
    let d = extract_bounded("a.csv".into(), b"x,y\n1,2\n".to_vec(), false)
        .await
        .unwrap();
    assert_eq!(d.summary, "2 rows · 2 columns");
    assert!(matches!(
        extract_bounded("a.bin".into(), vec![1, 2], true).await,
        Err(DocError::Unsupported(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_file_at_a_time_and_a_stuck_one_times_out() {
    use std::time::{Duration, Instant};
    static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let ms = Duration::from_millis;
    let (release, stuck) = std::sync::mpsc::channel::<()>();
    let permit = take_slot(&SLOT, ms(100)).await.unwrap();
    // A reader that ignores its deadline until it's let go.
    let started = Instant::now();
    let r = blocking(permit, ms(200), "file", move |_| {
        stuck.recv().ok();
        Ok(())
    })
    .await;
    assert_eq!(r, Err(DocError::TooComplex));
    assert!(started.elapsed() < Duration::from_secs(3));
    // The async side kept running; the stuck reader still holds the slot,
    // so the next file is told to wait rather than starting a second one.
    assert_eq!(take_slot(&SLOT, ms(100)).await.unwrap_err(), DocError::Busy);
    release.send(()).unwrap();
    // Once it stops, the slot is free again.
    let permit = take_slot(&SLOT, Duration::from_secs(5)).await.unwrap();
    // A reader that panics on a malformed file reports it as unreadable.
    let r: Result<(), DocError> = blocking(permit, Duration::from_secs(5), "PDF", |_| {
        panic!("bad file")
    })
    .await;
    assert_eq!(r, Err(DocError::Unreadable("PDF")));
    assert!(take_slot(&SLOT, ms(100)).await.is_ok());
}

#[test]
fn numbers_read_like_a_spreadsheet_shows_them() {
    use super::sheets::number;
    assert_eq!(number(3.0), "3");
    assert_eq!(number(-12.0), "-12");
    assert_eq!(number(0.1 + 0.2), "0.3");
    assert_eq!(number(1234.5678), "1234.5678");
    assert_eq!(number(1e20), "100000000000000000000");
    assert_eq!(thousands(1_234_567), "1,234,567");
    assert_eq!(thousands(999), "999");
}

// ------------------------------------------------------------------ prompt

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
    assert!(s.contains(
        "[Truncated: only the first 20 of 30 characters are included, so this document is \
         partial."
    ));
    assert!(s.contains("<document title=\"Late.md\">\n[Left out"));
    assert!(!s.contains('c'.to_string().repeat(5).as_str()));
    let all = prompt_sections(&docs, MAX_DOC_CHARS);
    assert!(!all.contains("Truncated") && !all.contains("Left out"));
}

// ------------------------------------------------------------ round 1 fixes

#[test]
fn pdf_text_inside_forms_is_read() {
    // Page 1 draws a form (with its own font) and has text of its own;
    // page 2 is only a form, drawn twice, that draws a nested form.
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 2 >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> /XObject << /Fm1 7 0 R >> >> /Contents 5 0 R >>".to_string(),
        stream("BT /F1 12 Tf 72 720 Td (Page text) Tj ET q /Fm1 Do Q"),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Fm2 8 0 R >> >> /Contents 9 0 R >>".to_string(),
        format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /Font << /F9 3 0 R >> >> /Length {} >>\nstream\n{}\nendstream",
            "BT /F9 10 Tf 72 600 Td (Form text) Tj ET".len(),
            "BT /F9 10 Tf 72 600 Td (Form text) Tj ET"
        ),
        format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> /XObject << /Fm1 7 0 R >> >> /Length {} >>\nstream\n{}\nendstream",
            "BT /F1 10 Tf (Outer form) Tj ET /Fm1 Do".len(),
            "BT /F1 10 Tf (Outer form) Tj ET /Fm1 Do"
        ),
        stream("/Fm2 Do /Fm2 Do /Missing Do"),
    ];
    let d = extract("forms.pdf", &pdf(&objects, "")).unwrap();
    assert_eq!(
        d.text,
        "[Page 1]\nPage text\nForm text\n\n\
         [Page 2]\nOuter form\nForm text\nOuter form\nForm text\n\n"
    );
    // A form that draws itself stops at the nesting limit.
    let looped = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [4 0 R] /Count 1 >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 3 0 R >> /XObject << /Me 6 0 R >> >> /Contents 5 0 R >>".to_string(),
        stream("/Me Do"),
        format!(
            "<< /Type /XObject /Subtype /Form /Length {} >>\nstream\n{}\nendstream",
            "BT /F1 9 Tf (loop) Tj ET /Me Do".len(),
            "BT /F1 9 Tf (loop) Tj ET /Me Do"
        ),
    ];
    let d = extract("loop.pdf", &pdf(&looped, "")).unwrap();
    assert_eq!(d.text.matches("loop").count(), 8, "{}", d.text);
}

/// The text PDF with every stream compressed, and optionally a page whose
/// content inflates past the limit.
fn compressed_pdf(pages: &[&str], huge_page: bool) -> Vec<u8> {
    let mut doc = lopdf::Document::load_mem(&text_pdf(pages)).unwrap();
    if huge_page {
        let page = *doc.get_pages().values().next().unwrap();
        let mut content = "BT /F1 9 Tf (x) Tj ET\n".to_string();
        content.push_str(&" ".repeat(MAX_PDF_STREAM_BYTES + 1));
        let id = doc.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            content.into_bytes(),
        ));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Contents", lopdf::Object::Reference(id));
    }
    doc.compress();
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn compressed_pdf_streams_are_inflated_within_the_limit() {
    let padded = format!("BT /F1 12 Tf (Squeezed text) Tj ET{}", " ".repeat(500));
    let small = compressed_pdf(&[&padded], false);
    assert!(!small.windows(13).any(|w| w == b"Squeezed text"));
    assert_eq!(
        extract("small.pdf", &small).unwrap().text,
        "[Page 1]\nSqueezed text\n\n"
    );
    let bomb = compressed_pdf(&["BT /F1 12 Tf (x) Tj ET"], true);
    assert!(bomb.len() < 1024 * 1024, "{}", bomb.len());
    assert_eq!(
        extract("bomb.pdf", &bomb).unwrap_err(),
        DocError::TooComplex
    );
}

#[test]
fn deeply_nested_html_stays_small() {
    let html = "<ul><li>x".repeat(50_000);
    let started = std::time::Instant::now();
    let d = extract("deep.html", html.as_bytes()).unwrap();
    assert!(d.text.len() < MAX_STORED_CHARS * 2, "{}", d.text.len());
    assert!(d.text.contains(&format!("{}- x", "  ".repeat(8))));
    assert!(!d.text.contains(&format!("{}- x", "  ".repeat(9))));
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn merged_table_cells_keep_later_cells_in_their_columns() {
    let cell = |t: &str, span: usize| {
        let pr = if span > 1 {
            format!("<w:tcPr><w:gridSpan w:val=\"{span}\"/></w:tcPr>")
        } else {
            String::new()
        };
        format!("<w:tc>{pr}<w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>")
    };
    let row = |cells: &[String]| format!("<w:tr>{}</w:tr>", cells.concat());
    let body = format!(
        "<w:tbl>{}{}{}</w:tbl>",
        row(&[cell("Item", 1), cell("Qty", 1), cell("Amount", 1)]),
        row(&[cell("Rice", 1), cell("2", 1), cell("17", 1)]),
        row(&[cell("Subtotal", 2), cell("17", 1)]),
    );
    let d = extract("t.docx", &docx(&body, "", "")).unwrap();
    assert!(
        d.text
            .ends_with("| Rice | 2 | 17 |\n| Subtotal |  | 17 |\n"),
        "{}",
        d.text
    );
    // OpenDocument: a merged cell is followed by covered cells, and equal
    // neighbours may be one repeated cell.
    let content = "<office:document-content xmlns:office=\"o\" xmlns:text=\"t\" xmlns:table=\"b\"><office:body><office:text><table:table>\
        <table:table-row><table:table-cell><text:p>Item</text:p></table:table-cell><table:table-cell><text:p>Qty</text:p></table:table-cell><table:table-cell><text:p>Amount</text:p></table:table-cell></table:table-row>\
        <table:table-row><table:table-cell table:number-columns-spanned=\"2\"><text:p>Subtotal</text:p></table:table-cell><table:covered-table-cell/><table:table-cell><text:p>17</text:p></table:table-cell></table:table-row>\
        <table:table-row><table:table-cell table:number-columns-repeated=\"3\"><text:p>-</text:p></table:table-cell></table:table-row>\
        </table:table></office:text></office:body></office:document-content>";
    let d = extract("t.odt", &zip_of(&[("content.xml", content)])).unwrap();
    assert_eq!(
        d.text,
        "| Item | Qty | Amount |\n| --- | --- | --- |\n| Subtotal |  | 17 |\n| - | - | - |\n"
    );
}

#[test]
fn long_word_tables_say_rows_were_left_out() {
    let rows: String = (0..1_200)
        .map(|i| format!("<w:tr><w:tc><w:p><w:r><w:t>r{i}</w:t></w:r></w:p></w:tc></w:tr>"))
        .collect();
    let d = extract(
        "long.docx",
        &docx(&format!("<w:tbl>{rows}</w:tbl>"), "", ""),
    )
    .unwrap();
    assert!(d.text.contains("| r1000 |") && !d.text.contains("| r1001 |"));
    assert!(
        d.text.contains("[Showing the first 1,000 of 1,199 rows.]"),
        "{}",
        &d.text[d.text.len() - 200..]
    );
}
