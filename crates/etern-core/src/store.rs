use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::domain::{EngineKind, Message, Role, Turn, TurnId, TurnKind};
use crate::error::{CoreError, CoreResult};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS turns (
    id          TEXT PRIMARY KEY,
    ts          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    engine      TEXT NOT NULL DEFAULT '',
    model       TEXT NOT NULL DEFAULT '',
    device_id   TEXT NOT NULL DEFAULT '',
    seq         INTEGER NOT NULL DEFAULT 0
);

-- Idempotency key for outbox replays. A device re-pushing a turn it already
-- delivered hits this index and the insert is ignored, not duplicated.
-- Partial so that locally-created turns with no sync identity (empty
-- device_id) never collide with each other.
CREATE UNIQUE INDEX IF NOT EXISTS idx_turns_device_seq
    ON turns (device_id, seq) WHERE device_id != '';

CREATE INDEX IF NOT EXISTS idx_turns_ts ON turns (ts);

CREATE TABLE IF NOT EXISTS messages (
    turn_id      TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    ordinal      INTEGER NOT NULL,
    role         TEXT NOT NULL,
    content      TEXT NOT NULL,
    name         TEXT,
    tool_calls   TEXT,
    tool_call_id TEXT,
    PRIMARY KEY (turn_id, ordinal)
);

CREATE TABLE IF NOT EXISTS journal_entries (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    day        TEXT NOT NULL,
    kind       TEXT NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_ts INTEGER NOT NULL,
    file       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_journal_day ON journal_entries (day);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Paired edge devices (PLAN §8). The token is the device's long-lived bearer
-- credential, minted by the desktop when the device redeems a one-time pair
-- code. Revocation flips `revoked` rather than deleting, so the audit trail
-- (which device existed, when it joined, when it was cut) survives.
CREATE TABLE IF NOT EXISTS devices (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    token      TEXT NOT NULL UNIQUE,
    created_ts INTEGER NOT NULL,
    revoked    INTEGER NOT NULL DEFAULT 0
);

-- Rebuildable sidecar. `turn_id` is UNINDEXED so a hit maps back to the
-- canonical row; the store itself stays the source of truth.
CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
    turn_id UNINDEXED,
    body,
    tokenize='unicode61'
);
"#;

pub struct Store {
    conn: std::sync::Arc<Mutex<Connection>>,
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
        }
    }
}

pub struct SearchHit {
    pub turn_id: String,
    pub snippet: String,
}

/// Which kind of `Journal/` file a row in `journal_entries` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalKind {
    Daily,
    Sync,
}

/// A row of the `devices` table: one paired edge device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub token: String,
    pub created_ts: i64,
    pub revoked: bool,
}

fn row_device(row: &Row<'_>) -> rusqlite::Result<DeviceRecord> {
    Ok(DeviceRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        token: row.get(2)?,
        created_ts: row.get(3)?,
        revoked: row.get::<_, i64>(4)? != 0,
    })
}

