//! Etern gateway.
//!
//! A thin, engine-agnostic HTTP layer between clients (desktop UI, Android app)
//! and whatever llama.cpp-compatible server currently owns the engine port. It
//! exists because `ModelSwitcher.ps1` can replace the model behind `:8080` at
//! any time, and because a phone needs one unauthenticated URL it can poll to
//! learn whether it is at home.
//!
//! Routes
//!   GET  /health                 unauthenticated reachability + engine state
//!   GET  /props                  cached engine properties + fingerprint
//!   GET  /v1/models              proxied
//!   POST /v1/chat/completions    proxied, streaming passed through
//!   POST /tokenize               proxied when the engine has it, else 501
//!   POST /etern/edge-window      proposes the 8k window for the edge model
//!   POST /etern/sync/push        edge device replays its offline outbox
//!   GET  /etern/sync/window      proposes the edge window from desktop state
//!   GET  /etern/sync/transcript  returns a device-scoped chat transcript
//!   POST /etern/memory/sync      merges the edge and desktop MEMORY.md files
//!   POST /etern/personality/sync reconciles the shared model style preference
//!   GET  /etern/search           full-text search over stored conversation
//!   GET  /etern/tools            tool definitions for this device profile
//!   POST /etern/tools/execute    validate + run a batch of tool calls
//!   GET  /etern/journal          list journal digest files
//!   POST /etern/pairings         mint a one-time pair code (owner only)
//!   POST /etern/pair             redeem a pair code for a device token
//!   GET  /etern/devices          device registry (owner only)
//!   POST /etern/devices/{id}/revoke  kill a device token (owner only)
//!   GET  /etern/journal/{name}   read one journal digest file
//!
//! Everything except `/health` requires a bearer token when one is configured.

mod auth;
mod config;
mod engine;
mod fetch;
mod health;
mod journal;
mod journal_api;
mod props;
mod proxy;
mod pairing_api;
mod sync_api;
mod tls;
mod turns_api;
mod websearch;
mod window;

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use etern_core::{
    DeviceProfile, Journal, Store, Summarizer, ToolRuntime, UnavailableSearcher,
};
use serde_json::{json, Value};

use crate::config::GatewayConfig;
use crate::engine::Engine;
use crate::health::Health;
use crate::props::{EngineStatus, PropsCache, TokenSource};
use crate::websearch::SearxngSearcher;

pub struct AppState {
    pub cfg: GatewayConfig,
    pub engine: Engine,
    pub health: Arc<Health>,
    pub props: PropsCache,
    /// Volatile engine state (loaded / sleeping / prompt-token usage). Kept out
    /// of the fingerprint so a model napping does not invalidate prompt-cache
    /// prefixes built against an identical template.
    pub status: std::sync::RwLock<EngineStatus>,
    /// Canonical conversation store. The source of truth the journal projects
    /// from; empty until M5/M6 populate it, which is expected.
    pub store: Store,
    /// Daily/sync journal over `store`. Shared with the background jobs.
    pub journal: Arc<Journal>,
    /// Shared tool runtime (sandboxed file I/O, `remember`, `web_search`). The
    /// gateway is the desktop, so it always runs the full desktop tool set.
    pub tools: Arc<ToolRuntime>,
    /// `base64(SHA-256(DER))` of the self-signed certificate, when TLS is on.
    /// Published on `/health` so a device can pin before it has paired.
    pub tls_pin: Option<String>,
    /// Gateway's LAN-reachable IPv4, used for local Wi-Fi pairing.
    pub lan_ip: Option<String>,
    /// Gateway's Tailscale IPv4, used for tailnet pairing when available.
    pub tailscale_ip: Option<String>,
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "etern_gateway=info,tower_http=warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    let cfg = GatewayConfig::build(std::env::args().skip(1), |k| std::env::var(k).ok());
    if cfg.token.is_none() && cfg.listen.ip().is_unspecified() {
        tracing::warn!(
            "no token configured on a LAN-reachable listener; \
             set ETERN_TOKEN before leaving the house"
        );
    }

