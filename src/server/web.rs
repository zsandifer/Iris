//! The Iris phone app (installable PWA), embedded in the binary. The app is
//! plain ES modules plus vendored Preact and htm (see `web/vendor`), so
//! building Iris needs no Node.js toolchain. The old test console stays at
//! `/console` for debugging.

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use std::sync::OnceLock;

pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub body: &'static [u8],
}

macro_rules! assets {
    ($($path:literal => $mime:expr),* $(,)?) => {
        &[$(Asset {
            path: $path,
            mime: $mime,
            body: include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/web/", $path)),
        }),*]
    };
}

const JS: &str = "text/javascript; charset=utf-8";
const PNG: &str = "image/png";

/// Every file served from `web/` (a test checks the directory matches).
pub static ASSETS: &[Asset] = assets![
    "index.html" => "text/html; charset=utf-8",
    "app.css" => "text/css; charset=utf-8",
    "manifest.webmanifest" => "application/manifest+json",
    "sw.js" => JS,
    "icons/icon.svg" => "image/svg+xml",
    "icons/icon-192.png" => PNG,
    "icons/icon-512.png" => PNG,
    "icons/icon-maskable-512.png" => PNG,
    "icons/apple-touch-icon.png" => PNG,
    "js/api.js" => JS,
    "js/app.js" => JS,
    "js/cards.js" => JS,
    "js/chat.js" => JS,
    "js/computer.js" => JS,
    "js/connect.js" => JS,
    "js/feed.js" => JS,
    "js/h.js" => JS,
    "js/hub.js" => JS,
    "js/lists.js" => JS,
    "js/md.js" => JS,
    "js/photo.js" => JS,
    "js/photo-math.js" => JS,
    "js/push.js" => JS,
    "js/settings.js" => JS,
    "js/status.js" => JS,
    "js/store.js" => JS,
    "js/ui.js" => JS,
    "js/viewport.js" => JS,
    "vendor/hooks.js" => JS,
    "vendor/htm.js" => JS,
    "vendor/preact.js" => JS,
];

fn find(path: &str) -> Option<(usize, &'static Asset)> {
    ASSETS.iter().enumerate().find(|(_, a)| a.path == path)
}

/// Strong ETags (content hashes), computed once.
fn etags() -> &'static [String] {
    static TAGS: OnceLock<Vec<String>> = OnceLock::new();
    TAGS.get_or_init(|| {
        use sha2::{Digest, Sha256};
        ASSETS
            .iter()
            .map(|a| format!("\"{}\"", &crate::util::hex(&Sha256::digest(a.body))[..20]))
            .collect()
    })
}

/// A valid `Host` header value is used to allow the same-origin WebSocket
/// explicitly (older Safari does not match `ws:` against `'self'`).
fn ws_sources(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 255
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-.:[]".contains(c))
        });
    match host {
        Some(h) => format!(" ws://{h} wss://{h}"),
        None => String::new(),
    }
}

fn app_csp(headers: &HeaderMap) -> String {
    // Feed photos and favicons are the source sites' own images (https).
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' blob: data: https:; connect-src 'self'{}; manifest-src 'self'; \
         worker-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self'; \
         frame-ancestors 'none'",
        ws_sources(headers)
    )
}

/// Serve an embedded asset, `index.html` for `/`, or 404.
pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let Some((i, asset)) = find(path) else {
        return super::ApiError::not_found("route").into_response();
    };
    // Development: `IRIS_WEB_DIR=web iris serve` serves the files from disk
    // (only the listed paths) so app changes need no rebuild.
    if let Some(dir) = std::env::var_os("IRIS_WEB_DIR")
        && let Ok(body) = tokio::fs::read(std::path::Path::new(&dir).join(asset.path)).await
    {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if asset.mime.starts_with("text/html")
            && let Ok(v) = HeaderValue::from_str(&app_csp(&headers))
        {
            h.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        return (h, body).into_response();
    }
    let etag = &etags()[i];
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, HeaderValue::from_str(etag).expect("hex etag"));
    // Revalidate every time: tiny files, and updates show up at once.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if asset.mime.starts_with("text/html") {
        if let Ok(v) = HeaderValue::from_str(&app_csp(&headers)) {
            h.insert(header::CONTENT_SECURITY_POLICY, v);
        }
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    if asset.path == "sw.js" {
        h.insert("service-worker-allowed", HeaderValue::from_static("/"));
    }
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    (h, asset.body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_under(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                files_under(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }

    #[test]
    fn every_web_file_is_embedded_and_cached_by_the_service_worker() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
        let mut files = Vec::new();
        files_under(&base, &base, &mut files);
        files.retain(|f| !f.ends_with(".md"));
        files.sort();
        let mut listed: Vec<String> = ASSETS.iter().map(|a| a.path.to_string()).collect();
        listed.sort();
        assert_eq!(files, listed, "web/ and ASSETS differ");
        let sw = std::str::from_utf8(find("sw.js").unwrap().1.body).unwrap();
        for a in ASSETS
            .iter()
            .filter(|a| a.path.ends_with(".js") && a.path != "sw.js")
        {
            assert!(
                sw.contains(&format!("'/{}'", a.path)),
                "sw.js SHELL misses {}",
                a.path
            );
        }
    }

    #[test]
    fn modules_only_import_relative_paths() {
        // No bare specifiers: the browser loads these files as they are.
        for a in ASSETS.iter().filter(|a| a.path.ends_with(".js")) {
            let src = std::str::from_utf8(a.body).unwrap();
            for marker in ["from'", "from\"", "from '", "from \""] {
                for (i, _) in src.match_indices(marker) {
                    let rest = &src[i + marker.len()..];
                    assert!(
                        rest.starts_with("./") || rest.starts_with("../"),
                        "{} imports a bare module: {}",
                        a.path,
                        &rest[..rest.len().min(30)]
                    );
                }
            }
        }
    }

    #[test]
    fn csp_allows_same_host_websocket_only_for_sane_hosts() {
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("iris.example.ts.net"),
        );
        assert!(
            app_csp(&h)
                .contains("connect-src 'self' ws://iris.example.ts.net wss://iris.example.ts.net;")
        );
        h.insert(
            header::HOST,
            HeaderValue::from_static("evil.example; script-src *"),
        );
        assert!(app_csp(&h).contains("connect-src 'self';"));
    }
}
