//! Tool runtime shared by both models (PLAN §6, §7).
//!
//! One implementation of the tool contract so the desktop gateway and the
//! Android app cannot silently diverge on what a tool call means. Three
//! concerns live here:
//!
//! * [`Sandbox`] — a path-allowlist jail for the file tools. It is the security
//!   boundary, so it is deliberately paranoid: absolute paths, drive letters,
//!   UNC shares and `..` are rejected by *shape* before anything touches the
//!   filesystem, and the resolved path is canonicalised (which collapses
//!   symlinks) and re-checked against the allowlist.
//! * [`validate`] — a small JSON-Schema validator. 4B tool-calling is flaky, so
//!   every call is checked against its schema before execution; a failure is
//!   reported as a *retryable* [`ToolError`] the harness feeds back to the model
//!   for one automatic retry (PLAN §6).
//! * [`MemoryStore`] — the append-only `MEMORY.md` behind `remember`, with the
//!   three guardrails from PLAN §12 baked in (append-only, stamped, capped).
//!
//! [`ToolRuntime`] ties the pieces together and dispatches a validated call.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::budget::{CharDivFour, TokenCounter};
use crate::error::{CoreError, CoreResult};
use crate::tier::DeviceProfile;

/// Tool names, so callers never hand-type a string that the registry does not
/// know about.
pub mod tool {
    pub const READ_FILE: &str = "read_file";
    pub const WRITE_FILE: &str = "write_file";
    pub const LIST_DIR: &str = "list_dir";
    pub const WEB_SEARCH: &str = "web_search";
    pub const REMEMBER: &str = "remember";
    pub const GREP: &str = "grep";
    pub const FETCH_PAGE: &str = "fetch_page";
}

/// How many times the harness may re-prompt the model after a *retryable*
/// tool-call failure. PLAN §6 fixes this at one.
pub const MAX_TOOL_RETRIES: u32 = 1;

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// A path-allowlist jail. Every file-tool path is resolved *relative to*
/// `root`, and the canonical result must fall under one of `allowed`.
///
/// Construction creates `root` and each allowed directory so that
/// canonicalisation (which requires the path to exist) works on a fresh setup.
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
    allowed: Vec<PathBuf>,
}

impl Sandbox {
    /// Builds a sandbox rooted at `root` exposing the given `allowed`
    /// sub-directories (relative to `root`). The special entry `"."` allows the
    /// whole root (the phone's app-scoped storage).
    pub fn new(root: impl AsRef<Path>, allowed: &[&str]) -> CoreResult<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|e| CoreError::io(root.display().to_string(), e))?;
        let root = std::fs::canonicalize(root)
            .map_err(|e| CoreError::io(root.display().to_string(), e))?;

        let mut dirs = Vec::with_capacity(allowed.len());
        for name in allowed {
            let dir = if *name == "." {
                root.clone()
            } else {
                let joined = root.join(name);
                std::fs::create_dir_all(&joined)
                    .map_err(|e| CoreError::io(joined.display().to_string(), e))?;
                std::fs::canonicalize(&joined)
                    .map_err(|e| CoreError::io(joined.display().to_string(), e))?
            };
            // An allowed dir must itself live under the root; refuse a config
            // that tries to whitelist the world.
            if !dir.starts_with(&root) {
                return Err(CoreError::Budget(format!(
                    "allowed directory {} escapes sandbox root {}",
                    dir.display(),
                    root.display()
                )));
            }
            dirs.push(dir);
        }
        if dirs.is_empty() {
            return Err(CoreError::Budget("sandbox has no allowed directories".into()));
        }
        Ok(Sandbox { root, allowed: dirs })
    }

    /// The desktop jail: `Journal/`, `memory/`, `inbox/` under the working dir.
    pub fn desktop(root: impl AsRef<Path>) -> CoreResult<Self> {
        Sandbox::new(root, &["Journal", "memory", "inbox"])
    }

    /// The phone jail: the app-scoped directory in its entirety. The root is
    /// already private to the app, so everything under it is fair game — but
    /// `..`/absolute escapes are still refused.
    pub fn edge(root: impl AsRef<Path>) -> CoreResult<Self> {
        Sandbox::new(root, &["."])
    }

    /// Canonical sandbox root (used by [`MemoryStore`] to locate `MEMORY.md`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a model-supplied path to a canonical absolute path, or a
    /// [`SandboxViolation`] describing why it was refused.
    ///
    /// The checks, in order:
    /// 1. Reject empty paths.
    /// 2. Reject by *shape*: absolute paths, drive/UNC prefixes, and any `..`
    ///    component. This stops escapes before the filesystem is consulted.
    /// 3. Join onto `root`, canonicalise the deepest existing ancestor (so a
    ///    not-yet-created file still resolves), then re-append the remainder.
    /// 4. Require the result to sit under an allowed directory. Canonicalising
    ///    collapses symlinks, so a link that points outside the jail is caught
    ///    here rather than followed.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, SandboxViolation> {
        let trimmed = requested.trim();
        if trimmed.is_empty() {
            return Err(SandboxViolation::Empty);
        }

        let candidate = Path::new(trimmed);

        // (2) Shape-based rejection. `components()` normalises away `.` and
        // separators, leaving only meaningful parts.
        for comp in candidate.components() {
            match comp {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(SandboxViolation::Absolute(trimmed.to_string()))
                }
                Component::ParentDir => return Err(SandboxViolation::Traversal),
                // `Component::CurDir` (`.`) is harmless; `Normal` is a name.
                _ => {}
            }
        }

        // (3) Join and canonicalise the existing prefix.
        let joined = self.root.join(candidate);
        let resolved = canonicalize_existing(&joined)?;

        // (4) Allowlist containment.
        if self.allowed.iter().any(|dir| resolved.starts_with(dir)) {
            Ok(resolved)
        } else {
            Err(SandboxViolation::OutsideSandbox)
        }
    }
}

