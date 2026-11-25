//! SearXNG-backed `web_search` (PLAN §6, decision #2).
//!
//! The desktop companion self-hosts a SearXNG instance (Docker) and the gateway
//! queries it over the LAN/loopback. The whole point of the design is that an
//! unreachable instance is *not* an error: it degrades to an explicit
//! `no_network` outcome so the model says "I'm offline" instead of inventing an
//! answer. Only a reachable-but-failing search is surfaced as an error.
//!
//! Like [`crate::journal::EngineSummarizer`], this uses a blocking `reqwest`
//! client built lazily via [`std::sync::OnceLock`]. `WebSearcher::search` is
//! only ever called from `spawn_blocking` (the tool endpoint), never on an
//! async worker, so the blocking client never trips its "runtime already
//! running" panic.

use std::time::Duration;

use etern_core::{WebSearchOutcome, WebSearcher, WebResult};
use serde_json::Value;

/// How long to wait for SearXNG before declaring the network unavailable. A
/// self-hosted instance answers in well under a second; a long timeout would
/// only stall the model's turn.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(8);

/// `web_search` backend backed by a self-hosted SearXNG instance.
pub struct SearxngSearcher {
    /// Built lazily on first use inside `spawn_blocking` (see module docs).
    client: std::sync::OnceLock<reqwest::blocking::Client>,
    base: String,
}

impl SearxngSearcher {
    pub fn new(base: &str) -> Self {
        Self {
            client: std::sync::OnceLock::new(),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.client.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .build()
                .expect("blocking reqwest client")
        })
    }
}

impl WebSearcher for SearxngSearcher {
    fn search(&self, query: &str, max_results: usize) -> WebSearchOutcome {
        let resp = self
            .client()
            .get(format!("{}/search", self.base))
            .query(&[("q", query), ("format", "json")])
            .send();

        let body = match resp {
            Ok(r) => match r.json::<Value>() {
                Ok(v) => v,
                // A reachable instance that returns non-JSON (e.g. JSON is
                // disabled in settings.yml) is a configuration fault, not an
                // offline condition.
                Err(e) => return WebSearchOutcome::Error(format!("bad SearXNG response: {e}")),
            },
            // Connection refused / DNS / timeout: treat as offline, which is
            // the graceful path PLAN §6 requires.
            Err(_) => return WebSearchOutcome::NoNetwork,
        };

        let results = body
            .get("results")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(parse_result)
                    .take(max_results)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        WebSearchOutcome::Results(results)
    }
}

/// Maps one SearXNG result object to a [`WebResult`], skipping entries with no
/// URL. SearXNG calls the snippet `content`.
fn parse_result(value: &Value) -> Option<WebResult> {
    let url = value.get("url").and_then(Value::as_str)?.to_string();
    Some(WebResult {
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        snippet: value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_result_maps_searxng_fields() {
        let v = serde_json::json!({
            "title": "Rust programming language",
            "url": "https://www.rust-lang.org/",
            "content": "A language empowering everyone to build reliable software."
        });
        let r = parse_result(&v).unwrap();
        assert_eq!(r.title, "Rust programming language");
        assert_eq!(r.url, "https://www.rust-lang.org/");
        assert_eq!(
            r.snippet,
            "A language empowering everyone to build reliable software."
        );
    }

    #[test]
    fn parse_result_requires_url() {
        let v = serde_json::json!({ "title": "no url", "content": "x" });
        assert!(parse_result(&v).is_none());
    }

    #[test]
    fn parse_result_tolerates_missing_fields() {
        let v = serde_json::json!({ "url": "https://example.com" });
        let r = parse_result(&v).unwrap();
        assert_eq!(r.title, "");
        assert_eq!(r.snippet, "");
        assert_eq!(r.url, "https://example.com");
    }
}
