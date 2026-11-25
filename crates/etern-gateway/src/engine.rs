//! Outbound calls to the llama.cpp-compatible server.
//!
//! Kept deliberately narrow: a health probe, a props fetch, a tokenize probe,
//! and a raw request passthrough. The gateway must not reinterpret inference
//! payloads — the whole point is that it survives a model swap and works with
//! whatever build is loaded.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Response, StatusCode};
use axum::response::IntoResponse;
use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use crate::config::GatewayConfig;
use crate::health::Probe;

/// Long enough for a full 262k-context generation; the gateway must not be the
/// reason a long turn fails.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3_600);
/// A health probe has to be quick or it stalls the state machine.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Headers that must not cross a proxy boundary. The client's `Authorization`
/// is dropped because the engine on this host runs with `api_key: false`.
const SKIP_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

const SKIP_RESPONSE_HEADERS: &[&str] =
    &["transfer-encoding", "connection", "content-length", "keep-alive"];

#[derive(Debug, Clone)]
pub struct Engine {
    client: Client,
    probe_client: Client,
    base: String,
}

impl Engine {
    pub fn new(cfg: &GatewayConfig) -> Self {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(8)
            .build()
            .expect("reqwest client");
        let probe_client =
            Client::builder().timeout(PROBE_TIMEOUT).build().expect("probe client");
        Self { client, probe_client, base: cfg.engine_base.clone() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Classifies the engine without ever leaking a transport detail to a client.
    pub async fn probe_health(&self) -> Probe {
        match self.probe_client.get(self.url("/health")).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                Probe::from_status(status, &body)
            }
            Err(_) => Probe::Unreachable,
        }
    }

    /// Fetches everything the fingerprint and status need in one pass.
    ///
    /// `/slots` is optional — an older build may not expose it — but `/props` is
    /// required, because without a chat template there is nothing to fingerprint.
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let props = self.get_json("/props").await?;
        Some(Snapshot {
            props,
            health: self.get_json("/health").await,
            slots: self.get_json("/slots").await,
            has_tokenize: self.tokenize_available().await,
        })
    }

    /// llama.cpp returns 404 (or 400) on builds without `/tokenize`, so any
    /// success-shaped answer means the route exists. The live Strata build 404s,
    /// which downgrades the gateway to estimated token counts.
    async fn tokenize_available(&self) -> bool {
        let body = json!({ "content": "hello", "add_special": false });
        match self.probe_client.post(self.url("/tokenize")).json(&body).send().await {
            Ok(resp) => resp.status().as_u16() < 400,
            Err(_) => false,
        }
    }

    pub async fn get_json(&self, path: &str) -> Option<Value> {
        let resp = self.probe_client.get(self.url(path)).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// Builds a passthrough request. `identity` encoding keeps the SSE stream
    /// uncompressed so the phone can incrementally parse it.
    pub fn request(&self, method: Method, path: &str, body: Vec<u8>) -> RequestBuilder {
        self.client
            .request(method, self.url(path))
            .body(body)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
    }
}

/// Everything one engine discovery pass learned.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub props: Value,
    pub health: Option<Value>,
    pub slots: Option<Value>,
    pub has_tokenize: bool,
}

/// Filters the request headers the gateway is willing to forward upstream.
pub fn forwardable_request_headers(headers: &HeaderMap) -> Vec<(HeaderName, String)> {
    headers
        .iter()
        .filter(|(name, _)| !SKIP_REQUEST_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| value.to_str().ok().map(|v| (name.clone(), v.to_string())))
        .collect()
}

/// Maps an engine transport failure onto a status the phone can act on.
///
/// `code: "loading"` is the important part: it tells a client "wait, do not
/// fall back to your edge model", which is precisely the fork being prevented.
pub fn engine_unreachable() -> axum::response::Response {
    let body = json!({
        "error": {
            "message": "engine not reachable; the desktop may be swapping models",
            "type": "engine_unavailable",
            "code": "loading"
        }
    });
    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response()
}

/// Streams an upstream response straight out, preserving status and headers.
pub fn relay(resp: reqwest::Response) -> axum::response::Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = resp
        .headers()
        .iter()
        .filter(|(name, _)| !SKIP_RESPONSE_HEADERS.contains(&name.as_str()))
        .filter_map(|(name, value)| {
            HeaderName::from_bytes(name.as_str().as_bytes()).ok().map(|n| (n, value.clone()))
        })
        .collect::<Vec<(HeaderName, reqwest::header::HeaderValue)>>();

    let stream = resp.bytes_stream();
    let body = Body::from_stream(futures_util::StreamExt::map(stream, |chunk| {
        chunk.map_err(std::io::Error::other)
    }));

    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.body(body).unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "malformed upstream response").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn cfg_with_base(base: &str) -> GatewayConfig {
        GatewayConfig { engine_base: base.to_string(), ..Default::default() }
    }

    #[test]
    fn base_url_is_used_verbatim() {
        let engine = Engine::new(&cfg_with_base("http://127.0.0.1:8080"));
        assert_eq!(engine.base(), "http://127.0.0.1:8080");
    }

    #[test]
    fn authorization_never_reaches_the_engine() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("x-client-id", HeaderValue::from_static("pixel"));

        let names: Vec<String> =
            forwardable_request_headers(&headers).into_iter().map(|(n, _)| n.to_string()).collect();
        assert!(!names.iter().any(|n| n == "authorization"), "the engine has no key");
        assert!(names.iter().any(|n| n == "content-type"));
        assert!(names.iter().any(|n| n == "x-client-id"), "client hints are useful upstream");
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("desktop:8090"));
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("content-length", HeaderValue::from_static("12"));
        assert!(forwardable_request_headers(&headers).is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_engine_is_reported_as_unreachable() {
        // Port 1 is never listening. The state machine turns this into
        // `loading` when the engine was recently ready, never a bare failure.
        let engine = Engine::new(&cfg_with_base("http://127.0.0.1:1"));
        assert_eq!(engine.probe_health().await, Probe::Unreachable);
    }

    #[tokio::test]
    async fn props_fetch_gives_up_rather_than_hanging() {
        let engine = Engine::new(&cfg_with_base("http://127.0.0.1:1"));
        assert!(engine.snapshot().await.is_none());
    }

    #[test]
    fn unreachable_maps_to_503_with_the_loading_code() {
        let resp = engine_unreachable();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
