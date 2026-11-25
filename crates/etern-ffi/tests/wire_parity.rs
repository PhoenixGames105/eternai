//! Wire-contract parity tests for the Android app.
//!
//! Every body here was captured verbatim from a running gateway (`etern-gateway
//! --tls-listen=127.0.0.1:18443`) driven through the exact HTTP calls the Kotlin layer
//! makes: POST /etern/pairings, POST /etern/pair, POST /etern/sync/push,
//! GET /etern/sync/window, revoke, GET /health.
//!
//! These catch a rename on one side of the UniFFI boundary that hand-written fixtures
//! on both sides would otherwise agree to ignore.

use etern_ffi::{
    dispose_window, parse_health, parse_paired_device, parse_push_ack, parse_window_proposal,
    push_request_json, route, window_query_string, MessageDto, SyncTurnDto,
};

const HEALTH: &str = r#"{"cert_pin":"osh1vHmAWNDm+uhw9hxSp9EpIyPcp5vnv3q36MXHkVg=","engine_loaded":true,"engine_ready":"ready","engine_ready_s":true,"fail_over":false,"gateway_reachable":true,"in_flight":0,"lan_ip":"192.168.1.98","model":"qwen3.8-flash-next-unsloth-ud-q4_k_xl","n_ctx":262144,"prompt_tokens":81826,"safety_margin":0.9,"supports_tools":true,"supports_vision":true,"swaps":0,"template_sha":"12827f24b742","tls_port":18443,"token_source":"estimated","waking":false}"#;

const PAIRED: &str = r#"{"device_id":"01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a","token":"01a1fa20-8ebf-73af-aaec-6fe0448996b9","device_name":"Z Fold 5"}"#;

const PUSH_ACK: &str = r#"{"accepted":2,"duplicates":0,"journal":"sync-20261120-130353.txt"}"#;

const PUSH_REPLAY: &str = r#"{"accepted":0,"duplicates":2,"journal":null}"#;

const WINDOW: &str = r#"{"messages":[{"id":"01940000-0000-7000-8000-000000000001","ts":1795197773219,"kind":"chat","engine":"edge","model":"edge","device_id":"01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a","seq":1,"messages":[{"role":"user","content":"what was my workout plan"},{"role":"assistant","content":"Three sessions this week: push, pull, legs."}]},{"id":"01940000-0000-7000-8000-000000000002","ts":1795197833219,"kind":"chat","engine":"edge","model":"edge","device_id":"01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a","seq":2,"messages":[{"role":"user","content":"add a 5k run"},{"role":"assistant","content":"Added a 5k on Saturday morning."}]}],"window_seq":2,"template_sha":"12827f24b742","est_tokens":68,"budget_tokens":6382,"token_source":"estimated","truncated":false,"apply":"replace","store_head_ts":1795197833219}"#;

/// The gateway's 401 body after a revoke, which the client turns into "re-pair".
const REVOKED: &str = r#"{"error":{"message":"invalid token","type":"authentication_error"}}"#;

fn msg(role: &str, content: &str) -> MessageDto {
    MessageDto {
        role: role.into(),
        content: content.into(),
        name: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

fn turns() -> Vec<SyncTurnDto> {
    vec![
        SyncTurnDto {
            turn_id: "01940000-0000-7000-8000-000000000001".into(),
            ts: 1_795_197_773_219,
            seq: 1,
            offline: true,
            messages: vec![
                msg("user", "what was my workout plan"),
                msg("assistant", "Three sessions this week: push, pull, legs."),
            ],
        },
        SyncTurnDto {
            turn_id: "01940000-0000-7000-8000-000000000002".into(),
            ts: 1_795_197_833_219,
            seq: 2,
            offline: false,
            messages: vec![
                msg("user", "add a 5k run"),
                msg("assistant", "Added a 5k on Saturday morning."),
            ],
        },
    ]
}

#[test]
fn health_body_drives_the_routing_matrix() {
    let h = parse_health(HEALTH.into());
    assert!(h.reachable && h.engine_serving && !h.fail_over);
    assert_eq!(h.engine_state, "ready");
    assert_eq!(h.n_ctx, 262_144);
    assert!(h.supports_tools);
    assert_eq!(
        h.cert_pin.as_deref(),
        Some("osh1vHmAWNDm+uhw9hxSp9EpIyPcp5vnv3q36MXHkVg=")
    );
    assert_eq!(h.lan_ip, "192.168.1.98");
    assert_eq!(h.tls_port, 18_443);
    assert_eq!(route(h.reachable, h.engine_serving).target, "high_power");
}

#[test]
fn pair_body_yields_the_credential_the_client_sends() {
    let d = parse_paired_device(PAIRED.into()).unwrap();
    assert_eq!(d.device_name, "Z Fold 5");
    assert!(!d.device_id.is_empty() && !d.token.is_empty());
}

#[test]
fn push_body_is_what_the_gateway_accepted() {
    let body = push_request_json("01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a".into(), turns());
    let sent: serde_json::Value = serde_json::from_str(&body.json).unwrap();
    assert_eq!(sent["device_id"], "01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a");
    assert_eq!(sent["turns"].as_array().unwrap().len(), 2);
    assert_eq!(sent["turns"][0]["messages"][0]["role"], "user");
    assert_eq!(sent["turns"][0]["offline"], true);
    assert_eq!(sent["turns"][1]["offline"], false);
    // Absent optional fields must stay absent, not become null.
    assert!(sent["turns"][0]["messages"][0].get("name").is_none());
    assert!(!body.truncated);
}

#[test]
fn push_ack_and_replay_parse() {
    let first = parse_push_ack(PUSH_ACK.into()).unwrap();
    assert_eq!((first.accepted, first.duplicates), (2, 0));
    assert_eq!(
        first.journal.as_deref(),
        Some("sync-20261120-130353.txt"),
        "the digest file name is what the phone shows the user"
    );

    let replay = parse_push_ack(PUSH_REPLAY.into()).unwrap();
    assert_eq!((replay.accepted, replay.duplicates), (0, 2));
    assert!(replay.journal.is_none(), "a pure replay writes no digest");
}

#[test]
fn revoked_token_body_is_not_mistaken_for_an_ack() {
    assert!(parse_push_ack(REVOKED.into()).is_err());
}

#[test]
fn window_body_disposes_into_an_8k_edge_context() {
    let proposal = parse_window_proposal(WINDOW.into()).unwrap();
    assert_eq!(proposal.turns.len(), 2);
    assert_eq!(proposal.window_seq, Some(2));
    assert_eq!(proposal.store_head_ts, Some(1_795_197_833_219));
    assert_eq!(proposal.apply, "replace");
    assert_eq!(proposal.template_sha, "12827f24b742");

    // The phone is the final authority: it re-fits the proposal against its own budget.
    let result = dispose_window(proposal.turns, 8192, 700, 400, None).unwrap();
    assert_eq!(result.turns.len(), 2, "both turns fit comfortably");
    assert!(result.estimated);
    assert!(result.tokens_used <= result.budget);
    assert!(result.budget <= 8192 - 700 - 400);
}

#[test]
fn window_query_uses_the_gateways_parameter_names() {
    // Renaming any of these four on the gateway would silently fall back to server
    // defaults, so the names are pinned here.
    let q = window_query_string(8192, 700, 400, "01a1fa20-8ebf-73af-aaec-6fdf3f3ef47a".into());
    for key in ["edge_ctx=", "response_reserve=", "overhead_tokens=", "device_id="] {
        assert!(q.contains(key), "missing {key} in {q}");
    }
}
