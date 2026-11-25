//! UniFFI bindings over `etern-core` for the Android edge app (PLAN.md Â§7).
//!
//! Everything the phone needs to honour the sync contract lives here in Rust so
//! the Kotlin and desktop sides can never drift:
//!
//! * the durable outbox (`seq` continuity, torn-line tolerance) â€” [`Outbox`]
//! * the routing matrix (PLAN.md Â§2) â€” [`route`]
//! * window dispose, where the phone's tokenizer is the sole authority over its
//!   8k window (PLAN.md Â§4b) â€” [`dispose_window`]
//! * wire shapes for the QR payload, `POST /etern/pair`, `POST /etern/sync/push`
//!   and `GET /health`.
//!
//! Pure proc-macro UniFFI (no UDL): scaffolding comes from
//! [`uniffi::setup_scaffolding`], and Kotlin bindings are generated from the
//! compiled library:
//!
//! ```text
//! cargo run --features bindgen-cli --bin uniffi-bindgen -- ^
//!   generate --library target/debug/etern_ffi.dll ^
//!   --language kotlin --out-dir apps/android/core/src/main/kotlin
//! ```

use etern_core::budget::{trim_to_budget, CharDivFour, TrimPolicy};
use etern_core::domain::{EngineKind, Message, Role, Turn, TurnKind};
use etern_core::error::CoreError;
use etern_core::sync::{Outbox as CoreOutbox, SyncTurn, MAX_SYNC_BATCH};
use etern_core::tools::{Sandbox, ToolRuntime, UnavailableSearcher};
use etern_core::DeviceProfile;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

uniffi::setup_scaffolding!();

/// Edge context window (PLAN.md Â§3).
pub const DEFAULT_EDGE_CTX: i64 = 8192;
/// Response reserve offered by `GET /etern/sync/window` (mirrors
/// `etern_gateway::window::DEFAULT_RESPONSE_RESERVE`).
pub const DEFAULT_RESPONSE_RESERVE: i64 = 700;
/// ChatML/system overhead the phone budgets for (mirrors
/// `etern_gateway::window::DEFAULT_EDGE_OVERHEAD`).
pub const DEFAULT_EDGE_OVERHEAD: i64 = 400;
/// Gateway TLS port assumed when a QR payload omits one.
pub const DEFAULT_TLS_PORT: i64 = 8443;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum EternError {
    #[error("io: {0}")]
    Io(String),
    #[error("{0}")]
    Invalid(String),
    #[error("budget: {0}")]
    Budget(String),
    #[error("{0}")]
    Other(String),
}

impl From<CoreError> for EternError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Io { path, source } => EternError::Io(format!("{path}: {source}")),
            CoreError::Budget(s) => EternError::Budget(s),
            CoreError::Json(e) => EternError::Invalid(e.to_string()),
            other => EternError::Other(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for EternError {
    fn from(e: serde_json::Error) -> Self {
        EternError::Invalid(e.to_string())
    }
}

// ---------------------------------------------------------------- DTOs

/// One OpenAI-shaped message. Tool fields stay raw JSON strings so the engine
/// can replay them byte-for-byte (see [`etern_core::domain::Message`]).
#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageDto {
    pub role: String,
    pub content: String,
    pub name: Option<String>,
    /// Raw JSON array of tool calls, or `None`.
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
}

/// A logical exchange. Turns are the unit the trimmer cuts on, so the phone
/// must never see one torn in half.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TurnDto {
    pub turn_id: String,
    /// Unix ms of the turn's start.
    pub ts: i64,
    /// `"chat" | "sync" | "journal" | "system"`.
    pub kind: String,
    /// `"strata" | "llama" | "edge" | "none"`.
    pub engine: String,
    pub model: String,
    pub device_id: String,
    pub seq: i64,
    pub messages: Vec<MessageDto>,
}

/// One offline turn as carried over the wire.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncTurnDto {
    pub turn_id: String,
    pub ts: i64,
    pub seq: i64,
    /// True when generated locally while the desktop was unavailable.
    pub offline: bool,
    pub messages: Vec<MessageDto>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncTurnList {
    pub turns: Vec<SyncTurnDto>,
    /// Lines that failed to parse and were skipped. Non-zero means the file was
    /// torn by a crash; the caller should surface it rather than lose silence.
    pub skipped: i64,
}

/// Credentials returned exactly once by `POST /etern/pair`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub token: String,
    pub device_name: String,
}

/// Parsed QR payload (PLAN.md Â§8): `{"host","port","cert_pin","code"}`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairTarget {
    pub host: String,
    pub port: i64,
    pub cert_pin: String,
    pub code: String,
}

/// Body for `POST /etern/pair`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairRequestBody {
    pub json: String,
}

/// Body for `POST /etern/sync/push`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PushRequestBody {
    pub json: String,
    /// True when the batch dropped the oldest turns to fit the cap.
    pub truncated: bool,
    /// Turns actually carried in `json`.
    pub count: i64,
}

/// Parsed `PushAck`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PushAckInfo {
    pub accepted: i64,
    pub duplicates: i64,
    /// Journal digest the desktop wrote for the new turns, if any.
    pub journal: Option<String>,
}

/// Parsed `GET /health`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HealthInfo {
    /// The gateway answered with parseable JSON.
    pub reachable: bool,
    /// A prompt sent now would eventually be served by the desktop.
    pub engine_serving: bool,
    /// `"ready" | "busy" | "loading" | "down"`.
    pub engine_state: String,
    pub fail_over: bool,
    pub waking: bool,
    pub model: String,
    pub template_sha: String,
    pub n_ctx: i64,
    pub supports_tools: bool,
    /// Pin the gateway is serving now; a change means the cert was replaced.
    pub cert_pin: Option<String>,
    pub lan_ip: String,
    pub tls_port: i64,
}

/// PLAN.md Â§2 routing decision.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RouteDecision {
    /// `"high_power"` | `"edge"`.
    pub target: String,
    /// Queue locally instead of sending (gateway unreachable).
    pub hold_outbox: bool,
    /// Push the outbox to the gateway now.
    pub flush_outbox: bool,
    /// For the model chip / logs.
    pub reason: String,
}

/// Result of disposing a window proposal into the phone's live context.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WindowResult {
    pub turns: Vec<TurnDto>,
    pub tokens_used: i64,
    pub budget: i64,
    /// True when older turns were dropped â€” the phone must *replace* its window.
    pub truncated: bool,
    /// True when the count came from the `CharDivFour` estimate rather than the
    /// phone's tokenizer.
    pub estimated: bool,
}

