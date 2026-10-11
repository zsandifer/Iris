//! Voice: replies as voice notes (text to speech) and voice input (speech
//! to text). Both run locally on the server with open-source models, so
//! audio never leaves the machine:
//!
//! - Runtime: sherpa-onnx (Apache-2.0), the official prebuilt Linux
//!   release (x86_64 or aarch64).
//! - Speech: a Piper voice (en_US "ljspeech", medium quality; trained on
//!   the public-domain LJ Speech dataset).
//! - Transcription: OpenAI Whisper `base.en` (MIT), as exported for
//!   sherpa-onnx (only the int8 files are kept).
//!
//! They are installed from the app like engines (`engines.rs`): fixed
//! official GitHub release URLs, every download checked against a SHA-256
//! and size pinned here before it is unpacked, into the engines directory
//! (`<root>/sherpa-onnx`, `<root>/tts`, `<root>/stt`). With the privilege
//! split the admin user installs them and the agent user only runs them.
//! Unpacking uses the system `tar` (with `bzip2`), so no archive code is
//! linked into Iris.
//!
//! Iris only runs the two command-line programs (`sherpa-onnx-offline-tts`,
//! `sherpa-onnx-offline`) as short-lived subprocesses, one at a time, with
//! a timeout and an empty environment; nothing stays resident.

use crate::engines::copy_hashed;
use crate::util::{Result, new_id};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest text spoken as one voice note; longer replies are cut at a
/// sentence and say the rest is in the text.
pub const MAX_SPEECH_CHARS: usize = 1500;
/// Longest a synthesis or transcription may run.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);
const THREADS: &str = "--num-threads=2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// Text to speech (voice replies).
    Tts,
    /// Speech to text (voice input).
    Stt,
}

impl Component {
    pub const ALL: [Component; 2] = [Component::Tts, Component::Stt];

    pub fn name(self) -> &'static str {
        match self {
            Component::Tts => "tts",
            Component::Stt => "stt",
        }
    }

    fn model(self) -> &'static Artifact {
        match self {
            Component::Tts => &TTS_MODEL,
            Component::Stt => &STT_MODEL,
        }
    }
}

/// One pinned download.
#[derive(Debug, Clone, Copy)]
pub struct Artifact {
    /// Recorded in `VERSION` once installed.
    pub id: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

#[cfg(test)]
const RELEASES: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download";

const RUNTIME_X64: Artifact = Artifact {
    id: "1.13.8",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.8/sherpa-onnx-v1.13.8-linux-x64-shared.tar.bz2",
    sha256: "c0bdb7907d3a74bba1d55d22bf4d9fa75586cf1530614ebe88a27b9118e015c4",
    size: 28_156_791,
};

const RUNTIME_ARM64: Artifact = Artifact {
    id: "1.13.8",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.8/sherpa-onnx-v1.13.8-linux-aarch64-shared-cpu.tar.bz2",
    sha256: "4e3734f82bc1379fd91f219f5869c7e9d03b7a4f7561907d8abca4849c51a789",
    size: 28_091_778,
};

const TTS_MODEL: Artifact = Artifact {
    id: "piper-en_US-ljspeech-medium",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-en_US-ljspeech-medium.tar.bz2",
    sha256: "3dfb4b759d8be032a4903a9538d128b0fda2a06ab1de6cbc2d93a97e2dd83dba",
    size: 67_169_893,
};

const STT_MODEL: Artifact = Artifact {
    id: "whisper-base.en",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.en.tar.bz2",
    sha256: "475bc7052ce299c007f6d5d5407ba8601f819a2867f6eecee510ed17df581542",
    size: 208_576_005,
};

/// The runtime for this machine, if there is an official build for it.
pub fn runtime_artifact() -> Result<&'static Artifact> {
    if std::env::consts::OS != "linux" {
        return Err("voice needs Linux (x86_64 or aarch64) on the server".into());
    }
    match std::env::consts::ARCH {
        "x86_64" => Ok(&RUNTIME_X64),
        "aarch64" => Ok(&RUNTIME_ARM64),
        other => Err(format!("voice is not available for {other} servers").into()),
    }
}

