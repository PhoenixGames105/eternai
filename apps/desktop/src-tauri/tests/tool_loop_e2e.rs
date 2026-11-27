//! End-to-end drive of the desktop tool loop against a mock gateway.
//!
//! Unit tests cover the pieces; this covers the wiring that only shows up with
//! a real socket: SSE framing, the native-vs-text decision, the batch execute
//! round trip, and what actually ends up in the transcript we journal.

use etern_desktop_lib::tool_loop::{self, CancelFlag, Sink};
use etern_desktop_lib::{recordable_turn, ChatMessage, Conn};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Records every event the loop emits, in order.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<(String, Value)>>,
}

impl Sink for Recorder {
    fn emit(&self, event: &str, payload: Value) {
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
    }
}

impl Recorder {
    /// Concatenated `delta` fields — what the user saw, in order.
    fn deltas(&self) -> String {
        self.join("chat://delta", "delta")
    }

    fn notices(&self) -> String {
        self.join("chat://notice", "message")
    }

    fn join(&self, event: &str, field: &str) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == event)
            .filter_map(|(_, p)| p.get(field).and_then(Value::as_str))
            .collect::<String>()
    }

    fn tools(&self) -> Vec<(String, bool)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "chat://tool")
            .map(|(_, p)| {
                (
                    p["tool"].as_str().unwrap_or_default().to_string(),
                    p["ok"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    }
}

/// What the mock engine answers each time it is asked to generate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// Native `delta.tool_calls` on the first turn, prose afterwards.
    Native,
    /// An `ETOOL` line on the first turn, prose afterwards.
    Text,
    /// A tool call every turn, to exhaust the round budget.
    Endless,
    /// Prose that merely contains braces.
    PlainProse,
    /// Prose that trickles forever, so a stop can interrupt it mid-stream.
    SlowProse,
}

/// A stand-in for the gateway: real socket, real SSE, no model.
struct MockGateway {
    base: String,
    generations: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl MockGateway {
    fn start(script: Script, supports_tools: bool, tools_available: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let generations = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));

        let (gen, body_tx) = (Arc::clone(&generations), Arc::clone(&bodies));
        // The accept loop lives for the rest of the test binary; nothing dials
        // the port again once the test that started it has finished.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (gen, body_tx) = (Arc::clone(&gen), Arc::clone(&body_tx));
                std::thread::spawn(move || {
                    if let Ok(stream) = stream {
                        let _ = serve(
                            stream,
                            script,
                            supports_tools,
                            tools_available,
                            &gen,
                            &body_tx,
                        );
                    }
                });
            }
        });

        Self {
            base,
            generations,
            bodies,
        }
    }

    fn conn(&self) -> Conn {
        Conn {
            base: self.base.clone(),
            token: Some("test-token".into()),
        }
    }

    /// Bodies of every `/v1/chat/completions` request, oldest first.
    fn prompts(&self) -> Vec<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .filter_map(|b| serde_json::from_str(b).ok())
            .collect()
    }
}