impl JournalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalKind::Daily => "daily",
            JournalKind::Sync => "sync",
        }
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> CoreResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> CoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inserts a turn. Returns `false` when its ID or `(device_id, seq)` key
    /// already exists; callers that distinguish replays from sequence
    /// collisions must check the ID.
    pub fn insert_turn(&self, turn: &Turn) -> CoreResult<bool> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;

        let id = turn.id.to_string();
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO turns (id, ts, kind, engine, model, device_id, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                turn.ts,
                turn.kind.as_str(),
                turn.engine.as_str(),
                turn.model,
                turn.device_id,
                turn.seq
            ],
        )?;

        if inserted == 0 {
            return Ok(false);
        }

        for (i, m) in turn.messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages (turn_id, ordinal, role, content, name, tool_calls, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    i as i64,
                    m.role.as_str(),
                    m.content,
                    m.name,
                    m.tool_calls,
                    m.tool_call_id
                ],
            )?;
            if let Some(text) = indexable_text(m) {
                tx.execute(
                    "INSERT INTO turns_fts (turn_id, body) VALUES (?1, ?2)",
                    params![id, text],
                )?;
            }
        }

        tx.commit()?;
        Ok(true)
    }

    /// Looks a single turn up by its primary key, messages included.
    pub fn turn_by_id(&self, id: &str) -> CoreResult<Option<Turn>> {
        let conn = self.lock();
        let rowid: Option<i64> = conn
            .query_row("SELECT rowid FROM turns WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        match rowid {
            Some(rowid) => turn_by_rowid(&conn, rowid),
            None => Ok(None),
        }
    }

    /// All turns in `[from_ts, to_ts)` ordered chronologically. Used by the
    /// journal job and the edge-window builder.
    pub fn turns_between(&self, from_ts: i64, to_ts: i64) -> CoreResult<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid FROM turns WHERE ts >= ?1 AND ts < ?2 ORDER BY ts ASC, rowid ASC",
        )?;
        let ids: Vec<i64> = stmt
            .query_map(params![from_ts, to_ts], |r| r.get::<_, i64>(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut out = Vec::new();
        for rowid in ids {
            if let Some(t) = turn_by_rowid(&conn, rowid)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// The `n` most recent turns, returned chronologically.
    pub fn latest_turns(&self, n: usize) -> CoreResult<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid FROM turns ORDER BY ts DESC, rowid DESC LIMIT ?1",
        )?;
        let rowids: Vec<i64> = stmt
            .query_map(params![n as i64], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut out = Vec::new();
        for rowid in rowids {
            if let Some(t) = turn_by_rowid(&conn, rowid)? {
                out.push(t);
            }
        }
        out.reverse();
        Ok(out)
    }

    /// The `n` most recent interactive turns (desktop chat and phone sync),
    /// returned chronologically. Journal/system records are not chat history.
    pub fn latest_conversation_turns(&self, n: usize) -> CoreResult<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid FROM turns
             WHERE kind IN ('chat', 'sync')
             ORDER BY ts DESC, rowid DESC LIMIT ?1",
        )?;
        let rowids: Vec<i64> = stmt
            .query_map(params![n as i64], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut out = Vec::new();
        for rowid in rowids {
            if let Some(t) = turn_by_rowid(&conn, rowid)? {
                out.push(t);
            }
        }
        out.reverse();
        Ok(out)
    }

    /// The `n` most recent turns visible to a device: all shared chats and
    /// only that device's private offline sync turns.
    pub fn latest_conversation_turns_for_device(
        &self,
        n: usize,
        device_id: &str,
    ) -> CoreResult<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid FROM turns
             WHERE kind = 'chat' OR (kind = 'sync' AND device_id = ?1)
             ORDER BY ts DESC, rowid DESC LIMIT ?2",
        )?;
        let rowids: Vec<i64> = stmt
            .query_map(params![device_id, n as i64], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut out = Vec::new();
        for rowid in rowids {
            if let Some(t) = turn_by_rowid(&conn, rowid)? {
                out.push(t);
            }
        }
        out.reverse();
        Ok(out)
    }

    pub fn all_turns(&self) -> CoreResult<Vec<Turn>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT rowid FROM turns ORDER BY ts ASC, rowid ASC")?;
        let rowids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        let mut out = Vec::new();
        for rowid in rowids {
            if let Some(t) = turn_by_rowid(&conn, rowid)? {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// Full-text search over message bodies. The FTS table is a rebuildable
    /// sidecar, never the source of truth.
    pub fn search(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT turn_id, snippet(turns_fts, 1, '[', ']', '...', 24)
             FROM turns_fts WHERE turns_fts MATCH ?1 LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![query, limit as i64], |r| {
                Ok(SearchHit {
                    turn_id: r.get(0)?,
                    snippet: r.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(hits)
    }

    pub fn get_meta(&self, key: &str) -> CoreResult<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> CoreResult<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Replaces a metadata value only when its current value matches `expected`.
    ///
    /// The comparison and write share the connection lock, so callers can
    /// implement conflict-safe read/modify/write without racing other requests.
    pub fn compare_and_set_meta(
        &self,
        key: &str,
        expected: Option<&str>,
        value: &str,
    ) -> CoreResult<bool> {
        let conn = self.lock();
        let current = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        if current.as_deref() != expected {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(true)
    }

    pub fn delete_meta(&self, key: &str) -> CoreResult<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Registers a paired device. Fails if the id or token already exists —
    /// re-pairing must revoke the old device first, not silently overwrite it.
    pub fn register_device(
        &self,
        id: &str,
        name: &str,
        token: &str,
        now_ts: i64,
    ) -> CoreResult<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO devices (id, name, token, created_ts) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, token, now_ts],
        )?;
        Ok(())
    }

    /// Looks a device up by bearer token, revoked ones included; callers must
    /// check [`DeviceRecord::revoked`] before admitting the request.
    pub fn device_by_token(&self, token: &str) -> CoreResult<Option<DeviceRecord>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, token, created_ts, revoked FROM devices WHERE token = ?1",
                params![token],
                row_device,
            )
            .optional()?)
    }

    pub fn list_devices(&self) -> CoreResult<Vec<DeviceRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, token, created_ts, revoked FROM devices ORDER BY created_ts ASC",
        )?;
        let rows = stmt
            .query_map([], row_device)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Marks a device revoked. Returns `false` when no such device exists.
    pub fn revoke_device(&self, id: &str) -> CoreResult<bool> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE devices SET revoked = 1 WHERE id = ?1",
            params![id],
        )? > 0)
    }

    /// Highest `seq` a device has acknowledged, so it can resume a partial
    /// outbox flush without re-sending anything.
    pub fn max_seq_for_device(&self, device_id: &str) -> CoreResult<i64> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE device_id = ?1",
                params![device_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn record_journal_entry(
        &self,
        day: &str,
        kind: JournalKind,
        file: &str,
    ) -> CoreResult<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO journal_entries (day, kind, title, body, created_ts, file)
             VALUES (?1, ?2, ?3, '', ?4, ?5)",
            params![
                day,
                kind.as_str(),
                day,
                chrono::Utc::now().timestamp_millis(),
                file
            ],
        )?;
        Ok(())
    }

    pub fn journal_entry_exists(&self, day: &str, kind: JournalKind) -> CoreResult<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE day = ?1 AND kind = ?2",
            params![day, kind.as_str()],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Deletes every turn (and, by cascade, every message), returning how many
    /// turns were removed.
    ///
    /// This is the "start over" lever: a run of garbage replies is otherwise
    /// permanent history that the sync window keeps re-installing on the phone.
    /// The FTS sidecar has no triggers — `insert_turn` populates it by hand — so
    /// it must be emptied in the same transaction, or search would keep hitting
    /// rows that no longer exist. Journal entries, device registrations and meta
    /// deliberately survive: they are history and pairing state, not context.
    pub fn delete_all_turns(&self) -> CoreResult<u64> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let turns = tx.execute("DELETE FROM turns", [])? as u64;
        tx.execute("DELETE FROM turns_fts", [])?;
        tx.commit()?;
        Ok(turns)
    }

    /// Deletes every turn belonging to one device, returning how many went.
    ///
    /// A paired phone may purge the turns it created while offline without being
    /// able to touch the rest of the household's transcript — the window it is
    /// offered is built from its own sync turns too, so a device that poisoned
    /// its own context can dig itself out.
    pub fn delete_turns_for_device(&self, device_id: &str) -> CoreResult<u64> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let turns = tx.execute("DELETE FROM turns WHERE device_id = ?1", params![device_id])? as u64;
        // The sidecar has no device column; rebuild it from what remains.
        tx.execute(
            "DELETE FROM turns_fts WHERE turn_id NOT IN (SELECT id FROM turns)",
            [],
        )?;
        tx.commit()?;
        Ok(turns)
    }

    /// Deletes exactly one turn by id, returning how many rows went (0 or 1).
    ///
    /// This is the surgical lever behind "remove that message from my context":
    /// the phone offers the whole exchange (prompt, reply, tool steps) as the
    /// unit of deletion because that is what the store and the sync protocol
    /// both use. Removing a single message row instead would leave a user
    /// message whose tool calls are answered by nothing — a shape the chat
    /// template rejects.
    ///
    /// Like [`Store::delete_turns_for_device`], the FTS sidecar has no triggers
    /// and is pruned in the same transaction. Messages go by cascade.
    pub fn delete_turn_by_id(&self, id: &str) -> CoreResult<u64> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let turns = tx.execute("DELETE FROM turns WHERE id = ?1", params![id])? as u64;
        if turns > 0 {
            tx.execute("DELETE FROM turns_fts WHERE turn_id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(turns)
    }

    /// Timestamp of the oldest stored turn — the starting point for a journal
    /// catch-up sweep.
    pub fn first_turn_ts(&self) -> CoreResult<Option<i64>> {
        let conn = self.lock();
        Ok(conn
            .query_row("SELECT MIN(ts) FROM turns", [], |r| r.get::<_, Option<i64>>(0))?
            .filter(|ts| *ts > 0))
    }
}

