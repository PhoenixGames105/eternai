//! Journal: plain-text digests written out of the turn store.
//!
//! The files in `Journal/` are a *projection* — the SQLite store is the source
//! of truth and [`Journal::rebuild`] can regenerate every daily file from it.
//! That keeps the user free to edit or delete a day's file without ever
//! corrupting real history.
//!
//! Two kinds of file live in the same folder, distinguished by name:
//!   * `YYYY-MM-DD.txt`            — one per local day with activity
//!   * `sync-YYYYMMDD-HHMMSS.txt`  — one per offline-sync consolidation
//!
//! The local day rolls at [`DAY_BOUNDARY_HOUR`], so a conversation at 01:00
//! belongs to the previous day's entry.

use std::path::PathBuf;

use chrono::{DateTime, Local, TimeZone, TimeDelta, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{Turn, TurnKind};
use crate::error::{CoreError, CoreResult};
use crate::store::{JournalKind, Store};

/// Hour (local time) at which a new journal day begins. Conversations after
/// midnight but before this hour are appended to the previous day.
pub const DAY_BOUNDARY_HOUR: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DailyOutcome {
    /// Nothing to summarise — no turns in the window. No file written.
    SkippedEmpty,
    /// A summary was produced and written.
    Written { path: PathBuf, turns: usize },
    /// A file for this day already exists; not overwritten.
    AlreadyWritten { path: PathBuf },
}

pub struct Journal {
    dir: PathBuf,
    store: Store,
}

impl Journal {
    pub fn new(dir: impl Into<PathBuf>, store: Store) -> CoreResult<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir, store })
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    fn path_for_day(&self, day: &str) -> PathBuf {
        self.dir.join(format!("{day}.txt"))
    }

    /// Summary of the day containing `now`. Skips days with no conversation.
    ///
    /// `now` is a parameter rather than read from the clock so the boundary
    /// logic is testable and a scheduled job can pass the tick it fired for.
    pub fn write_daily(
        &self,
        now: DateTime<Utc>,
        summarize: &dyn Summarizer,
    ) -> CoreResult<DailyOutcome> {
        let day = day_of(now);
        let path = self.path_for_day(&day);
        if path.exists() {
            return Ok(DailyOutcome::AlreadyWritten { path });
        }

        let (from, to) = day_bounds(now);
        let turns = self
            .store
            .turns_between(from.timestamp_millis(), to.timestamp_millis())?;
        let chat: Vec<&Turn> = turns.iter().filter(|t| t.kind == TurnKind::Chat).collect();
        if chat.is_empty() {
            return Ok(DailyOutcome::SkippedEmpty);
        }

        let body = summarize.daily(&day, &chat)?;
        let text = render_digest(
            JournalKind::Daily,
            &day,
            now,
            chat.len(),
            chat.first().map(|t| t.ts),
            chat.last().map(|t| t.ts),
            &body,
        );
        std::fs::write(&path, text)?;
        self.store
            .record_journal_entry(&day, JournalKind::Daily, &path.to_string_lossy())?;
        Ok(DailyOutcome::Written {
            path,
            turns: chat.len(),
        })
    }

    /// Summary of a batch of turns ingested from an offline edge device.
    pub fn write_sync(
        &self,
        now: DateTime<Utc>,
        device_id: &str,
        turns: &[Turn],
        summarize: &dyn Summarizer,
    ) -> CoreResult<PathBuf> {
        if turns.is_empty() {
            return Err(CoreError::EmptyJournal(device_id.to_string()));
        }
        // No ':' in filenames — illegal on Windows. Stamp in *local* time so
        // the file sorts against the daily files on the same timeline.
        let stamp = now.with_timezone(&Local).format("%Y%m%d-%H%M%S").to_string();
        let path = self.dir.join(format!("sync-{stamp}.txt"));
        let body = summarize.sync(device_id, turns)?;
        let text = render_digest(
            JournalKind::Sync,
            &format!("{device_id} @ {stamp}"),
            now,
            turns.len(),
            turns.first().map(|t| t.ts),
            turns.last().map(|t| t.ts),
            &body,
        );
        std::fs::write(&path, text)?;
        self.store.record_journal_entry(
            &format!("sync-{stamp}"),
            JournalKind::Sync,
            &path.to_string_lossy(),
        )?;
        Ok(path)
    }

    /// Sweep for un-written daily digests over `[oldest turn, now]`.
    ///
    /// Needed because the desktop is usually asleep at the nominal rollover
    /// time; on launch we backfill every day that has turns but no file.
    pub fn catch_up(
        &self,
        now: DateTime<Utc>,
        summarize: &dyn Summarizer,
    ) -> CoreResult<Vec<DailyOutcome>> {
        self.sweep(now, summarize, false)
    }

    /// Regenerate every `YYYY-MM-DD.txt` from the store.
    ///
    /// Existing daily files are replaced; `sync-*.txt` files are left alone
    /// because their source batch is not reproducible from a day window.
    pub fn rebuild(&self, summarize: &dyn Summarizer) -> CoreResult<usize> {
        let outcomes = self.sweep(Utc::now(), summarize, true)?;
        Ok(outcomes
            .iter()
            .filter(|o| matches!(o, DailyOutcome::Written { .. }))
            .count())
    }

    fn sweep(
        &self,
        now: DateTime<Utc>,
        summarize: &dyn Summarizer,
        overwrite: bool,
    ) -> CoreResult<Vec<DailyOutcome>> {
        let Some(oldest_ms) = self.store.first_turn_ts()? else {
            return Ok(Vec::new());
        };
        let Some(mut cursor) = DateTime::<Utc>::from_timestamp_millis(oldest_ms) else {
            return Ok(Vec::new());
        };
        let end_day = day_of(now);
        let mut out = Vec::new();
        // Bounded walk: 10 years of days is a bug, not a backlog.
        for _ in 0..3660 {
            let day = day_of(cursor);
            if day > end_day {
                break;
            }
            if overwrite {
                let path = self.path_for_day(&day);
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
            // Tick just before the next boundary so the whole day is closed.
            let (_, tick) = day_bounds(cursor);
            let tick = (tick - TimeDelta::minutes(1)).max(cursor);
            let outcome = self.write_daily(tick, summarize)?;
            if !matches!(outcome, DailyOutcome::AlreadyWritten { .. }) {
                out.push(outcome);
            }
            if day == end_day {
                break;
            }
            cursor = next_day_start(cursor);
        }
        Ok(out)
    }
}