fn serve(
    mut stream: TcpStream,
    script: Script,
    supports_tools: bool,
    tools_available: bool,
    generations: &AtomicUsize,
    bodies: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut authorized = false;
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        // `Authorization: Bearer test-token` with every space removed.
        let flat = line.trim().to_ascii_lowercase().replace(' ', "");
        if flat == "authorization:bearertest-token" {
            authorized = true;
        }
        if let Some(v) = flat.strip_prefix("content-length:") {
            length = v.parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();

    if !authorized {
        return write_json(&mut stream, "401 Unauthorized", &json!({"error": "no"}));
    }
    if path == "/health" {
        return write_json(
            &mut stream,
            "200 OK",
            &json!({"status": "ok", "supports_tools": supports_tools}),
        );
    }
    if path.starts_with("/etern/tools?") {
        if !tools_available {
            return write_json(
                &mut stream,
                "404 Not Found",
                &json!({"error": "every tool is disabled"}),
            );
        }
        return write_json(
            &mut stream,
            "200 OK",
            &json!([
                {"type": "function", "function": {"name": "read_file", "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}},
                {"type": "function", "function": {"name": "remember", "description": "Save a fact",
                    "parameters": {"type": "object", "properties": {"fact": {"type": "string"}}}}}
            ]),
        );
    }
    if path == "/etern/tools/execute" {
        return write_json(&mut stream, "200 OK", &execute_response(&body));
    }
    if path == "/v1/chat/completions" {
        bodies.lock().unwrap().push(body);
        let turn = generations.fetch_add(1, Ordering::SeqCst);
        return sse(stream, script, turn);
    }
    write_json(&mut stream, "404 Not Found", &json!({"error": "unknown path"}))
}

fn write_json(stream: &mut TcpStream, status: &str, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
}

/// Streams a generation as SSE. Fragments are deliberately split mid-token so
/// reassembly is exercised, not just parsing.
fn sse(mut stream: TcpStream, script: Script, turn: usize) -> std::io::Result<()> {
    let chunks: Vec<String> = match (script, turn) {
        (Script::Native, 0) => vec![
            chunk(&json!({"choices":[{"delta":{"role":"assistant","content":""}}]})),
            chunk(&json!({"choices":[{"delta":{"content":"Let me check"}}]})),
            chunk(&json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]})),
            chunk(&json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"path\":\""}}]}}]})),
            chunk(&json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"inbox/a.md\"}"}}]}}]})),
        ],
        (Script::Text, 0) => vec![
            chunk(&json!({"choices":[{"delta":{"content":"Sure. ET"}}]})),
            chunk(&json!({"choices":[{"delta":{"content":
                "OOL {\"tool\":\"remember\",\"arguments\":{\"fact\":\"the sky is green\"}}"}}]})),
        ],
        (Script::Endless, _) => vec![chunk(&json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_x","function":{"name":"read_file",
                "arguments":"{\"path\":\"inbox/a.md\"}"}}]}}]}))],
        (Script::PlainProse, _) => vec![chunk(&json!({"choices":[{"delta":{"content":
            "Braces { like this } are not tool calls."}}]}))],
        (Script::SlowProse, _) => vec![],
        _ => vec![chunk(&json!({"choices":[{"delta":{"content":"Your note says hello."}}]}))],
    };

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )?;
    if script == Script::SlowProse {
        // Trickles tokens forever. The test's stop has to cut this short, so
        // the mock keeps writing until the client hangs the connection up and
        // the write fails.
        for word in ["the ", "quick ", "brown ", "fox ", "jumps ", "over "]
            .iter()
            .cycle()
            .take(5_000)
        {
            let piece = chunk(&json!({"choices":[{"delta":{"content":word}}]}));
            stream.write_all(piece.as_bytes())?;
            stream.flush()?;
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        return Ok(());
    }
    for c in chunks {
        write!(stream, "{c}")?;
    }
    write!(stream, "data: [DONE]\n\n")
}

fn chunk(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// Mirrors the gateway: one result per call, echoed back with its id.
fn execute_response(body: &str) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(body) else {
        return json!({"error": "bad json"});
    };
    let results = request["calls"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|call| {
            let name = call["name"].as_str().unwrap_or_default();
            let id = call["id"].clone();
            match name {
                "remember" => json!({"id": id, "name": name, "ok": true, "content": {"saved": true}}),
                "read_file" => json!({"id": id, "name": name, "ok": true, "content": "hello from the tool"}),
                other => json!({"id": id, "name": other, "ok": false, "retryable": false,
                    "error": {"kind": "unknown_tool", "message": "no such tool", "retryable": false}}),
            }
        })
        .collect::<Vec<_>>();
    json!({"results": results})
}

fn prompt() -> Vec<ChatMessage> {
    vec![
        ChatMessage::text("system", "Be brief."),
        ChatMessage::text("user", "what did my note say?"),
    ]
}

