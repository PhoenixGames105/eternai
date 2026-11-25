//! Tool calling for a model whose chat template cannot render schemas.
//!
//! llama.cpp's template API (`llama_chat_apply_template`, and the HTTP server's
//! `tools` parameter when the template has no tool grammar) cannot carry JSON
//! schemas, so the schemas are written into the system prompt as text and the
//! model is asked to answer a tool request with a single JSON object on its own
//! line. That keeps one tool contract across both models — same names, same
//! arguments, same results — and confines the deviation to how the request is
//! *encoded*, which is the part a template limitation actually affects.
//!
//! The Android app carries its own Kotlin implementation of this protocol
//! (`apps/android/.../tools/TextToolProtocol.kt`) because the edge runtime
//! cannot call into this crate mid-generation. The two are kept in lockstep by
//! the shared fixtures in [`tests`]: same instructions, same marker, same
//! tolerance for a model that does not quite obey.

use serde_json::Value;

/// The marker the model is told to use; also what [`parse`] looks for.
pub const ETOOL_MARKER: &str = "ETOOL";
/// Internal alias so the encodings below read naturally.
const MARKER: &str = ETOOL_MARKER;

/// A tool request recovered from assistant text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextToolCall {
    pub id: String,
    pub name: String,
    /// Serialised JSON object, kept as a string so it is replayed byte-for-byte.
    pub arguments: String,
}

/// Prompt text describing `definitions` (an OpenAI `tools` array).
///
/// Deliberately terse: PLAN §6 counts the schema block as a real token cost on
/// an 8k context, so only name, one-line description and argument names are
/// included.
pub fn instructions(definitions: &Value) -> String {
    let list = match definitions.as_array() {
        Some(list) => list,
        None => return String::new(),
    };
    if list.is_empty() {
        return String::new();
    }
    let mut sb = String::from("You can call tools. Available tools:\n");
    for def in list {
        let Some(func) = def.get("function") else { continue };
        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        sb.push_str("- ");
        sb.push_str(name);
        if let Some(desc) = func.get("description").and_then(Value::as_str) {
            if !desc.is_empty() {
                sb.push_str(": ");
                sb.push_str(desc);
            }
        }
        if let Some(props) = func
            .get("parameters")
            .and_then(|p| p.get("properties"))
            .and_then(Value::as_object)
        {
            if !props.is_empty() {
                sb.push_str(" args{");
                for (i, key) in props.keys().enumerate() {
                    if i > 0 {
                        sb.push(',');
                    }
                    sb.push_str(key);
                }
                sb.push('}');
            }
        }
        sb.push('\n');
    }
    sb.push_str(
        "To call a tool, reply with exactly one line and nothing else:\n\
         ETOOL {\"tool\":\"<name>\",\"arguments\":{...}}\n\
         Wait for the result before continuing. Only call a tool if you need it.\n",
    );
    sb
}

/// Extracts a tool request from `text`, or `None` when the model just answered.
///
/// Tolerates a missing marker, a code fence, and prose around the object,
/// because small models rarely obey formatting exactly. The object after the
/// marker is tried first, then the first brace-balanced object in the text, so
/// prose containing an unrelated `{` does not hide a real request.
///
/// Arguments that are not a JSON object degrade to `{}` rather than to "no
/// call": the tool itself will report the missing field, which is more useful
/// to the model than silence.
pub fn parse(text: &str) -> Option<TextToolCall> {
    let mut starts: Vec<usize> = Vec::with_capacity(2);
    if let Some(pos) = text.find(MARKER) {
        starts.push(pos);
    }
    if let Some(pos) = text.find('{') {
        starts.push(pos);
    }
    for start in starts {
        let Some(from) = text[start..].find('{') else {
            continue;
        };
        let begin = start + from;
        let Some(json) = extract_object(text, begin) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let name = ["tool", "name"]
            .iter()
            .filter_map(|key| value.get(*key).and_then(Value::as_str))
            .find(|s| !s.is_empty());
        let Some(name) = name else { continue };
        let arguments = match value.get("arguments") {
            Some(obj) if obj.is_object() => obj.to_string(),
            _ => "{}".to_string(),
        };
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("text_0")
            .to_string();
        return Some(TextToolCall {
            id,
            name: name.to_string(),
            arguments,
        });
    }
    None
}

/// The answer with the tool request removed, so the user sees prose rather than
/// a protocol line.
pub fn strip(text: &str) -> String {
    let Some(pos) = text.find(MARKER) else {
        return text.trim().to_string();
    };
    let head = &text[..pos];
    let rest = &text[pos + MARKER.len()..];
    let tail = match rest.find('{').and_then(|i| object_end(rest, i)) {
        Some(end) => &rest[end..],
        None => rest,
    };
    format!("{head}{tail}").trim().to_string()
}

/// Returns the byte offset just past the JSON object that starts at `start`.
fn object_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        if c == b'\\' && in_string {
            escaped = true;
        } else if c == b'"' {
            in_string = !in_string;
        } else if in_string {
            // Braces inside a string literal are data, not structure.
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            match depth.checked_sub(1) {
                Some(0) => return Some(i + 1),
                Some(next) => depth = next,
                None => return None,
            }
        }
    }
    None
}

