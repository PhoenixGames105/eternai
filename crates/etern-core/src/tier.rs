//! Context-tier assembly (PLAN.md §3).
//!
//! Both devices build a prompt from the same four tiers; only the budget
//! differs per device ([`TierBudget`]):
//!
//! | Tier | Content                       | Desktop | Edge  |
//! |------|-------------------------------|---------|-------|
//! | T0   | Persona, pinned, byte-stable  | ~800    | ~600  |
//! | T1   | `MEMORY.md` distilled facts   | ~1.5k   | ~1k   |
//! | T2   | Journal digest (rolling ~7d)  | ~4k     | ~1.5k |
//! | T3   | Raw recent turns              | remainder | ~4.4k |
//! | —    | reserved for output           | 8k      | 700   |
//!
//! T0–T2 are rendered into a **byte-stable prefix** ([`Assembly::prefix`]): the
//! same persona/memory/journal always produces the same bytes, so the engine's
//! prompt cache keeps hitting. Everything that churns (T3, the raw turns) is
//! placed *after* the prefix, never before it.
//!
//! Each prefix tier is truncated independently to its own budget, so a long
//! journal entry can never crowd out the persona. The turns then receive
//! whatever of the window is left after the prefix and the output reserve
//! ([`TierBudget::turn_budget`]), trimmed on whole-turn boundaries by
//! [`crate::budget::trim_to_budget`] — never mid-turn, never orphaning a
//! `tool` result from its `assistant` `tool_calls`.
//!
//! The token figures here are always an *estimate* of the target device's
//! window (see the counting strategy in PLAN.md §4b); the phone re-checks with
//! its own tokenizer and is the sole authority for the final trim.

use crate::budget::{trim_to_budget, TokenCounter, TrimPolicy};
use crate::domain::{Message, Turn};
use crate::error::CoreResult;

/// Which device a tier budget belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceProfile {
    /// The high-power desktop model (full window).
    Desktop,
    /// The on-device edge model (8k window).
    Edge,
}

impl DeviceProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceProfile::Desktop => "desktop",
            DeviceProfile::Edge => "edge",
        }
    }

    /// Parses a profile name; anything unrecognised falls back to [`DeviceProfile::Edge`]
    /// because that is the budget that must never be overflowed.
    pub fn parse(s: &str) -> Self {
        match s {
            "desktop" | "high" | "high_power" => DeviceProfile::Desktop,
            _ => DeviceProfile::Edge,
        }
    }

    pub fn budget(self) -> TierBudget {
        match self {
            DeviceProfile::Desktop => TierBudget::desktop(),
            DeviceProfile::Edge => TierBudget::edge(),
        }
    }
}

/// Per-tier token budgets for one device (PLAN.md §3 table).
///
/// `t3` is not stored: it is the remainder after the prefix tiers and the
/// output reserve ([`TierBudget::t3_nominal`]), and the *actual* turn budget
/// absorbs any slack the prefix leaves unused
/// ([`TierBudget::turn_budget`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierBudget {
    /// T0 — persona cap.
    pub t0_persona: usize,
    /// T1 — `MEMORY.md` cap.
    pub t1_memory: usize,
    /// T2 — journal digest cap.
    pub t2_journal: usize,
    /// Tokens held back for the model's reply.
    pub output_reserve: usize,
    /// Total window size.
    pub total: usize,
}

impl TierBudget {
    /// Desktop: 256k window, 8k output reserve.
    pub fn desktop() -> Self {
        Self {
            t0_persona: 800,
            t1_memory: 1_500,
            t2_journal: 4_000,
            output_reserve: 8_000,
            total: 256_000,
        }
    }

    /// Edge: 8192 window, 700 output reserve (matches the gateway's
    /// `DEFAULT_EDGE_CTX` / `DEFAULT_RESPONSE_RESERVE`).
    pub fn edge() -> Self {
        Self {
            t0_persona: 600,
            t1_memory: 1_000,
            t2_journal: 1_500,
            output_reserve: 700,
            total: 8_192,
        }
    }

