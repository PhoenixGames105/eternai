//! Recording of finished exchanges into the canonical store.
//!
//! Routine 1 of the harness (the daily journal) reads the store, so a chat
//! that is never stored is a chat that never happened as far as the journal is
//! concerned. `/v1/chat/completions` could not be the place that fixes this:
//! it is a byte-for-byte passthrough to the engine by design, and parsing the
//! SSE body there would mean the gateway re-serialises a stream it promised not
//! to touch. Instead the caller — the desktop shell, which already reassembles
//! the stream to render it — reports the completed exchange here once, after
//! the stream ends.
//!
//! Recorded turns are `chat` turns under the desktop's own device identity, so
//! they land in the daily digest and in every device's edge window, and they
//! cannot collide with a phone's `(device_id, seq)` sync identity.
//!
//! Idempotency: the caller may supply the `turn_id` it minted for the request.
//! The store keys on it, so a client that retries a record it already delivered
//! is absorbed rather than duplicated.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use etern_core::{EngineKind, Message, Turn, TurnId, TurnKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth;
use crate::AppState;

/// Device identity for exchanges that happened on the desktop itself. Kept
/// distinct from every paired phone so the partial unique index on
/// `(device_id, seq)` never mixes the two seq spaces.
pub const DESKTOP_DEVICE_ID: &str = "desktop";

/// A single exchange is short. These caps only trip on a client bug or a
/// hostile caller; a legitimate turn is a handful of messages.
const MAX_RECORD_MESSAGES: usize = 64;
const MAX_RECORD_CHARS: usize = 200_000;
const MAX_HISTORY_TURNS: usize = 500;

/// How many times to re-allocate a seq after losing a race on the unique
/// index. Two desktop windows answering at the same instant is the worst case.
const SEQ_RETRIES: usize = 8;

#[derive(Debug, Deserialize)]
pub struct RecordRequest {
    /// Correlation id the caller minted for the chat request. Reusing it here
    /// makes this endpoint safe to retry.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Defaults to the desktop identity; a paired device may record on its own
    /// behalf through the same door.
    #[serde(default)]
    pub device_id: Option<String>,
    pub messages: Vec<Message>,
    /// Model label override. Normally the gateway knows better than the client.
    #[serde(default)]
    pub model: Option<String>,
    /// Epoch milliseconds. Defaults to now; a client recording an exchange that
    /// finished a moment earlier may pass the original start time.
    #[serde(default)]
    pub ts: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RecordAck {
    pub id: String,
    pub seq: i64,
    /// `false` when the turn was already present (a retry).
    pub stored: bool,
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct HistoryResponse {
    turns: Vec<Turn>,
}

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": {"message": message.to_string()}}))).into_response()
}

/// Checks the request and builds the canonical turn, leaving `seq` at zero for
/// the store step to fill in.
fn validate(req: RecordRequest) -> Result<Turn, String> {
    if req.messages.is_empty() {
        return Err("messages is empty".into());
    }
    if req.messages.len() > MAX_RECORD_MESSAGES {
        return Err(format!(
            "{} messages exceeds the {MAX_RECORD_MESSAGES}-message limit",
            req.messages.len()
        ));
    }
    if !req.messages.iter().any(|m| m.role == etern_core::Role::User) {
        return Err("a recorded turn needs at least one user message".into());
    }
    let chars: usize = req.messages.iter().map(|m| m.content.len()).sum();
    if chars > MAX_RECORD_CHARS {
        return Err(format!(
            "{chars} characters exceeds the {MAX_RECORD_CHARS}-character limit"
        ));
    }

    let device_id = match req.device_id.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => DESKTOP_DEVICE_ID.to_string(),
    };

    let mut turn = Turn::new(TurnKind::Chat, device_id, 0, req.messages);
    if let Some(raw) = req.turn_id.as_deref().filter(|s| !s.trim().is_empty()) {
        let id = TurnId::parse_str(raw).map_err(|e| format!("turn_id is not a UUID: {e}"))?;
        turn.id = id;
    }
    if let Some(ts) = req.ts {
        turn = turn.with_ts(ts);
    }
    Ok(turn)
}

