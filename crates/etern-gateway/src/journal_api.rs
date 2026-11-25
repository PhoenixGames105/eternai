//! Read-only journal browsing for the desktop UI (PLAN §5, M5).
//!
//! The journal is a plain folder of text digests that the model and the user
//! both edit, so the API is deliberately thin: list the files, open one. It
//! reads the directory rather than the `journal_entries` table so a file the
//! user dropped in by hand (or one written before the store existed) still
//! shows up — the folder is the source of truth for browsing, the table is a
//! projection for search.

use std::path::{Component, Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::auth;
use crate::AppState;

/// Refuse to stream a journal file larger than this into the UI. Day digests
/// are a few KB; anything past this is a mistake or an attack.
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;

/// Classifies a journal filename.
fn kind_of(name: &str) -> &'static str {
    let stem = name.strip_suffix(".txt").unwrap_or(name);
    if stem.starts_with("sync-") {
        "sync"
    } else if is_day_stem(stem) {
        "daily"
    } else {
        "other"
    }
}

/// `YYYY-MM-DD`, checked structurally so a malformed date is not a parse error.
fn is_day_stem(stem: &str) -> bool {
    let b = stem.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// Lists `*.txt` digests newest-first.
///
/// Sorted by name descending rather than mtime: the names are timestamps, so
/// this is stable across a file sync that rewrites mtimes.
async fn list(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let dir = state.journal.dir().clone();
    let dir_for_scan = dir.clone();
    let entries = tokio::task::spawn_blocking(move || scan(&dir_for_scan)).await;
    match entries {
        Ok(files) => Json(json!({ "dir": dir.display().to_string(), "count": files.len(), "files": files })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("journal listing failed: {e}")}})),
        )
            .into_response(),
    }
}

fn scan(dir: &Path) -> Vec<Value> {
    let mut files: Vec<(String, Value)> = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let meta = entry.metadata().ok();
        let modified_ms = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        files.push((
            name.clone(),
            json!({ "name": name, "kind": kind_of(&name), "size_bytes": size, "modified_ms": modified_ms }),
        ));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().map(|(_, v)| v).collect()
}

/// Opens one journal file by name.
async fn read(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if let Err(err) = auth::check_any(&state.cfg, &state.store, &headers) {
        return err.into();
    }

    let safe = match safe_name(&name) {
        Ok(n) => n,
        Err(reason) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": reason, "type": "invalid_journal_name"}})),
            )
                .into_response()
        }
    };

    let dir = state.journal.dir().clone();
    let safe_for_move = safe.clone();
    let result = tokio::task::spawn_blocking(move || read_file(&dir, &safe_for_move)).await;
    match result {
        Ok(Ok(body)) => Json(json!({ "name": safe, "kind": kind_of(&safe), "content": body })).into_response(),
        Ok(Err(ReadError::NotFound)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"message": "journal entry not found", "type": "not_found"}})),
        )
            .into_response(),
        Ok(Err(ReadError::TooLarge(len))) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": {"message": format!("journal entry too large ({len} bytes)"), "type": "too_large"}})),
        )
            .into_response(),
        Ok(Err(ReadError::Io(e))) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("cannot read journal entry: {e}")}})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("journal read task failed: {e}")}})),
        )
            .into_response(),
    }
}

enum ReadError {
    NotFound,
    TooLarge(u64),
    Io(std::io::Error),
}

fn read_file(dir: &Path, name: &str) -> Result<String, ReadError> {
    let path: PathBuf = dir.join(name);
    let meta = std::fs::metadata(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound,
        _ => ReadError::Io(e),
    })?;
    if !meta.is_file() {
        return Err(ReadError::NotFound);
    }
    if meta.len() > MAX_JOURNAL_BYTES {
        return Err(ReadError::TooLarge(meta.len()));
    }
    // Journal digests are UTF-8 text we wrote ourselves; a hand-edited file in
    // another encoding should fail loudly rather than return mojibake.
    let bytes = std::fs::read(&path).map_err(ReadError::Io)?;
    String::from_utf8(bytes).map_err(|e| ReadError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

/// Accepts only a bare `*.txt` filename — no separators, traversal, drive
/// letters, or alternate data streams.
fn safe_name(raw: &str) -> Result<String, &'static str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("empty journal entry name");
    }
    if !name.ends_with(".txt") {
        return Err("journal entries are .txt files");
    }
    // Exactly one normal component rejects `..`, `a/b`, `\`, absolute paths,
    // drive prefixes and UNC shares without a per-OS special case.
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err("journal entry name must be a bare filename"),
    }
    if name.contains(':') || name.contains("..") {
        return Err("journal entry name must be a bare filename");
    }
    Ok(name.to_string())
}