/// Parsed `GET /etern/sync/window` body.
///
/// Parsing lives here rather than in Kotlin because `messages` is a full
/// `Vec<Turn>`: re-declaring that shape in Kotlin would put a second copy of the
/// wire contract where the desktop's can drift.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WindowProposalInfo {
    /// Candidate turns, oldest first.
    pub turns: Vec<TurnDto>,
    /// `seq` of the newest turn in the proposal; `None` for an empty window.
    pub window_seq: Option<i64>,
    /// Template fingerprint the estimate was measured against.
    pub template_sha: String,
    pub est_tokens: i64,
    pub budget_tokens: i64,
    pub token_source: String,
    /// The desktop dropped older turns.
    pub truncated: bool,
    /// The desktop's stated apply semantics ("replace"). Surfaced so a phone
    /// that ignores it is visibly wrong rather than subtly wrong.
    pub apply: String,
    /// Store watermark: `ts` of the newest turn eligible for this device.
    pub store_head_ts: Option<i64>,
}

// ---------------------------------------------------------------- conversions

fn to_role(s: &str) -> Role {
    Role::parse(s).unwrap_or(Role::User)
}

fn to_kind(s: &str) -> TurnKind {
    TurnKind::parse(s).unwrap_or(TurnKind::Chat)
}

fn to_engine(s: &str) -> EngineKind {
    EngineKind::parse(s)
}

fn to_message(m: MessageDto) -> Message {
    Message {
        role: to_role(&m.role),
        content: m.content,
        name: m.name,
        tool_calls: m.tool_calls,
        tool_call_id: m.tool_call_id,
    }
}

fn from_message(m: &Message) -> MessageDto {
    MessageDto {
        role: m.role.as_str().to_string(),
        content: m.content.clone(),
        name: m.name.clone(),
        tool_calls: m.tool_calls.clone(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

fn to_turn(t: TurnDto) -> Result<Turn, EternError> {
    let id = Uuid::parse_str(&t.turn_id)
        .map_err(|e| EternError::Invalid(format!("turn_id {:?}: {e}", t.turn_id)))?;
    if t.messages.is_empty() {
        return Err(EternError::Invalid(format!("turn {id} has no messages")));
    }
    Ok(Turn {
        id,
        ts: t.ts,
        kind: to_kind(&t.kind),
        engine: to_engine(&t.engine),
        model: t.model,
        device_id: t.device_id,
        seq: t.seq,
        messages: t.messages.into_iter().map(to_message).collect(),
    })
}

fn from_turn(t: &Turn) -> TurnDto {
    TurnDto {
        turn_id: t.id.to_string(),
        ts: t.ts,
        kind: t.kind.as_str().to_string(),
        engine: t.engine.as_str().to_string(),
        model: t.model.clone(),
        device_id: t.device_id.clone(),
        seq: t.seq,
        messages: t.messages.iter().map(from_message).collect(),
    }
}

fn to_sync_turn(s: SyncTurnDto) -> Result<SyncTurn, EternError> {
    Ok(SyncTurn {
        turn_id: s.turn_id,
        ts: s.ts,
        seq: s.seq,
        offline: s.offline,
        messages: s.messages.into_iter().map(to_message).collect(),
    })
}

fn from_sync_turn(s: &SyncTurn) -> SyncTurnDto {
    SyncTurnDto {
        turn_id: s.turn_id.clone(),
        ts: s.ts,
        seq: s.seq,
        offline: s.offline,
        messages: s.messages.iter().map(from_message).collect(),
    }
}

// ---------------------------------------------------------------- turn helpers

/// New turn id (UUIDv7) as a string. Minted on the phone: it is the idempotency
/// key the desktop dedupes on, so it must be globally unique.
#[uniffi::export]
pub fn mint_turn_id() -> String {
    Uuid::now_v7().to_string()
}

/// Builds a chat turn from a prompt, a reply, and the messages that came before
/// the reply (assistant tool calls and tool results, in order).
///
/// `ts` is passed in rather than read from the clock so the caller can write the
/// outbox line and stamp the UI from one value.
#[allow(clippy::too_many_arguments)]
#[uniffi::export]
pub fn new_chat_turn(
    turn_id: String,
    user_text: String,
    assistant_text: String,
    engine: String,
    model: String,
    device_id: String,
    seq: i64,
    ts: i64,
    tool_messages: Vec<MessageDto>,
) -> Result<TurnDto, EternError> {
    let id = Uuid::parse_str(&turn_id).map_err(|e| EternError::Invalid(format!("turn_id {e}")))?;
    let mut messages = vec![Message::new(Role::User, user_text)];
    messages.extend(tool_messages.into_iter().map(to_message));
    messages.push(Message::new(Role::Assistant, assistant_text));
    let turn = Turn {
        id,
        ts,
        kind: TurnKind::Chat,
        engine: to_engine(&engine),
        model,
        device_id,
        seq,
        messages,
    };
    Ok(from_turn(&turn))
}

/// Flattens a turn into a [`SyncTurnDto`] for the outbox â€” the shape the
/// gateway ingests at `POST /etern/sync/push`.
#[uniffi::export]
pub fn sync_turn_from_turn(turn: TurnDto) -> Result<SyncTurnDto, EternError> {
    let core = to_turn(turn)?;
    Ok(from_sync_turn(&SyncTurn::from_turn(&core)))
}

/// Materialises a synced turn as a canonical turn attributed to `device_id`, for
/// a phone that keeps its own history too.
#[uniffi::export]
pub fn turn_from_sync_turn(
    sync: SyncTurnDto,
    device_id: String,
    model: String,
) -> Result<TurnDto, EternError> {
    let wire = to_sync_turn(sync)?;
    let turn = wire.to_turn(&device_id, &model).map_err(EternError::Invalid)?;
    Ok(from_turn(&turn))
}

/// Renders a turn's messages for the engine prompt.
#[uniffi::export]
pub fn turn_messages(turn: TurnDto) -> Result<Vec<MessageDto>, EternError> {
    let core = to_turn(turn)?;
    Ok(core.messages.iter().map(from_message).collect())
}

/// Final assistant text of a turn, ignoring intermediate tool steps.
#[uniffi::export]
pub fn turn_final_text(turn: TurnDto) -> Result<Option<String>, EternError> {
    let core = to_turn(turn)?;
    Ok(core.final_text().map(|s| s.to_string()))
}

// ---------------------------------------------------------------- outbox

/// JSONL outbox wrapping [`etern_core::sync::Outbox`].
///
/// `next_seq` derives from the highest seq on disk rather than an in-memory
/// counter, so a crash between "append" and "remember the counter" can never
/// mint a duplicate seq.
#[derive(uniffi::Object)]
pub struct Outbox {
    inner: CoreOutbox,
}

#[uniffi::export]
impl Outbox {
    #[uniffi::constructor]
    pub fn new(path: String) -> Result<Self, EternError> {
        Ok(Outbox {
            inner: CoreOutbox::new(path)?,
        })
    }

    /// Appends one turn, flushed to the OS before returning.
    pub fn append_sync_turn(&self, turn: SyncTurnDto) -> Result<(), EternError> {
        self.inner.append(&to_sync_turn(turn)?)?;
        Ok(())
    }

    pub fn read_all(&self) -> Result<SyncTurnList, EternError> {
        let (turns, skipped) = self.inner.read_all()?;
        Ok(SyncTurnList {
            turns: turns.iter().map(from_sync_turn).collect(),
            skipped: skipped as i64,
        })
    }

    /// Seq to use for the next append.
    pub fn next_seq(&self) -> Result<i64, EternError> {
        let (turns, _) = self.inner.read_all()?;
        Ok(turns
            .iter()
            .map(|t| t.seq)
            .max()
            .map(|m| m.saturating_add(1))
            .unwrap_or(0))
    }

    pub fn len_bytes(&self) -> Result<i64, EternError> {
        Ok(self.inner.len_bytes()? as i64)
    }

    /// Discard the outbox after a push has been acked.
    pub fn clear(&self) -> Result<(), EternError> {
        self.inner.clear()?;
        Ok(())
    }
}

// ---------------------------------------------------------------- tools

/// The phone's own tool runtime (PLAN.md §6).
///
/// The edge model's tool calls must run *on the phone*: when it is offline the
/// gateway is by definition unreachable, so there is no server to execute them.
/// The jail is [`Sandbox::edge`] — the app-scoped directory in its entirety —
/// and the tool set is the reduced edge set, which deliberately excludes
/// `web_search` (SearXNG lives on the desktop).
///
/// The JSON shapes returned here are byte-compatible with the gateway's
/// `POST /etern/tools/execute` results, so the Kotlin harness feeds either
/// source back to the model identically.
#[derive(uniffi::Object)]
pub struct EdgeTools {
    inner: ToolRuntime,
}

#[uniffi::export]
impl EdgeTools {
    /// Builds the runtime rooted at `root` (pass the app's `filesDir`).
    #[uniffi::constructor]
    pub fn new(root: String) -> Result<Self, EternError> {
        let sandbox = Sandbox::edge(&root)?;
        Ok(EdgeTools {
            inner: ToolRuntime::new(DeviceProfile::Edge, sandbox, Arc::new(UnavailableSearcher))?,
        })
    }

    /// The OpenAI `tools` array for the edge profile, as a JSON string ready to
    /// embed in a chat request.
    pub fn definitions(&self) -> String {
        self.inner.tool_definitions().to_string()
    }

    /// Names the edge profile actually exposes, for UI and diagnostics.
    pub fn names(&self) -> Vec<String> {
        self.inner.specs().iter().map(|s| s.name.to_string()).collect()
    }

    /// Validates and executes one call.
    ///
    /// Returns a JSON string in the gateway's result shape:
    /// `{"ok":true,"content":...}` or
    /// `{"ok":false,"retryable":bool,"error":{...}}`. Validation failures never
    /// touch the filesystem; they come back retryable so the harness can feed
    /// the error to the model for its single automatic retry.
    pub fn execute(
        &self,
        name: String,
        arguments_json: String,
        model: String,
        turn_id: String,
        now_ms: i64,
    ) -> Result<String, EternError> {
        // A malformed arguments payload is the model's mistake, not the caller's,
        // so report it as a retryable tool error rather than raising across FFI.
        let args: serde_json::Value = match serde_json::from_str(&arguments_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "name": name,
                    "retryable": true,
                    "error": {
                        "kind": "validation",
                        "message": format!("arguments are not valid JSON: {e}"),
                        "retryable": true
                    }
                })
                .to_string())
            }
        };

        let outcome = self.inner.execute(&name, &args, &model, &turn_id, now_ms);
        let obj = match outcome {
            Ok(res) => serde_json::json!({ "ok": true, "content": res.content }),
            Err(err) => serde_json::json!({
                "ok": false,
                "retryable": err.retryable,
                "error": err.to_result_value()["error"]
            }),
        };
        Ok(obj.to_string())
    }
}