/// `POST /etern/turns` — record one completed exchange.
async fn record(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<RecordRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let axum::Json(parsed) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let req_model = parsed.model.clone();
    let mut turn = match validate(parsed) {
        Ok(t) => t,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };

    // The gateway knows the loaded model better than the caller, so it fills
    // the label in whenever the client left it blank.
    let model = match req_model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => turn_model(&state),
    };
    turn = turn.with_engine(EngineKind::Llama, model);

    let store = state.store.clone();
    let log_device = turn.device_id.clone();
    let log_id = turn.id.to_string();
    let device_id = log_device.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // The seq space belongs to the device. Recomputing it per attempt means
        // two concurrent recorders cannot silently drop a turn by picking the
        // same number: the loser's insert is ignored and it simply tries again.
        let mut last_err = String::from("no attempt made");
        for _ in 0..SEQ_RETRIES {
            let seq = store
                .max_seq_for_device(&device_id)
                .map_err(|e| e.to_string())?
                + 1;
            let candidate = Turn { seq, ..turn.clone() };
            match store.insert_turn(&candidate).map_err(|e| e.to_string())? {
                true => return Ok((candidate.seq, true)),
                false => {
                    // Either the seq was taken by a concurrent recorder (retry)
                    // or this exact turn_id was already stored (a client retry,
                    // which is a success, not a failure).
                    if turn_id_exists(&store, &candidate.id) {
                        return Ok((candidate.seq, false));
                    }
                    last_err = format!("seq {seq} already in use");
                }
            }
        }
        Err(format!("could not allocate a turn sequence: {last_err}"))
    })
    .await;

    match outcome {
        Ok(Ok((seq, stored))) => {
            tracing::info!(
                device = %log_device,
                turn = %log_id,
                seq,
                stored,
                "turn recorded"
            );
            Json(RecordAck {
                id: log_id,
                seq,
                stored,
            })
            .into_response()
        }
        Ok(Err(msg)) => error(StatusCode::INTERNAL_SERVER_ERROR, msg),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `GET /etern/turns` — read a bounded shared transcript for the desktop shell.
/// The transcript is owner-only; paired devices read their budgeted window.
async fn history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(err) = auth::check_configured(&state.cfg, &headers) {
        return err.into();
    }

    let Query(query) = match params {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let limit = query
        .limit
        .unwrap_or(MAX_HISTORY_TURNS)
        .clamp(1, MAX_HISTORY_TURNS);
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.latest_conversation_turns(limit)).await {
        Ok(Ok(turns)) => Json(HistoryResponse { turns }).into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `GET /etern/sync/transcript` — fetch history for a paired device without
/// exposing another phone's private offline turns.
async fn device_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    params: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let access = match auth::check_owner_or_device(&state.cfg, &state.store, &headers) {
        Ok(access) => access,
        Err(err) => return err.into(),
    };
    let Query(query) = match params {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let limit = query
        .limit
        .unwrap_or(MAX_HISTORY_TURNS)
        .clamp(1, MAX_HISTORY_TURNS);
    let store = state.store.clone();
    let device_id = match access {
        auth::AuthOutcome::Owner => None,
        auth::AuthOutcome::Device(device) => Some(device.id),
    };
    let loaded = tokio::task::spawn_blocking(move || match device_id {
        Some(device_id) => store.latest_conversation_turns_for_device(limit, &device_id),
        None => store.latest_conversation_turns(limit),
    })
    .await;

    match loaded {
        Ok(Ok(turns)) => Json(HistoryResponse { turns }).into_response(),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// True when a turn with this id is already in the store, which distinguishes
/// a client retry from a sequence-number collision.
fn turn_id_exists(store: &etern_core::Store, id: &TurnId) -> bool {
    store
        .turn_by_id(&id.to_string())
        .map(|t| t.is_some())
        .unwrap_or(false)
}

/// Model label for a recorded turn: the loaded model when the gateway has
/// discovered one, otherwise a neutral placeholder.
fn turn_model(state: &Arc<AppState>) -> String {
    state
        .props
        .fingerprint()
        .map(|f| f.model)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "llama.cpp".to_string())
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/etern/turns", axum::routing::post(record).get(history))
        .route("/etern/sync/transcript", axum::routing::get(device_history))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use crate::engine::Engine;
    use crate::health::Health;
    use crate::props::{EngineStatus, PropsCache};
    use axum::body::Body;
    use axum::http::Request;
    use etern_core::{
        DeviceProfile, Journal, Sandbox, Store, ToolRuntime, Turn as CoreTurn, UnavailableSearcher,
    };
    use serde_json::Value;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("etern-turns-{}-{}-{}", tag, std::process::id(), n))
    }

    fn test_state(token: Option<&str>) -> Arc<AppState> {
        let cfg = GatewayConfig {
            token: token.map(str::to_string),
            engine_base: "http://127.0.0.1:9".into(),
            ..GatewayConfig::default()
        };
        let dir = temp_dir("state");
        let sandbox = Sandbox::desktop(&dir).expect("sandbox");
        let tools = Arc::new(
            ToolRuntime::new(DeviceProfile::Desktop, sandbox, Arc::new(UnavailableSearcher))
                .expect("tools"),
        );
        let store = Store::open_in_memory().expect("store");
        let journal = Arc::new(Journal::new(dir.join("journal"), store.clone()).expect("journal"));
        Arc::new(AppState {
            engine: Engine::new(&cfg),
            health: Arc::new(Health::new(cfg.load_grace, 1)),
            props: PropsCache::new(cfg.props_ttl),
            status: std::sync::RwLock::new(EngineStatus::default()),
            store,
            journal,
            tools,
            tls_pin: None,
            lan_ip: None,
            tailscale_ip: None,
            cfg,
        })
    }

    fn app(state: Arc<AppState>) -> axum::Router {
        routes().with_state(state)
    }

    fn exchange(user: &str, reply: &str) -> Vec<Message> {
        vec![Message::user(user), Message::assistant(reply)]
    }

    fn body(turn_id: Option<&str>, messages: Vec<Message>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "turn_id": turn_id,
            "messages": messages,
        }))
        .unwrap()
    }

    async fn post(app: axum::Router, token: Option<&str>, payload: Vec<u8>) -> (StatusCode, Value) {
        let mut builder = Request::post("/etern/turns").header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(builder.body(Body::from(payload)).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get_history(
        app: axum::Router,
        token: Option<&str>,
        path: &str,
    ) -> (StatusCode, Value) {
        let mut builder = Request::get(path);
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn record_requires_auth_when_configured() {
        let state = test_state(Some("secret"));
        let (status, json) = post(app(state), None, body(None, exchange("hi", "yo"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(json["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn record_stores_a_chat_turn_under_the_desktop_identity() {
        let state = test_state(None);
        let store = state.store.clone();
        let (status, json) = post(app(state), None, body(None, exchange("hi", "yo"))).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["stored"], true);
        assert_eq!(json["seq"], 1);

        let turns = store.latest_turns(10).unwrap();
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn.kind, TurnKind::Chat);
        assert_eq!(turn.device_id, DESKTOP_DEVICE_ID);
        assert_eq!(turn.engine, EngineKind::Llama);
        assert!(!turn.model.is_empty());
        assert_eq!(turn.user_text(), Some("hi"));
        assert_eq!(turn.final_text(), Some("yo"));
    }

    #[tokio::test]
    async fn history_returns_recent_chat_and_sync_turns_for_the_owner() {
        let state = test_state(Some("secret"));
        let store = state.store.clone();
        store
            .insert_turn(
                &CoreTurn::new(
                    TurnKind::Journal,
                    "desktop",
                    1,
                    exchange("daily", "summary"),
                )
                .with_ts(1),
            )
            .unwrap();
        store
            .insert_turn(&CoreTurn::new(TurnKind::Chat, "desktop", 2, exchange("home", "hello")).with_ts(2))
            .unwrap();
        store
            .insert_turn(&CoreTurn::new(TurnKind::Sync, "phone", 1, exchange("mobile", "hi")).with_ts(3))
            .unwrap();

        let (status, json) = get_history(app(state), Some("secret"), "/etern/turns?limit=1").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let turns = json["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["kind"], "sync");
        assert_eq!(turns[0]["messages"][0]["content"], "mobile");
    }

    #[tokio::test]
    async fn history_requires_owner_authentication() {
        let state = test_state(Some("secret"));
        state
            .store
            .register_device("phone", "Phone", "device-secret", 1)
            .unwrap();
        let router = app(state.clone());
        let (status, json) = get_history(router, None, "/etern/turns").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(json["error"]["message"].is_string());

        let (status, _) = get_history(app(state), Some("device-secret"), "/etern/turns").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_transcript_keeps_shared_chat_and_only_its_own_offline_turns() {
        let state = test_state(None);
        state
            .store
            .register_device("phone", "Phone", "device-secret", 1)
            .unwrap();
        state
            .store
            .insert_turn(&CoreTurn::new(TurnKind::Chat, "desktop", 1, exchange("desktop", "reply")).with_ts(1))
            .unwrap();
        state
            .store
            .insert_turn(&CoreTurn::new(TurnKind::Sync, "phone", 1, exchange("phone", "reply")).with_ts(2))
            .unwrap();
        state
            .store
            .insert_turn(&CoreTurn::new(TurnKind::Sync, "other-phone", 1, exchange("private", "reply")).with_ts(3))
            .unwrap();

        let (status, json) = get_history(
            app(state),
            Some("device-secret"),
            "/etern/sync/transcript?limit=10",
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{json}");
        let turns = json["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["messages"][0]["content"], "desktop");
        assert_eq!(turns[1]["messages"][0]["content"], "phone");
    }

    #[tokio::test]
    async fn device_transcript_requires_a_valid_device_or_owner_token() {
        let state = test_state(None);
        let (status, _) = get_history(
            app(state),
            None,
            "/etern/sync/transcript",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn seq_increments_per_turn() {
        let state = test_state(None);
        let router = app(state.clone());
        let (_, first) = post(router, None, body(None, exchange("one", "1"))).await;
        let (_, second) = post(app(state.clone()), None, body(None, exchange("two", "2"))).await;
        assert_eq!(first["seq"], 1);
        assert_eq!(second["seq"], 2);
        assert_eq!(state.store.latest_turns(10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_replayed_turn_id_is_absorbed_not_duplicated() {
        let state = test_state(None);
        let store = state.store.clone();
        let id = TurnId::now_v7().to_string();
        let payload = body(Some(&id), exchange("hi", "yo"));

        let (_, first) = post(app(state.clone()), None, payload.clone()).await;
        let (_, again) = post(app(state.clone()), None, payload).await;
        assert_eq!(first["stored"], true);
        assert_eq!(again["stored"], false);
        assert_eq!(again["id"], id);
        assert_eq!(store.latest_turns(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn desktop_recording_does_not_disturb_a_phones_seq_space() {
        let state = test_state(None);
        let store = state.store.clone();
        // A phone turn already owns (device, 1..=3).
        for seq in 1..=3i64 {
            let turn = CoreTurn::new(
                TurnKind::Sync,
                "pixel-7",
                seq,
                exchange(&format!("phone {seq}"), "ok"),
            );
            store.insert_turn(&turn).unwrap();
        }

        let (status, json) = post(app(state.clone()), None, body(None, exchange("hi", "yo"))).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["seq"], 1, "desktop must allocate from its own space");
        assert_eq!(store.max_seq_for_device("pixel-7").unwrap(), 3);
    }

    #[tokio::test]
    async fn rejects_empty_and_userless_batches() {
        let state = test_state(None);
        let (status, _) = post(app(state.clone()), None, body(None, vec![])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, json) = post(
            app(state.clone()),
            None,
            body(None, vec![Message::assistant("only a reply")]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("user message"));
    }

    #[tokio::test]
    async fn rejects_a_malformed_turn_id() {
        let state = test_state(None);
        let (status, json) = post(
            app(state),
            None,
            body(Some("not-a-uuid"), exchange("hi", "yo")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"]["message"].as_str().unwrap().contains("turn_id"));
    }

    #[tokio::test]
    async fn rejects_an_oversized_batch() {
        let state = test_state(None);
        let messages: Vec<Message> = (0..=MAX_RECORD_MESSAGES)
            .map(|i| Message::user(format!("msg {i}")))
            .collect();
        let (status, json) = post(app(state), None, body(None, messages)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"]["message"].as_str().unwrap().contains("limit"));
    }

    #[tokio::test]
    async fn honours_an_explicit_ts_and_model() {
        let state = test_state(None);
        let store = state.store.clone();
        let payload = serde_json::to_vec(&json!({
            "messages": exchange("hi", "yo"),
            "model": "qwen3-32b",
            "ts": 1_737_000_000_000i64,
        }))
        .unwrap();
        let (status, json) = post(app(state), None, payload).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        let turn = &store.latest_turns(1).unwrap()[0];
        assert_eq!(turn.ts, 1_737_000_000_000);
        assert_eq!(turn.model, "qwen3-32b");
    }
}
