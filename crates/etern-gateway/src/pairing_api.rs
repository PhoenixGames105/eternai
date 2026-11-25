//! Device pairing and the device registry (PLAN §8).
//!
//! A QR code carrying the *static* token would hand every device the admin
//! credential, so pairing mints a per-device token instead:
//!
//! 1. The owner (desktop app, static token) calls `POST /etern/pairings` and
//!    gets a short-lived one-time **pair code**.
//! 2. The desktop renders that code as a QR next to the gateway's host/port and
//!    TLS pin. The phone scans it and calls `POST /etern/pair` — no auth header,
//!    the code *is* the auth.
//! 3. The gateway returns a `device_id` + long-lived `token`. The phone stores
//!    them and sends `Authorization: Bearer <token>` from then on.
//! 4. The owner lists and revokes devices at will; revocation bites on the
//!    device's very next request.
//!
//! The pair code lives in the store's `meta` table (one active code, latest
//! wins) — transient by design, and burned before redemption proceeds so a
//! failure cannot leave it spendable twice.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth;
use crate::AppState;

/// `meta` key holding the one active pair code.
const PAIR_CODE_META_KEY: &str = "pair_code";
const DEFAULT_TTL_SECS: i64 = 600;
const MAX_TTL_SECS: i64 = 3600;

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": "authentication_error"}})),
    )
        .into_response()
}

// ---------------------------------------------------------------- pair code

#[derive(Debug, Deserialize)]
pub struct CreatePairingRequest {
    /// Seconds the code stays redeemable. Default 600, capped at 3600.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PairingCode {
    pub code: String,
    pub expires_ts: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredPairCode {
    code: String,
    expires_ts: i64,
}

/// 8 uppercase hex characters taken from a v7 UUID. Ample entropy for a code
/// that is single-use, short-lived, and costs a store write to attempt — the
/// QR handoff in front of the user is the real gate.
fn generate_code() -> String {
    Uuid::now_v7()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// `POST /etern/pairings` — mint a one-time pair code (owner only).
async fn create_pairing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<CreatePairingRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(err) = auth::check_configured(&state.cfg, &headers) {
        return err.into();
    }
    // An absent or malformed body just means "default TTL"; pairing is simpler
    // to scan from a QR when a strict body isn't required.
    let req = match body {
        Ok(Json(r)) => r,
        Err(_) => CreatePairingRequest { ttl_secs: None },
    };
    let ttl = req.ttl_secs.unwrap_or(DEFAULT_TTL_SECS).clamp(1, MAX_TTL_SECS);
    let expires_ts = now_ts() + ttl;
    let code = generate_code();
    let stored = json!({ "code": code, "expires_ts": expires_ts });
    if let Err(e) = state.store.set_meta(PAIR_CODE_META_KEY, &stored.to_string()) {
        tracing::error!(error = %e, "failed to store pair code");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing unavailable",
        );
    }
    tracing::info!(expires_ts, "pair code issued");
    Json(PairingCode { code, expires_ts }).into_response()
}

// ------------------------------------------------------------------- redeem

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    pub code: String,
    /// Human-friendly device name for the registry ("Z Fold 5", "Pixel 8").
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct PairedDevice {
    pub device_id: String,
    /// Long-lived bearer token for this device. Revealed exactly once: pairing.
    pub token: String,
    pub device_name: String,
}

