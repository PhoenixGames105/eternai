use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cannot represent timestamp {0} as a local datetime")]
    Timestamp(i64),

    #[error("journal entry {0} has no turns to summarise")]
    EmptyJournal(String),

    #[error("token budget invalid: {0}")]
    Budget(String),
}

impl CoreError {
    /// Annotates an IO failure with the path it concerned.
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        CoreError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Lets `?` propagate raw IO errors; use [`CoreError::io`] where the path is
/// known so the message is actionable.
impl From<std::io::Error> for CoreError {
    fn from(source: std::io::Error) -> Self {
        CoreError::Io {
            path: String::new(),
            source,
        }
    }
}