/// Rewrites an assistant message so its recorded `tool_calls` appear as
/// protocol lines, which is how the model originally asked for them.
///
/// `tool_calls_json` is the OpenAI array stored on the message; unparsable
/// input leaves the text untouched rather than inventing a request.
pub fn render_tool_calls(text: &str, tool_calls_json: &str) -> String {
    let Ok(Value::Array(calls)) = serde_json::from_str(tool_calls_json) else {
        return text.to_string();
    };
    let mut sb = text.to_string();
    for call in calls {
        let Some(func) = call.get("function") else {
            continue;
        };
        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = match func.get("arguments") {
            Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                Ok(obj) if obj.is_object() => obj.to_string(),
                _ => "{}".to_string(),
            },
            Some(obj) if obj.is_object() => obj.to_string(),
            _ => "{}".to_string(),
        };
        if !sb.is_empty() {
            sb.push('\n');
        }
        sb.push_str(MARKER);
        sb.push_str(" {\"tool\":");
        sb.push_str(&serde_json::Value::String(name.to_string()).to_string());
        sb.push_str(",\"arguments\":");
        sb.push_str(&arguments);
        sb.push('}');
    }
    sb
}

/// The text a tool result becomes when the template has no `tool` role: most
/// chat templates only handle system/user/assistant, and a result the template
/// refuses to render is a result the model never sees.
pub fn tool_result_text(name: &str, content: &str) -> String {
    format!("[tool {name} result] {content}")
}

/// Extracts the first balanced `{...}` object starting at `start`.
fn extract_object(text: &str, start: usize) -> Option<&str> {
    object_end(text, start).map(|end| &text[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instructions_list_names_and_argument_keys() {
        let defs = json!([
            {"type":"function","function":{
                "name":"read_file",
                "description":"Read a text file",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}
        ]);
        let text = instructions(&defs);
        assert!(text.contains("read_file: Read a text file"), "{text}");
        assert!(text.contains("args{path}"), "{text}");
        assert!(text.contains("ETOOL"), "{text}");
    }

    #[test]
    fn instructions_are_empty_without_definitions() {
        assert_eq!(instructions(&json!([])), "");
        assert_eq!(instructions(&json!({})), "");
    }

    #[test]
    fn a_marked_tool_line_parses_out_of_surrounding_prose() {
        let reply =
            "Let me check.\nETOOL {\"tool\":\"remember\",\"arguments\":{\"fact\":\"milk\"}}\ndone";
        let call = parse(reply).expect("a call");
        assert_eq!(call.name, "remember");
        assert_eq!(call.id, "text_0");
        assert_eq!(call.arguments, r#"{"fact":"milk"}"#);
    }

    #[test]
    fn a_bare_object_without_the_marker_still_parses() {
        let call = parse(r#"{"name":"read_file","arguments":{"path":"a.txt"}}"#).expect("a call");
        assert_eq!(call.name, "read_file");
    }

    #[test]
    fn prose_is_not_mistaken_for_a_tool_request() {
        assert_eq!(parse("The capital of France is Paris."), None);
        assert_eq!(parse(r#"{"unrelated": true}"#), None);
        assert_eq!(parse(r#"{"tool":"","arguments":{}}"#), None);
    }

    #[test]
    fn an_earlier_unrelated_brace_does_not_hide_the_request() {
        let reply = r#"The file {notes} is small. ETOOL {"tool":"read_file","arguments":{"path":"a"}}"#;
        let call = parse(reply).expect("the marker's object wins");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, r#"{"path":"a"}"#);
    }

    #[test]
    fn braces_inside_argument_strings_do_not_end_the_object_early() {
        let call = parse(
            r#"ETOOL {"tool":"write_file","arguments":{"path":"a","body":"{not json}"}}"#,
        )
        .expect("a call");
        assert_eq!(call.name, "write_file");
        assert!(call.arguments.contains("{not json}"), "{}", call.arguments);
    }

    #[test]
    fn an_unclosed_object_is_not_a_call() {
        assert_eq!(parse(r#"ETOOL {"tool":"read_file","arguments":"#), None);
    }

    #[test]
    fn non_object_arguments_degrade_to_empty() {
        let call = parse(r#"ETOOL {"tool":"list_dir","arguments":"oops"}"#).expect("a call");
        assert_eq!(call.arguments, "{}");
    }

    #[test]
    fn strip_removes_the_protocol_line_and_keeps_the_prose() {
        let text = "Reading it now.\nETOOL {\"tool\":\"read_file\",\"arguments\":{\"path\":\"a\"}}";
        assert_eq!(strip(text), "Reading it now.");
    }

    #[test]
    fn strip_without_the_marker_only_trims() {
        assert_eq!(strip("  just an answer \n"), "just an answer");
    }

    #[test]
    fn render_turns_recorded_calls_back_into_protocol_lines() {
        let calls = json!([
            {"id":"c0","type":"function","function":{"name":"remember","arguments":"{\"fact\":\"x\"}"}}
        ]);
        let text = render_tool_calls("Noted.", &calls.to_string());
        assert_eq!(text, "Noted.\nETOOL {\"tool\":\"remember\",\"arguments\":{\"fact\":\"x\"}}");
    }

    #[test]
    fn render_leaves_text_alone_when_calls_are_unparsable() {
        assert_eq!(render_tool_calls("hi", "not json"), "hi");
    }

    #[test]
    fn strip_round_trips_what_render_produces() {
        let calls = json!([
            {"id":"c0","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}
        ]);
        let rendered = render_tool_calls("", &calls.to_string());
        assert_eq!(strip(&rendered), "");
        assert_eq!(parse(&rendered).expect("a call").name, "read_file");
    }

    #[test]
    fn tool_results_read_as_attributed_user_text() {
        assert_eq!(
            tool_result_text("read_file", "{\"content\":\"hi\"}"),
            "[tool read_file result] {\"content\":\"hi\"}"
        );
    }
}
