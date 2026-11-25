//! Edge-window proposal (`POST /etern/edge-window`).
//!
//! The contract from PLAN.md §4b is **propose/dispose**: the desktop proposes the
//! window the phone should hold, and the phone disposes — it re-renders the
//! messages with the *edge* chat template, re-tokenizes with the edge tokenizer,
//! and trims further if it must. The desktop can never compute the edge count
//! exactly, because the two templates differ (9993 vs 7992 chars).
//!
//! The desktop-side count is always an estimate of the *edge* window, for two
//! independent reasons: it is produced by `CharDivFour`, and even an exact
//! engine `/tokenize` count would be measured against the *desktop* chat
//! template, which differs from the edge template. So the applied safety margin
//! tracks the counter actually used (see [`applied_margin`]), never the engine's
//! advertised capability, and the hard `edge_ctx` ceiling is never exceeded.
//!
//! The endpoint is stateless on purpose — the caller posts the candidate turns,
//! the gateway trims them. The gateway does not own the conversation store.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use etern_core::budget::{trim_to_budget, CharDivFour, TrimPolicy};
use etern_core::domain::Turn;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::props::TokenSource;

/// Default edge window: the Galaxy Z Fold 5 budget from PLAN.md.
pub const DEFAULT_EDGE_CTX: u64 = 8_192;
/// Tokens held back on the edge for the model's own reply. Without this the
/// window fills to exactly 8192 and the first generation has nowhere to write.
pub const DEFAULT_RESPONSE_RESERVE: u64 = 700;

#[derive(Debug, Clone, Deserialize)]
pub struct WindowRequest {
    /// Candidate turns, oldest first. Usually the newest slice of the desktop's
    /// transcript plus any offline batch that was just merged in.
    pub turns: Vec<Turn>,
    #[serde(default = "default_edge_ctx")]
    pub edge_ctx: u64,
    #[serde(default = "default_response_reserve")]
    pub response_reserve: u64,
    /// Tokens the edge template + system prompt + tool schemas will cost. The
    /// phone knows this best; when omitted the gateway uses a conservative
    /// standing figure and the phone re-checks regardless.
    pub overhead_tokens: Option<u64>,
}

fn default_edge_ctx() -> u64 {
    DEFAULT_EDGE_CTX
}

fn default_response_reserve() -> u64 {
    DEFAULT_RESPONSE_RESERVE
}

/// Standing overhead estimate for the edge template plus a compact system
/// prompt. Deliberately generous: over-reserving shortens the window, which is
/// merely suboptimal, while under-reserving truncates it, which loses memory.
pub const DEFAULT_EDGE_OVERHEAD: u64 = 400;

/// Safety margin actually applied to a proposal.
///
/// The margin guards against *under-counting*, which silently overflows the
/// edge window. The desktop-side count is **always** an estimate of the edge
/// window, for two independent reasons:
///
/// 1. It is produced by `CharDivFour`, a rough heuristic.
/// 2. Even an exact engine `/tokenize` count would be measured against the
///    *desktop* chat template, whereas the phone re-renders with the *edge*
///    template. The two templates frame turns differently, so a number that is
///    exact on one device is only an approximation on the other.
///
/// The conservative [`TokenSource::Estimated`] margin therefore applies to
/// every proposal, regardless of the engine's advertised capability. The
/// residual cross-template gap is ultimately resolved by the phone's own
/// re-tokenize-and-trim (the "dispose" half of propose/dispose), which is the
/// only party that can measure its own window exactly.
///
/// Applying `TokenSource::safety_margin()` from the *engine's* capability would
/// be a bug: it would hand a `CharDivFour` estimate the engine's `1.0` margin,
/// stripping the safety headroom from the very estimate that needs it.
/// Safety margin actually applied to a proposal.
///
/// Published on `/health` as `safety_margin` so a phone knows exactly how much
/// headroom the desktop's `budget_tokens` already carries.
pub fn applied_margin() -> f64 {
    TokenSource::Estimated.safety_margin()
}
#[derive(Debug, Clone, Serialize)]
pub struct WindowProposal {
    /// Messages to load, flattened in chronological order.
    pub messages: Vec<Turn>,
    /// `seq` of the newest turn in the window. The phone stores this as its
    /// watermark so it can tell the desktop which slice it holds.
    pub window_seq: Option<i64>,
    /// Fingerprint of the template the estimate was measured against. If the
    /// phone's cached copy disagrees, the proposal is stale and must be refetched.
    pub template_sha: String,
    pub est_tokens: usize,
    pub budget_tokens: usize,
    pub token_source: &'static str,
    /// True when older turns were dropped. The phone must *replace* its window,
    /// never merge onto it, or the dropped turns linger forever.
    pub truncated: bool,
    /// The phone's obligation, stated explicitly so the two implementations
    /// cannot drift into "merge" semantics.
    pub apply: &'static str,
}