#[tokio::test]
async fn native_tool_call_round_trips_and_is_journaled() {
    let gateway = MockGateway::start(Script::Native, true, true);
    let recorder = Recorder::default();

    let outcome = tool_loop::run(
        &recorder,
        &gateway.conn(),
        prompt(),
        &json!({"model": "test"}),
        "turn-1",
        "",
        &CancelFlag::default(),
    )
    .await
    .expect("loop completes");

    assert!(!outcome.aborted);
    assert_eq!(outcome.reply, "Your note says hello.");
    assert_eq!(recorder.tools(), vec![("read_file".to_string(), true)]);
    assert_eq!(recorder.deltas(), "Let me checkYour note says hello.");
    assert_eq!(recorder.notices(), "");

    // Asked natively, with the caller's options preserved; the second request
    // carries the assistant's call and the tool's answer.
    let prompts = gateway.prompts();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(prompts[0]["model"], "test");
    assert_eq!(prompts[0]["stream"], true);
    let second = prompts[1]["messages"].as_array().unwrap();
    assert_eq!(second[2]["tool_calls"][0]["function"]["name"], "read_file");
    assert_eq!(
        second[2]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"inbox/a.md"}"#
    );
    assert_eq!(second[3]["role"], "tool");
    assert_eq!(second[3]["tool_call_id"], "call_1");
    assert_eq!(second[3]["content"], "hello from the tool");

    // The exchange is what gets recorded: the prompt, then only the new
    // messages, tool traffic included, so the edge window can replay them.
    let roles: Vec<&str> = outcome.exchange.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
    assert_eq!(outcome.exchange[0].content, "what did my note say?");
    assert_eq!(outcome.exchange[3].content, "Your note says hello.");

    // And the record the shell posts must carry the tool traffic in the shape
    // the canonical store wants: `tool_calls` as a JSON string, `tool` results
    // keyed by id. The digest and the edge window both read this.
    let record = recordable_turn("turn-1", &outcome.exchange).expect("tool turn is journalable");
    let stored = record["messages"].as_array().unwrap();
    assert_eq!(stored.len(), 4);
    assert!(stored[1]["tool_calls"].is_string());
    assert!(stored[1]["tool_calls"].as_str().unwrap().contains("read_file"));
    assert_eq!(stored[2]["role"], "tool");
    assert_eq!(stored[2]["tool_call_id"], "call_1");
    assert_eq!(stored[2]["name"], "read_file");
}

#[tokio::test]
async fn text_protocol_is_used_when_the_template_cannot_carry_tools() {
    let gateway = MockGateway::start(Script::Text, false, true);
    let recorder = Recorder::default();

    let outcome = tool_loop::run(
        &recorder,
        &gateway.conn(),
        prompt(),
        &json!({}),
        "turn-2",
        "warm and concise",
        &CancelFlag::default(),
    )
        .await
        .expect("loop completes");

    assert_eq!(outcome.reply, "Your note says hello.");
    assert_eq!(recorder.tools(), vec![("remember".to_string(), true)]);
    // The protocol line must never reach the screen, even split across chunks.
    assert_eq!(recorder.deltas(), "Sure. Your note says hello.");

    let prompts = gateway.prompts();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts[0].get("tools").is_none(),
        "text mode sends no tools array"
    );
    let system = prompts[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("Be brief.\n\n"));
    assert!(system.contains("ETOOL"));
    let personality = prompts[0]["messages"][1]["content"].as_str().unwrap();
    assert!(personality.contains("style only"));
    assert!(personality.contains("\"warm and concise\""));

    // The text protocol has no tool role: the request is the assistant's own
    // text, and the result arrives as a labelled user turn.
    let second = prompts[1]["messages"].as_array().unwrap();
    assert!(second[3]["content"].as_str().unwrap().contains("ETOOL"));
    assert_eq!(second[4]["role"], "user");
    assert!(second[4]["content"]
        .as_str()
        .unwrap()
        .starts_with("[tool remember result]"));
    // The stored exchange carries the prompt, the model's protocol line, the
    // result as a user turn, then the closing prose.
    let roles: Vec<&str> = outcome.exchange.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    // The intermediate message keeps the protocol line the model actually
    // wrote, so a replay is faithful; the closing message is prose only.
    assert!(outcome.exchange[1].content.contains("ETOOL"));
    assert!(!outcome.exchange[3].content.contains("ETOOL"));
}

