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

/// A real `iris serve` on a temporary data directory, stopped on drop.
struct Server {
    child: std::process::Child,
    base: String,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn iris(dir: &std::path::Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_iris"));
    c.args(args)
        .arg("--config")
        .arg(dir.join("config.toml"))
        .arg("--data-dir")
        .arg(dir.join("data"));
    c
}

fn start_server() -> (Server, String) {
    let dir = tempfile::tempdir().unwrap();
    let init = iris(dir.path(), &["init"]).output().unwrap();
    assert!(init.status.success(), "{init:?}");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = iris(
        dir.path(),
        &["serve", "--bind", &format!("127.0.0.1:{port}")],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let server = Server {
        child,
        base: base.clone(),
        _dir: dir,
    };
    let http = agent();
    let mut up = false;
    for _ in 0..150 {
        if http.get(format!("{base}/api/health")).call().is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(up, "server did not start");
    let pair = iris(server._dir.path(), &["pair"]).output().unwrap();
    let out = String::from_utf8_lossy(&pair.stdout);
    let code = out
        .lines()
        .find_map(|l| l.split_once("Pairing code for ").map(|(_, r)| r))
        .and_then(|r| r.split_once(": ").map(|(_, c)| c.trim().to_string()))
        .expect("pairing code");
    let mut res = http
        .post(format!("{base}/api/pair"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({"code": code, "device_name": "Test"}).to_string())
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&res.body_mut().read_to_string().unwrap()).unwrap();
    let token = v["token"].as_str().expect("token").to_string();
    (server, token)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into()
}

#[test]
fn the_server_reads_uploads_in_a_limited_child() {
    let (server, token) = start_server();
    let http = agent();
    let upload = |name: &str, body: &[u8]| {
        let mut res = http
            .post(format!("{}/api/routines/docs?name={name}", server.base))
            .header("Authorization", &format!("Bearer {token}"))
            .send(body)
            .unwrap();
        let status = res.status().as_u16();
        let v: serde_json::Value =
            serde_json::from_str(&res.body_mut().read_to_string().unwrap()).unwrap();
        (status, v)
    };
    // The spreadsheet bomb is refused, from the child, without any limit
    // set by this test: the server's own launcher sets them.
    let started = std::time::Instant::now();
    let (status, v) = upload("bomb.ods", &ods_bomb());
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["error"]["code"], "too_complex");
    // Fast: the memory limit stops it (not the 30-second deadline).
    assert!(started.elapsed() < std::time::Duration::from_secs(15));
    // The server is still up and reads the next file.
    let (status, v) = upload("orders.csv", b"item,qty\nRice,2\n");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["doc"]["summary"], "2 rows · 2 columns");
}