fn next_day_start(ts: DateTime<Utc>) -> DateTime<Utc> {
    let (_, to) = day_bounds(ts);
    to + TimeDelta::milliseconds(1)
}

// ---------------------------------------------------------------------------
// Summarisation is injected: the core has no model access, the apps do.
// ---------------------------------------------------------------------------

pub trait Summarizer {
    /// Prose summary of one day's conversation.
    fn daily(&self, day: &str, turns: &[&Turn]) -> CoreResult<String>;
    /// Prose summary of an offline batch ingested from `device_id`.
    fn sync(&self, device_id: &str, turns: &[Turn]) -> CoreResult<String>;
}

/// Deterministic, model-free summary. Used by tests and as a fallback when the
/// engine is unavailable, so a day's record is never lost entirely.
pub struct ExtractiveSummarizer {
    pub max_bullets: usize,
    pub max_line_chars: usize,
}

impl Default for ExtractiveSummarizer {
    fn default() -> Self {
        Self {
            max_bullets: 12,
            max_line_chars: 240,
        }
    }
}

impl Summarizer for ExtractiveSummarizer {
    fn daily(&self, day: &str, turns: &[&Turn]) -> CoreResult<String> {
        Ok(self.bullets(
            &format!("Topics discussed on {day}:"),
            &turns_text(turns),
        ))
    }

    fn sync(&self, device_id: &str, turns: &[Turn]) -> CoreResult<String> {
        let refs: Vec<&Turn> = turns.iter().collect();
        Ok(self.bullets(
            &format!("Offline conversation on device {device_id}:"),
            &turns_text(&refs),
        ))
    }
}

impl ExtractiveSummarizer {
    fn bullets(&self, header: &str, lines: &[String]) -> String {
        let mut out = format!("{header}\n");
        for line in lines.iter().take(self.max_bullets) {
            out.push_str("- ");
            out.push_str(&one_line(line, self.max_line_chars));
            out.push('\n');
        }
        if lines.len() > self.max_bullets {
            out.push_str(&format!(
                "- … {} further exchanges omitted from the digest.\n",
                lines.len() - self.max_bullets
            ));
        }
        out
    }
}

fn turns_text(turns: &[&Turn]) -> Vec<String> {
    let mut lines = Vec::new();
    for t in turns {
        if let Some(u) = t.user_text() {
            lines.push(format!("User: {u}"));
        }
        if let Some(a) = t.final_text() {
            lines.push(format!("Assistant: {a}"));
        }
    }
    lines
}

