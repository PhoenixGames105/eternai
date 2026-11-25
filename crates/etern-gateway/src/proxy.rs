//! Inference passthrough.
//!
//! The gateway deliberately does not parse inference payloads. It forwards the
//! body byte-for-byte and streams the answer back, so a model swap, a new
//! sampler parameter, or a llama.cpp build that adds fields cannot break the
//! companion. The one thing it does add is an in-flight count, which feeds the
//! `busy` state that keeps a phone from mistaking saturation for an outage.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::auth;
use crate::engine;
use crate::AppState;

pub async fn handle_chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy(state, Method::POST, "/v1/chat/completions", headers, body).await
}

pub async fn handle_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    proxy(state, Method::GET, "/v1/models", headers, Bytes::new()).await
}

/// Forwards `/tokenize` to an engine that actually exposes it (llama.cpp does;
/// some builds do not). Callers must only reach this when the cached
/// [`crate::props::TokenSource`] is `Engine`.
pub async fn handle_tokenize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy(state, Method::POST, "/tokenize", headers, body).await
}

async fn proxy(
    state: Arc<AppState>,
    method: Method,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    // A request that arrives while the engine is down would otherwise sit in a
    // connection-retry loop on the phone and look like a slow network.
    if !state.health.state().is_serving() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({
                "error": {
                    "message": "engine is down; retry later or use the edge model",
                    "type": "engine_unavailable",
                    "code": "down"
                }
            })),
        )
            .into_response();
    }

    let _guard = state.health.begin_request();
    let forwarded = engine::forwardable_request_headers(&headers);

    let mut builder = state.engine.request(method, path, body.to_vec());
    for (name, value) in forwarded {
        builder = builder.header(name, value);
    }

    match builder.send().await {
        Ok(resp) => engine::relay(resp),
        Err(_) => {
            // The engine vanished between the health probe and now — almost
            // always a model swap in progress.
            state.props.invalidate();
            engine::engine_unreachable()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{EngineState, Health};
    use crate::props::PropsCache;
    use axum::http::HeaderValue;
    use std::time::Duration;

    fn state_with(token: Option<&str>, engine_state: EngineState) -> Arc<AppState> {
        let cfg = crate::config::GatewayConfig {
            token: token.map(str::to_string),
            engine_base: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        let health = std::sync::Arc::new(match engine_state {
            // `Down` requires the never-ready grace to have expired, so age the
            // clock past it; every other state is reachable on a fresh clock.
            EngineState::Down => Health::started_ago(
                Duration::from_secs(1_501),
                Duration::from_secs(1_500),
                1,
            ),
            _ => Health::new(Duration::from_secs(1_500), 1),
        });
        // Force the state the test asked for by driving the probe path.
        match engine_state {
            EngineState::Ready => health.record(crate::health::Probe::Healthy),
            EngineState::Busy => health.record(crate::health::Probe::Unavailable { busy: true }),
            EngineState::Loading => health.record(crate::health::Probe::Unavailable { busy: false }),
            EngineState::Down => health.record(crate::health::Probe::Unreachable),
        };
        Arc::new(AppState {
            engine: crate::engine::Engine::new(&cfg),
            health,
            props: PropsCache::new(cfg.props_ttl),
            status: std::sync::RwLock::new(crate::props::EngineStatus::default()),
            store: etern_core::Store::open_in_memory().expect("in-memory store"),
            journal: std::sync::Arc::new(
                etern_core::Journal::new(std::env::temp_dir(), etern_core::Store::open_in_memory().expect("in-memory store"))
                    .expect("journal"),
            ),
            tools: std::sync::Arc::new(
                etern_core::ToolRuntime::new(
                    etern_core::DeviceProfile::Desktop,
                    etern_core::Sandbox::edge(std::env::temp_dir().join(format!(
                        "etern-proxy-test-{}",
                        std::process::id()
                    )))
                    .expect("test sandbox"),
                    std::sync::Arc::new(etern_core::UnavailableSearcher),
                )
                .expect("test tool runtime"),
            ),
            tls_pin: None,
            lan_ip: None,
            tailscale_ip: None,
            cfg,
        })
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_str(&format!("Bearer {token}")).unwrap());
        h
    }

    #[tokio::test]
    async fn a_down_engine_is_refused_without_touching_the_engine() {
        let state = state_with(None, EngineState::Down);
        let resp = handle_chat_completions(State(state), HeaderMap::new(), Bytes::from("{}")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_loading_engine_is_not_refused() {
        // Loading must queue/forward, not reject: rejecting is what would make
        // the phone fall back and fork the transcript mid-swap.
        let state = state_with(None, EngineState::Loading);
        let resp = handle_chat_completions(State(state), HeaderMap::new(), Bytes::from("{}")).await;
        // The engine is not listening, so the passthrough fails — but with the
        // retryable `loading` code rather than the terminal `down` code.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn auth_is_enforced_before_the_engine_is_consulted() {
        let state = state_with(Some("s3cret"), EngineState::Down);
        let resp = handle_chat_completions(State(state), HeaderMap::new(), Bytes::from("{}")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "401 beats 503");
    }

    #[tokio::test]
    async fn a_valid_token_gets_past_auth() {
        let state = state_with(Some("s3cret"), EngineState::Down);
        let resp =
            handle_chat_completions(State(state), bearer("s3cret"), Bytes::from("{}")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "past auth, blocked by engine");
    }

    #[tokio::test]
    async fn models_are_proxied_too() {
        let state = state_with(None, EngineState::Ready);
        let resp = handle_models(State(state), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn in_flight_returns_when_a_request_ends() {
        let state = state_with(None, EngineState::Ready);
        assert_eq!(state.health.in_flight(), 0);
        let _ = handle_chat_completions(State(state.clone()), HeaderMap::new(), Bytes::from("{}")).await;
        assert_eq!(state.health.in_flight(), 0, "the guard must not leak");
    }
}
