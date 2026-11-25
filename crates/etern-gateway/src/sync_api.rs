//! Sync protocol endpoints (PLAN §4a/§4b, M6).
//!
//! The edge↔desktop contract:
//!
//!   * `POST /etern/sync/push` — an edge device replays its durable outbox.
//!     Replayed UUIDv7 `turn_id`s are absorbed as duplicates; if a stale client
//!     reuses a `(device_id, seq)` for a different turn, the gateway assigns a
//!     fresh sequence instead of dropping that turn. Only new turns marked as
//!     offline are included in the sync journal digest.
//!   * `GET /etern/sync/window` — the desktop proposes the 8k window from its
//!     *own* store, so the phone never has to send data to stay in sync. The
//!     candidate set is the newest turns the phone is allowed to see: desktop
//!     chat turns plus the phone's own synced turns (foreign devices' sync
//!     batches stay private).
//!   * `POST /etern/memory/sync` — merge append-only phone memory into the
//!     desktop's canonical `MEMORY.md` and return the merged file.
//!   * `POST /etern/personality/sync` — reconcile the editable shared
//!     style preference and return the gateway's canonical value.
//!
//! The push handler is deliberately ordered: validate, insert, then summarise.
//! A crash between insert and journal write loses nothing — the turns are
//! canonical and the daily catch-up sweep will cover them.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use etern_core::{CoreError, Journal, MemoryStore, PushAck, SyncBatch, Turn, TurnKind, MAX_SYNC_BATCH};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth;
use crate::journal::EngineSummarizer;
use crate::props::TokenSource;
use crate::window::{self, WindowProposal, WindowRequest};
use crate::AppState;

/// Candidate turns pulled from the store per window request. The trimmer keeps
/// the newest that fit an 8k budget, so a few hundred is plenty headroom for
/// short turns without letting a client force an unbounded scan.
const MAX_WINDOW_CANDIDATES: usize = 500;
const MAX_MEMORY_SYNC_BYTES: usize = 64 * 1024;
const MAX_PERSONALITY_CHARS: usize = 2000;
const MAX_PERSONALITY_BYTES: usize = 8 * 1024;
const SYNC_SEQ_RETRIES: usize = 8;
const PERSONALITY_META_KEY: &str = "shared_personality";

/// Turns pushed from an edge device are recorded against this model label;
/// the phone's exact quantised model name is not yet carried on the wire.
const EDGE_MODEL_LABEL: &str = "edge";

fn error(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": {"message": message.to_string()}}))).into_response()
}

/// Validates a batch before touching the store, materialising wire turns as
/// canonical ones. Errors are returned as human-readable rejections.
fn validate_batch(batch: &SyncBatch) -> Result<Vec<(Turn, bool)>, String> {
    if batch.device_id.trim().is_empty() {
        return Err("device_id is required".into());
    }
    if batch.turns.is_empty() {
        return Err("batch contains no turns".into());
    }
    if batch.turns.len() > MAX_SYNC_BATCH {
        return Err(format!(
            "batch of {} turns exceeds the {MAX_SYNC_BATCH}-turn limit",
            batch.turns.len()
        ));
    }
    batch
        .turns
        .iter()
        .map(|st| {
            st.to_turn(&batch.device_id, EDGE_MODEL_LABEL)
                .map(|turn| (turn, st.offline))
                .map_err(|e| format!("turn {}: {e}", st.turn_id))
        })
        .collect()
}

