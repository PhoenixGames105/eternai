//! Journal integration: the desktop-only summariser and the two triggers that
//! drive it.
//!
//! Summarisation is exclusively a desktop job (PLAN §4b — "the phone never
//! summarises"), so it lives here in the gateway, not in `etern-core`. The core
//! exposes a synchronous [`Summarizer`] trait; the engine is an async HTTP
//! server, so the bridge is a blocking `reqwest` client invoked from
//! `spawn_blocking` — journal jobs are background work, never on a request's
//! critical path, so occupying a blocking thread is acceptable and keeps the
//! runtime free.
//!
//! Two triggers, per PLAN §5:
//!   * a scheduled run at the 4 a.m. day boundary, and
//!   * a catch-up sweep on launch, because a desktop is usually asleep at 4 a.m.
//!     and would otherwise skip every day it wasn't awake to close.
//!
//! When the engine is down, mid-swap, or returns nothing, the summariser falls
//! back to the deterministic [`ExtractiveSummarizer`] so a day's record is never
//! lost entirely — a rough digest beats a missing file.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use etern_core::{
    day_bounds, DailyOutcome, ExtractiveSummarizer, Journal, Summarizer, Turn,
};
use serde_json::json;

/// A summary generation is short and must not hang a background job for an
/// hour the way an inference passthrough might.
const SUMMARIZE_TIMEOUT: Duration = Duration::from_secs(180);
/// Cap the transcript handed to the model. A day is rarely this long; the cap
/// only trips on pathological sessions and keeps a summarisation request bounded.
const MAX_TRANSCRIPT_CHARS: usize = 24_000;
/// Woken exactly on a boundary, we tick this far back so the write lands inside
/// the day that just closed rather than the one that just opened.
const CLOSE_TICK: TimeDelta = TimeDelta::minutes(1);

/// Model-backed summariser with a deterministic fallback.
pub struct EngineSummarizer {
    /// Built lazily, on first use inside `spawn_blocking`. A `reqwest` blocking
    /// client spins up its own tokio runtime, which panics if constructed on an
    /// async worker (e.g. in `main`); deferring it to the blocking pool avoids
    /// that entirely.
    client: std::sync::OnceLock<reqwest::blocking::Client>,
    base: String,
    fallback: ExtractiveSummarizer,
}

impl EngineSummarizer {
    pub fn new(base: &str) -> Self {
        Self {
            client: std::sync::OnceLock::new(),
            base: base.trim_end_matches('/').to_string(),
            fallback: ExtractiveSummarizer::default(),
        }
    }

    fn client(&self) -> &reqwest::blocking::Client {
        self.client.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(SUMMARIZE_TIMEOUT)
                .build()
                .expect("blocking reqwest client")
        })
    }

    /// Asks the engine to summarise `transcript`; on any failure (engine down,
    /// mid-swap, empty or malformed reply) falls back to the extractive digest.
    fn summarize(&self, system: &str, transcript: &str, fallback: impl FnOnce() -> String) -> String {
        let body = json!({
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": transcript },
            ],
            "stream": false,
            "temperature": 0.2,
            "max_tokens": 512,
        });
        let reply = self
            .client()
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .send()
            .ok()
            .and_then(|r| r.json::<serde_json::Value>().ok())
            .and_then(|v| {
                v["choices"]
                    .get(0)
                    .and_then(|c| c["message"]["content"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            });
        match reply {
            Some(text) => text,
            None => {
                tracing::debug!("engine summarisation unavailable; using extractive digest");
                fallback()
            }
        }
    }
}

impl Summarizer for EngineSummarizer {
    fn daily(&self, day: &str, turns: &[&Turn]) -> etern_core::CoreResult<String> {
        let transcript = build_transcript(turns);
        let system = "You keep a personal AI companion's journal. Summarise the \
day's conversation into the most important topics discussed, preserving concrete \
details: names, decisions, plans, and open questions. Use short bullet points. \
Output only the summary, no preamble.";
        let user = format!("Journal day {day}.\n\n{transcript}");
        Ok(self.summarize(system, &user, || {
            self.fallback.daily(day, turns).unwrap_or_default()
        }))
    }

    fn sync(&self, device_id: &str, turns: &[Turn]) -> etern_core::CoreResult<String> {
        let refs: Vec<&Turn> = turns.iter().collect();
        let transcript = build_transcript(&refs);
        let system = "You keep a personal AI companion's journal. A phone used an \
offline edge model and has reconnected. Summarise that offline conversation into \
the most important topics, preserving concrete details: names, decisions, plans, \
and open questions. Use short bullet points. Output only the summary, no preamble.";
        let user = format!("Offline conversation from device {device_id}.\n\n{transcript}");
        Ok(self.summarize(system, &user, || {
            self.fallback.sync(device_id, turns).unwrap_or_default()
        }))
    }
}

