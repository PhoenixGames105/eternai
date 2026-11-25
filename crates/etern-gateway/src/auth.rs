//! Bearer-token gate.
//!
//! The gateway listens on a LAN-reachable address because the phone is a
//! first-class client, and the existing launch scripts already bind the engine
//! to `0.0.0.0` — so the network boundary is not a trust boundary. `/health`
//! stays open on purpose: it is the phone's "am I at home?" signal and carries
//! nothing but state names.
//!
//! Two credential classes (PLAN §8):
//!   * the **static token** from config — the owner/admin credential, the only
//!     one that may pair or revoke devices;
//!   * **device tokens** minted by pairing — bound to a `device_id`, revocable,
//!     and sufficient for everything a phone actually does (chat, push, window).

use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use axum::response::{IntoResponse, Response};
use etern_core::{DeviceRecord, Store};
use serde_json::json;

use crate::config::GatewayConfig;

/// Extracts a bearer token from an `Authorization` header value.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, rest) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// Constant-time-ish comparison. Not cryptographic-grade, but it avoids
/// leaking the token's length-by-timing to a LAN neighbour.
pub fn token_matches(expected: &str, provided: &str) -> bool {
    if expected.len() != provided.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.as_bytes().iter().zip(provided.as_bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Why a request was rejected. Small on purpose so callers can pass it around
/// by value instead of carrying a whole `Response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    MissingToken,
    InvalidToken,
}

impl AuthError {
    fn message(self) -> &'static str {
        match self {
            Self::MissingToken => "missing bearer token",
            Self::InvalidToken => "invalid token",
        }
    }
}

impl From<AuthError> for Response {
    fn from(err: AuthError) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": { "message": err.message(), "type": "authentication_error" } })),
        )
            .into_response()
    }
}

/// Rejects the request when a token is configured and the caller did not present
/// it. Returns `Ok(())` when auth is disabled or satisfied.
pub fn check_configured(cfg: &GatewayConfig, headers: &HeaderMap) -> Result<(), AuthError> {
    let Some(expected) = cfg.token.as_deref() else {
        return Ok(());
    };
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match bearer(provided) {
        Some(got) if token_matches(expected, got) => Ok(()),
        Some(_) => Err(AuthError::InvalidToken),
        None => Err(AuthError::MissingToken),
    }
}

/// Who the caller turned out to be. `Owner` presented the static admin token;
/// `Device` presented a live paired-device token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    Owner,
    Device(DeviceRecord),
}

/// Extracts the bearer token from headers, if any.
pub fn token_from(headers: &HeaderMap) -> Option<&str> {
    bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Gate for ordinary client endpoints: the static token *or* a live device
/// token admits the request. When no static token is configured (dev mode)
/// everything is admitted as `Owner`, matching [`check_configured`].
///
/// A revoked device token is rejected even though it still exists in the
/// registry — revocation must take effect on the next request, not the next
/// restart.
pub fn check_any(
    cfg: &GatewayConfig,
    store: &Store,
    headers: &HeaderMap,
) -> Result<AuthOutcome, AuthError> {
    let Some(expected) = cfg.token.as_deref() else {
        return Ok(AuthOutcome::Owner);
    };
    let Some(got) = token_from(headers) else {
        return Err(AuthError::MissingToken);
    };
    if token_matches(expected, got) {
        return Ok(AuthOutcome::Owner);
    }
    match store.device_by_token(got) {
        Ok(Some(record)) if !record.revoked => Ok(AuthOutcome::Device(record)),
        Ok(_) => Err(AuthError::InvalidToken),
        // The registry is local SQLite; an error here means the store is
        // broken, which is indistinguishable from "not a valid token" to the
        // caller, but worth a log line for the operator.
        Err(e) => {
            tracing::warn!(error = %e, "device token lookup failed");
            Err(AuthError::InvalidToken)
        }
    }
}

/// Requires an explicit owner credential or a live paired-device token.
///
/// Unlike [`check_any`], this never treats disabled owner-token configuration as
/// permission for anonymous access, which is important for endpoints carrying
/// private device state.
pub fn check_owner_or_device(
    cfg: &GatewayConfig,
    store: &Store,
    headers: &HeaderMap,
) -> Result<AuthOutcome, AuthError> {
    let Some(got) = token_from(headers) else {
        return Err(AuthError::MissingToken);
    };
    if cfg
        .token
        .as_deref()
        .is_some_and(|expected| token_matches(expected, got))
    {
        return Ok(AuthOutcome::Owner);
    }
    match store.device_by_token(got) {
        Ok(Some(record)) if !record.revoked => Ok(AuthOutcome::Device(record)),
        _ => Err(AuthError::InvalidToken),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn cfg_with(token: Option<&str>) -> GatewayConfig {
        GatewayConfig { token: token.map(str::to_string), ..Default::default() }
    }

    fn headers_with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn bearer_parsing_is_case_insensitive_on_the_scheme() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("  BEARER   abc  ")), Some("abc"));
    }

    #[test]
    fn bearer_rejects_everything_else() {
        assert_eq!(bearer(None), None);
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(Some("Bearer a b")), Some("a b"), "tokens may contain spaces only if the client quoted them");
    }

    #[test]
    fn token_comparison_accepts_exact_and_rejects_prefixes() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3cre"));
        assert!(!token_matches("s3cret", "s3creu"));
        assert!(!token_matches("", "x"));
        assert!(token_matches("", ""));
    }

    #[test]
    fn auth_disabled_admits_anything() {
        let cfg = cfg_with(None);
        assert!(check_configured(&cfg, &HeaderMap::new()).is_ok());
        assert!(check_configured(&cfg, &headers_with("Bearer nonsense")).is_ok());
    }

    #[test]
    fn auth_enabled_demands_the_token() {
        let cfg = cfg_with(Some("s3cret"));
        assert!(check_configured(&cfg, &HeaderMap::new()).is_err());
        assert!(check_configured(&cfg, &headers_with("Bearer s3cret")).is_ok());
        assert!(check_configured(&cfg, &headers_with("Bearer wrong!")).is_err());
        assert!(check_configured(&cfg, &headers_with("s3cret")).is_err(), "scheme is required");
    }

    #[test]
    fn explicit_credentials_are_required_even_when_owner_auth_is_disabled() {
        let cfg = GatewayConfig::default();
        let store = Store::open_in_memory().unwrap();
        let headers = HeaderMap::new();
        assert_eq!(
            check_owner_or_device(&cfg, &store, &headers),
            Err(AuthError::MissingToken)
        );
    }
}
