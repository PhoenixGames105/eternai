use crate::domain::{Message, Role, Turn};
use crate::error::{CoreError, CoreResult};

/// Estimates token cost without an engine.
///
/// Only used for journal/memory tier bookkeeping and as a fallback when no
/// engine is reachable. The trimmer never trusts this for the final decision —
/// see [`TokenCounter`].
pub struct CharDivFour;

impl TokenCounter for CharDivFour {
    fn count(&self, text: &str) -> usize {
        // Ceiling, not floor: flooring would cost a short turn zero tokens,
        // which lets a budget loop spin forever on a run of tiny turns.
        text.chars().count().div_ceil(4)
    }
}

/// Measures token cost for a specific model.
///
/// Desktop implements this with `POST /tokenize` against the live engine; the
/// phone implements it with the already-loaded edge tokenizer. Both Qwen models
/// share the `qwen35` pre-tokenizer and BPE vocab, so a desktop measurement is a
/// good estimate of edge *content* cost — but the templates differ, so the
/// phone always re-checks and trims further if needed.
pub trait TokenCounter {
    fn count(&self, text: &str) -> usize;

    fn count_message(&self, msg: &Message) -> usize {
        let mut n = self.count(msg.role.as_str()) + self.count(&msg.content);
        // Per-message framing emitted by the Qwen chat template
        // (<|im_start|>role ... <|im_end|>\n). Not exact, but consistent, and
        // the phone re-verifies against the real tokenizer anyway.
        n += 8;
        if let Some(tc) = &msg.tool_calls {
            n += self.count(tc) + 12;
        }
        if let Some(id) = &msg.tool_call_id {
            n += self.count(id) + 6;
        }
        n
    }

    fn count_turn(&self, turn: &Turn) -> usize {
        turn.messages.iter().map(|m| self.count_message(m)).sum()
    }
}

/// How a trim pass treats non-chat turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimPolicy {
    /// Keep only [`TurnKind::Chat`](crate::domain::TurnKind::Chat) turns.
    /// Used for the edge window: a synced offline batch is replayed as its own
    /// turns first, so it must not also appear as raw history.
    ChatOnly,
    /// Keep every turn. Used for the desktop's own context assembly.
    All,
}

/// Result of a trim pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimPlan {
    /// Turns in chronological order, newest last, that fit the budget.
    pub turns: Vec<Turn>,
    /// Tokens consumed by [`Self::turns`].
    pub tokens_used: usize,
    /// Tokens still available after the pass.
    pub tokens_remaining: usize,
    /// True when the oldest eligible turn had to be dropped.
    pub truncated: bool,
    /// `ts` of the oldest kept turn, for watermarking.
    pub oldest_ts: Option<i64>,
}

impl TrimPlan {
    pub fn empty() -> Self {
        Self {
            turns: Vec::new(),
            tokens_used: 0,
            tokens_remaining: 0,
            truncated: false,
            oldest_ts: None,
        }
    }

    /// Flatten to OpenAI-shaped messages, ready for a chat-completion request.
    pub fn to_messages(&self) -> Vec<Message> {
        self.turns.iter().flat_map(|t| t.messages.clone()).collect()
    }
}

/// Selects the newest whole turns that fit `budget_tokens`.
///
/// Walks backwards from the newest turn and stops at the first turn that does
/// not fit — it does not skip over an oversized turn to grab older ones. A
/// context window with a hole in the middle confuses models far more than a
/// slightly shorter window, and it keeps the kept-set contiguous, which is what
/// makes the desktop's proposal safe for the phone to accept verbatim.
///
/// `overhead_tokens` is reserved for the chat template, system prompt, and tool
/// schemas, none of which live in the turns.
pub fn trim_to_budget<C: TokenCounter + ?Sized>(
    turns: &[Turn],
    budget_tokens: usize,
    overhead_tokens: usize,
    counter: &C,
    policy: TrimPolicy,
) -> CoreResult<TrimPlan> {
    if budget_tokens < overhead_tokens {
        return Err(CoreError::Budget(format!(
            "budget {budget_tokens} is smaller than template overhead {overhead_tokens}"
        )));
    }
    let available = budget_tokens - overhead_tokens;

    let eligible: Vec<&Turn> = match policy {
        TrimPolicy::All => turns.iter().collect(),
        TrimPolicy::ChatOnly => turns
            .iter()
            .filter(|t| t.kind == crate::domain::TurnKind::Chat)
            .collect(),
    };

    let mut kept: Vec<Turn> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;

    for turn in eligible.iter().rev() {
        let cost = counter.count_turn(turn);
        if used + cost > available {
            // First miss ends the walk; everything older is by definition out.
            truncated = true;
            break;
        }
        used += cost;
        kept.push((*turn).clone());
    }

    kept.reverse();

    let oldest_ts = kept.first().map(|t| t.ts);
    Ok(TrimPlan {
        turns: kept,
        tokens_used: used,
        tokens_remaining: available.saturating_sub(used),
        truncated,
        oldest_ts,
    })
}

