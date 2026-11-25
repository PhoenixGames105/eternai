//! Engine property discovery and caching.
//!
//! `ModelSwitcher.ps1` hot-swaps whichever GGUF is loaded on the same port, so
//! the gateway cannot assume the engine behind `:8080` is stable. Two things
//! break the moment it changes:
//!
//! * the prompt-cache prefix assumption — a different chat template invalidates
//!   every cached prefix, so the gateway must notice and say so;
//! * the edge window proposal — the phone trims against the *big* model's
//!   template, and a stale template hash means it trimmed against a model that
//!   no longer exists.
//!
//! So `/props` is cached with a TTL, fingerprinted, and the fingerprint is
//! published on `/health` and `/props` for clients to compare.
//!
//! ## What the engine serves
//!
//! The companion's engine is llama.cpp `llama-server` (0.7.0-dev on this host),
//! launched by `ModelSwitcher.ps1` on `:8080`. It serves the standard routes:
//!
//! * `GET /health` → `{"status":"ok",...}`
//! * `GET /props` → `default_generation_settings.n_ctx`, `total_slots`,
//!   `model_alias`, `chat_template`, `chat_template_caps`, `modalities`,
//!   `model_path`, `build_info`
//! * `GET /slots` → `[{"id":0,"n_ctx":...,"is_processing":false,
//!   "n_prompt_tokens":...}]`
//! * `POST /tokenize` → **200** with a `tokens` array. llama.cpp has a
//!   server-side tokenizer, so [`TokenSource::Engine`] is the normal path.
//!
//! `ModelSwitcher.ps1` can also run a *Strata* service on the same port, and
//! Strata 0.1.57 answers `POST /tokenize` with **404**. The gateway probes the
//! route on every discovery pass and selects the [`TokenSource`] accordingly, so
//! it survives either engine without reconfiguration.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How the gateway is able to count tokens, which decides how much margin the
/// edge-window proposal has to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// The engine answers `POST /tokenize`, so counts are exact.
    Engine,
    /// No server-side tokenizer: counts are estimates and the phone's own
    /// engine is the authority that must do the final trim.
    Estimated,
}

impl TokenSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Estimated => "estimated",
        }
    }

    /// Multiplier applied to any budget computed under this source. An estimate
    /// must leave headroom, because under-counting overflows an 8k window and
    /// the engine then truncates the *oldest* context silently.
    pub fn safety_margin(self) -> f64 {
        match self {
            Self::Engine => 1.0,
            Self::Estimated => 0.90,
        }
    }
}

/// Fingerprint of the engine's identity as far as the harness cares.
///
/// Deliberately excludes anything that flickers per-request (`is_processing`,
/// `is_sleeping`) — a spurious "changed" would invalidate prompt caches that
/// are still perfectly good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineFingerprint {
    /// `model_alias` when the engine sets one, else the GGUF filename stem.
    pub model: String,
    /// SHA-256 of the chat template exactly as the engine serves it.
    pub template_sha: String,
    /// Context size the engine will actually honour.
    pub n_ctx: Option<u64>,
    pub token_source: TokenSource,
    /// From `chat_template_caps.supports_tools`. Gates the agentic loop.
    pub supports_tools: bool,
    /// From `modalities.vision`. Gates image input.
    pub supports_vision: bool,
}