// ---------------------------------------------------------------- pairing

/// Parses the QR payload into a [`PairTarget`].
///
/// Accepts `port` as a JSON number or string and rejects anything missing a
/// required field or with an out-of-range port, so pairing fails loudly instead
/// of silently dialling a wrong address.
#[uniffi::export]
pub fn parse_qr_payload(json: String) -> Result<PairTarget, EternError> {
    qr_payload(&json)
}

fn qr_payload(json: &str) -> Result<PairTarget, EternError> {
    #[derive(Deserialize)]
    struct Raw {
        host: String,
        code: String,
        cert_pin: String,
        #[serde(default)]
        port: Option<serde_json::Value>,
    }
    let raw: Raw = serde_json::from_str(json)?;
    let port = match raw.port {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| EternError::Invalid("port is not an integer".into()))?,
        Some(serde_json::Value::String(s)) => s
            .parse::<i64>()
            .map_err(|_| EternError::Invalid(format!("bad port {s:?}")))?,
        Some(_) => return Err(EternError::Invalid("port has an unexpected type".into())),
        None => DEFAULT_TLS_PORT,
    };
    if raw.host.trim().is_empty() {
        return Err(EternError::Invalid("QR payload missing host".into()));
    }
    if raw.cert_pin.trim().is_empty() {
        return Err(EternError::Invalid(
            "QR payload missing cert_pin â€” refusing to pair without pinning".into(),
        ));
    }
    if raw.code.trim().is_empty() {
        return Err(EternError::Invalid("QR payload missing code".into()));
    }
    if !(1..=65535).contains(&port) {
        return Err(EternError::Invalid(format!("port {port} out of range")));
    }
    Ok(PairTarget {
        host: raw.host.trim().to_string(),
        port,
        cert_pin: raw.cert_pin.trim().to_string(),
        code: raw.code.clone(),
    })
}

/// Body for `POST /etern/pair`. The gateway requires a 1-64 character name.
#[uniffi::export]
pub fn pair_request_json(code: String, name: String) -> Result<PairRequestBody, EternError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(EternError::Invalid(
            "device name must be 1-64 characters".into(),
        ));
    }
    Ok(PairRequestBody {
        json: serde_json::json!({ "code": code, "name": name }).to_string(),
    })
}

