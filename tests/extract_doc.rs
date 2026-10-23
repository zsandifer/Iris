//! `iris extract-doc`, the child process the daemon reads routine document
//! uploads in: bytes on stdin, one JSON answer on stdout, and a file that
//! makes the reader run out of memory kills only the child.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(script: &str, ext: &str, input: &[u8]) -> std::process::Output {
    let mut child = Command::new("/bin/sh")
        .args(["-c", script])
        .arg(env!("CARGO_BIN_EXE_iris"))
        .arg(ext)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn reads_stdin_and_answers_on_stdout() {
    let out = run(
        r#"exec "$0" extract-doc "$1""#,
        "csv",
        b"item,qty\nRice,2\n",
    );
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["summary"], "2 rows · 2 columns");
    assert_eq!(v["text"], "| item | qty |\n| --- | --- |\n| Rice | 2 |\n");
    let out = run(r#"exec "$0" extract-doc "$1""#, "pdf", b"not a pdf");
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"], "unreadable");
    let out = run(r#"exec "$0" extract-doc "$1""#, "exe", b"x");
    assert_eq!(out.status.code(), Some(2));
}

/// A tiny spreadsheet that asks for 50 million cells.
fn ods_bomb() -> Vec<u8> {
    let content = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" office:version=\"1.2\">\
<office:body><office:spreadsheet><table:table table:name=\"Big\">\
<table:table-row table:number-rows-repeated=\"50000\"><table:table-cell table:number-columns-repeated=\"1000\" office:value-type=\"float\" office:value=\"1\"><text:p>1</text:p></table:table-cell></table:table-row>\
</table:table></office:spreadsheet></office:body></office:document-content>";
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in [
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        (
            "META-INF/manifest.xml",
            "<?xml version=\"1.0\"?><manifest:manifest xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\"><manifest:file-entry manifest:full-path=\"/\" manifest:media-type=\"application/vnd.oasis.opendocument.spreadsheet\"/></manifest:manifest>",
        ),
        ("content.xml", content),
    ] {
        w.start_file(name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn a_spreadsheet_bomb_dies_in_the_child() {
    let bomb = ods_bomb();
    assert!(bomb.len() < 4096);
    // The same address-space limit the daemon sets (1 GiB, in KiB here).
    let started = std::time::Instant::now();
    let out = run(
        r#"ulimit -v 1048576 && ulimit -c 0 && exec "$0" extract-doc "$1""#,
        "ods",
        &bomb,
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}