    /// Nominal T3 budget: the window minus the prefix caps and the reserve.
    /// For the edge profile this is ~4.4k, matching the §3 table.
    pub fn t3_nominal(&self) -> usize {
        self.total
            .saturating_sub(self.t0_persona)
            .saturating_sub(self.t1_memory)
            .saturating_sub(self.t2_journal)
            .saturating_sub(self.output_reserve)
    }

    /// Tokens the raw turns may use, given the prefix actually cost
    /// `prefix_tokens`. Unused prefix budget flows to the turns; the result is
    /// clamped so the prefix + turns + reserve never exceed the window.
    pub fn turn_budget(&self, prefix_tokens: usize) -> usize {
        self.total
            .saturating_sub(self.output_reserve)
            .saturating_sub(prefix_tokens)
    }
}

/// Which tiers had to be cut to fit their budgets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierTruncation {
    /// Persona exceeded T0.
    pub persona: bool,
    /// `MEMORY.md` exceeded T1.
    pub memory: bool,
    /// Journal digest exceeded T2.
    pub journal: bool,
    /// At least one recent turn was dropped from T3.
    pub turns: bool,
}

impl TierTruncation {
    pub fn any(&self) -> bool {
        self.persona || self.memory || self.journal || self.turns
    }
}

/// A fully assembled prompt: the byte-stable prefix followed by the recent
/// turns, plus accounting for what was dropped.
#[derive(Debug, Clone)]
pub struct Assembly {
    /// T0–T2 as system messages, in tier order. Empty tiers are omitted, so a
    /// device with no journal yet sends only the persona (and memory).
    pub prefix: Vec<Message>,
    /// T3 — whole recent turns, chronological, newest last.
    pub turns: Vec<Turn>,
    /// Estimated tokens used by [`Self::prefix`].
    pub prefix_tokens: usize,
    /// Estimated tokens used by [`Self::turns`].
    pub turn_tokens: usize,
    /// What had to be trimmed.
    pub truncated: TierTruncation,
}

impl Assembly {
    /// Flatten to OpenAI-shaped messages, ready for a chat-completion request:
    /// the stable prefix first, then the raw turns.
    pub fn to_messages(&self) -> Vec<Message> {
        let mut out = self.prefix.clone();
        out.extend(self.turns.iter().flat_map(|t| t.messages.clone()));
        out
    }

    /// Estimated tokens for the whole assembly (prefix + turns). The output
    /// reserve is *not* included — it is headroom, not content.
    pub fn total_tokens(&self) -> usize {
        self.prefix_tokens + self.turn_tokens
    }
}

/// Truncates `text` to the longest leading run of characters whose estimated
/// cost fits `budget`, measured with `counter`.
///
/// Deterministic for any monotonic counter, which is what keeps the prefix
/// byte-stable across calls. Keeps the head of the text (the persona and the
/// most important facts lead), never the tail.
pub fn truncate_to_tokens<C: TokenCounter + ?Sized>(
    text: &str,
    budget: usize,
    counter: &C,
) -> (String, bool) {
    if counter.count(text) <= budget {
        return (text.to_string(), false);
    }
    let chars: Vec<char> = text.chars().collect();
    // Binary-search the largest prefix length whose cost fits the budget.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        if counter.count(&candidate) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    (chars[..lo].iter().collect(), true)
}

/// Builds one prefix system message from a tier's text, or `None` when the
/// tier is empty (so an unset tier never emits an empty system turn).
fn tier_message<C: TokenCounter + ?Sized>(
    text: &str,
    budget: usize,
    counter: &C,
    truncated: &mut bool,
) -> Option<Message> {
    if text.trim().is_empty() {
        return None;
    }
    let (body, was_cut) = truncate_to_tokens(text, budget, counter);
    *truncated = *truncated || was_cut;
    Some(Message::system(body))
}