/// Approximate download size in MB (runtime + model), for the app.
pub fn download_mb(c: Component) -> u64 {
    let rt = runtime_artifact().map(|a| a.size).unwrap_or(0);
    (rt + c.model().size).div_ceil(1_000_000)
}

fn runtime_dir(root: &Path) -> PathBuf {
    root.join("sherpa-onnx")
}

fn component_dir(root: &Path, c: Component) -> PathBuf {
    root.join(c.name())
}

fn tts_bin(root: &Path) -> PathBuf {
    runtime_dir(root).join("bin/sherpa-onnx-offline-tts")
}

fn stt_bin(root: &Path) -> PathBuf {
    runtime_dir(root).join("bin/sherpa-onnx-offline")
}

fn read_version(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("VERSION"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The component and the runtime it needs are installed.
pub fn installed(root: &Path, c: Component) -> bool {
    let bin = match c {
        Component::Tts => tts_bin(root),
        Component::Stt => stt_bin(root),
    };
    read_version(&component_dir(root, c)).is_some()
        && read_version(&runtime_dir(root)).is_some()
        && bin.is_file()
}

/// `<model> on sherpa-onnx <version>`, when installed.
pub fn installed_version(root: &Path, c: Component) -> Option<String> {
    if !installed(root, c) {
        return None;
    }
    Some(format!(
        "{} (sherpa-onnx {})",
        read_version(&component_dir(root, c))?,
        read_version(&runtime_dir(root))?
    ))
}

/// What the server lacks to unpack downloads, if anything.
pub fn missing_tools() -> Option<&'static str> {
    if crate::harness::resolve_command("tar").is_none() {
        Some("tar")
    } else if crate::harness::resolve_command("bzip2").is_none() {
        Some("bzip2")
    } else {
        None
    }
}

// ---------------------------------------------------------------- install

/// Download `a` into `dir`, verifying size and SHA-256 while streaming.
fn download(a: &Artifact, dir: &Path) -> Result<PathBuf> {
    let file = dir.join(format!("{}.tar.bz2", new_id()));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60 * 60)))
        .timeout_connect(Some(Duration::from_secs(20)))
        .build()
        .into();
    let res = agent.get(a.url).call()?;
    let (_, body) = res.into_parts();
    let out = std::fs::File::create(&file)?;
    let copied = copy_hashed(body.into_reader(), std::io::BufWriter::new(out), a.size);
    verify_download(&file, copied, a)?;
    Ok(file)
}

fn verify_download(file: &Path, copied: Result<(String, u64)>, a: &Artifact) -> Result<()> {
    let ok = matches!(&copied, Ok((hash, n)) if hash == a.sha256 && *n == a.size);
    if !ok {
        let _ = std::fs::remove_file(file);
        copied?;
        return Err(format!("{} failed checksum verification", a.id).into());
    }
    Ok(())
}

/// Unpack a verified archive with the system tar.
fn unpack(archive: &Path, into: &Path, excludes: &[&str]) -> Result<()> {
    if let Some(tool) = missing_tools() {
        return Err(format!("the server needs `{tool}` to unpack voice files").into());
    }
    std::fs::create_dir_all(into)?;
    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-xjf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .arg("--no-same-owner");
    for e in excludes {
        cmd.arg(format!("--exclude={e}"));
    }
    let out = cmd.stdin(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "could not unpack voice files: {}",
            err.lines().next().unwrap_or("tar failed")
        )
        .into());
    }
    Ok(())
}