    let store = Store::open(&cfg.store_path).unwrap_or_else(|e| {
        panic!("cannot open store {}: {e}", cfg.store_path.display())
    });
    let journal_instance = Journal::new(cfg.journal_dir.clone(), store.clone()).unwrap_or_else(|e| {
        panic!("cannot open journal {}: {e}", cfg.journal_dir.display())
    });

    // Build the desktop tool runtime. `web_search` reaches SearXNG only when a
    // base URL is configured; otherwise it degrades to an explicit `no_network`
    // result (PLAN §6), which is the correct behaviour before Docker is set up.
    let tools = build_tool_runtime(&cfg);

    // TLS material for the LAN listener, when enabled. A failure here is
    // logged but not fatal: the plain listener still serves loopback, and the
    // operator sees the error and fixes the disk/permissions.
    let tls_dir = cfg
        .tls_dir
        .clone()
        .unwrap_or_else(|| tls::tls_dir_for(&cfg.store_path));
    let lan_address = cfg.tls_listen.and_then(|_| tls::local_ip());
    let tailscale_address = cfg.tls_listen.and_then(|_| tls::tailscale_ip());
    let extra_ips: Vec<std::net::IpAddr> =
        [lan_address, tailscale_address].into_iter().flatten().collect();
    let tls_material: Option<tls::TlsMaterial> = cfg.tls_listen.and_then(|_| {
        match tls::load_or_generate(&tls_dir, extra_ips) {
            Ok(material) => Some(material),
            Err(e) => {
                tracing::error!(error = %e, dir = %tls_dir.display(), "TLS disabled: cannot prepare cert");
                None
            }
        }
    });
    let lan_ip = lan_address.map(|ip| ip.to_string());
    let tailscale_ip = tailscale_address.map(|ip| ip.to_string());
    let mut tls_shutdown: Option<axum_server::Handle<std::net::SocketAddr>> = None;

    let state = Arc::new(AppState {
        engine: Engine::new(&cfg),
        health: Arc::new(Health::new(cfg.load_grace, 1)),
        props: PropsCache::new(cfg.props_ttl),
        status: std::sync::RwLock::new(EngineStatus::default()),
        store,
        journal: Arc::new(journal_instance),
        tools,
        tls_pin: tls_material.as_ref().map(|m| m.pin.clone()),
        lan_ip,
        tailscale_ip,
        cfg,
    });

    // Discover the engine before binding, so the first client never reads an
    // empty fingerprint and concludes the desktop is offline.
    if let Some(snap) = state.engine.snapshot().await {
        apply_snapshot(&state, snap);
    }
    state.health.record(state.engine.probe_health().await);

    tokio::spawn(prober(state.clone()));

    // Journal triggers (PLAN §5): recover any closed-but-unwritten days now,
    // then close each day as its 4 a.m. boundary passes. Both run off the
    // request path and fall back to an extractive digest if the engine is busy.
    let summarizer: Arc<dyn Summarizer + Send + Sync> =
        Arc::new(journal::EngineSummarizer::new(&state.cfg.engine_base));
    journal::spawn_catch_up(state.journal.clone(), summarizer.clone());
    journal::spawn_daily_scheduler(state.journal.clone(), summarizer);

    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(state.cfg.listen)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {}: {e}", state.cfg.listen));
    tracing::info!(
        listen = %state.cfg.listen,
        engine = %state.cfg.engine_base,
        auth = state.cfg.token.is_some(),
        "gateway up"
    );