impl EngineFingerprint {
    /// Short but collision-safe fingerprint tag; 12 hex chars keeps payloads
    /// small and matches the length used when the two GGUF templates were first
    /// diffed (big `12827f24b742`, small `8452ca85cb1e`).
    pub fn template_sha(template: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(template.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(12);
        for byte in digest.iter().take(6) {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Builds a fingerprint from a `/props` body, the `/health` body (which on
    /// this build carries `max_context` that `/props` does not), and whether
    /// `/tokenize` answered.
    pub fn from_props(props: &Value, health: Option<&Value>, has_tokenize: bool) -> Self {
        let text = |keys: &[&str]| -> Option<String> {
            keys.iter()
                .find_map(|k| props.get(*k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        let model = text(&["model_alias", "model_path", "model", "base_model"])
            .or_else(|| health.and_then(|h| h.get("model")).and_then(Value::as_str).map(str::to_string))
            .map(|m| {
                // A path is not a name; keep the file stem so the value is
                // stable across a model directory move.
                if m.contains('/') || m.contains('\\') {
                    m.rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(m.as_str())
                        .trim_end_matches(".gguf")
                        .to_string()
                } else {
                    m
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let template = props.get("chat_template").and_then(Value::as_str).unwrap_or_default();

        // `n_ctx` lives in different places depending on build; `/health` is
        // checked last because it reports the *server* maximum rather than the
        // slot's allocation.
        let n_ctx = props
            .get("default_generation_settings")
            .and_then(|d| d.get("n_ctx"))
            .and_then(Value::as_u64)
            .or_else(|| props.get("n_ctx").and_then(Value::as_u64))
            .or_else(|| props.get("total_n_ctx").and_then(Value::as_u64))
            .or_else(|| health.and_then(|h| h.get("max_context")).and_then(Value::as_u64));

        let flag = |container: &str, key: &str| -> bool {
            props
                .get(container)
                .and_then(|c| c.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };

        Self {
            model,
            template_sha: Self::template_sha(template),
            n_ctx,
            token_source: if has_tokenize { TokenSource::Engine } else { TokenSource::Estimated },
            supports_tools: flag("chat_template_caps", "supports_tools"),
            supports_vision: flag("modalities", "vision"),
        }
    }

    /// True when the engine behind the port is no longer the one clients were
    /// told about.
    pub fn changed(&self, previous: Option<&EngineFingerprint>) -> bool {
        previous.is_some_and(|prev| prev != self)
    }
}

/// Volatile engine status read from `/health`, `/props` and `/slots`. Kept apart
/// from the fingerprint so a model going to sleep is not mistaken for a swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct EngineStatus {
    pub loaded: bool,
    pub sleeping: bool,
    pub max_context: Option<u64>,
    /// Slot count. This host reports `total_slots: 1`, which is why the health
    /// machine is built with a capacity of one.
    pub slots: Option<u64>,
    /// Prompt tokens of the request that last used the slot — the only
    /// server-side token figure this build leaks, and the anchor for
    /// calibrating the token estimate.
    pub last_prompt_tokens: Option<u64>,
}

impl EngineStatus {
    pub fn from_health(health: &Value, props: Option<&Value>, slots: Option<&Value>) -> Self {
        let slot_count = props
            .and_then(|p| p.get("total_slots"))
            .and_then(Value::as_u64)
            .or_else(|| slots.and_then(Value::as_array).map(|a| a.len() as u64));

        let last_prompt_tokens = slots
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .filter_map(|s| s.get("n_prompt_tokens").and_then(Value::as_u64))
                    .max()
            });

        Self {
            loaded: health.get("loaded").and_then(Value::as_bool).unwrap_or(false),
            sleeping: props
                .and_then(|p| p.get("is_sleeping"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            max_context: health.get("max_context").and_then(Value::as_u64),
            slots: slot_count,
            last_prompt_tokens,
        }
    }
}

/// Cached `/props` view with its TTL.
#[derive(Debug)]
pub struct PropsCache {
    ttl: Duration,
    inner: std::sync::RwLock<Option<Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    fetched_at: Instant,
    props: Value,
    fingerprint: EngineFingerprint,
}

impl PropsCache {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, inner: std::sync::RwLock::new(None) }
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        self.inner
            .read()
            .expect("props lock")
            .as_ref()
            .is_some_and(|e| now.duration_since(e.fetched_at) < self.ttl)
    }

    pub fn fingerprint(&self) -> Option<EngineFingerprint> {
        self.inner.read().expect("props lock").as_ref().map(|e| e.fingerprint.clone())
    }

    pub fn props(&self) -> Option<Value> {
        self.inner.read().expect("props lock").as_ref().map(|e| e.props.clone())
    }

    /// Stores a refresh and reports whether the engine identity changed.
    pub fn store(&self, props: Value, has_tokenize: bool) -> (EngineFingerprint, bool) {
        self.store_full(props, None, None, has_tokenize).into_pair()
    }

    /// Same as [`Self::store`] but folds in the `/health` and `/slots` bodies so
    /// `n_ctx` can be recovered from `max_context` on builds that need it.
    pub fn store_full(
        &self,
        props: Value,
        health: Option<&Value>,
        slots: Option<&Value>,
        has_tokenize: bool,
    ) -> StoreOutcome {
        let fingerprint = EngineFingerprint::from_props(&props, health, has_tokenize);
        let status = EngineStatus::from_health(health.unwrap_or(&Value::Null), Some(&props), slots);
        let mut guard = self.inner.write().expect("props lock");
        let changed =
            EngineFingerprint::changed(&fingerprint, guard.as_ref().map(|e| &e.fingerprint));
        *guard = Some(Entry { fetched_at: Instant::now(), props, fingerprint: fingerprint.clone() });
        StoreOutcome { fingerprint, status, changed }
    }

    /// Drops the cache. Called after a swap so the next client sees the new
    /// engine rather than a TTL-stale one.
    pub fn invalidate(&self) {
        *self.inner.write().expect("props lock") = None;
    }
}

/// What a props refresh learned.
#[derive(Debug, Clone)]
pub struct StoreOutcome {
    pub fingerprint: EngineFingerprint,
    pub status: EngineStatus,
    pub changed: bool,
}

impl StoreOutcome {
    fn into_pair(self) -> (EngineFingerprint, bool) {
        (self.fingerprint, self.changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Shaped from the live Strata server's `/props`, with the 9993-char
    /// template reduced to its distinguishing prefix.
    fn live_props(template: &str) -> Value {
        json!({
            "default_generation_settings": {
                "n_ctx": 262144,
                "params": { "temperature": 1.0, "top_p": 0.95, "top_k": 20, "n_predict": -1 }
            },
            "total_slots": 1,
            "model_alias": "qwen3.8-flash-next-unsloth-ud-q4_k_xl",
            "chat_template": template,
            "chat_template_caps": {
                "supports_tools": true,
                "supports_tool_calls": true,
                "supports_system_role": true,
                "supports_parallel_tool_calls": true,
                "supports_preserve_reasoning": true
            },
            "modalities": { "vision": true },
            "models_autoload": [],
            "is_sleeping": false,
            "model_path": "C:\\Users\\phoenix\\AppData\\Local\\Strata\\Strata-data\\models\\unsloth-UD-Q4_K_XL\\Qwen3.8-Flash-Next-UD-Q4_K_XL-00001-of-00004.gguf",
            "build_info": "Strata 0.1.57"
        })
    }

    fn live_health() -> Value {
        json!({
            "status": "ok",
            "max_context": 262144,
            "model": "qwen3.8-flash-next-unsloth-ud-q4_k_xl",
            "images": true,
            "api_key": false,
            "image_generation": false,
            "loaded": true,
            "service": "strata"
        })
    }

    fn live_slots() -> Value {
        json!([{ "id": 0, "n_ctx": 262144, "is_processing": false, "n_prompt_tokens": 70254 }])
    }

    const BIG_MARKERS: &str = "{%- if messages[0].role == 'system' %}";
    const SMALL_MARKERS: &str = "{%- if tools %}";

    #[test]
    fn reads_the_live_props_shape() {
        let fp = EngineFingerprint::from_props(&live_props(BIG_MARKERS), None, false);
        assert_eq!(fp.model, "qwen3.8-flash-next-unsloth-ud-q4_k_xl", "alias beats path");
        assert_eq!(fp.n_ctx, Some(262_144), "n_ctx hides under default_generation_settings");
        assert!(fp.supports_tools, "the agentic loop depends on this");
        assert!(fp.supports_vision);
        assert_eq!(
            fp.token_source,
            TokenSource::Estimated,
            "this build 404s on /tokenize; the plan must not pretend otherwise"
        );
    }

    #[test]
    fn an_engine_with_tokenize_is_reported_as_exact() {
        let fp = EngineFingerprint::from_props(&live_props(BIG_MARKERS), None, true);
        assert_eq!(fp.token_source, TokenSource::Engine);
        assert_eq!(fp.token_source.safety_margin(), 1.0);
    }

    #[test]
    fn estimates_carry_headroom_because_under_counting_is_silent() {
        assert!(TokenSource::Estimated.safety_margin() < 1.0);
        assert!(TokenSource::Estimated.safety_margin() > 0.5);
    }

    #[test]
    fn a_path_only_model_name_is_reduced_to_its_stem() {
        let body = json!({ "model_path": "D:\\models\\qwen35-4b-Q4_K_M.gguf", "chat_template": "" });
        let fp = EngineFingerprint::from_props(&body, None, false);
        assert_eq!(fp.model, "qwen35-4b-Q4_K_M");
    }

    #[test]
    fn health_supplies_n_ctx_when_props_omit_it() {
        let body = json!({ "model_alias": "x", "chat_template": "" });
        let fp = EngineFingerprint::from_props(&body, Some(&live_health()), false);
        assert_eq!(fp.n_ctx, Some(262_144));
    }

    #[test]
    fn health_supplies_the_model_name_when_props_omit_it() {
        let body = json!({ "chat_template": "" });
        let fp = EngineFingerprint::from_props(&body, Some(&live_health()), false);
        assert_eq!(fp.model, "qwen3.8-flash-next-unsloth-ud-q4_k_xl");
    }

    #[test]
    fn template_hash_is_short_but_distinguishes_the_two_models() {
        let big = EngineFingerprint::template_sha(BIG_MARKERS);
        let small = EngineFingerprint::template_sha(SMALL_MARKERS);
        assert_eq!(big.len(), 12);
        assert_ne!(big, small);
        assert_eq!(big, EngineFingerprint::template_sha(BIG_MARKERS), "stable across calls");
    }

    #[test]
    fn missing_fields_degrade_instead_of_failing() {
        let fp = EngineFingerprint::from_props(&json!({}), None, false);
        assert_eq!(fp.model, "unknown");
        assert_eq!(fp.n_ctx, None);
        assert!(!fp.supports_tools, "never assume tool support");
        assert_eq!(fp.template_sha, EngineFingerprint::template_sha(""));
    }

    #[test]
    fn a_model_swap_is_detected() {
        let big = EngineFingerprint::from_props(&live_props(BIG_MARKERS), None, false);
        let small = EngineFingerprint::from_props(&live_props(SMALL_MARKERS), None, false);
        assert!(!big.changed(None), "first sighting is not a change");
        assert!(!big.changed(Some(&big)));
        assert!(big.changed(Some(&small)), "different template must invalidate the prefix");
    }

    #[test]
    fn a_different_context_size_is_a_change() {
        let a = EngineFingerprint::from_props(&live_props(BIG_MARKERS), None, false);
        let mut b = a.clone();
        b.n_ctx = Some(32_768);
        assert!(a.changed(Some(&b)), "a different n_ctx changes what we can promise");
    }

    #[test]
    fn status_reads_the_live_bodies() {
        let status =
            EngineStatus::from_health(&live_health(), Some(&live_props("")), Some(&live_slots()));
        assert!(status.loaded);
        assert!(!status.sleeping);
        assert_eq!(status.max_context, Some(262_144));
        assert_eq!(status.slots, Some(1), "one slot, so capacity 1 in the health machine");
        assert_eq!(status.last_prompt_tokens, Some(70_254));
    }

    #[test]
    fn status_tolerates_absent_sections() {
        let status = EngineStatus::from_health(&json!({}), None, None);
        assert!(!status.loaded);
        assert_eq!(status.slots, None);
        assert_eq!(status.last_prompt_tokens, None);
    }

    #[test]
    fn sleeping_is_status_not_identity() {
        let awake = live_props(BIG_MARKERS);
        let mut asleep = awake.clone();
        asleep["is_sleeping"] = json!(true);

        let a = EngineFingerprint::from_props(&awake, None, false);
        let b = EngineFingerprint::from_props(&asleep, None, false);
        assert!(!a.changed(Some(&b)), "a nap must not invalidate the prompt cache");

        let status = EngineStatus::from_health(&live_health(), Some(&asleep), None);
        assert!(status.sleeping, "but it must be visible as status");
    }

    #[test]
    fn slots_without_a_prior_request_have_no_token_count() {
        let fresh = json!([{ "id": 0, "n_ctx": 262144, "is_processing": false }]);
        let status =
            EngineStatus::from_health(&live_health(), Some(&live_props("")), Some(&fresh));
        assert_eq!(status.last_prompt_tokens, None);
        assert_eq!(status.slots, Some(1));
    }

    #[test]
    fn cache_respects_ttl_and_reports_changes() {
        let cache = PropsCache::new(Duration::from_millis(50));
        assert!(!cache.is_fresh(Instant::now()));

        let (fp, changed) = cache.store(live_props(BIG_MARKERS), false);
        assert_eq!(fp.model, "qwen3.8-flash-next-unsloth-ud-q4_k_xl");
        assert!(!changed, "first store cannot be a change");
        assert!(cache.is_fresh(Instant::now()));

        let (_, changed) = cache.store(live_props(SMALL_MARKERS), false);
        assert!(changed, "the swap must be reported to whoever refreshes");

        std::thread::sleep(Duration::from_millis(60));
        assert!(!cache.is_fresh(Instant::now()), "the entry expired");
    }

    #[test]
    fn store_full_returns_the_status_alongside_the_fingerprint() {
        let cache = PropsCache::new(Duration::from_secs(60));
        let outcome = cache.store_full(
            live_props(BIG_MARKERS),
            Some(&live_health()),
            Some(&live_slots()),
            false,
        );
        assert_eq!(outcome.fingerprint.n_ctx, Some(262_144));
        assert_eq!(outcome.status.last_prompt_tokens, Some(70_254));
        assert!(!outcome.changed);
    }

    #[test]
    fn invalidate_forces_a_refresh() {
        let cache = PropsCache::new(Duration::from_secs(60));
        cache.store(live_props(BIG_MARKERS), false);
        assert!(cache.fingerprint().is_some());
        cache.invalidate();
        assert!(cache.fingerprint().is_none());
        assert!(!cache.is_fresh(Instant::now()));
    }
}