/// First path under `dir` (depth-first, at most 3 levels, no symlinks)
/// whose file name satisfies `pred`.
fn find(dir: &Path, depth: usize, pred: &dyn Fn(&str, bool) -> bool) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in &entries {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if !ft.is_symlink() && pred(&name, ft.is_dir()) {
            return Some(e.path());
        }
    }
    if depth == 0 {
        return None;
    }
    for e in &entries {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(p) = find(&e.path(), depth - 1, pred) {
                return Some(p);
            }
        }
    }
    None
}

fn need(dir: &Path, what: &str, pred: &dyn Fn(&str, bool) -> bool) -> Result<PathBuf> {
    find(dir, 3, pred).ok_or_else(|| format!("the download has no {what}").into())
}

/// Move `from` into place as `to`, replacing any previous version.
fn swap_into_place(root: &Path, from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        let old = root.join(format!(".old-{}", new_id()));
        std::fs::rename(to, &old)?;
        std::fs::rename(from, to)?;
        let _ = std::fs::remove_dir_all(old);
    } else {
        std::fs::rename(from, to)?;
    }
    Ok(())
}

fn install_runtime(root: &Path, staging: &Path) -> Result<bool> {
    let a = runtime_artifact()?;
    let dir = runtime_dir(root);
    if read_version(&dir).as_deref() == Some(a.id)
        && tts_bin(root).is_file()
        && stt_bin(root).is_file()
    {
        return Ok(false);
    }
    let archive = download(a, staging)?;
    let x = staging.join("runtime");
    unpack(&archive, &x, &[])?;
    let _ = std::fs::remove_file(&archive);
    let tts = need(&x, "speech program", &|n, d| {
        !d && n == "sherpa-onnx-offline-tts"
    })?;
    let stt = need(&x, "transcription program", &|n, d| {
        !d && n == "sherpa-onnx-offline"
    })?;
    let lib = need(&x, "runtime library", &|n, d| {
        !d && n.starts_with("libonnxruntime.so")
    })?;
    let new = staging.join("sherpa-onnx");
    std::fs::create_dir_all(new.join("bin"))?;
    std::fs::create_dir_all(new.join("lib"))?;
    std::fs::rename(&tts, new.join("bin/sherpa-onnx-offline-tts"))?;
    std::fs::rename(&stt, new.join("bin/sherpa-onnx-offline"))?;
    // The programs find their libraries through `$ORIGIN/../lib`.
    for e in std::fs::read_dir(lib.parent().unwrap_or(&x))?.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.contains(".so") && e.file_type().is_ok_and(|t| t.is_file()) {
            std::fs::rename(e.path(), new.join("lib").join(&n))?;
        }
    }
    std::fs::write(new.join("VERSION"), a.id)?;
    let _ = std::fs::remove_dir_all(&x);
    swap_into_place(root, &new, &dir)?;
    Ok(true)
}