/// Computes the window.
///
/// `budget` is `edge_ctx − response_reserve − overhead`, then scaled by
/// [`applied_margin`]. The margin is applied to the *budget*, not to the
/// measured cost, so an under-estimate eats into headroom rather than
/// overflowing the window. `token_source` is reported back to the phone for
/// diagnostics only; it does not change the budget, because the count is always
/// an estimate of the edge window (see [`applied_margin`]).
pub fn propose(
    req: &WindowRequest,
    template_sha: &str,
    token_source: TokenSource,
) -> Result<WindowProposal, String> {
    let overhead = req.overhead_tokens.unwrap_or(DEFAULT_EDGE_OVERHEAD);
    let edge_ctx = req.edge_ctx;
    let reserve = req.response_reserve;

    if edge_ctx <= reserve + overhead {
        return Err(format!(
            "edge context {edge_ctx} cannot hold a {reserve}-token reply reserve \
             and {overhead}-token template overhead"
        ));
    }

    let raw_budget = edge_ctx - reserve - overhead;
    let budget = (raw_budget as f64 * applied_margin()).floor() as usize;

    let plan = trim_to_budget(&req.turns, budget, 0, &CharDivFour, TrimPolicy::ChatOnly)
        .map_err(|e| e.to_string())?;

    let window_seq = plan.turns.last().map(|t| t.seq);
    Ok(WindowProposal {
        messages: plan.turns,
        window_seq,
        template_sha: template_sha.to_string(),
        est_tokens: plan.tokens_used,
        budget_tokens: budget,
        token_source: token_source.as_str(),
        truncated: plan.truncated,
        apply: "replace",
    })
}