    // TLS listener (PLAN §8): same router, second port, self-signed cert the
    // phone pins. It supplements the plain listener rather than replacing it
    // so loopback development keeps working without a cert handshake.
    if let (Some(addr), Some(material)) = (state.cfg.tls_listen, tls_material.as_ref()) {
        let cert_path = tls_dir.join("cert.pem");
        let key_path = tls_dir.join("key.pem");
        let pin = material.pin.clone();
        let tls_handle = axum_server::Handle::new();
        let shutdown_tls = tls_handle.clone();
        tokio::spawn(async move {
            let config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path).await {
                Ok(config) => config,
                Err(e) => {
                    tracing::error!(error = %e, "TLS listener disabled: cannot load cert");
                    return;
                }
            };
            tracing::info!(
                listen = %addr,
                cert_pin = %pin,
                "TLS listener up - devices must pin this hash"
            );
            if let Err(e) = axum_server::bind_rustls(addr, config)
                .handle(shutdown_tls)
                .serve(build_router(state.clone()).into_make_service())
                .await
            {
                tracing::error!(error = %e, "TLS listener stopped");
            }
        });
        tls_shutdown = Some(tls_handle);
    }

    let (handle, shutdown) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = handle.send(());
            if let Some(h) = tls_shutdown {
                h.graceful_shutdown(None);
            }
        }
    });

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
        })
        .await
    {
        tracing::error!(error = %e, "server stopped");
    }
}

/// The full route table, shared by the plain and TLS listeners.
fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route("/props", get(handle_props))
        .route("/v1/models", get(proxy::handle_models))
        .route("/v1/chat/completions", post(proxy::handle_chat_completions))
        .route("/tokenize", post(handle_tokenize))
        .route("/etern/edge-window", post(handle_edge_window))
        .route("/etern/search", get(handle_search))
        .route("/etern/tools", get(handle_tools))
        .route("/etern/tools/execute", post(handle_tools_execute))
        .merge(journal_api::routes())
        .merge(sync_api::routes())
        .merge(turns_api::routes())
        .merge(pairing_api::routes())
        .with_state(state)
}

/// Stores one discovery pass and republishes everything derived from it.
fn apply_snapshot(state: &Arc<AppState>, snap: engine::Snapshot) {
    let outcome = state.props.store_full(
        snap.props,
        snap.health.as_ref(),
        snap.slots.as_ref(),
        snap.has_tokenize,
    );
    if let Some(slots) = outcome.status.slots {
        state.health.set_capacity(slots);
    }
    if outcome.changed {
        tracing::info!(
            model = %outcome.fingerprint.model,
            template_sha = %outcome.fingerprint.template_sha,
            n_ctx = ?outcome.fingerprint.n_ctx,
            token_source = outcome.fingerprint.token_source.as_str(),
            tools = outcome.fingerprint.supports_tools,
            vision = outcome.fingerprint.supports_vision,
            "engine identity changed - prompt-cache prefixes are invalid"
        );
    }
    *state.status.write().expect("status lock") = outcome.status;
}

/// Polls the engine and keeps `/health` honest. Also refreshes `/props` when it
/// goes stale, and drops the cache when the engine's identity changes.
async fn prober(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(state.cfg.probe_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;

        let probe = state.engine.probe_health().await;
        let was = state.health.state();
        let now = state.health.record(probe);
        if was != now {
            match now {
                // The distinction that prevents a transcript fork: `loading`
                // means "hold your prompt", `down` means "go to the edge model".
                health::EngineState::Loading => tracing::warn!(
                    grace_secs = state.cfg.load_grace.as_secs(),
                    "engine unavailable - treating as a model swap, not an outage"
                ),
                health::EngineState::Down => tracing::error!(
                    "engine unreachable past the load grace window; \
                     edge devices should fail over now"
                ),
                other => tracing::info!(state = other.as_str(), "engine state"),
            }
        }

        // Props are only worth fetching once there is a model behind the port.
        if now.is_serving() && !state.props.is_fresh(std::time::Instant::now()) {
            match state.engine.snapshot().await {
                Some(snap) => apply_snapshot(&state, snap),
                None => {
                    // The engine answered the health probe but not `/props`:
                    // it is mid-swap. Keep the old fingerprint rather than
                    // publishing a hole.
                    tracing::debug!("props unavailable while engine is serving");
                }
            }
        }
    }
}