fn install_model(root: &Path, c: Component, staging: &Path) -> Result<bool> {
    let a = c.model();
    let dir = component_dir(root, c);
    if read_version(&dir).as_deref() == Some(a.id) {
        return Ok(false);
    }
    let archive = download(a, staging)?;
    let x = staging.join("model");
    let excludes: &[&str] = match c {
        // Keep only the int8 Whisper files (about 160 MB instead of 450).
        Component::Stt => &["*-encoder.onnx", "*-decoder.onnx", "*test_wavs*"],
        Component::Tts => &[],
    };
    unpack(&archive, &x, excludes)?;
    let _ = std::fs::remove_file(&archive);
    let new = staging.join(c.name());
    std::fs::create_dir_all(&new)?;
    match c {
        Component::Tts => {
            let model = need(&x, "voice model", &|n, d| !d && n.ends_with(".onnx"))?;
            let tokens = need(&x, "voice tokens", &|n, d| !d && n == "tokens.txt")?;
            let data = need(&x, "voice data", &|n, d| d && n == "espeak-ng-data")?;
            std::fs::rename(model, new.join("model.onnx"))?;
            std::fs::rename(tokens, new.join("tokens.txt"))?;
            std::fs::rename(data, new.join("espeak-ng-data"))?;
            if let Some(card) = find(&x, 3, &|n, d| !d && n == "MODEL_CARD") {
                std::fs::rename(card, new.join("MODEL_CARD"))?;
            }
        }
        Component::Stt => {
            let enc = need(&x, "Whisper encoder", &|n, d| {
                !d && n.ends_with("-encoder.int8.onnx")
            })?;
            let dec = need(&x, "Whisper decoder", &|n, d| {
                !d && n.ends_with("-decoder.int8.onnx")
            })?;
            let tokens = need(&x, "Whisper tokens", &|n, d| {
                !d && n.ends_with("tokens.txt")
            })?;
            std::fs::rename(enc, new.join("encoder.int8.onnx"))?;
            std::fs::rename(dec, new.join("decoder.int8.onnx"))?;
            std::fs::rename(tokens, new.join("tokens.txt"))?;
        }
    }
    std::fs::write(new.join("VERSION"), a.id)?;
    let _ = std::fs::remove_dir_all(&x);
    swap_into_place(root, &new, &dir)?;
    Ok(true)
}

/// Install (or update to the pinned version) a voice component and the
/// runtime it needs (blocking). Idempotent: nothing is downloaded when
/// the pinned versions are already in place.
pub fn install(root: &Path, c: Component) -> Result<crate::engines::Report> {
    std::fs::create_dir_all(root)?;
    // Both components share the runtime; one install at a time, also
    // across processes (the privileged helper runs as its own process).
    let _lock = lock(root)?;
    clean_leftovers(root);
    let staging = root.join(format!(".staging-{}", new_id()));
    std::fs::create_dir_all(&staging)?;
    let res = install_runtime(root, &staging).and_then(|rt| {
        let model = install_model(root, c, &staging)?;
        Ok(rt || model)
    });
    let _ = std::fs::remove_dir_all(&staging);
    let changed = res?;
    Ok(crate::engines::Report {
        name: c.name().into(),
        version: installed_version(root, c),
        path: Some(component_dir(root, c).to_string_lossy().into_owned()),
        changed,
    })
}

/// Remove staging folders an interrupted install left behind (call with
/// the lock held).
fn clean_leftovers(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if (n.starts_with(".staging-") || n.starts_with(".old-"))
            && e.file_type().is_ok_and(|t| t.is_dir())
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Exclusive lock on `<root>/.voice.lock`, released when dropped.
fn lock(root: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".voice.lock"))?;
    // SAFETY: flock on a file descriptor we own; blocks until it is ours.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(f)
}

/// Remove a component; the runtime goes too once nothing uses it.
pub fn uninstall(root: &Path, c: Component) -> Result<()> {
    let _lock = lock(root)?;
    let dir = component_dir(root, c);
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    if Component::ALL
        .iter()
        .all(|o| !component_dir(root, *o).exists())
        && runtime_dir(root).exists()
    {
        std::fs::remove_dir_all(runtime_dir(root))?;
    }
    Ok(())
}

// -------------------------------------------------------------------- run

async fn run(program: &Path, args: &[String]) -> Result<(String, String)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(RUN_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "voice processing timed out")??;
    if !out.status.success() {
        // stderr may echo the text; never log it.
        return Err(format!("voice program exited with {}", out.status).into());
    }
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Speak `text` into a WAV file at `out` (16-bit mono).
pub async fn synthesize(root: &Path, text: &str, out: &Path) -> Result<()> {
    if !installed(root, Component::Tts) {
        return Err("voice replies are not set up on this server".into());
    }
    let m = component_dir(root, Component::Tts);
    let args = vec![
        format!("--vits-model={}", m.join("model.onnx").display()),
        format!("--vits-tokens={}", m.join("tokens.txt").display()),
        format!("--vits-data-dir={}", m.join("espeak-ng-data").display()),
        THREADS.into(),
        format!("--output-filename={}", out.display()),
        // Everything after `--` is the text, even if it starts with `-`.
        "--".into(),
        text.to_string(),
    ];
    run(&tts_bin(root), &args).await?;
    if !out.is_file() {
        return Err("the voice program wrote no audio".into());
    }
    Ok(())
}