pub async fn handle(
    body: Result<axum::Json<WindowRequest>, axum::extract::rejection::JsonRejection>,
    template_sha: String,
    token_source: TokenSource,
) -> Response {
    let axum::Json(req) = match body {
        Ok(parsed) => parsed,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": { "message": err.body_text(), "type": "invalid_request" } })),
            )
                .into_response()
        }
    };

    match propose(&req, &template_sha, token_source) {
        Ok(proposal) => axum::Json(proposal).into_response(),
        Err(message) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": { "message": message, "type": "invalid_request" } })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use etern_core::domain::{EngineKind, Message, Role, TurnKind};

    fn turn(seq: i64, text: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            seq,
            vec![Message::new(Role::User, text), Message::new(Role::Assistant, "ok")],
        )
        .with_engine(EngineKind::Strata, "big")
        .with_ts(1_700_000_000_000 + seq)
    }

    /// ~1000 chars per message pair, so ~500 tokens per turn under CharDivFour
    /// plus 16 tokens of framing.
    fn fat_turn(seq: i64) -> Turn {
        turn(seq, &"x".repeat(2_000))
    }

    fn req(turns: Vec<Turn>) -> WindowRequest {
        WindowRequest {
            turns,
            edge_ctx: DEFAULT_EDGE_CTX,
            response_reserve: DEFAULT_RESPONSE_RESERVE,
            overhead_tokens: None,
        }
    }

    #[test]
    fn a_small_transcript_is_passed_through_untouched() {
        let turns = vec![turn(1, "hi"), turn(2, "how are you")];
        let p = propose(&req(turns.clone()), "abc123def456", TokenSource::Engine).unwrap();
        assert_eq!(p.messages.len(), 2, "everything fits, nothing is cut");
        assert!(!p.truncated);
        assert_eq!(p.window_seq, Some(2));
        assert_eq!(p.template_sha, "abc123def456");
        assert_eq!(p.token_source, "engine");
        assert_eq!(p.apply, "replace");
    }

    #[test]
    fn the_reply_reserve_is_held_back() {
        let p = propose(&req(vec![]), "sha", TokenSource::Engine).unwrap();
        assert_eq!(
            p.budget_tokens,
            ((8_192 - 700 - DEFAULT_EDGE_OVERHEAD) as f64 * 0.90).floor() as usize,
            "the edge model needs room to answer, inside the safety margin"
        );
    }

    #[test]
    fn the_budget_is_conservative_whatever_the_engine_advertises() {
        // The desktop count is always an estimate of the *edge* window (wrong
        // tokenizer heuristic, wrong chat template), so the conservative margin
        // applies even when the engine reports an exact /tokenize. Relaxing it
        // for TokenSource::Engine would strip headroom from an estimate.
        let exact = propose(&req(vec![]), "sha", TokenSource::Engine).unwrap();
        let est = propose(&req(vec![]), "sha", TokenSource::Estimated).unwrap();
        assert_eq!(
            est.budget_tokens, exact.budget_tokens,
            "the margin tracks the counter used, not the engine's capability"
        );
        assert_eq!(
            est.budget_tokens,
            ((8_192 - 700 - DEFAULT_EDGE_OVERHEAD) as f64 * 0.90).floor() as usize
        );
        // The capability is still reported so the phone knows how much to trust
        // the number.
        assert_eq!(exact.token_source, "engine");
        assert_eq!(est.token_source, "estimated");
    }

    #[test]
    fn an_oversized_transcript_is_cut_on_a_turn_boundary() {
        let turns: Vec<Turn> = (1..=40).map(fat_turn).collect();
        let p = propose(&req(turns), "sha", TokenSource::Estimated).unwrap();
        assert!(p.truncated);
        assert!(!p.messages.is_empty(), "a 8k window still holds recent turns");
        assert!(p.est_tokens <= p.budget_tokens, "the promise must hold");

        // Contiguity: the kept turns must be the newest N with no holes.
        let seqs: Vec<i64> = p.messages.iter().map(|t| t.seq).collect();
        let expected: Vec<i64> = ((40 - seqs.len() as i64 + 1)..=40).collect();
        assert_eq!(seqs, expected, "a window with a hole in it confuses the model");
    }

    #[test]
    fn the_window_never_exceeds_the_edge_context() {
        let turns: Vec<Turn> = (1..=200).map(fat_turn).collect();
        let p = propose(&req(turns), "sha", TokenSource::Estimated).unwrap();
        let total = p.est_tokens as u64 + DEFAULT_EDGE_OVERHEAD + DEFAULT_RESPONSE_RESERVE;
        assert!(total <= DEFAULT_EDGE_CTX, "used {total} of {DEFAULT_EDGE_CTX}");
    }

    #[test]
    fn a_window_too_small_for_reserve_and_overhead_is_an_error() {
        let bad = WindowRequest {
            turns: vec![],
            edge_ctx: 500,
            response_reserve: 700,
            overhead_tokens: None,
        };
        let err = propose(&bad, "sha", TokenSource::Engine).unwrap_err();
        assert!(err.contains("cannot hold"), "got {err}");
    }

    #[test]
    fn caller_supplied_overhead_is_honoured() {
        let r = WindowRequest {
            turns: vec![],
            edge_ctx: 8_192,
            response_reserve: 0,
            overhead_tokens: Some(2_000),
        };
        let p = propose(&r, "sha", TokenSource::Engine).unwrap();
        assert_eq!(
            p.budget_tokens,
            ((8_192 - 2_000) as f64 * applied_margin()).floor() as usize
        );
    }

    #[test]
    fn non_chat_turns_are_excluded_from_the_window() {
        let chat = turn(1, "real conversation");
        let mut journal = turn(2, "journal summary");
        journal.kind = TurnKind::Journal;

        let p = propose(&req(vec![chat, journal]), "sha", TokenSource::Engine).unwrap();
        assert_eq!(p.messages.len(), 1, "only the chat turn belongs in the window");
        assert_eq!(p.window_seq, Some(1));
    }

    #[test]
    fn an_empty_transcript_proposes_an_empty_window_not_an_error() {
        let p = propose(&req(vec![]), "sha", TokenSource::Estimated).unwrap();
        assert!(p.messages.is_empty());
        assert_eq!(p.window_seq, None);
        assert!(!p.truncated);
    }
}