/// Assembles a prompt from the four tiers for a given device budget.
///
/// * `persona` — T0, pinned.
/// * `memory` — T1, the distilled `MEMORY.md`.
/// * `journal` — T2, the rolling journal digest.
/// * `recent_turns` — T3 candidates in chronological order (oldest first).
///
/// The prefix is built first and truncated per tier; the turns then receive
/// [`TierBudget::turn_budget`] of the remaining window, trimmed on whole-turn
/// boundaries. `policy` decides whether non-chat turns (sync/journal) are
/// eligible for T3 — use [`TrimPolicy::ChatOnly`] when building the edge
/// window (a synced batch is replayed as its own turns first) and
/// [`TrimPolicy::All`] for the desktop's own context.
pub fn assemble<C: TokenCounter + ?Sized>(
    persona: &str,
    memory: &str,
    journal: &str,
    recent_turns: &[Turn],
    budget: &TierBudget,
    counter: &C,
    policy: TrimPolicy,
) -> CoreResult<Assembly> {
    let mut trunc = TierTruncation::default();

    let mut prefix = Vec::new();
    if let Some(m) = tier_message(persona, budget.t0_persona, counter, &mut trunc.persona) {
        prefix.push(m);
    }
    if let Some(m) = tier_message(memory, budget.t1_memory, counter, &mut trunc.memory) {
        prefix.push(m);
    }
    if let Some(m) = tier_message(journal, budget.t2_journal, counter, &mut trunc.journal) {
        prefix.push(m);
    }

    let prefix_tokens = prefix.iter().map(|m| counter.count_message(m)).sum();
    let turn_budget = budget.turn_budget(prefix_tokens);

    let plan = trim_to_budget(recent_turns, turn_budget, 0, counter, policy)?;
    trunc.turns = plan.truncated;

    Ok(Assembly {
        prefix,
        turns: plan.turns,
        prefix_tokens,
        turn_tokens: plan.tokens_used,
        truncated: trunc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::CharDivFour;
    use crate::domain::{Message, Role, Turn, TurnKind};

    fn chat_turn(i: i64, user: &str, reply: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            i,
            vec![Message::user(user), Message::assistant(reply)],
        )
    }

    #[test]
    fn edge_nominal_t3_matches_the_plan_table() {
        // 8192 - 600 - 1000 - 1500 - 700 = 4392 (~4.4k).
        assert_eq!(TierBudget::edge().t3_nominal(), 4_392);
    }

    #[test]
    fn desktop_nominal_t3_is_the_remainder() {
        // 256000 - 800 - 1500 - 4000 - 8000 = 241700.
        assert_eq!(TierBudget::desktop().t3_nominal(), 241_700);
    }

    #[test]
    fn prefix_is_byte_stable_across_calls() {
        let counter = CharDivFour;
        let turns = vec![chat_turn(1, "hi", "hello")];
        let a = assemble(
            "persona text",
            "memory text",
            "journal text",
            &turns,
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        let b = assemble(
            "persona text",
            "memory text",
            "journal text",
            &turns,
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert_eq!(a.prefix, b.prefix);
        assert_eq!(a.prefix_tokens, b.prefix_tokens);
    }

    #[test]
    fn prefix_order_is_persona_memory_journal() {
        let counter = CharDivFour;
        let asm = assemble(
            "P",
            "M",
            "J",
            &[],
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert_eq!(asm.prefix.len(), 3);
        assert!(asm.prefix.iter().all(|m| m.role == Role::System));
        assert_eq!(asm.prefix[0].content, "P");
        assert_eq!(asm.prefix[1].content, "M");
        assert_eq!(asm.prefix[2].content, "J");
    }

    #[test]
    fn empty_tiers_are_omitted_from_the_prefix() {
        let counter = CharDivFour;
        let asm = assemble(
            "persona only",
            "   ",
            "",
            &[],
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert_eq!(asm.prefix.len(), 1);
        assert_eq!(asm.prefix[0].content, "persona only");
    }

    #[test]
    fn each_prefix_tier_is_capped_independently() {
        let counter = CharDivFour;
        // A huge journal must not crowd out the persona.
        let huge_journal = "x".repeat(100_000);
        let asm = assemble(
            "short persona",
            "short memory",
            &huge_journal,
            &[],
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert!(asm.truncated.journal);
        assert!(!asm.truncated.persona);
        assert!(!asm.truncated.memory);
        // Persona survives intact.
        assert_eq!(asm.prefix[0].content, "short persona");
        // Journal was cut down to roughly its T2 budget (1500 tokens ≈ 6000 chars).
        assert!(asm.prefix[2].content.chars().count() <= 1_500 * 4);
    }

    #[test]
    fn truncate_to_tokens_keeps_the_head_and_is_deterministic() {
        let counter = CharDivFour;
        let text = "abcdefghij".repeat(1_000);
        let (a, cut_a) = truncate_to_tokens(&text, 10, &counter);
        let (b, cut_b) = truncate_to_tokens(&text, 10, &counter);
        assert!(cut_a && cut_b);
        assert_eq!(a, b);
        assert!(counter.count(&a) <= 10);
        assert!(text.starts_with(&a));
    }

    #[test]
    fn truncate_leaves_short_text_untouched() {
        let (out, cut) = truncate_to_tokens("tiny", 100, &CharDivFour);
        assert_eq!(out, "tiny");
        assert!(!cut);
    }

    #[test]
    fn turns_receive_the_slack_the_prefix_leaves() {
        // Empty prefix → turns may use the whole window minus the reserve.
        let empty_prefix_budget = TierBudget::edge().turn_budget(0);
        assert_eq!(empty_prefix_budget, 8_192 - 700);
        // A big prefix eats into the turn budget one-for-one.
        let big_prefix = TierBudget::edge().turn_budget(1_000);
        assert_eq!(big_prefix, 8_192 - 700 - 1_000);
    }

    #[test]
    fn turns_are_trimmed_on_the_edge_budget() {
        let counter = CharDivFour;
        // Each turn is ~ (2 + 2 + 16 framing) tokens; make many turns so the
        // edge T3 budget forces a trim.
        let turns: Vec<Turn> = (0..200)
            .map(|i| chat_turn(i, &"u".repeat(400), &"r".repeat(400)))
            .collect();
        let asm = assemble(
            "persona",
            "memory",
            "journal",
            &turns,
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert!(asm.truncated.turns);
        assert!(asm.turns.len() < turns.len());
        // Kept turns are the newest contiguous run.
        assert_eq!(asm.turns.last().unwrap().seq, turns.last().unwrap().seq);
        // Total content never exceeds window minus reserve.
        assert!(asm.total_tokens() <= 8_192 - 700);
    }

    #[test]
    fn chat_only_policy_excludes_sync_turns_from_t3() {
        let counter = CharDivFour;
        let mut sync = chat_turn(1, "offline", "reply");
        sync.kind = TurnKind::Sync;
        let chat = chat_turn(2, "hi", "yo");
        let asm = assemble(
            "p",
            "m",
            "j",
            &[sync, chat.clone()],
            &TierBudget::edge(),
            &counter,
            TrimPolicy::ChatOnly,
        )
        .unwrap();
        assert_eq!(asm.turns.len(), 1);
        assert_eq!(asm.turns[0].kind, TurnKind::Chat);
    }

    #[test]
    fn to_messages_puts_prefix_before_turns() {
        let counter = CharDivFour;
        let asm = assemble(
            "persona",
            "",
            "",
            &[chat_turn(1, "hi", "hello")],
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        let msgs = asm.to_messages();
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "persona");
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[2].role, Role::Assistant);
    }

    #[test]
    fn desktop_budget_keeps_far_more_than_edge() {
        let counter = CharDivFour;
        let turns: Vec<Turn> = (0..50)
            .map(|i| chat_turn(i, &"u".repeat(400), &"r".repeat(400)))
            .collect();
        let edge = assemble(
            "p",
            "m",
            "j",
            &turns,
            &TierBudget::edge(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        let desktop = assemble(
            "p",
            "m",
            "j",
            &turns,
            &TierBudget::desktop(),
            &counter,
            TrimPolicy::All,
        )
        .unwrap();
        assert!(desktop.turns.len() > edge.turns.len());
        assert!(!desktop.truncated.turns);
    }

    #[test]
    fn device_profile_round_trips_and_defaults_to_edge() {
        assert_eq!(DeviceProfile::parse("desktop"), DeviceProfile::Desktop);
        assert_eq!(DeviceProfile::parse("edge"), DeviceProfile::Edge);
        // Unknown → Edge (the budget that must never overflow).
        assert_eq!(DeviceProfile::parse("nonsense"), DeviceProfile::Edge);
        assert_eq!(DeviceProfile::Edge.budget(), TierBudget::edge());
    }
}