/// Parses a `PairedDevice` response into the credentials the app must persist.
#[uniffi::export]
pub fn parse_paired_device(json: String) -> Result<DeviceIdentity, EternError> {
    #[derive(Deserialize)]
    struct Raw {
        device_id: String,
        token: String,
        #[serde(default)]
        device_name: String,
    }
    let raw: Raw = serde_json::from_str(&json)?;
    if raw.device_id.trim().is_empty() || raw.token.trim().is_empty() {
        return Err(EternError::Invalid(
            "pair response missing device_id or token".into(),
        ));
    }
    Ok(DeviceIdentity {
        device_id: raw.device_id,
        token: raw.token,
        device_name: raw.device_name,
    })
}

// ---------------------------------------------------------------- push

/// Body for `POST /etern/sync/push`: oldest-first, capped at
/// [`MAX_SYNC_BATCH`] (the gateway rejects an oversized batch outright).
///
/// When the outbox exceeds the cap the *newest* turns go first and the flag
/// reports the truncation; the remainder follows in the next push because the
/// outbox is only cleared after an ack.
#[uniffi::export]
pub fn push_request_json(device_id: String, turns: Vec<SyncTurnDto>) -> PushRequestBody {
    let mut sorted = turns;
    sorted.sort_by_key(|t| t.seq);
    let truncated = sorted.len() > MAX_SYNC_BATCH;
    if truncated {
        let excess = sorted.len() - MAX_SYNC_BATCH;
        sorted.drain(0..excess);
    }
    let count = sorted.len();
    let json = serde_json::json!({
        "device_id": device_id,
        "turns": sorted
            .iter()
            .map(|t| {
                serde_json::json!({
                    "turn_id": t.turn_id,
                    "ts": t.ts,
                    "seq": t.seq,
                    "offline": t.offline,
                    "messages": t.messages.iter().map(message_json).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
    });
    PushRequestBody {
        json: json.to_string(),
        truncated,
        count: count as i64,
    }
}

/// Serialises a message exactly as `etern_core::domain::Message` does on the
/// desktop side (`name`/`tool_calls`/`tool_call_id` omitted when absent), so the
/// gateway stores byte-identical turns whichever device produced them.
fn message_json(m: &MessageDto) -> serde_json::Value {
    let mut v = serde_json::json!({ "role": m.role, "content": m.content });
    let obj = v.as_object_mut().expect("json object");
    if let Some(name) = &m.name {
        obj.insert("name".into(), serde_json::Value::String(name.clone()));
    }
    if let Some(tc) = &m.tool_calls {
        obj.insert(
            "tool_calls".into(),
            serde_json::Value::String(tc.clone()),
        );
    }
    if let Some(id) = &m.tool_call_id {
        obj.insert(
            "tool_call_id".into(),
            serde_json::Value::String(id.clone()),
        );
    }
    v
}

/// Parses a `PushAck`.
#[uniffi::export]
pub fn parse_push_ack(json: String) -> Result<PushAckInfo, EternError> {
    #[derive(Deserialize)]
    struct Raw {
        accepted: i64,
        #[serde(default)]
        duplicates: i64,
        #[serde(default)]
        journal: Option<String>,
    }
    let ack: Raw = serde_json::from_str(&json)?;
    Ok(PushAckInfo {
        accepted: ack.accepted,
        duplicates: ack.duplicates,
        journal: ack.journal,
    })
}

/// Parses a `GET /etern/sync/window` body into [`WindowProposalInfo`].
///
/// The gateway serialises `WindowProposal` with `#[serde(flatten)]` plus
/// `store_head_ts`, so the fields arrive flat on one object.
#[uniffi::export]
pub fn parse_window_proposal(json: String) -> Result<WindowProposalInfo, EternError> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        messages: Vec<Turn>,
        #[serde(default)]
        window_seq: Option<i64>,
        #[serde(default)]
        template_sha: String,
        #[serde(default)]
        est_tokens: usize,
        #[serde(default)]
        budget_tokens: usize,
        #[serde(default)]
        token_source: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        apply: String,
        #[serde(default)]
        store_head_ts: Option<i64>,
    }

    let raw: Raw = serde_json::from_str(&json)?;
    Ok(WindowProposalInfo {
        turns: raw.messages.iter().map(from_turn).collect(),
        window_seq: raw.window_seq,
        template_sha: raw.template_sha,
        est_tokens: raw.est_tokens as i64,
        budget_tokens: raw.budget_tokens as i64,
        token_source: raw.token_source,
        truncated: raw.truncated,
        apply: raw.apply,
        store_head_ts: raw.store_head_ts,
    })
}

// ---------------------------------------------------------------- health

/// Parses `GET /health`.
///
/// `reachable` is false when the body isn't parseable JSON at all (the gateway
/// never answered). `engine_serving` follows the gateway's own rule â€” only
/// `down` means "use the edge model"; `loading` means a model is swapping and
/// the phone should not treat the desktop as lost (PLAN.md Â§2).
#[uniffi::export]
pub fn parse_health(json: String) -> HealthInfo {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
        return HealthInfo {
            reachable: false,
            engine_serving: false,
            engine_state: "down".into(),
            fail_over: true,
            waking: false,
            model: String::new(),
            template_sha: String::new(),
            n_ctx: 0,
            supports_tools: false,
            cert_pin: None,
            lan_ip: String::new(),
            tls_port: 0,
        };
    };
    let engine_state = v["engine_ready"].as_str().unwrap_or("down").to_string();
    let fail_over = v["fail_over"]
        .as_bool()
        .unwrap_or_else(|| engine_state == "down");
    HealthInfo {
        reachable: true,
        engine_serving: !fail_over,
        engine_state,
        fail_over,
        waking: v["waking"].as_bool().unwrap_or(false),
        model: v["model"].as_str().unwrap_or("").to_string(),
        template_sha: v["template_sha"].as_str().unwrap_or("").to_string(),
        n_ctx: v["n_ctx"].as_i64().unwrap_or(0),
        supports_tools: v["supports_tools"].as_bool().unwrap_or(false),
        cert_pin: v["cert_pin"].as_str().map(|s| s.to_string()),
        lan_ip: v["lan_ip"].as_str().unwrap_or("").to_string(),
        tls_port: v["tls_port"].as_i64().unwrap_or(0),
    }
}

// ---------------------------------------------------------------- routing

/// PLAN.md Â§2 routing matrix.
///
/// * gateway unreachable â†’ edge, hold the outbox
/// * engine serving (ready/busy/loading) â†’ high power, flush the outbox
/// * engine down but reachable â†’ edge, and still flush: a model swap or an
///   engine outage must not look like a network loss, and the desktop should
///   take the backlog as soon as it recovers.
#[uniffi::export]
pub fn route(gateway_reachable: bool, engine_serving: bool) -> RouteDecision {
    if !gateway_reachable {
        RouteDecision {
            target: "edge".into(),
            hold_outbox: true,
            flush_outbox: false,
            reason: "gateway unreachable".into(),
        }
    } else if engine_serving {
        RouteDecision {
            target: "high_power".into(),
            hold_outbox: false,
            flush_outbox: true,
            reason: "gateway serving".into(),
        }
    } else {
        RouteDecision {
            target: "edge".into(),
            hold_outbox: false,
            flush_outbox: true,
            reason: "gateway reachable but engine down".into(),
        }
    }
}

/// Backoff between gateway probes: 2 s doubling up to 30 s.
///
/// `failures` counts consecutive probe failures (0 = last probe succeeded), so
/// recovery returns to the base cadence immediately.
#[uniffi::export]
pub fn probe_backoff_ms(failures: i64) -> i64 {
    if failures <= 0 {
        return 2_000;
    }
    2_000i64
        .saturating_mul(1i64 << (failures.min(5) as u32))
        .min(30_000)
}

// ---------------------------------------------------------------- window dispose

/// Disposes a `GET /etern/sync/window` proposal into the phone's live context.
///
/// The phone's tokenizer is the sole authority over its 8k window (PLAN.md Â§4b):
/// when `token_costs` is supplied (one entry per turn, same order) the backward
/// walk uses those exact counts; otherwise it falls back to the shared
/// [`CharDivFour`] estimate and flags the result `estimated`. A length mismatch
/// is a hard error â€” silently mis-trimming the only copy of a conversation is
/// worse than failing.
///
/// The walk mirrors `etern_core::budget::trim_to_budget`: the first turn that
/// does not fit ends the walk, so a window can come back empty when even the
/// newest turn is over budget. `truncated` means *replace* the window, never
/// merge onto it.
#[uniffi::export]
pub fn dispose_window(
    proposal: Vec<TurnDto>,
    edge_ctx: i64,
    response_reserve: i64,
    overhead: i64,
    token_costs: Option<Vec<i64>>,
) -> Result<WindowResult, EternError> {
    let budget = edge_budget_checked(edge_ctx, response_reserve, overhead)?;

    let core: Vec<Turn> = proposal
        .iter()
        .map(|t| to_turn(t.clone()))
        .collect::<Result<_, _>>()?;

    match token_costs {
        Some(costs) if costs.len() != core.len() => Err(EternError::Invalid(format!(
            "token_costs has {} entries for {} turns",
            costs.len(),
            core.len()
        ))),
        Some(costs) => {
            let mut used: i64 = 0;
            let mut kept = 0usize;
            let mut truncated = false;
            for cost in costs.iter().rev() {
                let next = used.saturating_add(*cost);
                if next > budget {
                    truncated = true;
                    break;
                }
                used = next;
                kept += 1;
            }
            let start = core.len() - kept;
            Ok(WindowResult {
                turns: core[start..].iter().map(from_turn).collect(),
                tokens_used: used,
                budget,
                truncated,
                estimated: false,
            })
        }
        None => {
            let plan = trim_to_budget(&core, budget as usize, 0, &CharDivFour, TrimPolicy::All)?;
            Ok(WindowResult {
                turns: plan.turns.iter().map(from_turn).collect(),
                tokens_used: plan.tokens_used as i64,
                budget,
                truncated: plan.truncated,
                estimated: true,
            })
        }
    }
}

/// The budget the phone should ask the gateway for, so the query string and the
/// local dispose pass can never disagree. Clamps to 0 rather than failing, which
/// is what a query string needs; [`dispose_window`] reports the error instead.
#[uniffi::export]
pub fn edge_budget(edge_ctx: i64, response_reserve: i64, overhead: i64) -> i64 {
    edge_budget_checked(edge_ctx, response_reserve, overhead).unwrap_or(0)
}

/// Query string for `GET /etern/sync/window`.
///
/// Sends the same three numbers [`edge_budget`] uses, so the desktop's proposal
/// and the phone's dispose start from an identical budget before the phone's
/// authoritative re-count.
#[uniffi::export]
pub fn window_query_string(
    edge_ctx: i64,
    response_reserve: i64,
    overhead: i64,
    device_id: String,
) -> String {
    let ctx = if edge_ctx <= 0 {
        DEFAULT_EDGE_CTX
    } else {
        edge_ctx
    };
    let reserve = if response_reserve < 0 {
        DEFAULT_RESPONSE_RESERVE
    } else {
        response_reserve
    };
    let overhead = if overhead < 0 {
        DEFAULT_EDGE_OVERHEAD
    } else {
        overhead
    };
    format!(
        "edge_ctx={ctx}&response_reserve={reserve}&overhead_tokens={overhead}&device_id={}",
        url_encode(&device_id)
    )
}

/// Percent-encodes everything outside RFC 3986 unreserved characters. Device
/// names are user-supplied, so a space or `&` must not reshape the query.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn edge_budget_checked(
    edge_ctx: i64,
    response_reserve: i64,
    overhead: i64,
) -> Result<i64, EternError> {
    let ctx = if edge_ctx <= 0 {
        DEFAULT_EDGE_CTX
    } else {
        edge_ctx
    };
    let reserve = if response_reserve < 0 {
        DEFAULT_RESPONSE_RESERVE
    } else {
        response_reserve
    };
    let overhead = if overhead < 0 {
        DEFAULT_EDGE_OVERHEAD
    } else {
        overhead
    };
    let budget = ctx - reserve - overhead;
    if budget <= 0 {
        return Err(EternError::Budget(format!(
            "edge_ctx {ctx} cannot cover response_reserve {reserve} + overhead {overhead}"
        )));
    }
    Ok(budget)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use etern_core::domain::Message as CoreMessage;
    use std::io::Write;

    fn msg(role: &str, content: &str) -> MessageDto {
        MessageDto {
            role: role.into(),
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn chat_turn(user: &str, reply: &str, seq: i64) -> TurnDto {
        new_chat_turn(
            mint_turn_id(),
            user.into(),
            reply.into(),
            "edge".into(),
            "qwen-edge".into(),
            "phone".into(),
            seq,
            1_700_000_000_000 + seq,
            vec![],
        )
        .unwrap()
    }

    fn temp_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "etern-ffi-{tag}-{}-{}.jsonl",
            std::process::id(),
            Uuid::now_v7()
        ));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn qr_payload_parses() {
        let t = parse_qr_payload(
            r#"{"host":"192.168.1.10","port":8443,"cert_pin":"AA==","code":"123456"}"#.into(),
        )
        .unwrap();
        assert_eq!(t.host, "192.168.1.10");
        assert_eq!(t.port, 8443);
        assert_eq!(t.cert_pin, "AA==");
        assert_eq!(t.code, "123456");
    }

    #[test]
    fn qr_payload_accepts_string_port_and_defaults() {
        let t = qr_payload(r#"{"host":"h","cert_pin":"p","code":"1","port":"8500"}"#).unwrap();
        assert_eq!(t.port, 8500);
        let d = qr_payload(r#"{"host":"h","cert_pin":"p","code":"1"}"#).unwrap();
        assert_eq!(d.port, DEFAULT_TLS_PORT);
    }

    #[test]
    fn qr_payload_rejects_bad_input() {
        assert!(qr_payload(r#"{"host":"","cert_pin":"p","code":"1"}"#).is_err());
        assert!(qr_payload(r#"{"host":"h","cert_pin":"","code":"1"}"#).is_err());
        assert!(qr_payload(r#"{"host":"h","cert_pin":"p","code":""}"#).is_err());
        assert!(qr_payload(r#"{"host":"h","cert_pin":"p","code":"1","port":0}"#).is_err());
        assert!(qr_payload(r#"{"host":"h","cert_pin":"p","code":"1","port":70000}"#).is_err());
        assert!(qr_payload(r#"{"host":"h","cert_pin":"p","code":"1","port":true}"#).is_err());
        assert!(qr_payload("not json").is_err());
    }

    #[test]
    fn routing_matrix_matches_plan_section_2() {
        let d = route(false, false);
        assert_eq!(d.target, "edge");
        assert!(d.hold_outbox && !d.flush_outbox);

        let d = route(true, true);
        assert_eq!(d.target, "high_power");
        assert!(!d.hold_outbox && d.flush_outbox);

        let d = route(true, false);
        assert_eq!(d.target, "edge");
        assert!(!d.hold_outbox && d.flush_outbox, "engine down still flushes");
    }

    #[test]
    fn probe_backoff_grows_and_caps() {
        assert_eq!(probe_backoff_ms(0), 2_000);
        assert_eq!(probe_backoff_ms(1), 4_000);
        assert_eq!(probe_backoff_ms(2), 8_000);
        assert_eq!(probe_backoff_ms(3), 16_000);
        assert_eq!(probe_backoff_ms(4), 30_000);
        assert_eq!(probe_backoff_ms(50), 30_000);
    }

    #[test]
    fn health_parses_gateway_shape() {
        let h = parse_health(
            r#"{"gateway_reachable":true,"engine_ready":"ready","engine_ready_s":true,
                "fail_over":false,"waking":false,"model":"big.gguf","template_sha":"ab12",
                "n_ctx":200000,"token_source":"measured","safety_margin":1.15,
                "supports_tools":true,"supports_vision":false,"engine_loaded":true,
                "cert_pin":"deadbeef","lan_ip":"192.168.1.10","tls_port":8443}"#
                .to_string(),
        );
        assert!(h.reachable);
        assert!(h.engine_serving);
        assert!(!h.fail_over);
        assert_eq!(h.engine_state, "ready");
        assert_eq!(h.n_ctx, 200_000);
        assert!(h.supports_tools);
        assert_eq!(h.cert_pin.as_deref(), Some("deadbeef"));
        assert_eq!(h.tls_port, 8443);
    }

    #[test]
    fn health_loading_is_serving_but_down_is_not() {
        let loading = parse_health(r#"{"engine_ready":"loading","fail_over":false}"#.into());
        assert!(loading.reachable && loading.engine_serving);

        let down = parse_health(r#"{"engine_ready":"down","fail_over":true}"#.into());
        assert!(down.reachable);
        assert!(!down.engine_serving);
        assert_eq!(down.engine_state, "down");
    }

    #[test]
    fn health_garbage_is_unreachable() {
        let h = parse_health("<html>502 bad gateway</html>".into());
        assert!(!h.reachable);
        assert!(!h.engine_serving);
    }

    #[test]
    fn outbox_round_trip_and_seq_continuity() {
        let path = temp_path("outbox");
        let ob = Outbox::new(path.clone()).unwrap();
        assert_eq!(ob.next_seq().unwrap(), 0);

        let t = chat_turn("hi", "hello", 0);
        ob.append_sync_turn(sync_turn_from_turn(t).unwrap()).unwrap();
        assert_eq!(ob.next_seq().unwrap(), 1);

        let list = ob.read_all().unwrap();
        assert_eq!(list.turns.len(), 1);
        assert_eq!(list.skipped, 0);
        assert!(ob.len_bytes().unwrap() > 0);

        ob.clear().unwrap();
        assert_eq!(ob.read_all().unwrap().turns.len(), 0);
        assert_eq!(ob.next_seq().unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn outbox_survives_a_torn_final_line() {
        let path = temp_path("torn");
        {
            let ob = Outbox::new(path.clone()).unwrap();
            let t = chat_turn("one", "ok", 0);
            ob.append_sync_turn(sync_turn_from_turn(t).unwrap()).unwrap();
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"turn_id\":\"trunc")
            .unwrap();
        let ob = Outbox::new(path.clone()).unwrap();
        let list = ob.read_all().unwrap();
        assert_eq!(list.turns.len(), 1, "the good line still loads");
        assert_eq!(list.skipped, 1, "the torn line is counted, not fatal");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dispose_uses_authoritative_costs() {
        let turns: Vec<TurnDto> = (0..5).map(|i| chat_turn("u", "a", i)).collect();
        let w = dispose_window(turns, 8_192, 700, 400, Some(vec![1_000; 5])).unwrap();
        assert_eq!(w.budget, 7_092);
        assert_eq!(w.turns.len(), 5, "5 x 1000 fits in 7092");
        assert_eq!(w.tokens_used, 5_000);
        assert!(!w.truncated);
        assert!(!w.estimated);
    }

    #[test]
    fn dispose_drops_oldest_and_reports_truncation() {
        let turns: Vec<TurnDto> = (0..5).map(|i| chat_turn("u", "a", i)).collect();
        let w = dispose_window(turns, 8_192, 700, 400, Some(vec![2_000; 5])).unwrap();
        assert_eq!(w.turns.len(), 3, "7092 fits three 2000-token turns");
        assert_eq!(w.tokens_used, 6_000);
        assert!(w.truncated);
        assert_eq!(w.turns.last().unwrap().seq, 4, "newest turn is kept");
        assert_eq!(w.turns.first().unwrap().seq, 2, "oldest two dropped");
    }

    #[test]
    fn dispose_can_return_an_empty_window() {
        let turns = vec![chat_turn("u", "a", 0)];
        let w = dispose_window(turns, 8_192, 700, 400, Some(vec![99_999])).unwrap();
        assert!(w.turns.is_empty(), "mirrors trim_to_budget: no forced keep");
        assert!(w.truncated);
    }

    #[test]
    fn dispose_rejects_mismatched_costs() {
        let turns = vec![chat_turn("u", "a", 1)];
        let err = dispose_window(turns, 8_192, 700, 400, Some(vec![10, 20])).unwrap_err();
        assert!(matches!(err, EternError::Invalid(_)));
    }

    #[test]
    fn dispose_falls_back_to_estimate() {
        let turns: Vec<TurnDto> = (0..3).map(|i| chat_turn("x", "y", i)).collect();
        let w = dispose_window(turns, 8_192, 700, 400, None).unwrap();
        assert!(w.estimated);
        assert_eq!(w.turns.len(), 3);
        assert!(w.tokens_used > 0);
    }

    #[test]
    fn dispose_rejects_impossible_budget() {
        let err = dispose_window(vec![], 1_000, 700, 400, None).unwrap_err();
        assert!(matches!(err, EternError::Budget(_)));
        assert_eq!(edge_budget(1_000, 700, 400), 0);
        assert_eq!(edge_budget(-1, -1, -1), 8_192 - 700 - 400);
    }

    #[test]
    fn push_body_shape_order_and_cap() {
        let turns = vec![
            SyncTurnDto {
                turn_id: mint_turn_id(),
                ts: 2,
                seq: 2,
                offline: true,
                messages: vec![msg("user", "b")],
            },
            SyncTurnDto {
                turn_id: mint_turn_id(),
                ts: 1,
                seq: 1,
                offline: true,
                messages: vec![msg("user", "a")],
            },
        ];
        let body = push_request_json("dev".into(), turns);
        let v: serde_json::Value = serde_json::from_str(&body.json).unwrap();
        assert_eq!(v["device_id"], "dev");
        assert_eq!(v["turns"][0]["seq"], 1, "sorted oldest-first");
        assert_eq!(v["turns"][1]["seq"], 2);
        assert!(!body.truncated);
        assert_eq!(body.count, 2);
    }

    #[test]
    fn push_body_keeps_newest_when_over_cap() {
        let turns: Vec<SyncTurnDto> = (0..(MAX_SYNC_BATCH + 5))
            .map(|i| SyncTurnDto {
                turn_id: mint_turn_id(),
                ts: i as i64,
                seq: i as i64,
                offline: true,
                messages: vec![msg("user", "x")],
            })
            .collect();
        let body = push_request_json("dev".into(), turns);
        assert!(body.truncated);
        assert_eq!(body.count, MAX_SYNC_BATCH as i64);
        let v: serde_json::Value = serde_json::from_str(&body.json).unwrap();
        assert_eq!(v["turns"][0]["seq"].as_i64(), Some(5), "oldest five dropped");
    }

    #[test]
    fn push_body_omits_absent_tool_fields() {
        let mut m = msg("assistant", "calling");
        m.tool_calls = Some(r#"[{"id":"c1","name":"web_search"}]"#.into());
        let body = push_request_json(
            "dev".into(),
            vec![SyncTurnDto {
                turn_id: mint_turn_id(),
                ts: 1,
                seq: 1,
                offline: true,
                messages: vec![m],
            }],
        );
        let v: serde_json::Value = serde_json::from_str(&body.json).unwrap();
        let msg0 = &v["turns"][0]["messages"][0];
        assert!(msg0["tool_calls"].is_string());
        assert!(msg0["tool_call_id"].is_null());
        assert!(msg0["name"].is_null());
    }

    #[test]
    fn push_ack_parses() {
        let a = parse_push_ack(r#"{"accepted":3,"duplicates":1,"journal":"sync-x.txt"}"#.into())
            .unwrap();
        assert_eq!(a.accepted, 3);
        assert_eq!(a.duplicates, 1);
        assert_eq!(a.journal.as_deref(), Some("sync-x.txt"));

        let replay =
            parse_push_ack(r#"{"accepted":0,"duplicates":2,"journal":null}"#.into()).unwrap();
        assert_eq!(replay.accepted, 0);
        assert!(replay.journal.is_none());
    }

    #[test]
    fn pair_request_validates_name() {
        let b = pair_request_json("123456".into(), "Z Fold 5".into()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&b.json).unwrap();
        assert_eq!(v["code"], "123456");
        assert_eq!(v["name"], "Z Fold 5");
        assert!(pair_request_json("1".into(), "   ".into()).is_err());
        assert!(pair_request_json("1".into(), "x".repeat(65)).is_err());
    }

    #[test]
    fn pair_response_parses() {
        let d = parse_paired_device(
            r#"{"device_id":"dev-1","token":"tok","device_name":"Z Fold 5"}"#.into(),
        )
        .unwrap();
        assert_eq!(d.device_id, "dev-1");
        assert_eq!(d.token, "tok");
        assert!(parse_paired_device(r#"{"device_id":"","token":"t"}"#.into()).is_err());
    }

    #[test]
    fn tool_fields_survive_the_round_trip() {
        let mut t = chat_turn("read my note", "here it is", 0);
        t.messages.insert(
            1,
            MessageDto {
                role: "assistant".into(),
                content: String::new(),
                name: None,
                tool_calls: Some(r#"[{"id":"c1","name":"read_file"}]"#.into()),
                tool_call_id: None,
            },
        );
        t.messages.insert(
            2,
            MessageDto {
                role: "tool".into(),
                content: "the note".into(),
                name: None,
                tool_calls: None,
                tool_call_id: Some("c1".into()),
            },
        );

        let sync = sync_turn_from_turn(t.clone()).unwrap();
        assert_eq!(sync.messages.len(), 4);
        assert_eq!(
            sync.messages[1].tool_calls.as_deref(),
            Some(r#"[{"id":"c1","name":"read_file"}]"#)
        );
        assert_eq!(sync.messages[2].tool_call_id.as_deref(), Some("c1"));

        let back = turn_from_sync_turn(sync, "phone".into(), "qwen-edge".into()).unwrap();
        assert_eq!(back.turn_id, t.turn_id);
        assert_eq!(back.kind, "sync");
        assert_eq!(back.engine, "edge");
        assert_eq!(back.messages[2].role, "tool");
    }

    #[test]
    fn turn_rejects_bad_id_and_empty_messages() {
        let mut t = chat_turn("u", "a", 0);
        t.turn_id = "not-a-uuid".into();
        assert!(sync_turn_from_turn(t).is_err());

        let mut e = chat_turn("u", "a", 0);
        e.messages.clear();
        assert!(sync_turn_from_turn(e).is_err());
    }

    #[test]
    fn final_text_skips_tool_steps() {
        let mut t = chat_turn("q", "final answer", 0);
        t.messages.insert(
            1,
            MessageDto {
                role: "assistant".into(),
                content: String::new(),
                name: None,
                tool_calls: Some("[]".into()),
                tool_call_id: None,
            },
        );
        assert_eq!(turn_final_text(t).unwrap().as_deref(), Some("final answer"));
    }

    #[test]
    fn core_messages_convert_losslessly() {
        let core = CoreMessage {
            role: Role::System,
            content: "sys".into(),
            name: Some("n".into()),
            tool_calls: None,
            tool_call_id: Some("id".into()),
        };
        let dto = from_message(&core);
        assert_eq!(dto.role, "system");
        assert_eq!(dto.name.as_deref(), Some("n"));
        assert_eq!(dto.tool_call_id.as_deref(), Some("id"));
        assert_eq!(to_message(dto), core);
    }

    fn edge_tools(tag: &str) -> EdgeTools {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "etern-ffi-tools-{tag}-{}-{}",
            std::process::id(),
            Uuid::now_v7()
        ));
        EdgeTools::new(dir.to_string_lossy().to_string()).unwrap()
    }

    #[test]
    fn edge_tools_advertise_the_reduced_set() {
        let rt = edge_tools("defs");
        let defs: serde_json::Value = serde_json::from_str(&rt.definitions()).unwrap();
        assert_eq!(defs.as_array().map(|a| a.len()), Some(3));
        assert_eq!(
            rt.names(),
            vec!["read_file".to_string(), "write_file".to_string(), "remember".to_string()]
        );
        // The edge profile must not promise a search it cannot perform.
        assert!(!rt.definitions().contains("web_search"));
        assert!(!rt.definitions().contains("list_dir"));
    }

    #[test]
    fn edge_tools_write_then_read_inside_the_jail() {
        let rt = edge_tools("roundtrip");
        let wrote: serde_json::Value = serde_json::from_str(
            &rt.execute(
                "write_file".into(),
                r#"{"path":"notes/x.txt","content":"offline note"}"#.into(),
                "qwen-edge".into(),
                "turn-1".into(),
                1_700_000_000_000,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(wrote["ok"], serde_json::json!(true));

        let read: serde_json::Value = serde_json::from_str(
            &rt.execute(
                "read_file".into(),
                r#"{"path":"notes/x.txt"}"#.into(),
                "qwen-edge".into(),
                "turn-1".into(),
                1_700_000_000_000,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(read["ok"], serde_json::json!(true));
        assert!(read["content"].to_string().contains("offline note"));
    }

    #[test]
    fn edge_tools_refuse_escapes_and_bad_json_as_retryable() {
        let rt = edge_tools("refuse");

        let escaped: serde_json::Value = serde_json::from_str(
            &rt
                .execute(
                    "read_file".into(),
                    r#"{"path":"../../Windows/win.ini"}"#.into(),
                    "m".into(),
                    "t".into(),
                    0,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(escaped["ok"], serde_json::json!(false));
        assert_eq!(escaped["retryable"], serde_json::json!(true));
        assert_eq!(escaped["error"]["kind"], serde_json::json!("sandbox"));

        let broken: serde_json::Value = serde_json::from_str(
            &rt.execute("read_file".into(), "{not json".into(), "m".into(), "t".into(), 0).unwrap(),
        )
        .unwrap();
        assert_eq!(broken["ok"], serde_json::json!(false));
        assert_eq!(broken["retryable"], serde_json::json!(true));

        let unknown: serde_json::Value = serde_json::from_str(
            &rt
                .execute(
                    "web_search".into(),
                    r#"{"query":"x"}"#.into(),
                    "m".into(),
                    "t".into(),
                    0,
                )
                .unwrap(),
        )
        .unwrap();
        // `ToolErrorKind` is debug-formatted then lower-cased, so the wire token
        // is "unknowntool" — the same string the gateway emits.
        assert_eq!(unknown["error"]["kind"], serde_json::json!("unknowntool"));
    }

    #[test]
    fn window_proposal_parses_gateway_shape() {
        let turn = chat_turn("u", "a", 7);
        let body = serde_json::json!({
            "messages": [serde_json::to_value(to_turn(turn.clone()).unwrap()).unwrap()],
            "window_seq": 7,
            "template_sha": "abc123",
            "est_tokens": 1234,
            "budget_tokens": 7092,
            "token_source": "estimated",
            "truncated": true,
            "apply": "replace",
            "store_head_ts": 1_700_000_000_000i64
        })
        .to_string();

        let p = parse_window_proposal(body).unwrap();
        assert_eq!(p.turns.len(), 1);
        assert_eq!(p.turns[0].turn_id, turn.turn_id);
        assert_eq!(p.turns[0].messages[0].content, "u");
        assert_eq!(p.turns[0].messages[1].content, "a");
        assert_eq!(p.window_seq, Some(7));
        assert_eq!(p.template_sha, "abc123");
        assert_eq!(p.est_tokens, 1234);
        assert_eq!(p.budget_tokens, 7092);
        assert!(p.truncated);
        assert_eq!(p.apply, "replace");
        assert_eq!(p.store_head_ts, Some(1_700_000_000_000));
    }

    #[test]
    fn window_proposal_tolerates_an_empty_window() {
        let p = parse_window_proposal(
            r#"{"messages":[],"template_sha":"","est_tokens":0,"budget_tokens":7092,
                "token_source":"estimated","truncated":false,"apply":"replace"}"#
                .replace(['\n', ' ', '\t'], ""),
        )
        .unwrap();
        assert!(p.turns.is_empty());
        assert_eq!(p.window_seq, None);
        assert_eq!(p.store_head_ts, None);
    }

    #[test]
    fn window_proposal_rejects_garbage() {
        assert!(parse_window_proposal("<html>502</html>".into()).is_err());
    }

    #[test]
    fn window_query_matches_edge_budget_inputs() {
        let q = window_query_string(8192, 700, 400, "Z Fold 5".into());
        assert_eq!(
            q,
            "edge_ctx=8192&response_reserve=700&overhead_tokens=400&device_id=Z%20Fold%205"
        );
        // Defaults kick in the same way edge_budget clamps them.
        let d = window_query_string(0, -1, -1, "p".into());
        assert!(d.starts_with(
            "edge_ctx=8192&response_reserve=700&overhead_tokens=400&device_id=p"
        ));
    }
}
