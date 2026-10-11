//! Local copies of feed photos and favicons. Posts keep the original URLs
//! (the source's own images, never generated); the app shows the cached
//! copy so the phone does not contact third-party sites, and old posts keep
//! their pictures if the source moves them.

use super::http::{Fetcher, Mode, Request};
use std::path::{Path, PathBuf};

const MAX_IMAGE_BYTES: u64 = 6 * 1024 * 1024;
const MAX_ICON_BYTES: u64 = 256 * 1024;
/// Photos narrower than this are icons, avatars, or tracking pixels.
const MIN_PHOTO_WIDTH: u32 = 200;

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("feed-images")
}

/// Image kind from magic bytes: (mime, extension).
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if let Some(k) = crate::server::sniff_image(bytes) {
        return Some(k);
    }
    if bytes.len() > 12
        && &bytes[4..8] == b"ftyp"
        && (&bytes[8..12] == b"avif" || &bytes[8..12] == b"avis")
    {
        return Some(("image/avif", "avif"));
    }
    if bytes.starts_with(&[0, 0, 1, 0]) && bytes.len() > 6 {
        return Some(("image/x-icon", "ico"));
    }
    None
}

/// MIME type for a cached file name.
pub fn mime_for(name: &str) -> Option<&'static str> {
    Some(match name.rsplit('.').next()? {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

/// Cached file names are `<32 hex>.<ext>`; anything else is refused.
pub fn valid_name(name: &str) -> bool {
    let Some((stem, ext)) = name.split_once('.') else {
        return false;
    };
    stem.len() == 32
        && stem
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && mime_for(&format!("x.{ext}")).is_some()
}

/// Width and height from the image header (PNG, GIF, JPEG, WebP).
pub fn dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32);
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    if b.starts_with(b"\x89PNG") {
        return Some((be32(16)?, be32(20)?));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            i += 2 + be16(i + 2)? as usize;
        }
        return None;
    }
    if b.len() > 30 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let le24 = |i: usize| {
            Some(u32::from_le_bytes([
                *b.get(i)?,
                *b.get(i + 1)?,
                *b.get(i + 2)?,
                0,
            ]))
        };
        return match &b[12..16] {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            _ => None,
        };
    }
    None
}

fn file_name(url: &str, ext: &str) -> String {
    format!("{}.{ext}", &crate::util::sha256_hex(url)[..32])
}

/// Download `url` into the cache (robots.txt respected) and return the
/// cached file name. `icon` relaxes the size checks for favicons.
pub fn cache(fetcher: &Fetcher, data_dir: &Path, url: &str, icon: bool) -> Option<String> {
    let dir = dir(data_dir);
    // Already cached under any extension?
    let stem = &crate::util::sha256_hex(url)[..32];
    for ext in ["jpg", "png", "gif", "webp", "avif", "ico"] {
        let name = format!("{stem}.{ext}");
        if dir.join(&name).exists() {
            return Some(name);
        }
    }
    let req = Request::new(
        url,
        Mode::Crawl,
        "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8",
        if icon {
            MAX_ICON_BYTES
        } else {
            MAX_IMAGE_BYTES
        },
    );
    let res = fetcher.get(&req).ok()?;
    if res.body.len() < 100 {
        return None;
    }
    let (_, ext) = sniff(&res.body)?;
    if !icon {
        if ext == "ico" {
            return None;
        }
        if let Some((w, h)) = dimensions(&res.body)
            && (w < MIN_PHOTO_WIDTH || h < MIN_PHOTO_WIDTH / 2)
        {
            return None;
        }
    }
    crate::config::ensure_private_dir(&dir).ok()?;
    let name = file_name(url, ext);
    crate::config::write_private_file(&dir.join(&name), &res.body).ok()?;
    Some(name)
}

/// Delete cached files no post references, once they are a day old.
pub fn prune(data_dir: &Path, referenced: &std::collections::HashSet<String>) -> usize {
    let Ok(rd) = std::fs::read_dir(dir(data_dir)) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    let mut removed = 0;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if referenced.contains(&name) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if old && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_mimes() {
        assert!(valid_name("0123456789abcdef0123456789abcdef.jpg"));
        assert!(!valid_name("0123456789ABCDEF0123456789abcdef.jpg"));
        assert!(!valid_name("../../etc/passwd"));
        assert!(!valid_name("0123456789abcdef0123456789abcdef.svg"));
        assert!(!valid_name("0123456789abcdef0123456789abcde.jpg"));
        assert_eq!(mime_for("x.webp"), Some("image/webp"));
        assert_eq!(file_name("https://a.example/x.jpg", "jpg").len(), 36);
    }

    #[test]
    fn header_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(dimensions(&png), Some((640, 480)));
        let gif = b"GIF89a\x01\x00\x01\x00rest".to_vec();
        assert_eq!(dimensions(&gif), Some((1, 1)));
        // JPEG: SOI, APP0 (length 16), SOF0 with 300x200.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpg.extend_from_slice(&[0u8; 14]);
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xC8, 0x01, 0x2C, 0x03]);
        jpg.extend_from_slice(&[0u8; 12]);
        assert_eq!(dimensions(&jpg), Some((300, 200)));
        assert_eq!(dimensions(b"nope"), None);
        let mut avif = vec![0, 0, 0, 0x1c];
        avif.extend_from_slice(b"ftypavif");
        avif.extend_from_slice(&[0; 8]);
        assert_eq!(sniff(&avif), Some(("image/avif", "avif")));
    }
}