/// Canonicalises the deepest existing ancestor of `path` and re-appends the
/// still-missing tail. This lets `write_file` target a file that does not exist
/// yet while still resolving any symlinks in the directories that *do* exist.
fn canonicalize_existing(path: &Path) -> Result<PathBuf, SandboxViolation> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::canonicalize(cursor) {
            Ok(mut canonical) => {
                for name in missing.iter().rev() {
                    canonical.push(name);
                }
                return Ok(canonical);
            }
            Err(_) => match (cursor.parent(), cursor.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    cursor = parent;
                }
                _ => return Err(SandboxViolation::Unresolvable),
            },
        }
    }
}

/// Why a path was refused. Serialized back to the model so it can correct
/// itself; every variant is a policy refusal, not a server fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxViolation {
    Empty,
    Traversal,
    Absolute(String),
    OutsideSandbox,
    Unresolvable,
}

impl SandboxViolation {
    pub fn message(&self) -> String {
        match self {
            SandboxViolation::Empty => "empty path".to_string(),
            SandboxViolation::Traversal => "'..' is not allowed; paths are relative to the sandbox root".to_string(),
            SandboxViolation::Absolute(p) => {
                format!("absolute paths, drive letters and UNC shares are not allowed (got {p:?})")
            }
            SandboxViolation::OutsideSandbox => {
                "path resolves outside the permitted directories".to_string()
            }
            SandboxViolation::Unresolvable => "path cannot be resolved".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool specs (the OpenAI-style function schemas)
// ---------------------------------------------------------------------------

/// One tool's declaration: name, human description, and a JSON-Schema object
/// describing its arguments.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// The full desktop tool set.
pub fn desktop_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: tool::READ_FILE,
            description: "Read a UTF-8 text file from a permitted directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the sandbox root, e.g. 'Journal/2026-01-15.txt'."}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: tool::WRITE_FILE,
            description: "Write (or append to) a UTF-8 text file in a permitted directory. Cannot modify MEMORY.md.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the sandbox root."},
                    "content": {"type": "string", "description": "Full text to write."},
                    "append": {"type": "boolean", "description": "Append instead of overwrite (default false)."}
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: tool::LIST_DIR,
            description: "List the entries of a permitted directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the sandbox root; empty lists the root."}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: tool::WEB_SEARCH,
            description: "Search the web. Returns an explicit no_network result when offline rather than a guess.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 10, "description": "Results to return (default 5)."}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: tool::REMEMBER,
            description: "Append a durable fact to MEMORY.md. Append-only; cannot delete or rewrite existing memory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact": {"type": "string", "description": "A single durable fact to remember."}
                },
                "required": ["fact"]
            }),
        },
        ToolSpec {
            name: tool::GREP,
            description: "Search file contents for a plain-text pattern inside the allowed directory. \
                          Returns matching lines with file path and line number. Use an empty path to \
                          search the whole allowed directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File or directory to search, relative to the allowed directory (empty = the whole directory)."},
                    "pattern": {"type": "string", "minLength": 1, "description": "Plain-text pattern to look for (not a regex)."},
                    "case_sensitive": {"type": "boolean", "description": "Match case exactly (default false)."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Stop after this many matches (default 50)."}
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: tool::FETCH_PAGE,
            description: "Fetch a web page and return its readable text content (HTML stripped). \
                          Use after web_search to read a result in full. http/https only.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "minLength": 8, "description": "Absolute http:// or https:// URL."},
                    "max_chars": {"type": "integer", "minimum": 200, "maximum": 20000, "description": "Truncate the extracted text to this many characters (default 8000)."}
                },
                "required": ["url"]
            }),
        },
    ]
}

/// The reduced edge tool set (PLAN §6): fewer schemas, better compliance, and
/// it saves the ~400 tokens of schema overhead. `web_search` is dropped because
/// the SearXNG instance lives on the desktop — if it were reachable the phone
/// would be using the high-power model anyway. `list_dir` is dropped as a
/// convenience the small model rarely needs.
pub fn edge_tools() -> Vec<ToolSpec> {
    desktop_tools()
        .into_iter()
        .filter(|s| matches!(s.name, tool::READ_FILE | tool::WRITE_FILE | tool::REMEMBER))
        .collect()
}

/// The tool set for a device profile.
pub fn tools_for(profile: DeviceProfile) -> Vec<ToolSpec> {
    match profile {
        DeviceProfile::Desktop => desktop_tools(),
        DeviceProfile::Edge => edge_tools(),
    }
}

/// Serialises specs into the OpenAI `tools` array shape.
fn serialize_specs(specs: &[ToolSpec]) -> Value {
    Value::Array(
        specs
            .iter()
            .map(|s| {
                json!({
                    "type": "function",
                    "function": {
                        "name": s.name,
                        "description": s.description,
                        "parameters": s.parameters
                    }
                })
            })
            .collect(),
    )
}

/// The OpenAI `tools` array a client for `profile` should advertise.
///
/// Exists so the gateway can hand a phone the *reduced* edge set: the phone
/// executes those tools itself, in its own sandbox, so it must never be told
/// about `web_search` or `list_dir`, which it cannot honour.
pub fn definitions_for(profile: DeviceProfile) -> Value {
    serialize_specs(&tools_for(profile))
}

// ---------------------------------------------------------------------------
// JSON-Schema-subset validation
// ---------------------------------------------------------------------------

/// Validates `value` against a JSON-Schema-subset `schema`, returning the first
/// violation as a human-readable string.
///
/// Supported keywords: `type` (object/array/string/integer/number/boolean/
/// null), `required`, `properties`, `items`, `enum`, `minLength`, `minimum`,
/// `maximum`. That is the whole vocabulary the tool schemas above use; keeping
/// the validator tiny means it is auditable and identical on both devices.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    if let Some(types) = schema.get("type") {
        let ok = match types {
            Value::Array(list) => list.iter().any(|t| type_matches(t, value)),
            single => type_matches(single, value),
        };
        if !ok {
            return Err(format!("expected type {types}, found {}", json_type(value)));
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|a| a == value) {
            return Err(format!("{value} is not one of {allowed:?}"));
        }
    }

    if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
        if let Some(s) = value.as_str() {
            if (s.chars().count() as u64) < min {
                return Err(format!("must be at least {min} characters"));
            }
        }
    }

    if let Some(max) = schema.get("maximum").and_then(Value::as_i64) {
        if let Some(n) = value.as_i64() {
            if n > max {
                return Err(format!("must be <= {max}"));
            }
        }
    }
    if let Some(min) = schema.get("minimum").and_then(Value::as_i64) {
        if let Some(n) = value.as_i64() {
            if n < min {
                return Err(format!("must be >= {min}"));
            }
        }
    }

    if let Some(obj) = value.as_object() {
        for req in schema.get("required").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]) {
            let key = req.as_str().unwrap_or_default();
            if !obj.contains_key(key) {
                return Err(format!("missing required property '{key}'"));
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (key, sub) in props {
                if let Some(child) = obj.get(key) {
                    validate(sub, child).map_err(|e| format!("property '{key}': {e}"))?;
                }
            }
        }
    }

    if let Some(items) = schema.get("items") {
        if let Some(arr) = value.as_array() {
            for (i, elem) in arr.iter().enumerate() {
                validate(items, elem).map_err(|e| format!("item {i}: {e}"))?;
            }
        }
    }

    Ok(())
}