/// Desktop-side reachability. Unauthenticated on purpose: this body *is* the
/// routing decision, and a phone that has not paired yet still has to be able
/// to ask it.
async fn handle_health(State(state): State<Arc<AppState>>) -> Json<Value> {
    let snapshot = state.health.snapshot();
    let fingerprint = state.props.fingerprint();
    let status = state.status.read().expect("status lock").clone();
    let token_source = fingerprint
        .as_ref()
        .map(|f| f.token_source)
        .unwrap_or(TokenSource::Estimated);

    json!({
        "gateway_reachable": snapshot.gateway_reachable,
        "engine_ready": snapshot.engine_ready,
        "engine_ready_s": snapshot.engine_ready_s,
        // Only `down` means "use the edge model". `loading` means "wait".
        "fail_over": !snapshot.engine_ready_s,
        // A sleeping model still serves; the first token is just slow.
        "waking": status.sleeping,
        "in_flight": snapshot.in_flight,
        "swaps": snapshot.swaps,
        "model": fingerprint.as_ref().map(|f| f.model.clone()),
        "template_sha": fingerprint.as_ref().map(|f| f.template_sha.clone()),
        "n_ctx": fingerprint.as_ref().and_then(|f| f.n_ctx).or(status.max_context),
        "token_source": token_source.as_str(),
        // The margin the edge-window proposal actually applies. This is the
        // conservative estimate margin regardless of `token_source`, because the
        // desktop count is always an estimate of the edge window (see
        // `window::applied_margin`).
        "safety_margin": window::applied_margin(),
        "supports_tools": fingerprint.as_ref().map(|f| f.supports_tools),
        "supports_vision": fingerprint.as_ref().map(|f| f.supports_vision),
        "engine_loaded": status.loaded,
        "prompt_tokens": status.last_prompt_tokens,
        // Present only when the TLS listener is on. A device pins this value
        // at pairing time; a changed pin means the cert was replaced.
        "cert_pin": state.tls_pin,
        // Where a phone should actually connect. The QR payload is
        // {host, port, cert_pin, code}; the LAN and Tailscale addresses are
        // discovered locally so the app can pair over either network.
        "lan_ip": state.lan_ip,
        "tailscale_ip": state.tailscale_ip,
        "tls_port": state.cfg.tls_listen.map(|a| a.port() as u64),
    })
    .into()
}

/// What the engine actually is, so a client can cache the matching template.
async fn handle_props(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }
    match (state.props.props(), state.props.fingerprint()) {
        (Some(props), Some(fingerprint)) => {
            let template_sha = fingerprint.template_sha.clone();
            let mut body = match props {
                Value::Object(map) => map,
                other => json!({ "props": other }).as_object().cloned().unwrap_or_default(),
            };
            body.insert("etern_template_sha".to_string(), json!(template_sha));
            body.insert("etern_model".to_string(), json!(fingerprint.model));
            body.insert(
                "etern_token_source".to_string(),
                json!(fingerprint.token_source.as_str()),
            );
            (StatusCode::OK, Json(Value::Object(body))).into_response()
        }
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "message": "engine properties not available yet" } })),
        )
            .into_response(),
    }
}

/// Proposes the edge model's 8k window from desktop-side turns.
async fn handle_edge_window(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<window::WindowRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let fingerprint = state.props.fingerprint();
    let template_sha = fingerprint
        .as_ref()
        .map(|f| f.template_sha.clone())
        .unwrap_or_default();
    let token_source = fingerprint
        .as_ref()
        .map(|f| f.token_source)
        .unwrap_or(TokenSource::Estimated);

    window::handle(body, template_sha, token_source).await
}

/// llama.cpp exposes `/tokenize`; some builds (e.g. the Strata service) 404 it.
/// Proxy when the engine has it, otherwise answer the question the caller is
/// really asking: how are tokens being counted here?
async fn handle_tokenize(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let token_source = state
        .props
        .fingerprint()
        .map(|f| f.token_source)
        .unwrap_or(TokenSource::Estimated);

    if token_source == TokenSource::Engine {
        return proxy::handle_tokenize(State(state), headers, body).await;
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": {
                "message": "this engine exposes no /tokenize route; token counts are \
                            estimated. Use POST /etern/edge-window, which applies the \
                            estimated safety margin, or count with the device's own tokenizer.",
                "type": "unsupported_endpoint",
                "code": "tokenize_unavailable"
            },
            "token_source": TokenSource::Estimated.as_str(),
            "safety_margin": TokenSource::Estimated.safety_margin()
        })),
    )
        .into_response()
}