/// `POST /etern/pair` — redeem a pair code for a device credential.
///
/// Unauthenticated by design: the code is the credential.
async fn pair(State(state): State<Arc<AppState>>, Json(req): Json<PairRequest>) -> Response {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return error(
            StatusCode::BAD_REQUEST,
            "name must be 1-64 characters",
        );
    }
    let raw = match state.store.get_meta(PAIR_CODE_META_KEY) {
        Ok(Some(v)) => v,
        Ok(None) => return error(StatusCode::FORBIDDEN, "no active pair code"),
        Err(e) => {
            tracing::error!(error = %e, "pair code lookup failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pairing unavailable",
            );
        }
    };
    let stored: StoredPairCode = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "stored pair code corrupt");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pairing unavailable",
            );
        }
    };
    if now_ts() > stored.expires_ts {
        let _ = state.store.delete_meta(PAIR_CODE_META_KEY);
        return error(StatusCode::FORBIDDEN, "pair code expired");
    }
    if !auth::token_matches(&stored.code, req.code.trim()) {
        return error(StatusCode::FORBIDDEN, "invalid pair code");
    }
    // Burn the code before doing anything else: a later failure must not leave
    // it redeemable, and a wrong code never reaches this line.
    if let Err(e) = state.store.delete_meta(PAIR_CODE_META_KEY) {
        tracing::error!(error = %e, "failed to consume pair code");
    }

    let device_id = Uuid::now_v7().to_string();
    let token = Uuid::now_v7().to_string();
    if let Err(e) = state
        .store
        .register_device(&device_id, name, &token, now_ts())
    {
        tracing::error!(error = %e, "device registration failed");
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device registration failed",
        );
    }
    tracing::info!(device_id, name, "device paired");
    Json(PairedDevice {
        device_id,
        token,
        device_name: name.to_string(),
    })
    .into_response()
}

// ----------------------------------------------------------------- registry

#[derive(Debug, Serialize)]
struct DeviceView {
    id: String,
    name: String,
    created_ts: i64,
    revoked: bool,
    /// Leading characters of the token, so the owner can recognise a device
    /// without the API ever handing back a usable credential.
    token_prefix: String,
}

/// `GET /etern/devices` — the registry, tokens masked (owner only).
async fn list_devices(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = auth::check_configured(&state.cfg, &headers) {
        return err.into();
    }
    let devices = match state.store.list_devices() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "device list failed");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device registry unavailable",
            );
        }
    };
    let views: Vec<DeviceView> = devices
        .into_iter()
        .map(|d| DeviceView {
            token_prefix: d.token.chars().take(8).collect(),
            id: d.id,
            name: d.name,
            created_ts: d.created_ts,
            revoked: d.revoked,
        })
        .collect();
    Json(json!({ "devices": views })).into_response()
}