/// Drops leading system messages from `msgs` except the first, which the Qwen
/// templates require to be the only one (the desktop template raises otherwise).
pub fn collapse_system_prefix(msgs: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut seen_system = false;
    for m in msgs {
        if m.role == Role::System {
            if seen_system {
                continue;
            }
            seen_system = true;
        }
        out.push(m.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EngineKind, Turn, TurnKind};

    fn turn(seq: i64, user: &str, reply: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            seq,
            vec![Message::user(user), Message::assistant(reply)],
        )
        .with_engine(EngineKind::Llama, "test-model")
    }

    #[test]
    fn char_div_four_never_returns_zero_for_nonempty() {
        assert_eq!(CharDivFour.count(""), 0);
        assert_eq!(CharDivFour.count("a"), 1);
        assert_eq!(CharDivFour.count("abcd"), 1);
        assert_eq!(CharDivFour.count("abcde"), 2);
    }

    #[test]
    fn trim_keeps_newest_turns_and_cuts_on_turn_boundary() {
        let turns: Vec<Turn> = (0..10).map(|i| turn(i, "hello there", "a reply")).collect();
        let one = CharDivFour.count_turn(&turns[0]);

        // Room for exactly three turns.
        let plan = trim_to_budget(&turns, one * 3, 0, &CharDivFour, TrimPolicy::All).unwrap();
        assert_eq!(plan.turns.len(), 3);
        assert_eq!(plan.turns[0].seq, 7);
        assert_eq!(plan.turns[2].seq, 9);
        assert!(plan.truncated);

        // Kept set must be contiguous in seq, never a hole.
        let seqs: Vec<i64> = plan.turns.iter().map(|t| t.seq).collect();
        assert_eq!(seqs, vec![7, 8, 9]);
    }

    #[test]
    fn trim_keeps_everything_when_budget_is_generous() {
        let turns: Vec<Turn> = (0..5).map(|i| turn(i, "u", "r")).collect();
        let plan = trim_to_budget(&turns, 100_000, 0, &CharDivFour, TrimPolicy::All).unwrap();
        assert_eq!(plan.turns.len(), 5);
        assert!(!plan.truncated);
        assert_eq!(plan.oldest_ts, Some(turns[0].ts));
    }

    #[test]
    fn trim_chat_only_excludes_sync_and_journal_turns() {
        let mut turns = vec![turn(1, "chat", "reply")];
        let mut sync = turn(2, "offline chat", "offline reply");
        sync.kind = TurnKind::Sync;
        turns.push(sync);

        let plan = trim_to_budget(&turns, 100_000, 0, &CharDivFour, TrimPolicy::ChatOnly).unwrap();
        assert_eq!(plan.turns.len(), 1);
        assert_eq!(plan.turns[0].kind, TurnKind::Chat);
    }

    #[test]
    fn trim_rejects_budget_smaller_than_overhead() {
        let turns = vec![turn(1, "u", "r")];
        let err = trim_to_budget(&turns, 10, 100, &CharDivFour, TrimPolicy::All).unwrap_err();
        assert!(matches!(err, CoreError::Budget(_)));
    }

    #[test]
    fn tool_result_stays_with_its_assistant_turn() {
        // A turn whose assistant message issues a tool call, answered by a
        // tool message. Trimming must never keep the tool result alone.
        let mut call = Message::assistant("");
        call.tool_calls = Some(r#"[{"id":"c1","name":"web_search"}]"#.into());
        let t = Turn::new(
            TurnKind::Chat,
            "desktop",
            1,
            vec![
                Message::user("search something"),
                call,
                Message::tool("c1", "result text"),
            ],
        );
        let turns = vec![t.clone()];
        let cost = CharDivFour.count_turn(&t);
        let plan = trim_to_budget(&turns, cost, 0, &CharDivFour, TrimPolicy::All).unwrap();
        assert_eq!(plan.turns.len(), 1);
        let msgs = plan.to_messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, Role::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn collapse_system_prefix_keeps_only_the_first_system_message() {
        let msgs = vec![
            Message::system("persona"),
            Message::system("extra"),
            Message::user("hi"),
        ];
        let out = collapse_system_prefix(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "persona");
        assert_eq!(out[1].role, Role::User);
    }
}

