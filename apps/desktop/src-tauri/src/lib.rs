//! eternAI desktop shell.
//!
//! The window is a thin view; every capability lives behind the gateway. The
//! Rust side is the gateway's client so the webview never talks to the network
//! directly (no CORS, and the bearer token never enters the page's JS reach).
//! Streaming chat is relayed to the frontend as Tauri events.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Emitter;

pub mod tool_loop;

/// Where the gateway lives and how to authenticate. The frontend owns these
/// (settings panel) and passes them per call, so a token is never persisted by
/// the shell.
#[derive(Debug, Clone, Deserialize)]
pub struct Conn {
    /// Gateway base URL, e.g. `http://127.0.0.1:8090` (no trailing slash).
    pub base: String,
    /// Bearer token; empty or absent means the gateway has auth disabled.
    #[serde(default)]
    pub token: Option<String>,
}

impl Conn {
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base.trim_end_matches('/'), path);
        let req = reqwest::Client::new().request(method, url);
        match self.token.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}")),
            None => req,
        }
    }
}

/// A chat message in the shape the OpenAI-compatible engine expects.
///
/// `tool_calls` is kept as a `Value` because the engine wants the real array;
/// the canonical store wants it as a string, so it is re-serialised once when
/// the turn is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Correlation id the frontend minted; every streamed event carries it.
    pub id: String,
    pub messages: Vec<ChatMessage>,
    /// Optional model override; the engine usually has exactly one loaded.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// User-authored style preference. This is added as a separate system
    /// message and is never part of the stored transcript.
    #[serde(default)]
    pub personality: Option<String>,
}

// -- simple request/response commands --------------------------------------

/// Unauthenticated reachability + engine state. The frontend polls this for the
/// model chip and the "am I at home" indicator.
#[tauri::command]
async fn gateway_health(conn: Conn) -> Result<Value, String> {
    conn.request(reqwest::Method::GET, "/health")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad health response: {e}"))
}

#[tauri::command]
async fn gateway_props(conn: Conn) -> Result<Value, String> {
    conn.request(reqwest::Method::GET, "/props")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad props response: {e}"))
}

#[tauri::command]
async fn personality_sync(conn: Conn, value: String, updated_at: i64) -> Result<Value, String> {
    let response = conn
        .request(reqwest::Method::POST, "/etern/personality/sync")
        .json(&json!({ "value": value, "updated_at": updated_at }))
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("bad personality response: {e}"))?;
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("gateway rejected the personality setting");
        return Err(format!("{status}: {message}"));
    }
    Ok(body)
}

/// Purges every stored turn from the gateway (journal files are untouched).
/// The gateway refuses a device token, so this only works with the owner token
/// configured in the app.
#[tauri::command]
async fn history_clear(conn: Conn) -> Result<Value, String> {
    let resp = conn
        .request(reqwest::Method::POST, "/etern/history/clear")
        .json(&serde_json::json!({ "confirm": true }))
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("bad clear response: {e}"))?;
    if !status.is_success() {
        let msg = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("gateway rejected the request");
        return Err(format!("{status}: {msg}"));
    }
    Ok(body)
}

/// Loads a bounded transcript from the gateway for the desktop chat view.
#[tauri::command]
async fn chat_history(conn: Conn) -> Result<Value, String> {
    let resp = conn
        .request(reqwest::Method::GET, "/etern/turns?limit=500")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("could not read history response: {e}"))?;
    if !status.is_success() {
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| {
                if body.trim().is_empty() {
                    "empty error response; restart the gateway to enable chat history sync".into()
                } else {
                    body.chars().take(240).collect()
                }
            });
        return Err(format!("{status}: {message}"));
    }
    serde_json::from_str(&body).map_err(|e| {
        let preview: String = body.chars().take(240).collect();
        format!("bad history response ({status}): {e}; body: {preview}")
    })
}

#[tauri::command]
async fn journal_list(conn: Conn) -> Result<Value, String> {
    conn.request(reqwest::Method::GET, "/etern/journal")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad journal response: {e}"))
}

#[tauri::command]
async fn journal_read(conn: Conn, name: String) -> Result<Value, String> {
    let path = format!("/etern/journal/{}", urlencoding(&name));
    conn.request(reqwest::Method::GET, &path)
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad journal response: {e}"))
}