/// Transcribe a WAV file; returns the text (possibly empty).
pub async fn transcribe(root: &Path, wav: &Path) -> Result<String> {
    if !installed(root, Component::Stt) {
        return Err("voice input is not set up on this server".into());
    }
    let m = component_dir(root, Component::Stt);
    let args = vec![
        format!(
            "--whisper-encoder={}",
            m.join("encoder.int8.onnx").display()
        ),
        format!(
            "--whisper-decoder={}",
            m.join("decoder.int8.onnx").display()
        ),
        format!("--tokens={}", m.join("tokens.txt").display()),
        THREADS.into(),
        "--".into(),
        wav.to_string_lossy().into_owned(),
    ];
    let (stdout, stderr) = run(&stt_bin(root), &args).await?;
    parse_transcript(&stdout)
        .or_else(|| parse_transcript(&stderr))
        .ok_or_else(|| "the transcription program gave no result".into())
}

/// The `"text"` of the JSON result line `sherpa-onnx-offline` prints.
pub fn parse_transcript(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v["text"].as_str().map(|t| t.trim().to_string()))
}

// ------------------------------------------------------------------- audio

/// Basic facts about a PCM WAV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits: u16,
    pub duration_ms: u64,
}

/// Parse a RIFF/WAVE header (PCM only). Used to validate uploads and to
/// report a voice note's length.
pub fn wav_info(bytes: &[u8]) -> Option<WavInfo> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let u16_at = |i: usize| Some(u16::from_le_bytes(bytes.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?));
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(pos + 4)? as usize;
        let body = pos + 8;
        if id == b"fmt " {
            fmt = Some((
                u16_at(body)?,
                u16_at(body + 2)?,
                u32_at(body + 4)?,
                u16_at(body + 14)?,
            ));
        } else if id == b"data" {
            let (format, channels, sample_rate, bits) = fmt?;
            // 1 = PCM, 0xFFFE = extensible (PCM in practice for 16-bit).
            if !(format == 1 || format == 0xFFFE) || channels == 0 || sample_rate == 0 {
                return None;
            }
            if bits != 16 {
                return None;
            }
            let data_len = len.min(bytes.len() - body) as u64;
            let per_sec = sample_rate as u64 * channels as u64 * (bits as u64 / 8);
            return Some(WavInfo {
                channels,
                sample_rate,
                bits,
                duration_ms: data_len * 1000 / per_sec.max(1),
            });
        }
        pos = body + len + (len & 1);
    }
    None
}

// ------------------------------------------------------------------ speech

/// Turn a Markdown reply into text worth hearing: no code, URLs, table
/// pipes, or formatting marks; capped at [`MAX_SPEECH_CHARS`] at a
/// sentence boundary.
pub fn speech_text(md: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut said_code = false;
    for raw in md.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            if !in_code && !said_code {
                out.push("I put the code in the text.".into());
                said_code = true;
            }
            in_code = !in_code;
            continue;
        }
        if in_code || line.is_empty() {
            continue;
        }
        // Table separator rows carry no words.
        if line.starts_with('|') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
            continue;
        }
        let mut l = line.trim_start_matches('#').trim_start_matches('>').trim();
        for marker in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
            if let Some(rest) = l.strip_prefix(marker) {
                l = rest;
            }
        }
        // Numbered list items: "1. ", "12) "
        let digits = l.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits < 4 {
            let rest = &l[digits..];
            if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
                l = r;
            }
        }
        let mut s = inline_text(l);
        if l.starts_with('|') {
            s = s
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
        }
        let s = s.trim().to_string();
        if s.is_empty() {
            continue;
        }
        // Lines become sentences so list items are not run together.
        let ends = s
            .chars()
            .last()
            .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':' | ';' | ','));
        out.push(if ends { s } else { format!("{s}.") });
    }
    let text = out.join(" ");
    let text = text.trim_start_matches(|c: char| c == '-' || c.is_whitespace());
    cap_sentences(text, MAX_SPEECH_CHARS)
}