fn type_matches(ty: &Value, value: &Value) -> bool {
    match ty.as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => true,
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// Errors and results
// ---------------------------------------------------------------------------

/// Why a tool call failed. `retryable` marks the failures the harness should
/// feed back to the model for one automatic retry (PLAN §6): a malformed call
/// or a wrong-but-fixable path is the model's mistake to correct, whereas an IO
/// fault or an offline search is not something re-prompting can fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Arguments failed schema validation (or were not a JSON object).
    Validation,
    /// The path was refused by the sandbox.
    Sandbox,
    /// The named tool does not exist in this profile's set.
    UnknownTool,
    /// The operation reached the filesystem/network and failed.
    Execution,
}

impl ToolError {
    fn validation(message: impl Into<String>) -> Self {
        ToolError { kind: ToolErrorKind::Validation, message: message.into(), retryable: true }
    }
    fn sandbox(v: SandboxViolation) -> Self {
        ToolError { kind: ToolErrorKind::Sandbox, message: v.message(), retryable: true }
    }
    fn unknown(name: &str) -> Self {
        ToolError {
            kind: ToolErrorKind::UnknownTool,
            message: format!("unknown tool '{name}'"),
            retryable: true,
        }
    }
    fn execution(message: impl Into<String>) -> Self {
        ToolError { kind: ToolErrorKind::Execution, message: message.into(), retryable: false }
    }

    /// The JSON body handed back to the model as the tool result.
    pub fn to_result_value(&self) -> Value {
        json!({
            "error": {
                "kind": format!("{:?}", self.kind).to_lowercase(),
                "message": self.message,
                "retryable": self.retryable
            }
        })
    }
}

/// A successful tool result. `content` is structured; the harness serialises it
/// into the tool message fed back to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: Value,
}

impl ToolResult {
    fn new(content: Value) -> Self {
        ToolResult { content }
    }
}

// ---------------------------------------------------------------------------
// Web search
// ---------------------------------------------------------------------------

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The outcome of a web search. `NoNetwork` is a *successful* tool call with an
/// explicit offline status — never an error — so the model reports "offline"
/// instead of improvising an answer (PLAN §6).
#[derive(Debug, Clone, PartialEq)]
pub enum WebSearchOutcome {
    Results(Vec<WebResult>),
    NoNetwork,
    Error(String),
}

/// Web-search backend. Implemented in the gateway against SearXNG; the core
/// ships [`UnavailableSearcher`] for tests and for a phone with no backend.
pub trait WebSearcher: Send + Sync {
    fn search(&self, query: &str, max_results: usize) -> WebSearchOutcome;
}

/// A searcher that always reports offline. Used when no SearXNG instance is
/// configured, so `web_search` degrades to an explicit `no_network` result.
pub struct UnavailableSearcher;

impl WebSearcher for UnavailableSearcher {
    fn search(&self, _query: &str, _max_results: usize) -> WebSearchOutcome {
        WebSearchOutcome::NoNetwork
    }
}

// ---------------------------------------------------------------------------
// Page fetch
// ---------------------------------------------------------------------------

/// The outcome of a page fetch. `NoNetwork` is a *successful* tool call with an
/// explicit offline status — never an error — mirroring `web_search` (PLAN §6).
#[derive(Debug, Clone, PartialEq)]
pub enum PageFetchOutcome {
    /// Readable text extracted from the page (HTML stripped).
    Text(String),
    /// The network is unreachable; the model should say so, not guess.
    NoNetwork,
    /// The page was reachable but could not be fetched/read.
    Error(String),
}

/// Page-fetch backend. Implemented in the gateway with a real HTTP client; the
/// core ships [`UnavailableFetcher`] for tests and offline builds.
pub trait PageFetcher: Send + Sync {
    fn fetch(&self, url: &str) -> PageFetchOutcome;
}

/// A fetcher that always reports offline.
pub struct UnavailableFetcher;

impl PageFetcher for UnavailableFetcher {
    fn fetch(&self, _url: &str) -> PageFetchOutcome {
        PageFetchOutcome::NoNetwork
    }
}

// ---------------------------------------------------------------------------
// MEMORY.md (the `remember` target)
// ---------------------------------------------------------------------------

/// Default cap on `MEMORY.md`, in tokens (PLAN §12 guardrail 3).
pub const MEMORY_CAP_TOKENS: usize = 2_000;

/// Append-only durable memory. `remember` can add a stamped line but never
/// delete or rewrite; when the cap is reached it refuses and tells the model to
/// consolidate. The consolidation primitive ([`MemoryStore::consolidate`]) is
/// what rewrites, and it always keeps a dated `.bak` first.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
    path: PathBuf,
    cap_tokens: usize,
}

impl MemoryStore {
    /// Memory lives at `<root>/memory/MEMORY.md`.
    pub fn new(root: &Path) -> CoreResult<Self> {
        Self::with_cap(root, MEMORY_CAP_TOKENS)
    }