/// Minimal percent-encoding for a single path segment. The journal names are
/// ASCII (`YYYY-MM-DD.txt`, `sync-*.txt`), but a hand-named file could contain
/// spaces, so encode everything outside the unreserved set.
fn urlencoding(s: &str) -> String {
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

// -- pairing (M6) -----------------------------------------------------------

/// Mints a one-time pairing code on the gateway (`POST /etern/pairings`).
#[tauri::command]
async fn pair_create(conn: Conn) -> Result<Value, String> {
    conn.request(reqwest::Method::POST, "/etern/pairings")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad pairing response: {e}"))
}

/// Lists paired devices (`GET /etern/devices`).
#[tauri::command]
async fn devices_list(conn: Conn) -> Result<Value, String> {
    conn.request(reqwest::Method::GET, "/etern/devices")
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad devices response: {e}"))
}

/// Revokes a device (`POST /etern/devices/{id}/revoke`).
#[tauri::command]
async fn device_revoke(conn: Conn, id: String) -> Result<Value, String> {
    let path = format!("/etern/devices/{}/revoke", urlencoding(&id));
    conn.request(reqwest::Method::POST, &path)
        .send()
        .await
        .map_err(|e| format!("gateway unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad revoke response: {e}"))
}

/// Renders an arbitrary string as a QR code SVG, ready for `innerHTML`.
#[tauri::command]
fn pair_qr(payload: String) -> Result<Value, String> {
    let svg = qr_svg(&payload, 6)?;
    Ok(json!({ "svg": svg, "payload": payload }))
}

/// Builds a black-on-white QR SVG for `data`. Each module is `module_px`
/// screen pixels; a 4-module quiet zone is added per the QR spec.
fn qr_svg(data: &str, module_px: u32) -> Result<String, String> {
    use qrcode::{Color, EcLevel, QrCode};
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .map_err(|e| format!("cannot encode pairing payload: {e}"))?;
    let width = code.width();
    let quiet: usize = 4;
    let side = width + quiet * 2;
    let mut path = String::new();
    for (i, color) in code.to_colors().iter().enumerate() {
        if *color == Color::Dark {
            let x = i % width + quiet;
            let y = i / width + quiet;
            path.push_str(&format!("M{x} {y}h1v1h-1z"));
        }
    }
    let px = side as u32 * module_px;
    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {side} {side}" width="{px}" height="{px}" shape-rendering="crispEdges"><rect width="100%" height="100%" fill="#ffffff"/><path fill="#000000" d="{path}"/></svg>"##
    ))
}

// -- streaming chat ---------------------------------------------------------

/// Runs one chat turn to completion, relaying progress to the frontend:
///   `chat://delta` `{ id, delta }`          assistant text
///   `chat://tool`  `{ id, tool, ok, error }` one executed tool step
///   `chat://done`  `{ id }`
///   `chat://error` `{ id, message }`
///
/// The frontend awaits this command only for the terminal error case; deltas
/// arrive as they are parsed from the SSE stream.
#[tauri::command]
async fn chat_send(app: tauri::AppHandle, conn: Conn, req: ChatRequest) -> Result<(), String> {
    let ChatRequest {
        id,
        messages,
        model,
        temperature,
        personality,
    } = req;
    let personality = personality.unwrap_or_default();
    if personality.chars().count() > 2000 {
        return Err("personality must be 2000 characters or fewer".into());
    }
    let mut options = json!({});
    if let Some(m) = model {
        options["model"] = json!(m);
    }
    if let Some(t) = temperature {
        options["temperature"] = json!(t);
    }

    let outcome = tool_loop::run(
        &tool_loop::TauriSink(&app),
        &conn,
        messages,
        &options,
        &id,
        &personality,
    )
    .await?;
    // When the round budget ran out the closing line is ours, not the model's,
    // so it never passed through the stream — deliver it like any other delta.
    if outcome.aborted {
        let _ = app.emit("chat://delta", json!({ "id": id, "delta": outcome.reply }));
    }
    let _ = app.emit("chat://done", json!({ "id": id }));

    // The daily journal is generated from the gateway's store, so an exchange
    // that is never recorded is a day that never happened. The gateway could
    // not do this itself without parsing the stream it promises to pass
    // through untouched, so the shell — which already reassembled the reply to
    // render it — reports it here. The whole exchange goes, tool traffic
    // included: the phone's edge window has to be able to replay it, and the
    // digest skips intermediate messages on its own. Only the new exchange is
    // sent, never the whole history, and the request's own id makes the write
    // idempotent.
    if let Some(payload) = recordable_turn(&id, &outcome.exchange) {
        if let Err(e) = conn
            .request(reqwest::Method::POST, "/etern/turns")
            .json(&payload)
            .send()
            .await
        {
            // The reply was already delivered; losing the record of it is
            // worth reporting but not worth failing the turn over.
            let _ = app.emit(
                "chat://notice",
                json!({ "id": id, "message": format!("journal record failed: {e}") }),
            );
        }
    }
    Ok(())
}