#[tokio::test]
async fn an_endless_model_hits_the_round_budget_and_says_so() {
    let gateway = MockGateway::start(Script::Endless, true, true);
    let recorder = Recorder::default();

    let outcome = tool_loop::run(&recorder, &gateway.conn(), prompt(), &json!({}), "turn-3", "", &CancelFlag::default())
        .await
        .expect("loop stops itself");

    assert!(outcome.aborted);
    assert!(!outcome.cancelled);
    assert_eq!(
        outcome.reply,
        format!("I stopped after {} tool rounds without reaching an answer.", tool_loop::MAX_ROUNDS)
    );
    assert_eq!(
        gateway.generations.load(Ordering::SeqCst),
        tool_loop::MAX_ROUNDS
    );
    assert_eq!(recorder.tools().len(), tool_loop::MAX_ROUNDS);
    assert!(recorder
        .notices()
        .contains(&format!("exhausted after {} tool calls", tool_loop::MAX_ROUNDS)));
}

#[tokio::test]
async fn a_stopped_turn_keeps_the_partial_reply_and_stops_reading() {
    // The engine streams prose forever; the user stops after the first chunk.
    let gateway = MockGateway::start(Script::SlowProse, true, true);
    let recorder = Recorder::default();
    let cancel = CancelFlag::default();

    let flag = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        flag.cancel();
    });

    let outcome = tool_loop::run(
        &recorder,
        &gateway.conn(),
        prompt(),
        &json!({}),
        "turn-5",
        "",
        &cancel,
    )
    .await
    .expect("stop ends the turn");

    assert!(outcome.cancelled);
    assert!(!outcome.aborted);
    // Some, but emphatically not all, of the reply reached the screen.
    let streamed = recorder.deltas();
    assert!(!streamed.is_empty(), "the partial reply is what the user got");
    assert!(
        streamed.len() < "the quick brown fox ".repeat(50).len(),
        "reading stopped instead of draining the stream"
    );
    assert_eq!(streamed, outcome.reply);

    // A stopped turn is still history: the journal record needs the question
    // and the partial answer, or the day looks like it never happened.
    let record = recordable_turn("turn-5", &outcome.exchange).expect("stopped turn is journalable");
    let messages = record["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages.last().unwrap()["role"], "assistant");
    assert_eq!(messages.last().unwrap()["content"], streamed);
}

#[tokio::test]
async fn a_stop_before_the_first_round_ends_the_turn_without_generating() {
    let gateway = MockGateway::start(Script::PlainProse, true, true);
    let recorder = Recorder::default();
    let cancel = CancelFlag::default();
    cancel.cancel();

    let outcome = tool_loop::run(&recorder, &gateway.conn(), prompt(), &json!({}), "turn-6", "", &cancel)
        .await
        .expect("a pre-set stop ends immediately");

    assert!(outcome.cancelled);
    assert_eq!(outcome.reply, "");
    assert_eq!(gateway.generations.load(Ordering::SeqCst), 0);
    assert!(recorder.deltas().is_empty());
}

#[tokio::test]
async fn a_gateway_without_tools_stays_a_plain_chat() {
    // No tool definitions available, so the loop must degrade to plain chat
    // and — critically — must not read a tool call out of ordinary prose.
    let gateway = MockGateway::start(Script::PlainProse, true, false);
    let recorder = Recorder::default();

    let outcome = tool_loop::run(&recorder, &gateway.conn(), prompt(), &json!({}), "turn-4", "", &CancelFlag::default())
        .await
        .expect("plain chat completes");

    assert_eq!(outcome.reply, "Braces { like this } are not tool calls.");
    assert!(recorder.tools().is_empty());
    assert_eq!(recorder.deltas(), outcome.reply);
    assert!(recorder.notices().contains("tools unavailable"));
    // Two messages: the prompt and the reply. Nothing invented.
    assert_eq!(outcome.exchange.len(), 2);
    assert_eq!(outcome.exchange[0].role, "user");
    assert_eq!(outcome.exchange[1].role, "assistant");
    assert_eq!(gateway.generations.load(Ordering::SeqCst), 1);

    // A plain chat still has to reach the journal. Before the prompt was part
    // of the exchange, `/etern/turns` rejected every one of these turns
    // because a record without a question is not a conversation.
    let record = recordable_turn("turn-4", &outcome.exchange).expect("plain chat is journalable");
    assert_eq!(record["turn_id"], "turn-4");
    let messages = record["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        "Braces { like this } are not tool calls."
    );
}