/// Inline Markdown to plain words: links keep their label, images and bare
/// URLs are dropped, emphasis and code marks removed.
fn inline_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // ![alt](url) -> nothing; [label](url) -> label
        if (c == '[' || (c == '!' && chars.get(i + 1) == Some(&'[')))
            && let Some((label, next)) = link_at(&chars, if c == '!' { i + 1 } else { i })
        {
            if c != '!' {
                out.push_str(&label);
            }
            i = next;
            continue;
        }
        if c == '`' || c == '*' || (c == '_' && word_edge(&chars, i)) || c == '~' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Bare URLs are noise when read aloud.
    out.split_whitespace()
        .map(|w| {
            if w.starts_with("http://") || w.starts_with("https://") || w.starts_with("www.") {
                "(link)"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn word_edge(chars: &[char], i: usize) -> bool {
    let before = i.checked_sub(1).and_then(|j| chars.get(j));
    let after = chars.get(i + 1);
    !(before.is_some_and(|c| c.is_alphanumeric()) && after.is_some_and(|c| c.is_alphanumeric()))
}

/// `[label](target)` starting at `start` (which holds '['): the label and
/// the index after ')'.
fn link_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let close = (start + 1..chars.len()).find(|&j| chars[j] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|&j| chars[j] == ')')?;
    Some((chars[start + 1..close].iter().collect(), end + 1))
}

fn cap_sentences(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let end = cut
        .char_indices()
        .filter(|(i, c)| matches!(c, '.' | '!' | '?') && cut[i + c.len_utf8()..].starts_with(' '))
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .filter(|&e| e > max / 3)
        .unwrap_or_else(|| cut.rfind(' ').unwrap_or(cut.len()));
    format!("{} The rest is in the text.", cut[..end].trim_end())
}

/// Remove cached voice notes older than `max_age` (run daily).
pub fn prune_cache(dir: &Path, max_age: Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_text_reads_like_speech() {
        let md = "# Plan\n\nHere's **the plan** for _Friday_:\n\n- Book [the table](https://x.example/a)\n\
                  - Call `Sam`\n1. Leave at 6\n\n```js\nconsole.log(1)\n```\n\n\
                  | Day | Time |\n|---|---|\n| Fri | 6pm |\n\nSee https://example.com/x ![pic](https://x/y.png) snake_case stays.";
        let s = speech_text(md);
        assert_eq!(
            s,
            "Plan. Here's the plan for Friday: Book the table. Call Sam. Leave at 6. \
             I put the code in the text. Day, Time. Fri, 6pm. See (link) snake_case stays."
        );
    }

    #[test]
    fn speech_text_is_capped_at_a_sentence() {
        let long = "This is one sentence. ".repeat(200);
        let s = speech_text(&long);
        assert!(s.chars().count() <= MAX_SPEECH_CHARS + 40, "{}", s.len());
        assert!(s.ends_with("sentence. The rest is in the text."));
        assert!(!speech_text("--flag style start").starts_with('-'));
        assert_eq!(
            speech_text("```\nonly code\n```"),
            "I put the code in the text."
        );
        assert_eq!(speech_text(""), "");
    }

    fn wav(channels: u16, rate: u32, bits: u16, data: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&((36 + data) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        v.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data as u32).to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, data));
        v
    }

    #[test]
    fn wav_headers() {
        let w = wav_info(&wav(1, 16000, 16, 32000)).unwrap();
        assert_eq!((w.channels, w.sample_rate, w.duration_ms), (1, 16000, 1000));
        assert_eq!(
            wav_info(&wav(2, 22050, 16, 88200)).unwrap().duration_ms,
            1000
        );
        assert!(wav_info(&wav(1, 16000, 8, 100)).is_none());
        assert!(wav_info(b"RIFF\0\0\0\0WAVE").is_none());
        assert!(wav_info(b"\x89PNG....").is_none());
    }

    #[test]
    fn transcript_parsing() {
        let out = "noise\n{\"lang\": \"\", \"text\": \" Hello there. \", \"tokens\": []}\n";
        assert_eq!(parse_transcript(out).as_deref(), Some("Hello there."));
        assert_eq!(parse_transcript("no json"), None);
    }

    #[test]
    fn artifacts_are_pinned_official_downloads() {
        for a in [&RUNTIME_X64, &RUNTIME_ARM64, &TTS_MODEL, &STT_MODEL] {
            assert!(a.url.starts_with(RELEASES), "{}", a.url);
            assert_eq!(a.sha256.len(), 64);
            assert!(a.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(a.size > 1_000_000);
        }
        assert_eq!(Component::Tts.name(), "tts");
        assert_eq!(
            crate::engines::source("stt").and_then(|e| e.voice()),
            Some(Component::Stt)
        );
    }

    /// Build a tar.bz2 shaped like the official runtime and models with
    /// fake programs, and check the install layout and verification.
    #[test]
    fn install_layout_and_verification() {
        if missing_tools().is_some() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let rt = src.join("sherpa-onnx-v9-linux/bin");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::create_dir_all(src.join("sherpa-onnx-v9-linux/lib")).unwrap();
        std::fs::write(rt.join("sherpa-onnx-offline-tts"), "#!/bin/sh\n").unwrap();
        std::fs::write(rt.join("sherpa-onnx-offline"), "#!/bin/sh\n").unwrap();
        std::fs::write(src.join("sherpa-onnx-v9-linux/lib/libonnxruntime.so"), "x").unwrap();
        let archive = tmp.path().join("rt.tar.bz2");
        let st = std::process::Command::new("tar")
            .arg("-cjf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .status()
            .unwrap();
        assert!(st.success());
        let bytes = std::fs::read(&archive).unwrap();
        let (hash, n) = copy_hashed(&bytes[..], std::io::sink(), u64::MAX).unwrap();
        let good = Artifact {
            id: "9",
            url: "",
            sha256: Box::leak(hash.into_boxed_str()),
            size: n,
        };
        let bad = Artifact {
            sha256: "00",
            ..good
        };
        let f = tmp.path().join("copy.tar.bz2");
        std::fs::write(&f, &bytes).unwrap();
        let copied = copy_hashed(&bytes[..], std::io::sink(), u64::MAX);
        assert!(verify_download(&f, copied, &bad).is_err());
        assert!(!f.exists(), "a bad download is deleted");
        std::fs::write(&f, &bytes).unwrap();
        let copied = copy_hashed(&bytes[..], std::io::sink(), u64::MAX);
        verify_download(&f, copied, &good).unwrap();
        let x = tmp.path().join("x");
        unpack(&f, &x, &[]).unwrap();
        assert!(
            find(&x, 3, &|n, d| !d && n == "sherpa-onnx-offline-tts").is_some(),
            "found nested program"
        );
        let root = tmp.path().join("engines");
        std::fs::create_dir_all(component_dir(&root, Component::Tts)).unwrap();
        std::fs::write(component_dir(&root, Component::Tts).join("VERSION"), "v").unwrap();
        assert!(!installed(&root, Component::Tts), "runtime missing");
        uninstall(&root, Component::Tts).unwrap();
        assert!(!component_dir(&root, Component::Tts).exists());
    }
}
