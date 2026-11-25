//! Sync wire format and the edge device's durable outbox (PLAN §4a).
//!
//! The phone appends every exchange to an append-only JSONL outbox
//! *before* showing the reply, so a crash cannot lose a turn. On reconnect it
//! pushes batches in `(device_id, seq)` order; the desktop dedupes on the
//! store's unique sync identity, which makes replays free. Turns carry an
//! `offline` marker so only edge-model exchanges produce reconnection journals.
//!
//! The outbox lives here in the shared core (not in the Android app) so the
//! UniFFI bindings and the desktop tests exercise the exact same durability
//! code — a divergent outbox is data loss on the phone.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{EngineKind, Message, Turn, TurnId, TurnKind};
use crate::error::{CoreError, CoreResult};

/// One phone turn as carried over the wire.
///
/// `turn_id` is a UUIDv7 minted on the phone; it is the idempotency key the
/// desktop dedupes on. `seq` is the device-local monotonic order used to
/// replay in sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTurn {
    pub turn_id: String,
    /// Unix ms of the *start* of the turn, from the phone's clock.
    pub ts: i64,
    pub seq: i64,
    /// True when this turn was generated locally while the desktop was offline.
    /// Older outbox records predate the field and are conservatively offline.
    #[serde(default = "default_offline")]
    pub offline: bool,
    pub messages: Vec<Message>,
}

fn default_offline() -> bool {
    true
}

impl SyncTurn {
    pub fn from_turn(turn: &Turn) -> Self {
        Self {
            turn_id: turn.id.to_string(),
            ts: turn.ts,
            seq: turn.seq,
            offline: turn.engine == EngineKind::Edge,
            messages: turn.messages.clone(),
        }
    }

    /// Materialise as a canonical store turn attributed to `device_id`.
    ///
    /// The kind is [`TurnKind::Sync`] so device-scoped sync history remains
    /// identifiable; `offline` distinguishes edge-generated turns for journaling.
    pub fn to_turn(&self, device_id: &str, model: &str) -> Result<Turn, String> {
        let id = TurnId::parse_str(&self.turn_id).map_err(|e| format!("bad turn_id: {e}"))?;
        if self.messages.is_empty() {
            return Err("turn has no messages".to_string());
        }
        Ok(Turn {
            id,
            ts: self.ts,
            kind: TurnKind::Sync,
            engine: if self.offline {
                EngineKind::Edge
            } else {
                EngineKind::Llama
            },
            model: if self.offline {
                model.to_string()
            } else {
                "desktop".to_string()
            },
            device_id: device_id.to_string(),
            seq: self.seq,
            messages: self.messages.clone(),
        })
    }
}

/// A batch push from one device. Turns must be chronological by `seq`; the
/// gateway does not re-sort them, so a device that pushes out of order gets
/// its turns stored out of order — its problem, and visible in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBatch {
    pub device_id: String,
    pub turns: Vec<SyncTurn>,
}

/// Result of a push. `accepted` counts newly stored turns, `duplicates` counts
/// turns absorbed by the idempotency key (replays). `journal` is the digest
/// for newly accepted offline turns, if any.
#[derive(Debug, Clone, Serialize)]
pub struct PushAck {
    pub accepted: usize,
    pub duplicates: usize,
    pub journal: Option<String>,
}

/// Cap on one push so a hostile or buggy client cannot pin the gateway in a
/// giant transaction. 2000 turns is far beyond a day of chatting.
pub const MAX_SYNC_BATCH: usize = 2000;

/// Durable, append-only outbox: one JSON [`SyncTurn`] per line.
///
/// Durability model (phone-grade): every append is written and flushed to the
/// OS before returning; rotation renames the full file to `<name>.old` rather
/// than deleting, so a rotation during a crash still loses nothing that was
/// already flushed.
pub struct Outbox {
    path: PathBuf,
    rotate_bytes: u64,
}

/// Rotate at ~500 MB per PLAN §4a.
pub const DEFAULT_ROTATE_BYTES: u64 = 500 * 1024 * 1024;

