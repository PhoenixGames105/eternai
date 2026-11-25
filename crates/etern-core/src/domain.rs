use serde::{Deserialize, Serialize};

/// Turn identifier, minted by whichever device created the turn (UUIDv7).
///
/// Idempotency across sync replays keys off this value: the store has a unique
/// index on it, so re-pushing an outbox is a no-op rather than a duplicate.
pub type TurnId = uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" | "developer" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    /// Ordinary interactive exchange.
    Chat,
    /// Batch imported from an edge device's offline outbox.
    Sync,
    /// Rendered by the summariser, never replayed into a prompt verbatim.
    Journal,
    /// Harness-generated notice (engine swap, tool failure, ...).
    System,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::Chat => "chat",
            TurnKind::Sync => "sync",
            TurnKind::Journal => "journal",
            TurnKind::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "chat" => Some(TurnKind::Chat),
            "sync" => Some(TurnKind::Sync),
            "journal" => Some(TurnKind::Journal),
            "system" => Some(TurnKind::System),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Strata,
    Llama,
    /// On-device Qwen3.5-4B.
    Edge,
    None,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::Strata => "strata",
            EngineKind::Llama => "llama",
            EngineKind::Edge => "edge",
            EngineKind::None => "none",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "strata" => EngineKind::Strata,
            "llama" => EngineKind::Llama,
            "edge" => EngineKind::Edge,
            _ => EngineKind::None,
        }
    }
}

/// One OpenAI-shaped message.
///
/// `tool_calls` and `tool_call_id` are kept as raw JSON strings rather than
/// typed values: the harness must replay them byte-for-byte into the chat
/// template, and re-serialising through a typed model risks reordering keys in
/// a way that changes the token count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }

    /// True when this message issued tool calls that later messages answer.
    pub fn has_tool_calls(&self) -> bool {
        match &self.tool_calls {
            Some(s) => !s.trim().is_empty() && s.trim() != "[]",
            None => false,
        }
    }

    /// Tokens this message costs *before* template overhead. The chat template
    /// itself is measured separately by the caller, since it differs between
    /// the desktop and edge models.
    pub fn text(&self) -> &str {
        &self.content
    }
}

/// A logical exchange: everything produced in response to one user prompt,
/// including any number of assistant/tool round trips.
///
/// Turns are the unit the trimmer cuts on. Cutting inside a turn would orphan a
/// tool result from the assistant message that requested it, which the desktop
/// chat template rejects outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub ts: i64,
    pub kind: TurnKind,
    pub engine: EngineKind,
    pub model: String,
    pub device_id: String,
    pub seq: i64,
    pub messages: Vec<Message>,
}

impl Turn {
    pub fn new(kind: TurnKind, device_id: impl Into<String>, seq: i64, messages: Vec<Message>) -> Self {
        Self {
            id: TurnId::now_v7(),
            ts: chrono::Utc::now().timestamp_millis(),
            kind,
            engine: EngineKind::None,
            model: String::new(),
            device_id: device_id.into(),
            seq,
            messages,
        }
    }

    pub fn with_engine(mut self, engine: EngineKind, model: impl Into<String>) -> Self {
        self.engine = engine;
        self.model = model.into();
        self
    }

    pub fn with_ts(mut self, ts: i64) -> Self {
        self.ts = ts;
        self
    }

    pub fn user_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
    }

    /// Final assistant text, ignoring intermediate tool-calling steps.
    pub fn final_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
            .map(|m| m.content.as_str())
    }
}
