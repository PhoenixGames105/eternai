//! HTTP-backed `fetch_page` (PLAN §6).
//!
//! Fetches a page with a blocking `reqwest` client (built lazily, same pattern
//! as [`crate::websearch::SearxngSearcher`]) and reduces the HTML to readable
//! text: script/style/nav blocks are dropped, tags stripped, entities decoded,
//! and whitespace collapsed. An unreachable network is *not* an error — it
//! degrades to an explicit `no_network` outcome so the model says "I'm
//! offline" instead of inventing page content.

use std::time::Duration;

use etern_core::{PageFetchOutcome, PageFetcher};

/// How long to wait for a page before giving up.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on the downloaded body; anything larger is truncated before
/// parsing so a huge page cannot blow up memory or the model's context.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// `fetch_page` backend backed by a real HTTP client.
pub struct HttpFetcher {
    /// Built lazily on first use inside `spawn_blocking` (see module docs).
    client: std::sync::OnceLock<reqwest::blocking::Client>,
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetcher {
    pub fn new() -> Self {
        Self { client: std::sync::OnceLock::new() }
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.client.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent("eternai/0.1 (+companion; text extraction)")
                .build()
                .expect("blocking reqwest client")
        })
    }
}

impl PageFetcher for HttpFetcher {
    fn fetch(&self, url: &str) -> PageFetchOutcome {
        let resp = self.client().get(url).send();
        let body = match resp {
            Ok(r) => {
                if !r.status().is_success() {
                    return PageFetchOutcome::Error(format!("HTTP {}", r.status()));
                }
                match r.bytes() {
                    Ok(b) => b,
                    Err(e) => return PageFetchOutcome::Error(format!("read body: {e}")),
                }
            }
            // Connection refused / DNS / timeout: offline, the graceful path.
            Err(_) => return PageFetchOutcome::NoNetwork,
        };

        let html = String::from_utf8_lossy(&body[..body.len().min(MAX_BODY_BYTES)]);
        let text = html_to_text(&html);
        if text.trim().is_empty() {
            return PageFetchOutcome::Error("page contained no readable text".into());
        }
        PageFetchOutcome::Text(text)
    }
}

/// Reduces HTML to readable text.
///
/// A small set of passes rather than a full parser: it drops the contents of
/// `script`, `style`, `noscript`, `template`, `svg` and `head` blocks, strips
/// all remaining tags, decodes the common entities, and collapses runs of
/// whitespace. Good enough for article text; not a DOM.
pub fn html_to_text(html: &str) -> String {
    let stripped = strip_skip_blocks(html);
    let no_tags = strip_tags(&stripped);
    decode_entities(&no_tags)
}

/// Removes the contents of non-content blocks (script/style/etc.) including
/// their tags.
fn strip_skip_blocks(html: &str) -> String {
    const BLOCKS: [&str; 6] = ["script", "style", "noscript", "template", "svg", "head"];
    let mut text = html.to_string();
    for name in BLOCKS {
        let lower = text.to_ascii_lowercase();
        let mut result = String::with_capacity(text.len());
        let mut rest = lower.as_str();
        let mut src = text.as_str();
        loop {
            // Find the next opening tag, making sure the tag name is a whole
            // word (`<head` must not match `<header`).
            let Some(open) = rest.find(&format!("<{name}")).filter(|&pos| {
                let after = pos + name.len() + 1;
                matches!(
                    rest.get(after..after + 1),
                    Some(">" | " " | "/" | "\t" | "\n")
                )
            }) else {
                result.push_str(src);
                break;
            };
            // Find the end of the opening tag.
            let Some(gt) = rest[open..].find('>') else {
                result.push_str(src);
                break;
            };
            let tag_end = open + gt;
            // Find the matching close tag.
            let close = format!("</{name}");
            let Some(close_pos) = rest[tag_end..].find(&close) else {
                // No close tag: drop the rest (the block runs to EOF).
                result.push_str(&src[..open]);
                break;
            };
            let after_close = tag_end + close_pos + close.len();
            result.push_str(&src[..open]);
            src = &src[after_close.min(src.len())..];
            rest = &rest[after_close.min(rest.len())..];
        }
        text = result;
    }
    text
}

/// Strips all remaining HTML tags.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Decodes the common HTML entities and collapses whitespace runs to single
/// spaces (newlines become spaces too — the model reads fine as one block).
fn decode_entities(text: &str) -> String {
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…");

    let mut out = String::with_capacity(decoded.len());
    let mut last_space = true; // leading whitespace is dropped
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_style_and_tags() {
        let html = r#"
            <html><head><title>T</title><style>body{color:red}</style></head>
            <body><script>var x = 1;</script>
            <h1>Hello World</h1>
            <p>This is a &nbsp;test &amp; demo.</p>
            </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello World"));
        assert!(text.contains("This is a test & demo."));
        assert!(!text.contains("var x"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn empty_page_yields_empty_text() {
        assert!(html_to_text("<html><body><script>x</script></body></html>").trim().is_empty());
    }

    #[test]
    fn fetcher_reports_no_network_for_unreachable() {
        let f = HttpFetcher::new();
        // A port that is almost certainly closed on loopback.
        let out = f.fetch("http://127.0.0.1:1/");
        assert!(
            matches!(out, PageFetchOutcome::NoNetwork | PageFetchOutcome::Error(_)),
            "got {out:?}"
        );
    }
}