impl Outbox {
    pub fn new(path: impl AsRef<Path>) -> CoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::io(parent.display().to_string(), e))?;
        }
        Ok(Self {
            path,
            rotate_bytes: DEFAULT_ROTATE_BYTES,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// For tests: rotate at a tiny size.
    pub fn with_rotate_bytes(mut self, bytes: u64) -> Self {
        self.rotate_bytes = bytes;
        self
    }

    /// Appends one turn. The line is flushed before returning.
    pub fn append(&self, turn: &SyncTurn) -> CoreResult<()> {
        let line = serde_json::to_string(turn)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        writeln!(file, "{line}").map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        file.flush()
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        drop(file);

        if self.len_bytes()? > self.rotate_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    /// Reads every turn back. Lines that fail to parse are *skipped and
    /// counted* — a torn final line from a crash must not wedge the whole
    /// outbox shut. The skipped count lets the caller alert.
    pub fn read_all(&self) -> CoreResult<(Vec<SyncTurn>, usize)> {
        let mut turns = Vec::new();
        let mut skipped = 0usize;
        if !self.path.exists() {
            return Ok((turns, skipped));
        }
        let file = File::open(&self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SyncTurn>(&line) {
                Ok(t) => turns.push(t),
                Err(_) => skipped += 1,
            }
        }
        Ok((turns, skipped))
    }

    /// Number of bytes currently in the live outbox file.
    pub fn len_bytes(&self) -> CoreResult<u64> {
        match std::fs::metadata(&self.path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(CoreError::io(self.path.display().to_string(), e)),
        }
    }

    /// Rename the full file to `<name>.old`, replacing any previous `.old`.
    fn rotate(&self) -> CoreResult<()> {
        let old = self.path.with_extension("old");
        if old.exists() {
            std::fs::remove_file(&old)
                .map_err(|e| CoreError::io(old.display().to_string(), e))?;
        }
        std::fs::rename(&self.path, &old)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        Ok(())
    }

    /// Discard the outbox after a successful push has been acked.
    pub fn clear(&self) -> CoreResult<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::io(self.path.display().to_string(), e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Message;

    fn sync_turn(seq: i64, text: &str) -> SyncTurn {
        SyncTurn {
            turn_id: TurnId::now_v7().to_string(),
            ts: 1_700_000_000_000 + seq,
            seq,
            offline: true,
            messages: vec![Message::user(text), Message::assistant("ok")],
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("etern-sync-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn wire_round_trip_preserves_identity_and_messages() {
        let turn = Turn::new(TurnKind::Chat, "phone", 7, vec![Message::user("hi")])
            .with_engine(EngineKind::Edge, "qwen-edge");
        let wire = SyncTurn::from_turn(&turn);
        let back = wire.to_turn("phone", "qwen-edge").unwrap();
        assert_eq!(back.id, turn.id);
        assert_eq!(back.seq, 7);
        assert_eq!(back.kind, TurnKind::Sync);
        assert_eq!(back.engine, EngineKind::Edge);
        assert_eq!(back.model, "qwen-edge");
        assert_eq!(back.messages, turn.messages);
    }

    #[test]
    fn outbox_defaults_legacy_records_to_offline_and_preserves_online_marker() {
        let legacy = serde_json::json!({
            "turn_id": TurnId::now_v7().to_string(),
            "ts": 1,
            "seq": 0,
            "messages": [{"role":"user","content":"legacy"}]
        });
        let old: SyncTurn = serde_json::from_value(legacy).unwrap();
        assert!(old.offline);

        let online_turn = Turn::new(TurnKind::Chat, "phone", 1, vec![Message::user("online")])
            .with_engine(EngineKind::Llama, "desktop-model");
        let online_wire = SyncTurn::from_turn(&online_turn);
        assert!(!online_wire.offline);
        let stored = online_wire.to_turn("phone", "edge").unwrap();
        assert_eq!(stored.engine, EngineKind::Llama);
        assert_eq!(stored.model, "desktop");
    }

    #[test]
    fn to_turn_rejects_garbage_ids_and_empty_messages() {
        let bad_id = SyncTurn {
            turn_id: "not-a-uuid".into(),
            ts: 0,
            seq: 0,
            offline: true,
            messages: vec![Message::user("x")],
        };
        assert!(bad_id.to_turn("d", "m").is_err());

        let empty = SyncTurn {
            turn_id: TurnId::now_v7().to_string(),
            ts: 0,
            seq: 0,
            offline: true,
            messages: vec![],
        };
        assert!(empty.to_turn("d", "m").is_err());
    }

    #[test]
    fn outbox_appends_and_reads_back_in_order() {
        let dir = tempdir("append");
        let ob = Outbox::new(dir.join("outbox.jsonl")).unwrap();
        ob.append(&sync_turn(1, "first")).unwrap();
        ob.append(&sync_turn(2, "second")).unwrap();
        let (turns, skipped) = ob.read_all().unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].seq, 1);
        assert_eq!(turns[1].seq, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outbox_survives_a_torn_final_line() {
        let dir = tempdir("torn");
        let path = dir.join("outbox.jsonl");
        let ob = Outbox::new(&path).unwrap();
        ob.append(&sync_turn(1, "kept")).unwrap();
        // Simulate a crash mid-write: a partial JSON line at the end.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"turn_id\":\"deadbeef")
            .unwrap();
        let (turns, skipped) = ob.read_all().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(skipped, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outbox_rotates_at_the_threshold() {
        let dir = tempdir("rotate");
        let path = dir.join("outbox.jsonl");
        let ob = Outbox::new(&path).unwrap().with_rotate_bytes(200);
        for i in 1..=20 {
            ob.append(&sync_turn(i, "a somewhat long message to burn bytes")).unwrap();
        }
        // Live file was rotated away and restarted; the .old holds history.
        assert!(dir.join("outbox.old").exists());
        let (turns, _) = ob.read_all().unwrap();
        assert!(turns.len() < 20, "live file restarted after rotation");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outbox_clear_is_idempotent() {
        let dir = tempdir("clear");
        let ob = Outbox::new(dir.join("outbox.jsonl")).unwrap();
        ob.clear().unwrap(); // never existed
        ob.append(&sync_turn(1, "x")).unwrap();
        ob.clear().unwrap();
        let (turns, _) = ob.read_all().unwrap();
        assert!(turns.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