/// Flattens turns into a `User:`/`Assistant:` transcript, bounded in length.
fn build_transcript(turns: &[&Turn]) -> String {
    let mut out = String::new();
    for t in turns {
        if let Some(u) = t.user_text() {
            out.push_str("User: ");
            out.push_str(u);
            out.push('\n');
        }
        if let Some(a) = t.final_text() {
            out.push_str("Assistant: ");
            out.push_str(a);
            out.push('\n');
        }
        if out.len() >= MAX_TRANSCRIPT_CHARS {
            out.truncate(MAX_TRANSCRIPT_CHARS);
            out.push_str("\n… (transcript truncated)");
            break;
        }
    }
    out
}

type SharedJournal = Arc<Journal>;
type SharedSummarizer = Arc<dyn Summarizer + Send + Sync>;

/// Runs the launch catch-up sweep on a blocking thread and logs what it wrote.
///
/// The sweep writes every closed day that has turns but no journal file yet —
/// the recovery path for a desktop that was asleep through past boundaries.
pub fn spawn_catch_up(journal: SharedJournal, summarizer: SharedSummarizer) {
    tokio::spawn(async move {
        let (journal, summarizer) = (journal, summarizer);
        let result = tokio::task::spawn_blocking(move || {
            journal.catch_up(Utc::now(), summarizer.as_ref())
        })
        .await;
        match result {
            Ok(Ok(outcomes)) => {
                for outcome in outcomes {
                    if let DailyOutcome::Written { path, turns } = outcome {
                        tracing::info!(path = %path.display(), turns, "catch-up wrote journal");
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "journal catch-up failed"),
            Err(e) => tracing::warn!(error = %e, "journal catch-up task panicked"),
        }
    });
}

/// Loops forever: sleep to the next 4 a.m. boundary, write the day that just
/// closed, repeat. Started once at launch alongside the catch-up sweep.
pub fn spawn_daily_scheduler(journal: SharedJournal, summarizer: SharedSummarizer) {
    tokio::spawn(async move {
        loop {
            let now = Utc::now();
            let boundary = next_boundary(now);
            let wait = (boundary - now)
                .to_std()
                .unwrap_or_else(|_| Duration::from_secs(60))
                .max(Duration::from_secs(1));
            tokio::time::sleep(wait).await;

            // Woken at the boundary, the day that just closed ends here; tick a
            // minute back so the write lands inside it, not the fresh day.
            let tick = boundary - CLOSE_TICK;
            let (journal, summarizer) = (journal.clone(), summarizer.clone());
            match tokio::task::spawn_blocking(move || {
                journal.write_daily(tick, summarizer.as_ref())
            })
            .await
            {
                Ok(Ok(DailyOutcome::Written { path, turns })) => {
                    tracing::info!(path = %path.display(), turns, "scheduled journal written");
                }
                Ok(Ok(DailyOutcome::SkippedEmpty)) => {
                    tracing::debug!("no conversation to journal at boundary");
                }
                Ok(Ok(DailyOutcome::AlreadyWritten { .. })) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "scheduled journal write failed"),
                Err(e) => tracing::warn!(error = %e, "scheduled journal task panicked"),
            }
        }
    });
}

/// The instant the current journal day closes (the next 4 a.m. boundary).
/// Exposed for tests that assert the scheduler targets the right wake time.
pub fn next_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    day_bounds(now).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use etern_core::{Store, TurnKind};
    use chrono::TimeZone;

    fn turn(seq: i64, ts_ms: i64, user: &str, assistant: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            seq,
            vec![
                etern_core::Message::user(user),
                etern_core::Message::assistant(assistant),
            ],
        )
        .with_ts(ts_ms)
    }

    /// Turns are stored and queried in epoch **milliseconds**, so every seeded
    /// timestamp must be in millis to land inside a day window.
    fn ms(dt: DateTime<Utc>) -> i64 {
        dt.timestamp_millis()
    }

    /// A summariser that never touches the network, so the file-writing tests
    /// are hermetic and deterministic.
    struct FakeSummarizer;
    impl Summarizer for FakeSummarizer {
        fn daily(&self, day: &str, _turns: &[&Turn]) -> etern_core::CoreResult<String> {
            Ok(format!("fake summary for {day}"))
        }
        fn sync(&self, device: &str, _turns: &[Turn]) -> etern_core::CoreResult<String> {
            Ok(format!("fake sync for {device}"))
        }
    }

    fn seeded_journal(dir: &std::path::Path) -> (Store, Journal) {
        let store = Store::open_in_memory().unwrap();
        let journal = Journal::new(dir, store.clone()).unwrap();
        (store, journal)
    }

    #[test]
    fn a_day_with_turns_produces_a_journal_file() {
        let dir = tempdir("written");
        let (store, journal) = seeded_journal(&dir);

        // Seed a synthetic day: two chat turns inside one journal day.
        let (from, to) = day_bounds(Utc::now());
        let mid = from + TimeDelta::hours(6);
        store.insert_turn(&turn(1, ms(mid), "Where should I vacation?", "Somewhere warm.")).unwrap();
        store.insert_turn(&turn(2, ms(mid + TimeDelta::hours(1)), "Book it.", "Done.")).unwrap();
        assert!(to > mid);

        let outcome = journal.write_daily(mid, &FakeSummarizer).unwrap();
        let DailyOutcome::Written { path, turns } = outcome else {
            panic!("expected a written journal, got {outcome:?}");
        };
        assert_eq!(turns, 2);
        assert!(path.exists(), "the journal file must exist on disk");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("fake summary for"), "the summariser output must be in the file");
        assert!(body.contains("turns: 2"), "the digest header must record the turn count: {body}");
        cleanup(&dir);
    }

    #[test]
    fn an_empty_day_is_skipped_and_writes_no_file() {
        let dir = tempdir("empty");
        let (_store, journal) = seeded_journal(&dir);
        // No turns at all in this day.
        let outcome = journal.write_daily(Utc::now(), &FakeSummarizer).unwrap();
        assert!(matches!(outcome, DailyOutcome::SkippedEmpty), "got {outcome:?}");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "no file should be written");
        cleanup(&dir);
    }

    #[test]
    fn writing_the_same_day_twice_is_idempotent() {
        let dir = tempdir("idempotent");
        let (store, journal) = seeded_journal(&dir);
        let (from, _) = day_bounds(Utc::now());
        let mid = from + TimeDelta::hours(6);
        store.insert_turn(&turn(1, ms(mid), "hi", "hello")).unwrap();

        let first = journal.write_daily(mid, &FakeSummarizer).unwrap();
        assert!(matches!(first, DailyOutcome::Written { .. }));
        let second = journal.write_daily(mid, &FakeSummarizer).unwrap();
        assert!(matches!(second, DailyOutcome::AlreadyWritten { .. }), "got {second:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "exactly one file");
        cleanup(&dir);
    }

    #[test]
    fn catch_up_writes_every_closed_day() {
        let dir = tempdir("catchup");
        let store = Store::open_in_memory().unwrap();
        let journal = Journal::new(&dir, store.clone()).unwrap();

        // Seed three consecutive past journal days plus today (still open).
        // The sweep runs from the oldest turn through today inclusive, so all
        // four days have turns and all four get a file.
        let today = Utc::now();
        let mut cursor = today - TimeDelta::days(3);
        for seq in 1..=4i64 {
            let (from, _) = day_bounds(cursor);
            let mid = from + TimeDelta::hours(6);
            store
                .insert_turn(&turn(seq, ms(mid), &format!("day {mid}"), "answer"))
                .unwrap();
            cursor = next_boundary(cursor);
        }

        let outcomes = journal.catch_up(today, &FakeSummarizer).unwrap();
        let written = outcomes
            .iter()
            .filter(|o| matches!(o, DailyOutcome::Written { .. }))
            .count();
        assert_eq!(written, 4, "expected four days written, got {written}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4, "one file per day");
        cleanup(&dir);
    }

    #[test]
    fn the_boundary_is_the_next_four_am() {
        // 2026-11-18 09:00 local → the day closes at 2026-11-19 04:00 local.
        // Build a local instant and check the boundary is strictly after it and
        // lands on the 4 a.m. boundary of the following day.
        let local = chrono::Local
            .with_ymd_and_hms(2026, 11, 18, 9, 0, 0)
            .single()
            .expect("valid local time");
        let boundary = next_boundary(local.with_timezone(&Utc));
        assert!(boundary > local.to_utc());
        let boundary_local = boundary.with_timezone(&chrono::Local);
        assert_eq!(boundary_local.format("%Y-%m-%d %H:%M").to_string(), "2026-11-19 04:00");
    }

    #[test]
    fn the_engine_summarizer_falls_back_when_the_engine_is_absent() {
        // Port 1 is never listening, so the HTTP call fails and the extractive
        // digest must be produced instead of an error.
        let s = EngineSummarizer::new("http://127.0.0.1:1");
        let t = turn(1, Utc::now().timestamp_millis(), "What is the weather?", "Sunny.");
        let refs = vec![&t];
        let out = s.daily("2026-11-18", &refs).unwrap();
        assert!(out.contains("weather"), "the extractive fallback must include the user line: {out}");
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("etern-journal-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