/// `POST /etern/sync/push` — replay an outbox batch.
async fn push(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<SyncBatch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let axum::Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let turns = match validate_batch(&batch) {
        Ok(t) => t,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };

    // Insert off the async worker: SQLite is a mutex + WAL writes, and a
    // 2000-turn batch should not park a tokio thread.
    let store = state.store.clone();
    let device_id = batch.device_id.clone();
    let inserted = tokio::task::spawn_blocking(move || {
        let mut accepted = Vec::new();
        let mut offline_accepted = Vec::new();
        let mut duplicates = 0usize;
        let mut turns = turns;
        turns.sort_by_key(|(turn, _)| turn.seq);
        for (turn, offline) in turns {
            match insert_sync_turn(&store, turn) {
                Ok(Some(turn)) => {
                    if offline {
                        offline_accepted.push(turn.clone());
                    }
                    accepted.push(turn);
                }
                Ok(None) => duplicates += 1,
                Err(msg) => return Err(msg),
            }
        }
        Ok::<_, String>((accepted, offline_accepted, duplicates, device_id))
    })
    .await;

    let (accepted, offline_accepted, duplicates, device_id) = match inserted {
        Ok(Ok(v)) => v,
        Ok(Err(msg)) => return error(StatusCode::INTERNAL_SERVER_ERROR, msg),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    // Journal the batch only when it brought something new; a pure replay must
    // not mint a second digest file.
    let accepted_count = accepted.len();
    let journal_file = if offline_accepted.is_empty() {
        None
    } else {
        let journal: Arc<Journal> = state.journal.clone();
        let summarizer = EngineSummarizer::new(&state.cfg.engine_base);
        let journal_device = device_id.clone();
        let file = tokio::task::spawn_blocking(move || {
            journal
                .write_sync(Utc::now(), &journal_device, &offline_accepted, &summarizer)
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .map_err(|e| e.to_string())
        })
        .await;
        match file {
            Ok(Ok(name)) => Some(name),
            Ok(Err(e)) => {
                // The turns are already canonical; a failed digest is a
                // degraded outcome, not a failed push.
                tracing::warn!(error = %e, "sync journal write failed");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "sync journal task panicked");
                None
            }
        }
    };

    tracing::info!(
        device = %device_id,
        accepted = accepted_count,
        duplicates,
        journal = journal_file.as_deref().unwrap_or("-"),
        "sync push"
    );

    Json(PushAck {
        accepted: accepted_count,
        duplicates,
        journal: journal_file,
    })
    .into_response()
}

/// Inserts a new sync turn, repairing a stale/reused device sequence instead
/// of treating a different turn as an idempotent replay.
fn insert_sync_turn(store: &etern_core::Store, mut turn: Turn) -> Result<Option<Turn>, String> {
    let id = turn.id.to_string();
    for _ in 0..SYNC_SEQ_RETRIES {
        if store.turn_by_id(&id).map_err(|e| e.to_string())?.is_some() {
            return Ok(None);
        }

        let max_seq = store
            .max_seq_for_device(&turn.device_id)
            .map_err(|e| e.to_string())?;
        turn.seq = turn.seq.max(max_seq.saturating_add(1));
        match store.insert_turn(&turn) {
            Ok(true) => return Ok(Some(turn)),
            Ok(false) => {
                // A concurrent push may have stored this ID or taken the same
                // sequence. Only the former is a duplicate; retry the latter.
                if store.turn_by_id(&id).map_err(|e| e.to_string())?.is_some() {
                    return Ok(None);
                }
            }
            Err(e) => return Err(format!("insert failed for {id}: {e}")),
        }
    }

    Err(format!(
        "could not allocate a sync sequence for {id} after {SYNC_SEQ_RETRIES} attempts"
    ))
}

/// Query for `GET /etern/sync/window`. Same knobs as `WindowRequest` minus the
/// turns, which the desktop sources from its own store.
#[derive(Debug, Deserialize)]
pub struct WindowQuery {
    pub edge_ctx: Option<u64>,
    pub response_reserve: Option<u64>,
    pub overhead_tokens: Option<u64>,
    /// Build the window for this device: it receives desktop chat turns plus
    /// *its own* synced turns, never another device's offline batches.
    pub device_id: Option<String>,
}

/// Response adds the store watermark to the core proposal so the phone can
/// record how far the desktop's transcript reached.
#[derive(Debug, Serialize)]
pub struct SyncWindow {
    #[serde(flatten)]
    pub proposal: WindowProposal,
    /// `ts` of the newest turn eligible for this device's window, whether or
    /// not it fit. The phone uses this to detect a stale view after reconnect.
    pub store_head_ts: Option<i64>,
}