fn indexable_text(m: &Message) -> Option<String> {
    (!m.content.trim().is_empty()).then(|| m.content.clone())
}

fn row_to_turn(_conn: &Connection, row: &Row<'_>) -> rusqlite::Result<Turn> {
    let id: String = row.get("id")?;
    let turn_id: TurnId = uuid::Uuid::parse_str(&id).unwrap_or_else(|_| TurnId::now_v7());
    let kind = TurnKind::parse(&row.get::<_, String>("kind")?).unwrap_or(TurnKind::Chat);
    Ok(Turn {
        id: turn_id,
        ts: row.get("ts")?,
        kind,
        engine: EngineKind::parse(&row.get::<_, String>("engine")?),
        model: row.get("model")?,
        device_id: row.get("device_id")?,
        seq: row.get("seq")?,
        messages: Vec::new(),
    })
}

fn turn_by_rowid(conn: &Connection, rowid: i64) -> CoreResult<Option<Turn>> {
    let mut turn = conn
        .query_row(
            "SELECT id, ts, kind, engine, model, device_id, seq FROM turns WHERE rowid = ?1",
            params![rowid],
            |r| row_to_turn(conn, r),
        )
        .optional()?;

    if let Some(t) = turn.as_mut() {
        let mut stmt = conn.prepare(
            "SELECT role, content, name, tool_calls, tool_call_id FROM messages
             WHERE turn_id = ?1 ORDER BY ordinal ASC",
        )?;
        t.messages = stmt
            .query_map(params![t.id.to_string()], |r| {
                Ok(Message {
                    role: Role::parse(&r.get::<_, String>(0)?).unwrap_or(Role::User),
                    content: r.get(1)?,
                    name: r.get(2)?,
                    tool_calls: r.get(3)?,
                    tool_call_id: r.get(4)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();
    }
    Ok(turn)
}

impl From<CoreError> for std::io::Error {
    fn from(e: CoreError) -> Self {
        std::io::Error::other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EngineKind, TurnKind};

    fn turn(device: &str, seq: i64, text: &str) -> Turn {
        Turn::new(
            TurnKind::Chat,
            device,
            seq,
            vec![Message::user(text), Message::assistant("ok")],
        )
        .with_engine(EngineKind::Llama, "test-model")
    }

    #[test]
    fn round_trip_preserves_messages_and_order() {
        let store = Store::open_in_memory().unwrap();
        let t = Turn::new(
            TurnKind::Chat,
            "desktop",
            1,
            vec![
                Message::system("persona"),
                Message::user("hi"),
                Message::assistant(""),
                Message::tool("c1", "result"),
            ],
        )
        .with_engine(EngineKind::Llama, "test-model");
        assert!(store.insert_turn(&t).unwrap());

        let all = store.all_turns().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, t.id);
        assert_eq!(all[0].engine, EngineKind::Llama);
        assert_eq!(all[0].model, "test-model");
        assert_eq!(all[0].messages.len(), 4);
        assert_eq!(all[0].messages[0].role, Role::System);
        assert_eq!(all[0].messages[3].tool_call_id.as_deref(), Some("c1"));
        // The only assistant message is empty, so there is no final text.
        assert_eq!(all[0].final_text(), None);
        assert_eq!(all[0].user_text(), Some("hi"));
    }

    #[test]
    fn duplicate_device_seq_is_ignored_not_duplicated() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.insert_turn(&turn("pixel", 7, "first")).unwrap());
        // Same (device, seq) but a different UUID — a replayed outbox entry.
        assert!(!store.insert_turn(&turn("pixel", 7, "replay")).unwrap());
        assert_eq!(store.all_turns().unwrap().len(), 1);

        // A different device may reuse the same seq.
        assert!(store.insert_turn(&turn("tablet", 7, "other")).unwrap());
        assert_eq!(store.all_turns().unwrap().len(), 2);
    }

    #[test]
    fn device_registry_round_trip_and_revocation() {
        let store = Store::open_in_memory().unwrap();
        store.register_device("dev-1", "Galaxy Fold", "tok-1", 100).unwrap();
        store.register_device("dev-2", "Pixel", "tok-2", 200).unwrap();

        let found = store.device_by_token("tok-1").unwrap().unwrap();
        assert_eq!(found.id, "dev-1");
        assert_eq!(found.name, "Galaxy Fold");
        assert!(!found.revoked);
        assert!(store.device_by_token("nope").unwrap().is_none());

        assert!(store.revoke_device("dev-1").unwrap());
        assert!(!store.revoke_device("dev-9").unwrap(), "unknown id");
        let found = store.device_by_token("tok-1").unwrap().unwrap();
        assert!(found.revoked, "revocation must be visible to the auth path");

        let all = store.list_devices().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "dev-1", "ordered by creation");
    }

    #[test]
    fn device_registration_rejects_collisions() {
        let store = Store::open_in_memory().unwrap();
        store.register_device("dev-1", "Fold", "tok-1", 100).unwrap();
        assert!(store.register_device("dev-1", "other name", "tok-9", 100).is_err());
        assert!(store.register_device("dev-2", "other name", "tok-1", 100).is_err());
    }

    #[test]
    fn max_seq_lets_a_device_resume_a_partial_flush() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.max_seq_for_device("pixel").unwrap(), 0);
        store.insert_turn(&turn("pixel", 1, "a")).unwrap();
        store.insert_turn(&turn("pixel", 2, "b")).unwrap();
        store.insert_turn(&turn("pixel", 9, "c")).unwrap();
        assert_eq!(store.max_seq_for_device("pixel").unwrap(), 9);
        assert_eq!(store.max_seq_for_device("tablet").unwrap(), 0);
    }

    #[test]
    fn latest_turns_returns_chronological_order() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            let base = 1_753_000_000_000 + i * 1000;
            store
                .insert_turn(&turn("desktop", i, &format!("q{i}")).with_ts(base))
                .unwrap();
        }
        let latest = store.latest_turns(3).unwrap();
        let texts: Vec<&str> = latest.iter().filter_map(|t| t.user_text()).collect();
        assert_eq!(texts, vec!["q2", "q3", "q4"]);
    }

    #[test]
    fn turns_between_is_half_open() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..4i64 {
            store
                .insert_turn(&turn("desktop", i, &format!("q{i}")).with_ts(1000 + i * 100))
                .unwrap();
        }
        let window = store.turns_between(1100, 1300).unwrap();
        let texts: Vec<&str> = window.iter().filter_map(|t| t.user_text()).collect();
        assert_eq!(texts, vec!["q1", "q2"]);
    }

    #[test]
    fn full_text_search_finds_a_turn() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_turn(&turn("desktop", 1, "how do I repot a monstera"))
            .unwrap();
        store
            .insert_turn(&turn("desktop", 2, "what time is it"))
            .unwrap();
        let hits = store.search("monstera", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("monstera"));
    }

    #[test]
    fn delete_all_turns_empties_history_but_keeps_journal_and_devices() {
        let store = Store::open_in_memory().unwrap();
        store.register_device("phone", "Pixel", "tok", 1_000).unwrap();
        store.insert_turn(&turn("phone", 1, "monstera")).unwrap();
        store.insert_turn(&turn("desktop", 1, "monstera again")).unwrap();
        store
            .record_journal_entry("2026-07-29", JournalKind::Daily, "c:/j/2026-07-29.txt")
            .unwrap();
        store.set_meta("window_seq", "7").unwrap();

        assert_eq!(store.delete_all_turns().unwrap(), 2);

        assert!(store.all_turns().unwrap().is_empty());
        assert!(store.search("monstera", 10).unwrap().is_empty());
        assert_eq!(store.first_turn_ts().unwrap(), None);
        assert_eq!(store.max_seq_for_device("phone").unwrap(), 0);
        // Journal, pairing state and meta are not conversation context.
        assert!(store.journal_entry_exists("2026-07-29", JournalKind::Daily).unwrap());
        assert_eq!(store.get_meta("window_seq").unwrap().unwrap(), "7");
        assert!(store.device_by_token("tok").unwrap().is_some());

        // Idempotent: a second clear is a no-op, not an error.
        assert_eq!(store.delete_all_turns().unwrap(), 0);
    }

    #[test]
    fn delete_turn_by_id_removes_one_exchange_and_its_search_row() {
        let store = Store::open_in_memory().unwrap();
        let doomed = turn("phone", 1, "please forget the monstera");
        let keep = turn("phone", 2, "the monstera stays");
        let doomed_id = doomed.id.to_string();
        let keep_id = keep.id.to_string();
        store.insert_turn(&doomed).unwrap();
        store.insert_turn(&keep).unwrap();

        assert_eq!(store.delete_turn_by_id(&doomed_id).unwrap(), 1);

        assert!(store.turn_by_id(&doomed_id).unwrap().is_none());
        assert!(store.turn_by_id(&keep_id).unwrap().is_some());
        assert_eq!(store.all_turns().unwrap().len(), 1);
        // The sidecar has no triggers; a stale row would still hit search.
        let hits = store.search("monstera", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].turn_id, keep_id);
        // Cascade took the messages with it. Scoped so the store mutex is
        // released before the next call, which locks again.
        let orphans = {
            let conn = store.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE turn_id = ?1",
                params![doomed_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(orphans, 0);

        // Deleting the same id twice, or an unknown id, is a no-op.
        assert_eq!(store.delete_turn_by_id(&doomed_id).unwrap(), 0);
        assert_eq!(store.delete_turn_by_id("not-a-turn").unwrap(), 0);
    }

    #[test]
    fn delete_turns_for_device_only_touches_that_device() {
        let store = Store::open_in_memory().unwrap();
        store.insert_turn(&turn("pixel-7", 1, "my garbage")).unwrap();
        store.insert_turn(&turn("pixel-7", 2, "more garbage")).unwrap();
        store.insert_turn(&turn("desktop", 1, "the good stuff")).unwrap();

        assert_eq!(store.delete_turns_for_device("pixel-7").unwrap(), 2);

        let left = store.all_turns().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].device_id, "desktop");
        // The FTS sidecar must not keep rows for turns that are gone.
        assert!(store.search("garbage", 10).unwrap().is_empty());
        assert_eq!(store.search("good", 10).unwrap().len(), 1);
        assert_eq!(store.max_seq_for_device("pixel-7").unwrap(), 0);
        assert_eq!(store.max_seq_for_device("desktop").unwrap(), 1);
    }

    #[test]
    fn meta_round_trips() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_meta("window_seq").unwrap(), None);
        store.set_meta("window_seq", "42").unwrap();
        store.set_meta("window_seq", "43").unwrap();
        assert_eq!(store.get_meta("window_seq").unwrap().unwrap(), "43");
    }

    #[test]
    fn compare_and_set_meta_rejects_stale_values() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.compare_and_set_meta("personality", None, "first").unwrap());
        assert!(!store
            .compare_and_set_meta("personality", None, "stale")
            .unwrap());
        assert_eq!(
            store.get_meta("personality").unwrap().as_deref(),
            Some("first")
        );
    }

    #[test]
    fn first_turn_ts_is_the_oldest_row() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.first_turn_ts().unwrap(), None);
        store
            .insert_turn(&turn("desktop", 1, "old").with_ts(500))
            .unwrap();
        store
            .insert_turn(&turn("desktop", 2, "new").with_ts(900))
            .unwrap();
        assert_eq!(store.first_turn_ts().unwrap(), Some(500));
    }

    #[test]
    fn journal_ledger_tracks_kind_and_day() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.journal_entry_exists("2026-07-29", JournalKind::Daily).unwrap());
        store
            .record_journal_entry("2026-07-29", JournalKind::Daily, "c:/j/2026-07-29.txt")
            .unwrap();
        assert!(store.journal_entry_exists("2026-07-29", JournalKind::Daily).unwrap());
        assert!(!store.journal_entry_exists("2026-07-29", JournalKind::Sync).unwrap());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("etern.db");
        {
            let store = Store::open(&path).unwrap();
            store.insert_turn(&turn("desktop", 1, "durable")).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.all_turns().unwrap().len(), 1);
    }
}