/// The `/etern/turns` body for one finished exchange, or `None` when there is
/// nothing worth journaling (no prompt, no reply).
///
/// The canonical store keeps `tool_calls` as a raw JSON *string* so a replay is
/// byte-for-byte, while the engine wants the real array — hence the conversion
/// here rather than serialising [`ChatMessage`] directly.
///
/// The gateway caps a record at 64 messages. A turn that long means a model
/// that would not stop, so the prompt and the most recent messages are kept and
/// the middle of the tool chatter is dropped: the digest needs the question and
/// the answer, and the edge window needs the tail.
pub fn recordable_turn(turn_id: &str, exchange: &[ChatMessage]) -> Option<Value> {
    if exchange.is_empty() {
        return None;
    }
    if !exchange.iter().any(|m| m.role == "user") {
        return None;
    }
    if !exchange
        .iter()
        .any(|m| m.role == "assistant" && !m.content.trim().is_empty())
    {
        return None;
    }

    const MAX_RECORD_MESSAGES: usize = 64;
    let kept: Vec<ChatMessage> = if exchange.len() <= MAX_RECORD_MESSAGES {
        exchange.to_vec()
    } else {
        let mut v = vec![exchange[0].clone()];
        v.extend_from_slice(&exchange[exchange.len() - (MAX_RECORD_MESSAGES - 1)..]);
        v
    };

    let messages: Vec<Value> = kept
        .iter()
        .map(|m| {
            let mut obj = json!({ "role": m.role, "content": m.content });
            if let Some(name) = &m.name {
                obj["name"] = json!(name);
            }
            if let Some(calls) = &m.tool_calls {
                obj["tool_calls"] = json!(calls.to_string());
            }
            if let Some(id) = &m.tool_call_id {
                obj["tool_call_id"] = json!(id);
            }
            obj
        })
        .collect();
    Some(json!({ "turn_id": turn_id, "messages": messages }))
}


#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            gateway_health,
            gateway_props,
            personality_sync,
            journal_list,
            journal_read,
            chat_history,
            chat_send,
            history_clear,
            pair_create,
            devices_list,
            device_revoke,
            pair_qr
        ])
        .run(tauri::generate_context!())
        .expect("error while running eternAI desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_escapes_spaces_and_keeps_journal_names() {
        assert_eq!(urlencoding("2026-11-19.txt"), "2026-11-19.txt");
        assert_eq!(urlencoding("sync-2026-11-19 01.txt"), "sync-2026-11-19%2001.txt");
        assert_eq!(urlencoding("a/b.txt"), "a%2Fb.txt");
    }

    #[test]
    fn qr_svg_contains_modules_and_quiet_zone() {
        let svg = qr_svg(r#"{"host":"192.168.1.5","port":8443}"#, 6).unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("shape-rendering=\"crispEdges\""));
        // Modules exist and the viewBox includes the 4-module quiet zone.
        assert!(svg.contains("<path fill=\"#000000\" d=\"M"));
        let vb = svg
            .split("viewBox=\"0 0 ")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(vb >= 21 + 8, "viewBox side too small: {vb}");
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    #[test]
    fn records_the_whole_new_exchange() {
        let exchange = vec![
            msg("user", "what is in my notes?"),
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                name: None,
                tool_calls: Some(json!([{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.md\"}"}}])),
                tool_call_id: None,
            },
            ChatMessage {
                role: "tool".into(),
                content: "note text".into(),
                name: Some("read_file".into()),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
            },
            msg("assistant", "Your note says: note text"),
        ];
        let payload = recordable_turn("11111111-1111-1111-1111-111111111111", &exchange).unwrap();
        assert_eq!(payload["turn_id"], "11111111-1111-1111-1111-111111111111");
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        // The store wants tool_calls as a raw JSON string, not a nested array.
        assert!(messages[1]["tool_calls"].is_string());
        let replayed: Value = serde_json::from_str(messages[1]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(replayed[0]["function"]["name"], "read_file");
        assert_eq!(messages[2]["tool_call_id"], "c1");
        assert_eq!(messages[2]["name"], "read_file");
    }

    #[test]
    fn nothing_is_recorded_without_a_reply_or_a_prompt() {
        assert!(recordable_turn("t", &[]).is_none());
        assert!(recordable_turn("t", &[msg("user", "hi")]).is_none());
        assert!(recordable_turn("t", &[msg("assistant", "   ")]).is_none());
        assert!(recordable_turn("t", &[msg("assistant", "no prompt")]).is_none());
    }

    #[test]
    fn an_over_long_exchange_keeps_its_ends() {
        let mut exchange = vec![msg("user", "the question")];
        for i in 0..100 {
            exchange.push(msg("assistant", &format!("step {i}")));
        }
        let payload = recordable_turn("t", &exchange).unwrap();
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 64);
        assert_eq!(messages[0]["content"], "the question");
        assert_eq!(messages[63]["content"], "step 99");
    }
}
