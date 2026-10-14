//! Deterministic memory write boundary. No model calls or credential-file reads.

/// Drop private spans (including nested or unclosed spans), PEM keys and
/// credential-bearing lines. Known token formats are hidden even unlabelled.
pub fn strip_private(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut public = String::new();
    let mut pos = 0;
    let mut depth = 0usize;
    while pos < text.len() {
        let open = lower[pos..].find("<private>").map(|i| (pos + i, true));
        let close = lower[pos..].find("</private>").map(|i| (pos + i, false));
        let next = match (open, close) {
            (Some(a), Some(b)) => Some(if a.0 < b.0 { a } else { b }),
            (a, b) => a.or(b),
        };
        let Some((at, opening)) = next else {
            if depth == 0 {
                public.push_str(&text[pos..]);
            }
            break;
        };
        if depth == 0 {
            public.push_str(&text[pos..at]);
        }
        if opening {
            depth += 1;
        } else {
            depth = depth.saturating_sub(1);
        }
        pos = at + if opening { 9 } else { 10 };
    }
    public
}

pub fn scrub_document(text: &str) -> String {
    let public = strip_private(text);
    let mut key = false;
    let mut lines = Vec::new();
    let source_lines: Vec<&str> = public.lines().collect();
    for (index, line) in source_lines.iter().enumerate() {
        let low = line.to_ascii_lowercase();
        if low.contains("-----begin") && low.contains("private key-----") {
            key = true;
        }
        if key {
            if low.contains("-----end") && low.contains("private key-----") {
                key = false;
            }
            continue;
        }
        let mut credential = low.contains("bearer ");
        for label in [
            "password",
            "passwd",
            "api_key",
            "apikey",
            "api-key",
            "api key",
            "access_token",
            "accesstoken",
            "refresh_token",
            "refreshtoken",
            "client_secret",
            "clientsecret",
            "secret",
            "token",
            "authorization",
        ] {
            for (at, _) in low.match_indices(label) {
                let tail = low[at + label.len()..].trim_start_matches(|c: char| {
                    c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '*' | '_')
                });
                if tail.starts_with([':', '=']) {
                    credential = true;
                    let value = tail[1..].trim();
                    // Arbitrary multiline values cannot be delimited safely
                    // as prose. Reject the document, including JSON and YAML
                    // blocks, rather than keep a credential's continuation.
                    let continued = source_lines[index + 1..]
                        .iter()
                        .find(|next| !next.trim().is_empty() && !next.trim_start().starts_with('#'))
                        .is_some_and(|next| {
                            next.len() - next.trim_start().len()
                                > line.len() - line.trim_start().len()
                        });
                    let open_quote = value.starts_with('"')
                        && serde_json::Deserializer::from_str(value)
                            .into_iter::<serde_json::Value>()
                            .next()
                            .is_none_or(|v| v.is_err());
                    if value.is_empty()
                        || value.starts_with(['|', '>', '[', '{', '\''])
                        || open_quote
                        || continued
                    {
                        return String::new();
                    }
                }
            }
        }
        if credential {
            continue;
        }
        let mut spans = Vec::new();
        for prefix in [
            "sk-",
            "ghp_",
            "github_pat_",
            "gho_",
            "xoxb-",
            "xoxp-",
            "AIza",
            "AKIA",
        ] {
            for (at, _) in line.match_indices(prefix) {
                let length = line[at..]
                    .bytes()
                    .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                    .count();
                if length >= 16 {
                    spans.push((at, at + length));
                }
            }
        }
        spans.sort_unstable();
        let mut kept = String::new();
        let mut start = 0;
        for (at, end) in spans {
            if at >= start {
                kept.push_str(&line[start..at]);
            }
            start = start.max(end);
        }
        kept.push_str(&line[start..]);
        lines.push(kept);
    }
    super::markdown::normalize(&lines.join("\n"))
}

pub fn scrub(text: &str) -> Result<String, String> {
    let out = scrub_document(text);
    if out.is_empty() {
        return Err(
            "I couldn't save this note safely. Please leave out private text and credentials."
                .into(),
        );
    }
    if out.chars().count() > 16_000 {
        return Err("I can save notes up to 16,000 characters. Please shorten this one.".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn document_scrub_preserves_public_markdown_and_excludes_cross_note_spans() {
        let text = "# Memory\n- Likes tea\n<private>\n- synthetic-canary\n\n- still-private\n</private>\n- Likes walking\n-----BEGIN PRIVATE KEY-----\n\n- key-canary\n-----END PRIVATE KEY-----";
        assert_eq!(
            scrub_document(text),
            "# Memory\n- Likes tea\n\n- Likes walking"
        );
        assert_eq!(
            scrub_document("- Uses a password manager\n  - every day"),
            "- Uses a password manager\n  - every day"
        );
    }

    #[test]
    fn strips_private_and_credentials_without_reading_credentials() {
        assert_eq!(scrub("Likes tea <PRIVATE>secret <private>nested</private> hidden</PRIVATE> daily\npassword: synthetic-canary\napi_key=synthetic-canary\n-----BEGIN PRIVATE KEY-----\ncanary\n-----END PRIVATE KEY-----\nUses sk-synthetic0000000000000000 notebooks").unwrap(), "Likes tea  daily\nUses  notebooks");
        assert_eq!(scrub("Public <private>unclosed").unwrap(), "Public");
        assert!(scrub("<private>hidden</private>").is_err());
        assert!(scrub("token=synthetic").is_err());
    }
}
