//! EternAI shared harness core.
//!
//! Single implementation of the canonical conversation store, turn-boundary
//! token trimming, and Journal rendering. Consumed by the desktop gateway
//! (Rust) and the Android app (via UniFFI) so the sync protocol cannot
//! silently diverge between the two.

pub mod budget;
pub mod domain;
pub mod error;
pub mod journal;
pub mod store;
pub mod sync;
pub mod text_tools;
pub mod tier;
pub mod tools;

pub use budget::{
    collapse_system_prefix, trim_to_budget, CharDivFour, TokenCounter, TrimPlan, TrimPolicy,
};
pub use domain::{EngineKind, Message, Role, Turn, TurnId, TurnKind};
pub use error::{CoreError, CoreResult};
pub use journal::{
    day_bounds, day_of, DailyOutcome, ExtractiveSummarizer, Journal, Summarizer,
    DAY_BOUNDARY_HOUR,
};
pub use store::{DeviceRecord, JournalKind, SearchHit, Store};
pub use sync::{Outbox, PushAck, SyncBatch, SyncTurn, MAX_SYNC_BATCH};
pub use tier::{
    assemble, truncate_to_tokens, Assembly, DeviceProfile, TierBudget, TierTruncation,
};
pub use text_tools::{
    instructions as text_tool_instructions, parse as parse_text_tool_call, render_tool_calls,
    strip as strip_text_tool, tool_result_text, TextToolCall, ETOOL_MARKER,
};
pub use tools::{
    tool, desktop_tools, edge_tools, tools_for, validate, MemoryStore, PageFetchOutcome,
    PageFetcher, Sandbox, SandboxViolation, ToolError, ToolErrorKind, ToolResult, ToolRuntime,
    ToolSpec, UnavailableFetcher, UnavailableSearcher, WebSearchOutcome, WebSearcher, WebResult,
    MEMORY_CAP_TOKENS,
};