pub fn routes() -> axum::Router<std::sync::Arc<AppState>> {
    axum::Router::new()
        .route("/etern/journal", axum::routing::get(list))
        .route("/etern/journal/{name}", axum::routing::get(read))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("etern-journal-api-{}-{}-{}", tag, std::process::id(), n))
    }

    async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn app_with(state: std::sync::Arc<AppState>) -> axum::Router {
        routes().with_state(state)
    }

    #[test]
    fn classifies_journal_kinds() {
        assert_eq!(kind_of("2026-11-19.txt"), "daily");
        assert_eq!(kind_of("sync-20261119-013000.txt"), "sync");
        assert_eq!(kind_of("scratch.txt"), "other");
        assert!(!is_day_stem("2026-1-19"));
        assert!(!is_day_stem("202611190"));
    }

    #[test]
    fn safe_name_accepts_only_bare_txt_files() {
        assert!(safe_name("2026-11-19.txt").is_ok());
        assert!(safe_name("sync-20261119-013000.txt").is_ok());
        assert!(safe_name("notes.md").is_err());
        assert!(safe_name("").is_err());
        assert!(safe_name("../secret.txt").is_err());
        assert!(safe_name("a/b.txt").is_err());
        assert!(safe_name(r"C:\Windows\x.txt").is_err());
        assert!(safe_name(r"\\server\share\x.txt").is_err());
        assert!(safe_name("sub\\dir.txt").is_err());
        assert!(safe_name(".hidden.txt").is_ok());
    }

    #[tokio::test]
    async fn lists_journal_files_newest_first() {
        let dir = temp_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-11-18.txt"), "older").unwrap();
        std::fs::write(dir.join("2026-11-19.txt"), "newer").unwrap();
        std::fs::write(dir.join("sync-20261119-010000.txt"), "sync").unwrap();
        std::fs::write(dir.join("ignore.md"), "not a journal file").unwrap();

        let state = crate::tests_support::test_state_with_journal(None, &dir);
        let (status, body) = get(app_with(state), "/etern/journal").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"].as_u64(), Some(3));
        let files = body["files"].as_array().unwrap();
        assert_eq!(files[0]["name"], "sync-20261119-010000.txt");
        assert_eq!(files[1]["name"], "2026-11-19.txt");
        assert_eq!(files[1]["kind"], "daily");
        assert_eq!(files[2]["name"], "2026-11-18.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reads_one_journal_file() {
        let dir = temp_dir("read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-11-19.txt"), "today we talked about rust").unwrap();

        let state = crate::tests_support::test_state_with_journal(None, &dir);
        let (status, body) = get(app_with(state), "/etern/journal/2026-11-19.txt").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["kind"], "daily");
        assert_eq!(body["content"], "today we talked about rust");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_404() {
        let dir = temp_dir("missing");
        std::fs::create_dir_all(&dir).unwrap();
        let state = crate::tests_support::test_state_with_journal(None, &dir);
        let (status, _) = get(app_with(state), "/etern/journal/2020-01-01.txt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn traversal_is_rejected_before_any_io() {
        let dir = temp_dir("trav");
        std::fs::create_dir_all(&dir).unwrap();
        // A file that exists *outside* the journal must never be readable.
        std::fs::write(dir.join("outside.txt"), "secret").unwrap();
        let state = crate::tests_support::test_state_with_journal(None, &dir);
        let (status, body) = get(app_with(state), "/etern/journal/..%2Foutside.txt").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_journal_name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn journal_endpoints_require_the_token() {
        let dir = temp_dir("auth");
        std::fs::create_dir_all(&dir).unwrap();
        let state = crate::tests_support::test_state_with_journal(Some("s3cret"), &dir);
        let (status, _) = get(app_with(state.clone()), "/etern/journal").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = get(app_with(state), "/etern/journal/2026-11-19.txt").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