    pub fn with_cap(root: &Path, cap_tokens: usize) -> CoreResult<Self> {
        let dir = root.join("memory");
        std::fs::create_dir_all(&dir).map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        let path = dir.join("MEMORY.md");
        Ok(MemoryStore { dir, path, cap_tokens })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current memory text; empty string when nothing has been remembered.
    pub fn read(&self) -> CoreResult<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(CoreError::io(self.path.display().to_string(), e)),
        }
    }

    /// Merges newline-delimited facts from another device without replacing
    /// existing memory. Exact duplicate lines are ignored, and the same cap
    /// used by `remember` applies to the merged result.
    pub fn merge(&self, incoming: &str) -> CoreResult<String> {
        let current = self.read()?;
        let mut seen: HashSet<String> = current
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let additions: Vec<&str> = incoming
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && seen.insert(line.to_string()))
            .collect();

        if additions.is_empty() {
            return Ok(current);
        }

        let mut merged = current.clone();
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        for line in &additions {
            merged.push_str(line);
            merged.push('\n');
        }
        let tokens = CharDivFour.count(&merged);
        if tokens > self.cap_tokens {
            return Err(CoreError::Budget(format!(
                "merged memory would use {tokens} tokens, exceeding the {} token cap",
                self.cap_tokens
            )));
        }

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        if !current.is_empty() && !current.ends_with('\n') {
            file.write_all(b"\n")
                .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        }
        for line in additions {
            writeln!(file, "{line}").map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        }
        Ok(merged)
    }

    pub fn token_count(&self) -> CoreResult<usize> {
        Ok(CharDivFour.count(&self.read()?))
    }

    /// Appends one stamped fact. Returns [`AppendOutcome::AtCap`] (writing
    /// nothing) if the append would exceed the cap — the model must consolidate
    /// rather than have its memory silently truncated.
    pub fn append(
        &self,
        fact: &str,
        model: &str,
        turn_id: &str,
        now_ms: i64,
    ) -> CoreResult<AppendOutcome> {
        let fact = fact.trim();
        if fact.is_empty() {
            return Ok(AppendOutcome::Empty);
        }
        let line = format!("- [{}] ({model} turn={turn_id}) {fact}\n", format_stamp(now_ms));
        let current = self.read()?;
        let prospective = CharDivFour.count(&(current.clone() + &line));
        if prospective > self.cap_tokens && !current.is_empty() {
            return Ok(AppendOutcome::AtCap { tokens: CharDivFour.count(&current), cap: self.cap_tokens });
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        file.write_all(line.as_bytes())
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        Ok(AppendOutcome::Appended { line, tokens: prospective })
    }

    /// Copies the current `MEMORY.md` to a dated `.bak` in the memory dir.
    /// Returns `None` when there is nothing to back up. Callers rewriting memory
    /// must call this first (PLAN §12 guardrail 3).
    pub fn backup_dated(&self, now_ms: i64) -> CoreResult<Option<PathBuf>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let stamp = format_stamp(now_ms).replace([' ', ':'], "-");
        let bak = self.dir.join(format!("MEMORY-{stamp}.md.bak"));
        std::fs::copy(&self.path, &bak)
            .map_err(|e| CoreError::io(bak.display().to_string(), e))?;
        Ok(Some(bak))
    }

    /// Replaces memory wholesale with a consolidated text — the *only* path that
    /// rewrites `MEMORY.md`. Always backs up first. Not exposed to the model via
    /// `remember`; the desktop app drives it (M5).
    pub fn consolidate(&self, new_text: &str, now_ms: i64) -> CoreResult<PathBuf> {
        let bak = self.backup_dated(now_ms)?;
        std::fs::write(&self.path, new_text)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        Ok(bak.unwrap_or_else(|| self.path.clone()))
    }
}

/// Result of a `remember` append.
#[derive(Debug, Clone, PartialEq)]
pub enum AppendOutcome {
    Appended { line: String, tokens: usize },
    AtCap { tokens: usize, cap: usize },
    Empty,
}