/// `GET /etern/sync/window` — propose the edge window from desktop state.
async fn window_endpoint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<WindowQuery>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let device_id = q.device_id.unwrap_or_default();
    let store = state.store.clone();
    let device_for_filter = device_id.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        store
            .latest_turns(MAX_WINDOW_CANDIDATES)
            .map(|all| latest_for_device(&all, &device_for_filter))
    })
    .await;

    let candidates = match loaded {
        Ok(Ok(turns)) => turns,
        Ok(Err(e)) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let store_head_ts = candidates.last().map(|t| t.ts);

    let req = WindowRequest {
        turns: candidates,
        edge_ctx: q.edge_ctx.unwrap_or(window::DEFAULT_EDGE_CTX),
        response_reserve: q.response_reserve.unwrap_or(window::DEFAULT_RESPONSE_RESERVE),
        overhead_tokens: q.overhead_tokens,
    };

    let fingerprint = state.props.fingerprint();
    let template_sha = fingerprint
        .as_ref()
        .map(|f| f.template_sha.clone())
        .unwrap_or_default();
    let token_source = fingerprint
        .as_ref()
        .map(|f| f.token_source)
        .unwrap_or(TokenSource::Estimated);

    match window::propose(&req, &template_sha, token_source) {
        Ok(proposal) => {
            if let Some(seq) = proposal.window_seq {
                // Watermark for observability; the phone keeps its own copy.
                let key = format!("window_seq:{device_id}");
                if let Err(e) = state.store.set_meta(&key, &seq.to_string()) {
                    tracing::warn!(error = %e, key, "window watermark write failed");
                }
            }
            Json(SyncWindow {
                proposal,
                store_head_ts,
            })
            .into_response()
        }
        Err(msg) => error(StatusCode::BAD_REQUEST, msg),
    }
}

/// Body of `POST /etern/memory/sync`.
#[derive(Debug, Deserialize)]
struct MemorySyncRequest {
    content: String,
}