/// Query parameters for `/etern/search`.
#[derive(Debug, serde::Deserialize)]
pub struct SearchParams {
    /// FTS5 query string (bare words, phrases in quotes, `AND`/`OR`/`NOT`).
    pub q: String,
    /// Maximum hits to return; clamped to `MAX_SEARCH_LIMIT`.
    pub limit: Option<usize>,
}

/// Cap so a client cannot ask the store for the whole corpus in one shot.
const MAX_SEARCH_LIMIT: usize = 100;
const DEFAULT_SEARCH_LIMIT: usize = 20;

/// Full-text search over the canonical store (FTS5 sidecar). The store is the
/// source of truth; the index is a rebuildable projection, so a hit always
/// carries the `turn_id` to fetch the authoritative turn.
async fn handle_search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Result<axum::extract::Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    use axum::extract::Query;

    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let Query(sp) = match params {
        Ok(q) => q,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "missing ?q= search query"}})),
            )
                .into_response()
        }
    };

    let query = sp.q.trim();
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "empty search query"}})),
        )
            .into_response();
    }

    let limit = sp.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT);

    // SQLite is synchronous and the connection is mutex-guarded; keep it off
    // the async worker with a short blocking hop.
    let store = state.store.clone();
    let text = query.to_string();
    let hits = match tokio::task::spawn_blocking(move || store.search(&text, limit)).await {
        Ok(Ok(hits)) => hits,
        Ok(Err(e)) => {
            // A malformed FTS5 expression (unbalanced quotes, stray operator)
            // surfaces here — report it as a bad query, not a server fault.
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("invalid search query: {e}")}})),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": format!("search task failed: {e}")}})),
            )
                .into_response()
        }
    };

    let results: Vec<_> = hits
        .into_iter()
        .map(|h| json!({ "turn_id": h.turn_id, "snippet": h.snippet }))
        .collect();
    Json(json!({ "query": query, "count": results.len(), "results": results })).into_response()
}

/// Builds the desktop tool runtime from config: the sandbox is rooted at
/// `tool_root` (exposing `Journal/`, `memory/`, `inbox/`), and `web_search` is
/// backed by SearXNG when configured, else always reports `no_network`.
fn build_tool_runtime(cfg: &GatewayConfig) -> Arc<ToolRuntime> {
    let sandbox = etern_core::Sandbox::desktop(&cfg.tool_root).unwrap_or_else(|e| {
        panic!("cannot open tool sandbox {}: {e}", cfg.tool_root.display())
    });
    let searcher = match cfg.searxng_url.as_deref() {
        Some(base) => {
            tracing::info!(searxng = %base, "web_search enabled");
            Arc::new(SearxngSearcher::new(base)) as Arc<dyn etern_core::WebSearcher>
        }
        None => {
            tracing::info!("no SearXNG configured; web_search will report no_network");
            Arc::new(UnavailableSearcher) as Arc<dyn etern_core::WebSearcher>
        }
    };
    Arc::new(
        ToolRuntime::new(DeviceProfile::Desktop, sandbox, searcher)
            .unwrap_or_else(|e| panic!("cannot init tool runtime: {e}"))
            .with_fetcher(Arc::new(fetch::HttpFetcher::new())),
    )
}

/// Advertise tool definitions (the OpenAI `tools` array) so a client can pass
/// them straight to `/v1/chat/completions`.
///
/// `?profile=edge` returns the reduced phone set. A phone executes those tools
/// itself in its own sandbox, so it must not be told about tools it cannot
/// honour (`web_search`, `list_dir`). Execution always goes through the
/// desktop's full runtime regardless of who was advertised what.
async fn handle_tools(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ToolsQuery>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }
    let profile = match q.profile.as_deref() {
        None | Some("desktop") => DeviceProfile::Desktop,
        Some("edge") => DeviceProfile::Edge,
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("unknown profile {other:?}")}})),
            )
                .into_response();
        }
    };
    Json(etern_core::tools::definitions_for(profile)).into_response()
}