/// Formats `{date} {time}` from epoch milliseconds, in local time, matching the
/// journal's timeline convention.
fn format_stamp(now_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(now_ms).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("epoch:{now_ms}"),
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Dispatches validated tool calls against a device's sandbox, memory and
/// search backend.
pub struct ToolRuntime {
    specs: Vec<ToolSpec>,
    sandbox: Sandbox,
    memory: MemoryStore,
    searcher: Arc<dyn WebSearcher>,
    fetcher: Arc<dyn PageFetcher>,
}

impl ToolRuntime {
    pub fn new(
        profile: DeviceProfile,
        sandbox: Sandbox,
        searcher: Arc<dyn WebSearcher>,
    ) -> CoreResult<Self> {
        let memory = MemoryStore::new(sandbox.root())?;
        Ok(ToolRuntime {
            specs: tools_for(profile),
            sandbox,
            memory,
            searcher,
            fetcher: Arc::new(UnavailableFetcher),
        })
    }

    /// Attach a real page-fetch backend (the gateway does this at startup).
    pub fn with_fetcher(mut self, fetcher: Arc<dyn PageFetcher>) -> Self {
        self.fetcher = fetcher;
        self
    }

    /// The tool declarations to advertise to the model for this profile.
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// Serialises the specs into the OpenAI `tools` array shape.
    pub fn tool_definitions(&self) -> Value {
        serialize_specs(&self.specs)
    }

    /// Validates `args` against the named tool's schema without executing it.
    /// A failure is retryable so the harness can feed it back for one retry.
    pub fn validate_call(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        let spec = self.specs.iter().find(|s| s.name == name).ok_or_else(|| ToolError::unknown(name))?;
        if !args.is_object() {
            return Err(ToolError::validation("arguments must be a JSON object"));
        }
        validate(&spec.parameters, args).map_err(ToolError::validation)
    }

    /// Validates then executes a call. On validation failure nothing runs and a
    /// retryable error is returned; the harness decides whether to retry.
    pub fn execute(
        &self,
        name: &str,
        args: &Value,
        model: &str,
        turn_id: &str,
        now_ms: i64,
    ) -> Result<ToolResult, ToolError> {
        self.validate_call(name, args)?;
        match name {
            tool::READ_FILE => self.read_file(args),
            tool::WRITE_FILE => self.write_file(args),
            tool::LIST_DIR => self.list_dir(args),
            tool::WEB_SEARCH => self.web_search(args),
            tool::REMEMBER => self.remember(args, model, turn_id, now_ms),
            tool::GREP => self.grep(args),
            tool::FETCH_PAGE => self.fetch_page(args),
            other => Err(ToolError::unknown(other)),
        }
    }

    fn read_file(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let path = self.sandbox.resolve(arg_str(args, "path"))
            .map_err(ToolError::sandbox)?;
        let bytes = std::fs::read(&path).map_err(|e| ToolError::execution(format!("{}: {e}", path.display())))?;
        const MAX_BYTES: u64 = 256 * 1024;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(ToolError::execution(format!(
                "file is {} bytes, over the {} byte read limit",
                bytes.len(),
                MAX_BYTES
            )));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| ToolError::execution("file is not valid UTF-8 text"))?;
        Ok(ToolResult::new(json!({ "path": display_rel(&self.sandbox, &path), "content": content })))
    }

    fn write_file(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let path = self.sandbox.resolve(arg_str(args, "path"))
            .map_err(ToolError::sandbox)?;
        // Guardrail: `write_file` must not rewrite MEMORY.md — only `remember`
        // (append) and the consolidation primitive may touch it.
        if path.file_name().map(|n| n == "MEMORY.md").unwrap_or(false) {
            return Err(ToolError::sandbox(SandboxViolation::OutsideSandbox));
        }
        let content = arg_str(args, "content");
        let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::execution(format!("{}: {e}", parent.display())))?;
        }
        let result = if append {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| f.write_all(content.as_bytes()))
        } else {
            std::fs::write(&path, content)
        };
        result.map_err(|e| ToolError::execution(format!("{}: {e}", path.display())))?;
        Ok(ToolResult::new(json!({
            "path": display_rel(&self.sandbox, &path),
            "bytes_written": content.len(),
            "append": append
        })))
    }

    fn list_dir(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let requested = arg_str(args, "path").trim();
        if requested.is_empty() || requested == "." {
            let items = self.list_root_entries()?;
            return Ok(ToolResult::new(json!({ "path": ".", "entries": items })));
        }

        let path = self.sandbox.resolve(requested).map_err(ToolError::sandbox)?;
        let items = self.list_entries(&path)?;
        Ok(ToolResult::new(json!({ "path": display_rel(&self.sandbox, &path), "entries": items })))
    }

    fn list_root_entries(&self) -> Result<Vec<Value>, ToolError> {
        if self.sandbox.allowed.iter().any(|dir| dir == &self.sandbox.root) {
            return self.list_entries(&self.sandbox.root);
        }

        // The desktop root itself is not readable: expose only the first path
        // component of each allowlisted directory, never unrelated root files.
        let mut entries = BTreeMap::new();
        for allowed in &self.sandbox.allowed {
            let relative = allowed
                .strip_prefix(&self.sandbox.root)
                .map_err(|e| ToolError::execution(e.to_string()))?;
            if let Some(component) = relative.components().next() {
                entries.insert(component.as_os_str().to_string_lossy().into_owned(), true);
            }
        }
        Ok(entries
            .into_iter()
            .map(|(name, is_dir)| json!({ "name": name, "is_dir": is_dir }))
            .collect())
    }

    fn list_entries(&self, path: &Path) -> Result<Vec<Value>, ToolError> {
        let entries = std::fs::read_dir(path)
            .map_err(|e| ToolError::execution(format!("{}: {e}", path.display())))?;
        let mut items = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| ToolError::execution(e.to_string()))?;
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            items.push(json!({ "name": entry.file_name().to_string_lossy(), "is_dir": is_dir }));
        }
        items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(items)
    }

    fn web_search(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let query = arg_str(args, "query");
        let max = args.get("max_results").and_then(Value::as_u64).unwrap_or(5) as usize;
        match self.searcher.search(query, max) {
            WebSearchOutcome::Results(results) => {
                let list: Vec<_> = results
                    .into_iter()
                    .map(|r| json!({ "title": r.title, "url": r.url, "snippet": r.snippet }))
                    .collect();
                Ok(ToolResult::new(json!({ "status": "ok", "query": query, "results": list })))
            }
            WebSearchOutcome::NoNetwork => Ok(ToolResult::new(json!({
                "status": "no_network",
                "query": query,
                "note": "No internet connection. Do not answer from memory as if you searched; tell the user you are offline."
            }))),
            WebSearchOutcome::Error(e) => Err(ToolError::execution(format!("search failed: {e}"))),
        }
    }

    fn grep(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let pattern = arg_str(args, "pattern");
        let case_sensitive = args.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(50) as usize;
        let requested = arg_str(args, "path").trim();

        // Collect the search roots: the requested file/dir, or every allowed
        // directory when the path is empty.
        let mut roots: Vec<PathBuf> = Vec::new();
        if requested.is_empty() || requested == "." {
            roots = self.sandbox.allowed.clone();
        } else {
            let path = self.sandbox.resolve(requested).map_err(ToolError::sandbox)?;
            roots.push(path);
        }

        let needle = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };

        let mut matches: Vec<Value> = Vec::new();
        let mut files_scanned = 0usize;
        let mut truncated = false;
        'outer: for root in &roots {
            let walker = walk_files(root);
            for file in walker {
                if truncated {
                    break;
                }
                files_scanned += 1;
                let Ok(bytes) = std::fs::read(&file) else {
                    continue;
                };
                const MAX_FILE_BYTES: usize = 512 * 1024;
                if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
                    continue; // skip oversized and binary files
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                for (idx, line) in text.lines().enumerate() {
                    let lowered = if case_sensitive {
                        None
                    } else {
                        // Lowercase per line; cheap enough for text files.
                        Some(line.to_lowercase())
                    };
                    let haystack = lowered.as_deref().unwrap_or(line);
                    if haystack.contains(&needle) {
                        matches.push(json!({
                            "path": display_rel(&self.sandbox, &file),
                            "line": idx + 1,
                            "text": line.trim_end()
                        }));
                        if matches.len() >= max_results {
                            truncated = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        Ok(ToolResult::new(json!({
            "pattern": pattern,
            "matches": matches,
            "match_count": matches.len(),
            "files_scanned": files_scanned,
            "truncated": truncated
        })))
    }

    fn fetch_page(&self, args: &Value) -> Result<ToolResult, ToolError> {
        let url = arg_str(args, "url").trim();
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .unwrap_or(8000) as usize;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ToolError::validation(
                "url must be an absolute http:// or https:// URL",
            ));
        }
        match self.fetcher.fetch(url) {
            PageFetchOutcome::Text(text) => {
                let truncated = text.chars().count() > max_chars;
                let text: String = text.chars().take(max_chars).collect();
                Ok(ToolResult::new(json!({
                    "status": "ok",
                    "url": url,
                    "chars": text.chars().count(),
                    "truncated": truncated,
                    "text": text
                })))
            }
            PageFetchOutcome::NoNetwork => Ok(ToolResult::new(json!({
                "status": "no_network",
                "url": url,
                "note": "No internet connection. Do not answer as if you read the page; tell the user you are offline."
            }))),
            PageFetchOutcome::Error(e) => Err(ToolError::execution(format!("fetch failed: {e}"))),
        }
    }

    fn remember(
        &self,
        args: &Value,
        model: &str,
        turn_id: &str,
        now_ms: i64,
    ) -> Result<ToolResult, ToolError> {
        let fact = arg_str(args, "fact");
        match self.memory.append(fact, model, turn_id, now_ms) {
            Ok(AppendOutcome::Appended { line, tokens }) => Ok(ToolResult::new(json!({
                "status": "remembered",
                "line": line,
                "memory_tokens": tokens
            }))),
            Ok(AppendOutcome::AtCap { tokens, cap }) => Ok(ToolResult::new(json!({
                "status": "memory_at_cap",
                "memory_tokens": tokens,
                "cap": cap,
                "note": "MEMORY.md is full. Consolidate it (merge and drop stale facts) before adding more; do not expect this fact to be saved until you do."
            }))),
            Ok(AppendOutcome::Empty) => Err(ToolError::validation("fact is empty")),
            Err(e) => Err(ToolError::execution(e.to_string())),
        }
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Depth-first walk yielding regular files under `dir`. Skips hidden entries
/// so a grep of the whole sandbox stays fast.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// Renders a canonical path back relative to the sandbox root for the model, so
/// it never sees (or learns to target) absolute host paths.
fn display_rel(sandbox: &Sandbox, path: &Path) -> String {
    path.strip_prefix(sandbox.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Tests — the M4 "verify by" criterion is adversarial path tests, so the
// sandbox gets the heaviest coverage, plus schema validation, memory
// guardrails, and the reduced edge tool set.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn desktop_sandbox(dir: &Path) -> Sandbox {
        Sandbox::desktop(dir).expect("desktop sandbox")
    }

    fn runtime(dir: &Path, profile: DeviceProfile) -> ToolRuntime {
        let sandbox = match profile {
            DeviceProfile::Desktop => Sandbox::desktop(dir).unwrap(),
            DeviceProfile::Edge => Sandbox::edge(dir).unwrap(),
        };
        ToolRuntime::new(profile, sandbox, Arc::new(UnavailableSearcher)).unwrap()
    }

    // -- Sandbox: shape-based escapes --------------------------------------

    #[test]
    fn rejects_parent_traversal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        assert_eq!(sb.resolve("../secret.txt"), Err(SandboxViolation::Traversal));
        assert_eq!(sb.resolve("Journal/../../secret.txt"), Err(SandboxViolation::Traversal));
        assert_eq!(sb.resolve(".."), Err(SandboxViolation::Traversal));
    }

    #[test]
    fn rejects_absolute_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        // Feed the (guaranteed-absolute) root back in as a request.
        let abs = tmp.path().canonicalize().unwrap().display().to_string();
        let err = sb.resolve(&abs).unwrap_err();
        assert!(matches!(err, SandboxViolation::Absolute(_)), "got {err:?}");
    }

    #[cfg(windows)]
    #[test]
    fn rejects_drive_letter_and_unc() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        assert!(matches!(sb.resolve(r"C:\Windows\x.txt"), Err(SandboxViolation::Absolute(_))));
        assert!(matches!(sb.resolve(r"\\server\share\x.txt"), Err(SandboxViolation::Absolute(_))));
        assert!(matches!(sb.resolve("C:/foo"), Err(SandboxViolation::Absolute(_))));
    }

    #[test]
    fn rejects_empty_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        assert_eq!(sb.resolve("   "), Err(SandboxViolation::Empty));
    }

    #[test]
    fn rejects_path_outside_allowlist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        // `notes/` is under the root but not one of Journal/memory/inbox.
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes/x.txt"), b"nope").unwrap();
        assert_eq!(sb.resolve("notes/x.txt"), Err(SandboxViolation::OutsideSandbox));
    }

    #[test]
    fn allows_paths_inside_allowlist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = desktop_sandbox(tmp.path());
        assert!(sb.resolve("Journal/2026-01-15.txt").is_ok());
        assert!(sb.resolve("memory/notes.txt").is_ok());
        assert!(sb.resolve("inbox/a/b/c.txt").is_ok());
        // A `.` component is harmless.
        assert!(sb.resolve("Journal/./today.txt").is_ok());
    }

    #[test]
    fn edge_sandbox_allows_whole_root_but_not_escapes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sb = Sandbox::edge(tmp.path()).unwrap();
        assert!(sb.resolve("anything/here.txt").is_ok());
        assert_eq!(sb.resolve("../out.txt"), Err(SandboxViolation::Traversal));
    }

    #[test]
    fn symlink_escape_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"top secret").unwrap();

        let sb = desktop_sandbox(tmp.path());
        let link = sb.root().join("Journal").join("escape");
        if !try_symlink_dir(outside.path(), &link) {
            // Symlink creation needs privileges on Windows; skip rather than
            // produce a false failure on a machine without developer mode.
            eprintln!("skipping symlink test: could not create symlink");
            return;
        }
        // Following the link lands outside the jail, so it must be refused.
        let err = sb.resolve("Journal/escape/secret.txt").unwrap_err();
        assert!(
            matches!(err, SandboxViolation::OutsideSandbox),
            "symlink escape should be OutsideSandbox, got {err:?}"
        );
    }

    fn try_symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (target, link);
            false
        }
    }

    // -- JSON-Schema validation --------------------------------------------

    #[test]
    fn validate_missing_required() {
        let schema = desktop_tools()
            .into_iter()
            .find(|s| s.name == tool::READ_FILE)
            .unwrap()
            .parameters;
        let err = validate(&schema, &json!({})).unwrap_err();
        assert!(err.contains("path"), "err was {err}");
    }

    #[test]
    fn validate_wrong_type() {
        let schema = desktop_tools()
            .into_iter()
            .find(|s| s.name == tool::READ_FILE)
            .unwrap()
            .parameters;
        let err = validate(&schema, &json!({ "path": 42 })).unwrap_err();
        assert!(err.contains("string"), "err was {err}");
    }

    #[test]
    fn validate_numeric_bounds() {
        let schema = json!({"type": "integer", "minimum": 1, "maximum": 10});
        assert!(validate(&schema, &json!(0)).is_err());
        assert!(validate(&schema, &json!(11)).is_err());
        assert!(validate(&schema, &json!(5)).is_ok());
    }

    #[test]
    fn validate_enum_and_min_length() {
        let schema = json!({"type": "string", "enum": ["a", "b"], "minLength": 1});
        assert!(validate(&schema, &json!("c")).is_err());
        assert!(validate(&schema, &json!("a")).is_ok());
        let min = json!({"type": "string", "minLength": 3});
        assert!(validate(&min, &json!("ab")).is_err());
    }

    #[test]
    fn validate_nested_and_items() {
        let schema = json!({
            "type": "object",
            "properties": { "nums": {"type": "array", "items": {"type": "integer"}} },
            "required": ["nums"]
        });
        assert!(validate(&schema, &json!({"nums": [1, 2, 3]})).is_ok());
        assert!(validate(&schema, &json!({"nums": [1, "two"]})).is_err());
    }

    // -- Tool set selection -------------------------------------------------

    #[test]
    fn desktop_has_all_tools_edge_is_reduced() {
        let desktop: Vec<_> = desktop_tools().into_iter().map(|s| s.name).collect();
        assert_eq!(desktop.len(), 7);
        assert!(desktop.contains(&tool::WEB_SEARCH));
        assert!(desktop.contains(&tool::LIST_DIR));
        assert!(desktop.contains(&tool::GREP));
        assert!(desktop.contains(&tool::FETCH_PAGE));

        let edge: Vec<_> = edge_tools().into_iter().map(|s| s.name).collect();
        assert_eq!(edge, vec![tool::READ_FILE, tool::WRITE_FILE, tool::REMEMBER]);
        assert!(!edge.contains(&tool::WEB_SEARCH));
        assert!(!edge.contains(&tool::LIST_DIR));
        assert!(!edge.contains(&tool::GREP));
        assert!(!edge.contains(&tool::FETCH_PAGE));
    }

    #[test]
    fn tool_definitions_have_openai_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let defs = rt.tool_definitions();
        let arr = defs.as_array().unwrap();
        assert_eq!(arr.len(), 7);
        for entry in arr {
            assert_eq!(entry["type"], "function");
            assert!(entry["function"]["name"].is_string());
            assert!(entry["function"]["parameters"].is_object());
        }
    }

    #[test]
    fn unknown_tool_is_retryable_validation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt.validate_call("no_such_tool", &json!({})).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::UnknownTool);
        assert!(err.retryable);
    }

    #[test]
    fn non_object_args_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt.validate_call(tool::READ_FILE, &json!("Journal/x")).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Validation);
    }

    // -- File tools round trip ---------------------------------------------

    #[test]
    fn write_then_read_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let now = 1_700_000_000_000i64;
        rt.execute(
            tool::WRITE_FILE,
            &json!({"path": "Journal/a.txt", "content": "hello world"}),
            "test-model",
            "turn-1",
            now,
        )
        .unwrap();
        let out = rt.execute(tool::READ_FILE, &json!({"path": "Journal/a.txt"}), "m", "t", now).unwrap();
        assert_eq!(out.content["content"], "hello world");
        assert_eq!(out.content["path"], "Journal/a.txt");
    }

    #[test]
    fn write_file_refuses_memory_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt
            .execute(tool::WRITE_FILE, &json!({"path": "memory/MEMORY.md", "content": "x"}), "m", "t", 0)
            .unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Sandbox);
    }

    #[test]
    fn write_outside_allowlist_is_sandbox_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt
            .execute(tool::WRITE_FILE, &json!({"path": "../evil.txt", "content": "x"}), "m", "t", 0)
            .unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Sandbox);
        assert!(err.retryable);
    }

    #[test]
    fn list_dir_sorted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        std::fs::write(tmp.path().join("Journal/b.txt"), b"b").unwrap();
        std::fs::write(tmp.path().join("Journal/a.txt"), b"a").unwrap();
        let out = rt.execute(tool::LIST_DIR, &json!({"path": "Journal"}), "m", "t", 0).unwrap();
        let names: Vec<_> = out.content["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn list_dir_root_shows_only_permitted_desktop_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        std::fs::write(tmp.path().join("private.txt"), b"not exposed").unwrap();

        let out = rt.execute(tool::LIST_DIR, &json!({"path": ""}), "m", "t", 0).unwrap();
        let names: Vec<_> = out.content["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(out.content["path"], ".");
        assert_eq!(names, vec!["Journal", "inbox", "memory"]);
    }

    // -- web_search ---------------------------------------------------------

    #[test]
    fn web_search_reports_no_network_as_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let out = rt.execute(tool::WEB_SEARCH, &json!({"query": "rust"}), "m", "t", 0).unwrap();
        assert_eq!(out.content["status"], "no_network");
    }

    #[test]
    fn web_search_unavailable_on_edge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Edge);
        let err = rt.validate_call(tool::WEB_SEARCH, &json!({"query": "x"})).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::UnknownTool);
    }

    // -- grep ---------------------------------------------------------------

    #[test]
    fn grep_finds_matches_across_allowed_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        std::fs::write(tmp.path().join("Journal/2026-01-01.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(tmp.path().join("memory/MEMORY.md"), "ALPHA fact\n").unwrap();
        std::fs::write(tmp.path().join("inbox/notes.txt"), "nothing here\n").unwrap();

        let out = rt
            .execute(tool::GREP, &json!({"pattern": "alpha"}), "m", "t", 0)
            .unwrap();
        assert_eq!(out.content["match_count"], 2);
        let paths: Vec<&str> = out.content["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(paths.iter().any(|p| p.contains("Journal")));
        assert!(paths.iter().any(|p| p.contains("memory")));
        // Case-insensitive by default.
        assert!(out.content["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"].as_str() == Some("ALPHA fact")));
    }

    #[test]
    fn grep_case_sensitive_and_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        std::fs::write(tmp.path().join("Journal/a.txt"), "x\nx\nx\nx\nx\n").unwrap();

        let out = rt
            .execute(
                tool::GREP,
                &json!({"pattern": "x", "max_results": 2}),
                "m", "t", 0,
            )
            .unwrap();
        assert_eq!(out.content["match_count"], 2);
        assert_eq!(out.content["truncated"], true);

        let out = rt
            .execute(
                tool::GREP,
                &json!({"pattern": "X", "case_sensitive": true}),
                "m", "t", 0,
            )
            .unwrap();
        assert_eq!(out.content["match_count"], 0);
    }

    #[test]
    fn grep_requires_pattern_and_respects_sandbox() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt.validate_call(tool::GREP, &json!({})).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Validation);

        let err = rt
            .execute(tool::GREP, &json!({"pattern": "x", "path": "../outside"}), "m", "t", 0)
            .unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Sandbox);
    }

    #[test]
    fn grep_unavailable_on_edge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Edge);
        let err = rt.validate_call(tool::GREP, &json!({"pattern": "x"})).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::UnknownTool);
    }

    // -- fetch_page ---------------------------------------------------------

    #[test]
    fn fetch_page_reports_no_network_as_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let out = rt
            .execute(tool::FETCH_PAGE, &json!({"url": "https://example.com"}), "m", "t", 0)
            .unwrap();
        assert_eq!(out.content["status"], "no_network");
    }

    #[test]
    fn fetch_page_rejects_non_http_urls() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt
            .execute(tool::FETCH_PAGE, &json!({"url": "file:///etc/passwd"}), "m", "t", 0)
            .unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Validation);
    }

    #[test]
    fn fetch_page_truncates_to_max_chars() {
        struct FixedFetcher;
        impl PageFetcher for FixedFetcher {
            fn fetch(&self, _url: &str) -> PageFetchOutcome {
                PageFetchOutcome::Text("ab".repeat(5000))
            }
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop).with_fetcher(Arc::new(FixedFetcher));
        let out = rt
            .execute(
                tool::FETCH_PAGE,
                &json!({"url": "https://example.com", "max_chars": 1000}),
                "m", "t", 0,
            )
            .unwrap();
        assert_eq!(out.content["status"], "ok");
        assert_eq!(out.content["chars"], 1000);
        assert_eq!(out.content["truncated"], true);
    }

    #[test]
    fn fetch_page_unavailable_on_edge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Edge);
        let err = rt
            .validate_call(tool::FETCH_PAGE, &json!({"url": "https://example.com"}))
            .unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::UnknownTool);
    }

    // -- MEMORY.md guardrails ----------------------------------------------

    #[test]
    fn remember_appends_stamped_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let now = 1_700_000_000_000i64;
        let out = rt
            .execute(tool::REMEMBER, &json!({"fact": "user prefers metric units"}), "big-model", "turn-9", now)
            .unwrap();
        assert_eq!(out.content["status"], "remembered");
        let line = out.content["line"].as_str().unwrap();
        assert!(line.starts_with("- ["), "line: {line}");
        assert!(line.contains("big-model"));
        assert!(line.contains("turn=turn-9"));
        assert!(line.contains("user prefers metric units"));

        let text = std::fs::read_to_string(rt.memory.path()).unwrap();
        assert!(text.contains("user prefers metric units"));
    }

    #[test]
    fn remember_empty_fact_is_validation_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rt = runtime(tmp.path(), DeviceProfile::Desktop);
        let err = rt.execute(tool::REMEMBER, &json!({"fact": "   "}), "m", "t", 0).unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Validation);
    }

    #[test]
    fn memory_cap_refuses_further_appends() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::with_cap(tmp.path(), 20).unwrap();
        // First append always lands (memory was empty).
        let first = store.append("first fact here", "m", "t1", 0).unwrap();
        assert!(matches!(first, AppendOutcome::Appended { .. }));
        // A second append that would exceed the cap is refused, nothing written.
        let second = store.append("a much longer second fact that pushes past the tiny cap", "m", "t2", 0).unwrap();
        assert!(matches!(second, AppendOutcome::AtCap { .. }), "got {second:?}");
        // The refused fact is not in the file.
        assert!(!store.read().unwrap().contains("much longer"));
    }

    #[test]
    fn consolidate_backs_up_then_rewrites() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path()).unwrap();
        store.append("old fact", "m", "t1", 0).unwrap();
        let bak = store.consolidate("consolidated memory\n", 1_700_000_000_000).unwrap();
        assert!(bak.exists(), "backup should exist at {}", bak.display());
        assert_eq!(store.read().unwrap(), "consolidated memory\n");
    }

    #[test]
    fn read_memory_when_missing_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path()).unwrap();
        assert_eq!(store.read().unwrap(), "");
        assert_eq!(store.token_count().unwrap(), 0);
    }

    #[test]
    fn memory_merge_is_union_and_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::new(tmp.path()).unwrap();
        store.merge("- phone fact\n- shared fact\n").unwrap();

        let merged = store.merge("- shared fact\n- desktop fact\n").unwrap();
        assert_eq!(merged, "- phone fact\n- shared fact\n- desktop fact\n");
        assert_eq!(store.merge("- phone fact\n- desktop fact\n").unwrap(), merged);
    }

    #[test]
    fn memory_merge_refuses_to_exceed_the_cap_without_losing_existing_facts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MemoryStore::with_cap(tmp.path(), 8).unwrap();
        store.merge("- existing fact\n").unwrap();

        let error = store.merge(&format!("- {}\n", "x".repeat(40))).unwrap_err();
        assert!(error.to_string().contains("exceeding the 8 token cap"));
        assert_eq!(store.read().unwrap(), "- existing fact\n");
    }
}