/// Merges a device's append-only memory into the desktop's canonical memory and
/// returns the union for the device to install locally.
async fn sync_memory(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<MemorySyncRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_owner_or_device(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let axum::Json(request) = match body {
        Ok(body) => body,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    if request.content.len() > MAX_MEMORY_SYNC_BYTES {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("memory exceeds the {MAX_MEMORY_SYNC_BYTES}-byte sync limit"),
        );
    }

    let root = state.cfg.tool_root.clone();
    let merged = tokio::task::spawn_blocking(move || {
        MemoryStore::new(&root)?.merge(&request.content)
    })
    .await;

    match merged {
        Ok(Ok(content)) => Json(json!({ "content": content })).into_response(),
        Ok(Err(err)) => {
            let status = if matches!(&err, CoreError::Budget(_)) {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error(status, err)
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct PersonalitySetting {
    value: String,
    updated_at: i64,
}

impl Default for PersonalitySetting {
    fn default() -> Self {
        Self {
            value: String::new(),
            updated_at: 0,
        }
    }
}

/// Body of `POST /etern/personality/sync`.
#[derive(Debug, Deserialize)]
struct PersonalitySyncRequest {
    value: String,
    updated_at: i64,
}

/// Reconciles an editable, shared personality preference using its local
/// update timestamp. The gateway's canonical value is returned to every client.
async fn sync_personality(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<PersonalitySyncRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let axum::Json(request) = match body {
        Ok(body) => body,
        Err(err) => return error(StatusCode::BAD_REQUEST, err),
    };
    let value = request.value.trim().to_string();
    if value.chars().count() > MAX_PERSONALITY_CHARS || value.len() > MAX_PERSONALITY_BYTES {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "personality exceeds the {MAX_PERSONALITY_CHARS}-character or \
                 {MAX_PERSONALITY_BYTES}-byte limit"
            ),
        );
    }
    if request.updated_at < 0 {
        return error(StatusCode::BAD_REQUEST, "updated_at cannot be negative");
    }

    let incoming = PersonalitySetting {
        value,
        updated_at: request.updated_at,
    };
    let store = state.store.clone();
    let canonical = tokio::task::spawn_blocking(move || {
        for _ in 0..8 {
            let stored = store
                .get_meta(PERSONALITY_META_KEY)
                .map_err(|e| e.to_string())?;
            let current = match stored.as_deref() {
                Some(raw) => serde_json::from_str::<PersonalitySetting>(raw)
                    .map_err(|e| format!("stored personality is invalid: {e}"))?,
                None => PersonalitySetting::default(),
            };
            if incoming.updated_at <= current.updated_at {
                return Ok(current);
            }

            let encoded = serde_json::to_string(&incoming)
                .map_err(|e| format!("could not encode personality: {e}"))?;
            if store
                .compare_and_set_meta(PERSONALITY_META_KEY, stored.as_deref(), &encoded)
                .map_err(|e| e.to_string())?
            {
                return Ok(incoming);
            }
        }
        Err::<PersonalitySetting, String>(
            "personality changed repeatedly while syncing; retry".into(),
        )
    })
    .await;

    match canonical {
        Ok(Ok(setting)) => Json(setting).into_response(),
        Ok(Err(err)) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

/// Newest slice the given device may hold: chat turns from anywhere (the
/// shared conversation) plus this device's own synced turns, re-labelled as
/// chat so the `TrimPolicy::ChatOnly` pass keeps them. Foreign `sync` turns
/// stay private; `journal`/`system` turns never enter an edge window.
fn latest_for_device(all: &[Turn], device_id: &str) -> Vec<Turn> {
    let mut out: Vec<Turn> = all
        .iter()
        .filter(|t| {
            t.kind == TurnKind::Chat || (t.kind == TurnKind::Sync && t.device_id == device_id)
        })
        .cloned()
        .collect();
    for t in out.iter_mut() {
        t.kind = TurnKind::Chat;
    }
    // latest_turns returns chronological order; keep it.
    out
}

/// Body of `POST /etern/history/clear`. The acknowledgement is mandatory:
/// the operation is unrecoverable, and a stray `curl` should not be able to
/// wipe months of transcript.
#[derive(Debug, Deserialize)]
pub struct ClearRequest {
    pub confirm: bool,
    /// When set, only this device's turns are deleted. A paired phone uses it
    /// to dig itself out of a poisoned context; the owner may omit it to purge
    /// everything.
    #[serde(default)]
    pub device_id: Option<String>,
}

/// `POST /etern/history/clear` — purge turns from the desktop store.
///
/// Deleting the *whole* transcript is owner-only: [`auth::check_any`] also
/// admits paired-device tokens, and a lost phone must not be able to erase the
/// household's history. A device token may still clear its own turns, which is
/// what an edge app needs when its own garbage keeps being re-offered in the
/// sync window. Journal files and their ledger survive either way — they are
/// the record of what happened, not live context.
async fn clear_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<ClearRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let outcome = match auth::check_any(&state.cfg, &state.store, &headers) {
        Ok(outcome) => outcome,
        Err(err) => return err.into(),
    };

    let axum::Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    if !req.confirm {
        return error(
            StatusCode::BAD_REQUEST,
            "clearing the history is irreversible; send {\"confirm\": true} to proceed",
        );
    }

    let scope = match (&outcome, req.device_id.as_deref()) {
        (auth::AuthOutcome::Owner, None) => None,
        (auth::AuthOutcome::Owner, Some(device)) => Some(device.to_string()),
        (auth::AuthOutcome::Device(record), None) => Some(record.id.clone()),
        (auth::AuthOutcome::Device(record), Some(device)) => {
            if device != record.id {
                return error(
                    StatusCode::FORBIDDEN,
                    format!("device {} may not clear device {device}", record.id),
                );
            }
            Some(record.id.clone())
        }
    };

    let store = state.store.clone();
    let cleared = match tokio::task::spawn_blocking(move || match scope {
        None => store.delete_all_turns(),
        Some(device) => store.delete_turns_for_device(&device),
    })
    .await
    {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    tracing::warn!(
        turns = cleared,
        device = req.device_id.as_deref().unwrap_or("*"),
        caller = match &outcome {
            auth::AuthOutcome::Owner => "owner",
            auth::AuthOutcome::Device(r) => r.id.as_str(),
        },
        "conversation history cleared"
    );
    Json(json!({ "cleared": cleared })).into_response()
}

pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/etern/sync/push", axum::routing::post(push))
        .route("/etern/sync/window", axum::routing::get(window_endpoint))
        .route("/etern/memory/sync", axum::routing::post(sync_memory))
        .route(
            "/etern/personality/sync",
            axum::routing::post(sync_personality),
        )
        .route("/etern/history/clear", axum::routing::post(clear_history))
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
        DeviceProfile, EngineKind, JournalKind, Message, Sandbox, Store, SyncTurn, ToolRuntime,
        TurnId, UnavailableSearcher,
    };
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    use tower::ServiceExt;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("etern-sync-{}-{}-{}", tag, std::process::id(), n))
    }

    fn test_state(token: Option<&str>) -> (Arc<AppState>, PathBuf) {
        let dir = temp_dir("state");
        let cfg = GatewayConfig {
            token: token.map(str::to_string),
            tool_root: dir.clone(),
            // Dead port: the sync summariser falls back to the deterministic
            // extractive digest, so journal assertions are stable regardless
            // of whatever engine happens to own :8080 during the test run.
            engine_base: "http://127.0.0.1:9".into(),
            ..GatewayConfig::default()
        };
        let sandbox = Sandbox::desktop(&dir).expect("sandbox");
        let tools = Arc::new(
            ToolRuntime::new(DeviceProfile::Desktop, sandbox, Arc::new(UnavailableSearcher))
                .expect("tools"),
        );
        // Journal and store must share one connection: write_sync records a
        // journal entry in the store, and tests assert on both.
        let store = Store::open_in_memory().expect("store");
        let journal_dir = dir.join("journal");
        let journal = Arc::new(Journal::new(&journal_dir, store.clone()).expect("journal"));
        let state = Arc::new(AppState {
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
        });
        (state, journal_dir)
    }

    fn app(state: Arc<AppState>) -> axum::Router {
        routes().with_state(state)
    }

    fn sync_turn(seq: i64, text: &str) -> SyncTurn {
        SyncTurn {
            turn_id: TurnId::now_v7().to_string(),
            ts: 1_737_000_000_000 + seq * 1000,
            seq,
            offline: true,
            messages: vec![Message::user(text), Message::assistant(format!("echo {text}"))],
        }
    }

    fn batch(device: &str, turns: Vec<SyncTurn>) -> Vec<u8> {
        serde_json::to_vec(&SyncBatch {
            device_id: device.to_string(),
            turns,
        })
        .unwrap()
    }

    async fn post(
        app: axum::Router,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::post(uri).header("content-type", "application/json");
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(builder.body(Body::from(body)).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get(app: axum::Router, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut builder = Request::get(uri);
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn sync_files(dir: &Path) -> Vec<String> {
        match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with("sync-").then_some(name)
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[tokio::test]
    async fn push_requires_auth_when_configured() {
        let (state, _) = test_state(Some("s3cret"));
        let (status, body) = post(
            app(state),
            "/etern/sync/push",
            None,
            batch("phone", vec![sync_turn(1, "hi")]),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"].as_str(), Some("authentication_error"));
    }

    #[tokio::test]
    async fn memory_sync_requires_auth_when_configured() {
        let (state, _) = test_state(Some("s3cret"));
        let (status, body) = post(
            app(state),
            "/etern/memory/sync",
            None,
            serde_json::to_vec(&json!({"content": "- phone fact\n"})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"].as_str(), Some("authentication_error"));
    }

    #[tokio::test]
    async fn memory_sync_rejects_anonymous_requests_when_owner_auth_is_disabled() {
        let (state, _) = test_state(None);
        let (status, _) = post(
            app(state),
            "/etern/memory/sync",
            None,
            serde_json::to_vec(&json!({"content": ""})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn memory_sync_accepts_a_paired_device_token_without_owner_auth() {
        let (state, _) = test_state(None);
        state.store.register_device("phone", "phone", "device-token", 0).unwrap();
        let (status, body) = post(
            app(state),
            "/etern/memory/sync",
            Some("device-token"),
            serde_json::to_vec(&json!({"content": "- phone fact\n"})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["content"], "- phone fact\n");
    }

    #[tokio::test]
    async fn memory_sync_merges_both_sides_and_is_idempotent() {
        let (state, _) = test_state(Some("owner"));
        let path = state.cfg.tool_root.join("memory").join("MEMORY.md");
        std::fs::write(&path, "- desktop fact\n- shared fact\n").unwrap();

        let (status, body) = post(
            app(state.clone()),
            "/etern/memory/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"content": "- phone fact\n- shared fact\n"})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["content"].as_str(),
            Some("- desktop fact\n- shared fact\n- phone fact\n")
        );

        let (status, replay) = post(
            app(state),
            "/etern/memory/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"content": "- phone fact\n"})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay["content"], body["content"]);
    }

    #[tokio::test]
    async fn memory_sync_reports_cap_conflicts_without_overwriting_memory() {
        let (state, _) = test_state(Some("owner"));
        let path = state.cfg.tool_root.join("memory").join("MEMORY.md");
        std::fs::write(&path, "- existing fact\n").unwrap();

        let (status, body) = post(
            app(state.clone()),
            "/etern/memory/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"content": format!("- {}\n", "x".repeat(9000))})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"]["message"].as_str().unwrap().contains("exceeding"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "- existing fact\n");
    }

    #[tokio::test]
    async fn personality_sync_keeps_the_newest_setting() {
        let (state, _) = test_state(Some("owner"));

        let (status, first) = post(
            app(state.clone()),
            "/etern/personality/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"value": "warm", "updated_at": 10})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["value"], "warm");

        let (status, stale) = post(
            app(state.clone()),
            "/etern/personality/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"value": "formal", "updated_at": 9})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stale["value"], "warm");

        let (status, newer) = post(
            app(state),
            "/etern/personality/sync",
            Some("owner"),
            serde_json::to_vec(&json!({"value": "", "updated_at": 11})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(newer["value"], "");
        assert_eq!(newer["updated_at"], 11);
    }

    #[tokio::test]
    async fn personality_sync_requires_auth_and_enforces_its_size_limit() {
        let (state, _) = test_state(Some("owner"));
        let (status, _) = post(
            app(state.clone()),
            "/etern/personality/sync",
            None,
            serde_json::to_vec(&json!({"value": "warm", "updated_at": 10})).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = post(
            app(state),
            "/etern/personality/sync",
            Some("owner"),
            serde_json::to_vec(&json!({
                "value": "x".repeat(MAX_PERSONALITY_BYTES + 1),
                "updated_at": 10
            }))
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn personality_sync_allows_anonymous_access_when_auth_is_disabled() {
        let (state, _) = test_state(None);
        let (status, body) = post(
            app(state),
            "/etern/personality/sync",
            None,
            serde_json::to_vec(&json!({"value": "warm", "updated_at": 10})).unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], "warm");
    }

    #[tokio::test]
    async fn push_rejects_empty_and_oversized_batches() {
        let (state, _) = test_state(None);
        let (status, _) =
            post(app(state.clone()), "/etern/sync/push", None, batch("phone", vec![])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let huge = (0..=MAX_SYNC_BATCH as i64)
            .map(|i| sync_turn(i + 1, "x"))
            .collect();
        let (status, body) = post(app(state), "/etern/sync/push", None, batch("phone", huge)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("limit"));
    }

    #[tokio::test]
    async fn push_rejects_a_malformed_turn_id() {
        let (state, _) = test_state(None);
        let mut t = sync_turn(1, "hi");
        t.turn_id = "not-a-uuid".into();
        let (status, body) =
            post(app(state), "/etern/sync/push", None, batch("phone", vec![t])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("turn_id"));
    }

    #[tokio::test]
    async fn push_stores_turns_and_writes_one_sync_journal() {
        let (state, journal_dir) = test_state(None);
        let turns = vec![
            sync_turn(1, "the cat needs vet on friday"),
            sync_turn(2, "thanks"),
        ];
        let (status, body) =
            post(app(state.clone()), "/etern/sync/push", None, batch("pixel-7", turns)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["accepted"].as_u64(), Some(2));
        assert_eq!(body["duplicates"].as_u64(), Some(0));
        assert!(body["journal"].as_str().unwrap().starts_with("sync-"));

        let stored = state.store.all_turns().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].kind, TurnKind::Sync);
        assert_eq!(stored[0].engine, EngineKind::Edge);
        assert_eq!(stored[0].device_id, "pixel-7");

        let files = sync_files(&journal_dir);
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(journal_dir.join(&files[0])).unwrap();
        assert!(text.contains("pixel-7"));
        assert!(text.to_lowercase().contains("vet"));
    }

    #[tokio::test]
    async fn online_android_turns_are_stored_without_a_sync_journal() {
        let (state, journal_dir) = test_state(None);
        let mut online = sync_turn(1, "connected desktop-model conversation");
        online.offline = false;

        let (status, body) = post(
            app(state.clone()),
            "/etern/sync/push",
            None,
            batch("pixel-7", vec![online]),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["accepted"].as_u64(), Some(1));
        assert!(body["journal"].is_null());
        assert!(sync_files(&journal_dir).is_empty());
        assert_eq!(state.store.all_turns().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn push_repairs_reused_sequences_without_dropping_distinct_turns() {
        let (state, _) = test_state(None);
        let first = sync_turn(0, "first online phone turn");
        let first_payload = batch("phone", vec![first]);
        let (status, first_ack) =
            post(app(state.clone()), "/etern/sync/push", None, first_payload).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first_ack["accepted"].as_u64(), Some(1));

        // The Android outbox is cleared after an ack, so a client that derives
        // its next sequence from that file can submit zero again.
        let second = sync_turn(0, "second online phone turn");
        let second_payload = batch("phone", vec![second]);
        let (status, second_ack) = post(
            app(state.clone()),
            "/etern/sync/push",
            None,
            second_payload.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second_ack["accepted"].as_u64(), Some(1));
        assert_eq!(second_ack["duplicates"].as_u64(), Some(0));

        let stored = state.store.latest_conversation_turns(10).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].messages[0].content, "first online phone turn");
        assert_eq!(stored[1].messages[0].content, "second online phone turn");
        assert!(stored[0].seq < stored[1].seq);

        let (_, replay_ack) = post(app(state), "/etern/sync/push", None, second_payload).await;
        assert_eq!(replay_ack["accepted"].as_u64(), Some(0));
        assert_eq!(replay_ack["duplicates"].as_u64(), Some(1));
    }

    #[tokio::test]
    async fn replaying_a_batch_produces_duplicates_and_no_second_journal() {
        let (state, journal_dir) = test_state(None);
        let turns = vec![sync_turn(1, "offline chat one"), sync_turn(2, "offline chat two")];
        let payload = batch("pixel-7", turns);

        let (status, first) =
            post(app(state.clone()), "/etern/sync/push", None, payload.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["accepted"].as_u64(), Some(2));

        // Chaos case from PLAN §10: replay the outbox twice.
        let (_, second) = post(app(state.clone()), "/etern/sync/push", None, payload).await;
        assert_eq!(second["accepted"].as_u64(), Some(0));
        assert_eq!(second["duplicates"].as_u64(), Some(2));
        assert_eq!(second["journal"], Value::Null, "a pure replay must not mint a digest");

        assert_eq!(state.store.all_turns().unwrap().len(), 2);
        assert_eq!(sync_files(&journal_dir).len(), 1);
    }

    #[tokio::test]
    async fn window_includes_own_sync_turns_but_not_foreign_ones() {
        let (state, _) = test_state(None);
        let mine = sync_turn(1, "my offline note about kites");
        let theirs = sync_turn(1, "someone elses secret about taxes");

        let store = state.store.clone();
        let mine_turn = mine.to_turn("pixel-7", "edge").unwrap();
        let theirs_turn = theirs.to_turn("galaxy-s", "edge").unwrap();
        store.insert_turn(&mine_turn).unwrap();
        store.insert_turn(&theirs_turn).unwrap();
        let chat = Turn::new(
            TurnKind::Chat,
            "desktop",
            1,
            vec![Message::user("desktop chat about bread")],
        );
        store.insert_turn(&chat).unwrap();

        let (status, body) = get(app(state), "/etern/sync/window?device_id=pixel-7", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["apply"].as_str(), Some("replace"));
        let text = body.to_string();
        assert!(text.contains("kites"), "own sync turns must be in the window: {text}");
        assert!(text.contains("bread"), "desktop chat turns must be in the window");
        assert!(!text.contains("taxes"), "foreign sync turns must stay private");
        assert!(body["store_head_ts"].is_number());
    }

    #[tokio::test]
    async fn window_trims_to_budget_and_reports_replacement() {
        let (state, _) = test_state(None);
        let filler = "y".repeat(4000);
        for i in 0..40i64 {
            let t = Turn::new(
                TurnKind::Chat,
                "desktop",
                i,
                vec![Message::user(format!("{i} {filler}"))],
            );
            state.store.insert_turn(&t).unwrap();
        }
        let (_, body) = get(app(state), "/etern/sync/window?edge_ctx=8192", None).await;
        assert_eq!(body["truncated"].as_bool(), Some(true));
        assert_eq!(body["apply"].as_str(), Some("replace"));
        let est = body["est_tokens"].as_u64().unwrap();
        let budget = body["budget_tokens"].as_u64().unwrap();
        assert!(est <= budget, "proposal must fit its budget: {est} > {budget}");
    }

    #[tokio::test]
    async fn window_rejects_impossible_budget() {
        let (state, _) = test_state(None);
        let (status, body) = get(app(state), "/etern/sync/window?edge_ctx=100", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("context"));
    }

    fn clear_body(confirm: bool) -> Vec<u8> {
        serde_json::to_vec(&json!({ "confirm": confirm })).unwrap()
    }

    #[tokio::test]
    async fn clear_requires_auth_when_configured() {
        let (state, _) = test_state(Some("s3cret"));
        let (status, body) = post(
            app(state),
            "/etern/history/clear",
            None,
            clear_body(true),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["type"].as_str(), Some("authentication_error"));
    }

    #[tokio::test]
    async fn clear_refuses_without_confirmation() {
        let (state, _) = test_state(None);
        state
            .store
            .insert_turn(&sync_turn(1, "keep me").to_turn("pixel-7", "edge").unwrap())
            .unwrap();

        let (status, body) = post(
            app(state.clone()),
            "/etern/history/clear",
            None,
            clear_body(false),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"].as_str().unwrap().contains("confirm"));
        assert_eq!(state.store.all_turns().unwrap().len(), 1, "must not clear");
    }

    #[tokio::test]
    async fn clear_removes_turns_but_keeps_devices_and_journal_ledger() {
        let (state, journal_dir) = test_state(None);
        let turn = sync_turn(1, "the cat needs vet on friday")
            .to_turn("pixel-7", "edge")
            .unwrap();
        state.store.insert_turn(&turn).unwrap();
        state
            .store
            .record_journal_entry("2026-07-29", JournalKind::Daily, "c:/j/2026-07-29.txt")
            .unwrap();

        let (status, body) = post(
            app(state.clone()),
            "/etern/history/clear",
            None,
            clear_body(true),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cleared"].as_u64(), Some(1));
        assert!(state.store.all_turns().unwrap().is_empty());
        assert!(state.store.search("monstera", 5).unwrap().is_empty());
        assert!(state
            .store
            .journal_entry_exists("2026-07-29", JournalKind::Daily)
            .unwrap());

        // A later window request is empty rather than resurrecting the purge.
        let (_, window) = get(app(state), "/etern/sync/window?device_id=pixel-7", None).await;
        assert_eq!(window["messages"].as_array().map(Vec::len), Some(0));
        let _ = journal_dir;
    }

    #[tokio::test]
    async fn a_device_may_clear_its_own_turns_but_not_another_devices() {
        let (state, _) = test_state(Some("s3cret"));
        state
            .store
            .register_device("pixel-7", "Fold", "dev-token", 1_000)
            .unwrap();
        state
            .store
            .insert_turn(&sync_turn(1, "my garbage").to_turn("pixel-7", "edge").unwrap())
            .unwrap();
        state
            .store
            .insert_turn(&sync_turn(1, "someone elses note").to_turn("galaxy-s", "edge").unwrap())
            .unwrap();
        let chat = Turn::new(TurnKind::Chat, "desktop", 1, vec![Message::user("desktop chat")]);
        state.store.insert_turn(&chat).unwrap();

        // A device token with no explicit scope clears only its own turns.
        let (status, body) = post(
            app(state.clone()),
            "/etern/history/clear",
            Some("dev-token"),
            clear_body(true),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cleared"].as_u64(), Some(1));
        let left = state.store.all_turns().unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|t| t.device_id != "pixel-7"));

        // Reaching for another device's turns is refused.
        let (status, _) = post(
            app(state.clone()),
            "/etern/history/clear",
            Some("dev-token"),
            serde_json::to_vec(&json!({ "confirm": true, "device_id": "galaxy-s" })).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(state.store.all_turns().unwrap().len(), 2);

        // The owner token still purges everything.
        let (status, body) = post(
            app(state),
            "/etern/history/clear",
            Some("s3cret"),
            clear_body(true),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cleared"].as_u64(), Some(2));
    }
}