/// Query parameters of `GET /etern/tools`.
#[derive(Debug, serde::Deserialize)]
pub struct ToolsQuery {
    /// `"desktop"` (default) or `"edge"`.
    #[serde(default)]
    pub profile: Option<String>,
}

/// One tool call in an `/etern/tools/execute` batch.
#[derive(Debug, serde::Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// Optional client-supplied id echoed back so the caller can match results
    /// to calls (mirrors the OpenAI `tool_call_id`).
    #[serde(default)]
    pub id: Option<String>,
}

/// The `/etern/tools/execute` request body.
#[derive(Debug, serde::Deserialize)]
pub struct ToolExecuteRequest {
    /// Model that issued the calls; stamped into `remember` lines.
    #[serde(default)]
    pub model: Option<String>,
    /// Turn id for provenance; defaults to `"unknown"`.
    #[serde(default)]
    pub turn_id: Option<String>,
    pub calls: Vec<ToolCall>,
}

/// Upper bound on calls per request so one turn cannot fan out unbounded.
const MAX_TOOL_CALLS: usize = 16;

/// Validates and executes a batch of tool calls. Each call is answered
/// independently: a validation failure returns a retryable error object for
/// that call (HTTP stays 200 so the harness can feed every result back to the
/// model for its one automatic retry, PLAN §6). Only a malformed *request* is a
/// 4xx.
async fn handle_tools_execute(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<ToolExecuteRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "invalid JSON body; expected {\"calls\": [...]}"}})),
            )
                .into_response()
        }
    };

    if req.calls.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "no tool calls provided"}})),
        )
            .into_response();
    }
    if req.calls.len() > MAX_TOOL_CALLS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": format!("too many calls (max {MAX_TOOL_CALLS})")}})),
        )
            .into_response();
    }

    let model = req.model.unwrap_or_else(|| "unknown".to_string());
    let turn_id = req.turn_id.unwrap_or_else(|| "unknown".to_string());
    let now_ms = chrono::Utc::now().timestamp_millis();

    // File I/O and the blocking SearXNG client must not run on an async worker.
    let runtime = state.tools.clone();
    let results = tokio::task::spawn_blocking(move || {
        req.calls
            .iter()
            .map(|call| {
                let outcome = runtime.execute(&call.name, &call.arguments, &model, &turn_id, now_ms);
                let mut obj = match outcome {
                    Ok(res) => json!({ "ok": true, "content": res.content }),
                    Err(err) => json!({ "ok": false, "retryable": err.retryable, "error": err.to_result_value()["error"] }),
                };
                if let Some(id) = &call.id {
                    obj["id"] = json!(id);
                }
                obj["name"] = json!(call.name);
                obj
            })
            .collect::<Vec<_>>()
    })
    .await;

    match results {
        Ok(results) => Json(json!({ "results": results })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("tool task failed: {e}")}})),
        )
            .into_response(),
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::path::Path;

    /// Builds an `AppState` whose journal points at `journal_dir`, for tests
    /// that exercise the on-disk journal API.
    pub(crate) fn test_state_with_journal(
        token: Option<&str>,
        journal_dir: &Path,
    ) -> Arc<AppState> {
        let cfg = GatewayConfig {
            token: token.map(str::to_string),
            ..GatewayConfig::default()
        };
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tools_dir =
            std::env::temp_dir().join(format!("etern-tools-sup-{}-{}", std::process::id(), n));
        let sandbox = etern_core::Sandbox::desktop(&tools_dir).expect("test sandbox");
        let tools = Arc::new(
            ToolRuntime::new(DeviceProfile::Desktop, sandbox, Arc::new(UnavailableSearcher))
                .expect("test tool runtime"),
        );
        Arc::new(AppState {
            engine: Engine::new(&cfg),
            health: Arc::new(Health::new(cfg.load_grace, 1)),
            props: PropsCache::new(cfg.props_ttl),
            status: std::sync::RwLock::new(EngineStatus::default()),
            store: Store::open_in_memory().expect("in-memory store"),
            journal: Arc::new(
                Journal::new(journal_dir.to_path_buf(), Store::open_in_memory().expect("store"))
                    .expect("journal"),
            ),
            tools,
            tls_pin: None,
            lan_ip: None,
            tailscale_ip: None,
            cfg,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use etern_core::{Message, Turn, TurnKind};
    use tower::ServiceExt;

    fn test_state(token: Option<&str>) -> Arc<AppState> {
        let cfg = GatewayConfig {
            token: token.map(str::to_string),
            ..GatewayConfig::default()
        };
        // A throwaway sandbox per test keeps `remember`/`write_file` off the
        // real working directory.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("etern-tools-test-{}-{}", std::process::id(), n));
        let sandbox = etern_core::Sandbox::desktop(&dir).expect("test sandbox");
        let tools = Arc::new(
            ToolRuntime::new(DeviceProfile::Desktop, sandbox, Arc::new(UnavailableSearcher))
                .expect("test tool runtime"),
        );
        Arc::new(AppState {
            engine: Engine::new(&cfg),
            health: Arc::new(Health::new(cfg.load_grace, 1)),
            props: PropsCache::new(cfg.props_ttl),
            status: std::sync::RwLock::new(EngineStatus::default()),
            store: Store::open_in_memory().expect("in-memory store"),
            journal: Arc::new(
                Journal::new(std::env::temp_dir(), Store::open_in_memory().expect("in-memory store"))
                    .expect("journal"),
            ),
            tools,
            tls_pin: None,
            lan_ip: None,
            tailscale_ip: None,
            cfg,
        })
    }

    fn app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/etern/search", get(handle_search))
            .route("/etern/tools", get(handle_tools))
            .route("/etern/tools/execute", post(handle_tools_execute))
            .with_state(state)
    }

    #[tokio::test]
    async fn health_publishes_cert_pin_when_tls_is_on() {
        let state = test_state(None);
        let router = Router::new().route("/health", get(handle_health)).with_state(state);
        let resp = router
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        // TLS is off in the default test state, so the pin is null.
        assert!(body["cert_pin"].is_null());
    }

    #[tokio::test]
    async fn health_includes_a_configured_cert_pin() {
        let mut state = Arc::try_unwrap(test_state(Some("tok"))).ok().expect("unique");
        state.tls_pin = Some("abcd1234".to_string());
        let router = Router::new()
            .route("/health", get(handle_health))
            .with_state(Arc::new(state));
        let resp = router
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["cert_pin"].as_str(), Some("abcd1234"));
    }

    #[tokio::test]
    async fn health_includes_a_configured_tailscale_address() {
        let mut state = Arc::try_unwrap(test_state(Some("tok"))).ok().expect("unique");
        state.tailscale_ip = Some("100.77.211.47".to_string());
        let router = Router::new()
            .route("/health", get(handle_health))
            .with_state(Arc::new(state));
        let resp = router
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["tailscale_ip"].as_str(), Some("100.77.211.47"));
    }

    #[tokio::test]
    async fn search_finds_a_stored_turn() {
        let state = test_state(None);
        let turn = Turn::new(TurnKind::Chat, "", 0, vec![Message::user("my plant is a monstera")]);
        state.store.insert_turn(&turn).unwrap();

        let resp = app(state)
            .oneshot(Request::get("/etern/search?q=monstera").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["count"].as_u64(), Some(1));
        assert_eq!(body["results"][0]["turn_id"].as_str(), Some(turn.id.to_string().as_str()));
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let state = test_state(None);
        let resp = app(state)
            .oneshot(Request::get("/etern/search").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_rejects_a_blank_query() {
        let state = test_state(None);
        let resp = app(state)
            .oneshot(Request::get("/etern/search?q=%20%20").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_survives_a_malformed_fts_expression() {
        // An unbalanced quote is a client error, not a 500.
        let state = test_state(None);
        let resp = app(state)
            .oneshot(Request::get("/etern/search?q=%22unbalanced").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_requires_the_token_when_one_is_configured() {
        let state = test_state(Some("s3cret"));
        let resp = app(state)
            .oneshot(Request::get("/etern/search?q=anything").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -- /etern/tools -------------------------------------------------------

    #[tokio::test]
    async fn tools_endpoint_lists_desktop_definitions() {
        let state = test_state(None);
        let resp = app(state)
            .oneshot(Request::get("/etern/tools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<_> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"web_search".to_string()));
        assert!(names.contains(&"remember".to_string()));
    }

    #[tokio::test]
    async fn tools_endpoint_serves_the_reduced_set_for_phones() {
        let state = test_state(None);
        let resp = app(state)
            .oneshot(
                Request::get("/etern/tools?profile=edge")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<_> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_string())
            .collect();
        // A phone runs these itself in its own sandbox, so it must not be told
        // about tools it cannot honour.
        assert_eq!(names, vec!["read_file", "write_file", "remember"]);
    }

    #[tokio::test]
    async fn tools_endpoint_rejects_an_unknown_profile() {
        let state = test_state(None);
        let resp = app(state)
            .oneshot(
                Request::get("/etern/tools?profile=tablet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tools_endpoint_requires_token() {
        let state = test_state(Some("s3cret"));
        let resp = app(state)
            .oneshot(Request::get("/etern/tools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -- /etern/tools/execute ----------------------------------------------

    async fn post_json(app: Router, path: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn execute_writes_then_reads_a_file() {
        let state = test_state(None);
        let (status, body) = post_json(
            app(state),
            "/etern/tools/execute",
            json!({
                "model": "big",
                "turn_id": "t1",
                "calls": [
                    {"id": "c1", "name": "write_file", "arguments": {"path": "inbox/note.txt", "content": "hi there"}},
                    {"id": "c2", "name": "read_file", "arguments": {"path": "inbox/note.txt"}}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["id"], "c1");
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[1]["ok"], true);
        assert_eq!(results[1]["content"]["content"], "hi there");
    }

    #[tokio::test]
    async fn execute_reports_retryable_validation_error() {
        let state = test_state(None);
        let (status, body) = post_json(
            app(state),
            "/etern/tools/execute",
            json!({"calls": [{"name": "read_file", "arguments": {}}]}),
        )
        .await;
        // HTTP stays 200 so the harness can feed the error back for a retry.
        assert_eq!(status, StatusCode::OK);
        let call = &body["results"][0];
        assert_eq!(call["ok"], false);
        assert_eq!(call["retryable"], true);
        assert_eq!(call["error"]["kind"], "validation");
    }

    #[tokio::test]
    async fn execute_blocks_sandbox_escape() {
        let state = test_state(None);
        let (status, body) = post_json(
            app(state),
            "/etern/tools/execute",
            json!({"calls": [{"name": "read_file", "arguments": {"path": "../../Windows/win.ini"}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let call = &body["results"][0];
        assert_eq!(call["ok"], false);
        assert_eq!(call["error"]["kind"], "sandbox");
    }

    #[tokio::test]
    async fn execute_web_search_reports_no_network() {
        let state = test_state(None);
        let (_status, body) = post_json(
            app(state),
            "/etern/tools/execute",
            json!({"calls": [{"name": "web_search", "arguments": {"query": "rust lang"}}]}),
        )
        .await;
        let call = &body["results"][0];
        assert_eq!(call["ok"], true);
        assert_eq!(call["content"]["status"], "no_network");
    }

    #[tokio::test]
    async fn execute_rejects_empty_and_malformed_batches() {
        let state = test_state(None);
        let (status, _) =
            post_json(app(state.clone()), "/etern/tools/execute", json!({"calls": []})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let resp = app(state)
            .oneshot(
                Request::post("/etern/tools/execute")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn execute_requires_token() {
        let state = test_state(Some("s3cret"));
        let resp = app(state)
            .oneshot(
                Request::post("/etern/tools/execute")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"calls": [{"name": "read_file", "arguments": {"path": "x"}}]}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