/// Local calendar day (as `YYYY-MM-DD`) a timestamp belongs to, honouring the
/// late-night boundary.
pub fn day_of(ts: DateTime<Utc>) -> String {
    let local = ts.with_timezone(&Local);
    if local.hour() < DAY_BOUNDARY_HOUR {
        (local - TimeDelta::days(1)).format("%Y-%m-%d").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

/// `[from, to]` UTC instants bounding the local journal day containing `ts`.
pub fn day_bounds(ts: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let local = ts.with_timezone(&Local);
    let base_date = if local.hour() < DAY_BOUNDARY_HOUR {
        (local - TimeDelta::days(1)).date_naive()
    } else {
        local.date_naive()
    };
    let start_local = base_date
        .and_hms_opt(DAY_BOUNDARY_HOUR, 0, 0)
        .expect("04:00 is always a valid time");
    let end_local = start_local + TimeDelta::days(1);

    // DST folds/springs are rare and shift at most an hour; `earliest` keeps
    // the window conservative rather than silently dropping turns.
    let from = resolve(start_local);
    let to = resolve(end_local);
    (from, to)
}

fn resolve(local: chrono::NaiveDateTime) -> DateTime<Utc> {
    // Ambiguous (DST fold) or nonexistent (DST spring-forward) instants take
    // the earliest candidate so the window is conservative rather than short.
    let zoned = local
        .and_local_timezone(Local)
        .single()
        .or_else(|| local.and_local_timezone(Local).earliest());
    zoned
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

/// Plain-text digest with a machine-scannable header.
pub fn render_digest(
    kind: JournalKind,
    subject: &str,
    generated_at: DateTime<Utc>,
    turn_count: usize,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    body: &str,
) -> String {
    let label = match kind {
        JournalKind::Daily => "daily",
        JournalKind::Sync => "sync",
    };
    let mut s = format!("# EternAI Journal — {subject}\nkind: {label}\n");
    s.push_str(&format!(
        "generated: {}\n",
        generated_at.with_timezone(&Local).to_rfc3339()
    ));
    s.push_str(&format!("turns: {turn_count}\n"));
    if let Some(ts) = first_ts {
        s.push_str(&format!("first: {}\n", fmt_ts(ts)));
    }
    if let Some(ts) = last_ts {
        s.push_str(&format!("last: {}\n", fmt_ts(ts)));
    }
    s.push('\n');
    s.push_str(body.trim_end());
    s.push('\n');
    s
}

fn fmt_ts(ts_ms: i64) -> String {
    match Local.timestamp_millis_opt(ts_ms).single() {
        Some(dt) => dt.to_rfc3339(),
        None => DateTime::<Utc>::from_timestamp_millis(ts_ms)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| ts_ms.to_string()),
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max_chars {
        return flat.to_string();
    }
    let mut truncated: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Message, Turn, TurnKind};
    use crate::store::Store;
    use chrono::{TimeZone, Utc as TzUtc};

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        // Build in *local* time then convert, so tests are timezone-correct.
        let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap();
        naive
            .and_local_timezone(Local)
            .single()
            .or_else(|| naive.and_local_timezone(Local).earliest())
            .unwrap()
            .with_timezone(&Utc)
    }

    fn chat_turn(text: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            1,
            vec![Message::user(text), Message::assistant("noted")],
        )
    }

    /// Turn with an explicit timestamp and a unique `(device, seq)` key — the
    /// store's idempotency index silently ignores duplicate seqs, which would
    /// otherwise make a test look like it lost a turn.
    fn chat_turn_at(at: DateTime<Utc>, seq: i64, text: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            "desktop",
            seq,
            vec![Message::user(text), Message::assistant("noted")],
        )
        .with_ts(at.timestamp_millis())
    }

    fn journal(dir: &tempfile::TempDir) -> (Journal, Store) {
        let store = Store::open_in_memory().unwrap();
        let j = Journal::new(dir.path().join("Journal"), store.clone()).unwrap();
        (j, store)
    }

    #[test]
    fn late_night_timestamp_belongs_to_previous_day() {
        assert_eq!(day_of(ts(2026, 7, 29, 3, 59)), "2026-07-28");
        assert_eq!(day_of(ts(2026, 7, 29, 4, 0)), "2026-07-29");
        assert_eq!(day_of(ts(2026, 7, 29, 23, 30)), "2026-07-29");
    }

    #[test]
    fn day_bounds_span_twenty_four_hours() {
        let (from, to) = day_bounds(ts(2026, 7, 29, 12, 0));
        assert_eq!(to - from, TimeDelta::hours(24));
        assert_eq!(from, ts(2026, 7, 29, 4, 0));
    }

    #[test]
    fn empty_day_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, _store) = journal(&dir);
        let out = j.write_daily(ts(2026, 7, 29, 20, 0), &ExtractiveSummarizer::default()).unwrap();
        assert_eq!(out, DailyOutcome::SkippedEmpty);
        assert_eq!(std::fs::read_dir(j.dir()).unwrap().count(), 0);
    }

    #[test]
    fn daily_file_is_written_once_and_named_by_day() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, store) = journal(&dir);
        store
            .insert_turn(&chat_turn_at(ts(2026, 7, 29, 9, 0), 1, "what is the weather"))
            .unwrap();

        let out = j
            .write_daily(ts(2026, 7, 29, 20, 0), &ExtractiveSummarizer::default())
            .unwrap();
        let DailyOutcome::Written { path, turns } = out else {
            panic!("expected Written, got {out:?}");
        };
        assert_eq!(turns, 1);
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "2026-07-29.txt");

        let again = j
            .write_daily(ts(2026, 7, 29, 21, 0), &ExtractiveSummarizer::default())
            .unwrap();
        assert!(matches!(again, DailyOutcome::AlreadyWritten { .. }));
        assert_eq!(std::fs::read_dir(j.dir()).unwrap().count(), 1);
    }

    #[test]
    fn digest_header_and_body_are_present() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, store) = journal(&dir);
        store
            .insert_turn(&chat_turn_at(ts(2026, 7, 29, 9, 0), 1, "plan the trip"))
            .unwrap();
        j.write_daily(ts(2026, 7, 29, 20, 0), &ExtractiveSummarizer::default())
            .unwrap();
        let text = std::fs::read_to_string(j.dir().join("2026-07-29.txt")).unwrap();
        assert!(text.contains("kind: daily"));
        assert!(text.contains("turns: 1"));
        assert!(text.contains("User: plan the trip"));
        assert!(text.contains("Assistant: noted"));
    }

    #[test]
    fn sync_entry_uses_a_separate_filename_and_never_collides() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, _store) = journal(&dir);
        let p1 = j
            .write_sync(
                ts(2026, 7, 29, 20, 0),
                "pixel-a",
                &[chat_turn("offline note")],
                &ExtractiveSummarizer::default(),
            )
            .unwrap();
        assert_eq!(
            p1.file_name().unwrap().to_string_lossy(),
            "sync-20260729-200000.txt"
        );
        assert!(!p1.file_name().unwrap().to_string_lossy().contains(':'));

        let err = j.write_sync(
            ts(2026, 7, 29, 20, 5),
            "pixel-a",
            &[],
            &ExtractiveSummarizer::default(),
        );
        assert!(matches!(err, Err(CoreError::EmptyJournal(_))));
    }

    #[test]
    fn catch_up_backfills_only_days_with_activity() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, store) = journal(&dir);
        // A turn from three days ago and one from today; the gap days are empty.
        store
            .insert_turn(&chat_turn_at(ts(2026, 7, 26, 10, 0), 1, "old thing"))
            .unwrap();
        store
            .insert_turn(&chat_turn_at(ts(2026, 7, 29, 10, 0), 2, "new thing"))
            .unwrap();

        let out = j
            .catch_up(ts(2026, 7, 29, 12, 0), &ExtractiveSummarizer::default())
            .unwrap();
        let written: Vec<_> = out
            .iter()
            .filter(|o| matches!(o, DailyOutcome::Written { .. }))
            .collect();
        assert_eq!(written.len(), 2);
        assert!(j.dir().join("2026-07-26.txt").exists());
        assert!(!j.dir().join("2026-07-27.txt").exists());
        assert!(!j.dir().join("2026-07-28.txt").exists());
        assert!(j.dir().join("2026-07-29.txt").exists());
    }

    #[test]
    fn rebuild_regenerates_deleted_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let (j, store) = journal(&dir);
        store
            .insert_turn(&chat_turn_at(ts(2026, 7, 29, 9, 0), 1, "remember me"))
            .unwrap();
        j.write_daily(ts(2026, 7, 29, 20, 0), &ExtractiveSummarizer::default())
            .unwrap();
        std::fs::remove_file(j.dir().join("2026-07-29.txt")).unwrap();

        let n = j.rebuild(&ExtractiveSummarizer::default()).unwrap();
        assert!(n >= 1);
        let text = std::fs::read_to_string(j.dir().join("2026-07-29.txt")).unwrap();
        assert!(text.contains("remember me"));
    }

    #[test]
    fn long_lines_are_flattened_and_truncated() {
        let long = "x".repeat(500);
        let out = one_line(&long, 50);
        assert_eq!(out.chars().count(), 50);
        assert!(out.ends_with('…'));
        assert_eq!(one_line("a\nb\r\nc", 100), "a b  c");
    }

    #[test]
    fn timestamps_render_as_local_rfc3339() {
        let instant = TzUtc.timestamp_millis_opt(ts(2026, 7, 29, 20, 0).timestamp_millis()).unwrap();
        let s = fmt_ts(instant.timestamp_millis());
        assert!(s.contains("2026-07-29"), "got {s}");
    }
}