/// `POST /etern/devices/{id}/revoke` — kill a device token (owner only).
async fn revoke_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = auth::check_configured(&state.cfg, &headers) {
        return err.into();
    }
    match state.store.revoke_device(&id) {
        Ok(true) => {
            tracing::info!(device_id = id, "device revoked");
            Json(json!({ "device_id": id, "revoked": true })).into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "unknown device"),
        Err(e) => {
            tracing::error!(error = %e, "revoke failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device registry unavailable",
            )
        }
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/etern/pairings", post(create_pairing))
        .route("/etern/pair", post(pair))
        .route("/etern/devices", get(list_devices))
        .route("/etern/devices/{id}/revoke", post(revoke_device))
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
        DeviceProfile, Journal, Sandbox, Store, ToolRuntime, UnavailableSearcher,
    };
    use serde_json::Value;
    use std::sync::RwLock;
    use tower::ServiceExt;

    const TOKEN: &str = "owner-token";

    fn test_state(token: Option<&str>) -> Arc<AppState> {
        let cfg = GatewayConfig {
            token: token.map(str::to_string),
            ..GatewayConfig::default()
        };
        let dir = std::env::temp_dir().join(format!(
            "etern-pair-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
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
            status: RwLock::new(EngineStatus::default()),
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

    async fn create_code(
        state: &Arc<AppState>,
        auth: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::post("/etern/pairings").header("content-type", "application/json");
        if let Some(t) = auth {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        let resp = app(state.clone())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn redeem(state: &Arc<AppState>, code: &str, name: &str) -> (StatusCode, Value) {
        let body = json!({ "code": code, "name": name }).to_string();
        let resp = app(state.clone())
            .oneshot(
                Request::post("/etern/pair")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn create_pairing_requires_owner_token() {
        let state = test_state(Some(TOKEN));
        assert_eq!(create_code(&state, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            create_code(&state, Some("wrong")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, pc) = create_code(&state, Some(TOKEN)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pc["code"].as_str().unwrap().len(), 8);
        assert!(pc["expires_ts"].as_i64().unwrap() > now_ts());
    }

    #[tokio::test]
    async fn full_pairing_lifecycle() {
        let state = test_state(Some(TOKEN));
        let (_, pc) = create_code(&state, Some(TOKEN)).await;
        let code = pc["code"].as_str().unwrap().to_string();

        let (status, paired) = redeem(&state, &code, "Z Fold 5").await;
        assert_eq!(status, StatusCode::OK);
        let device_token = paired["token"].as_str().unwrap().to_string();
        let device_id = paired["device_id"].as_str().unwrap().to_string();

        // The device token authenticates ordinary client endpoints...
        let headers = HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {device_token}")).unwrap(),
        )]);
        let outcome = auth::check_any(&state.cfg, &state.store, &headers).unwrap();
        assert!(matches!(&outcome, auth::AuthOutcome::Device(d) if d.id == device_id));
        // ...but not the admin surface.
        let (status, _) = create_code(&state, Some(&device_token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // The owner sees the device in the registry, token masked.
        let resp = app(state.clone())
            .oneshot(
                Request::get("/etern/devices")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let list: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list["devices"][0]["name"], "Z Fold 5");
        assert_eq!(list["devices"][0]["token_prefix"], &device_token[..8]);
        assert!(list["devices"][0].get("token").is_none());

        // Revoke — the device token must stop working immediately.
        let resp = app(state.clone())
            .oneshot(
                Request::post(format!("/etern/devices/{device_id}/revoke"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let err = auth::check_any(&state.cfg, &state.store, &headers).unwrap_err();
        assert!(matches!(err, auth::AuthError::InvalidToken));
    }

    #[tokio::test]
    async fn pair_code_is_single_use() {
        let state = test_state(Some(TOKEN));
        let (_, pc) = create_code(&state, Some(TOKEN)).await;
        let code = pc["code"].as_str().unwrap();
        assert_eq!(redeem(&state, code, "phone").await.0, StatusCode::OK);
        let (status, body) = redeem(&state, code, "phone").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["message"], "no active pair code");
    }

    #[tokio::test]
    async fn expired_code_is_rejected_and_purged() {
        let state = test_state(Some(TOKEN));
        let stale = json!({"code": "DEADBEEF", "expires_ts": now_ts() - 1});
        state
            .store
            .set_meta(PAIR_CODE_META_KEY, &stale.to_string())
            .unwrap();
        let (status, body) = redeem(&state, "DEADBEEF", "phone").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["message"], "pair code expired");
        assert!(state
            .store
            .get_meta(PAIR_CODE_META_KEY)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn wrong_code_rejected_without_consuming() {
        let state = test_state(Some(TOKEN));
        let (_, pc) = create_code(&state, Some(TOKEN)).await;
        let code = pc["code"].as_str().unwrap().to_string();
        let (status, _) = redeem(&state, "FFFFFFFF", "phone").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A failed guess must not burn the real code.
        assert_eq!(redeem(&state, &code, "phone").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn revoke_unknown_device_is_404() {
        let state = test_state(Some(TOKEN));
        let resp = app(state.clone())
            .oneshot(
                Request::post("/etern/devices/nope/revoke")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pairing_works_with_auth_disabled() {
        // Dev mode (no static token): everything is admitted as owner.
        let state = test_state(None);
        let (status, pc) = create_code(&state, None).await;
        assert_eq!(status, StatusCode::OK);
        let code = pc["code"].as_str().unwrap();
        assert_eq!(redeem(&state, code, "phone").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn name_validated() {
        let state = test_state(Some(TOKEN));
        let (_, pc) = create_code(&state, Some(TOKEN)).await;
        let code = pc["code"].as_str().unwrap();
        assert_eq!(
            redeem(&state, code, "   ").await.0,
            StatusCode::BAD_REQUEST
        );
        let long_name = "x".repeat(65);
        assert_eq!(
            redeem(&state, code, &long_name).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}
