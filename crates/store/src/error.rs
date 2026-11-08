use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    /// Another process owns the session.
    Owned {
        session_id: String,
    },
    NotFound {
        session_id: String,
    },
    AlreadyExists {
        session_id: String,
    },
    /// A save's suffix does not start inside the stored sequence.
    InvalidSuffix {
        sequence: &'static str,
        start: u64,
        len: u64,
    },
    Cancelled,
    Integrity(String),
    UnsupportedSchema {
        found: i32,
        expected: i32,
    },
}

impl StoreError {
    /// Busy or locked databases and interrupted I/O can succeed when retried.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ),
            Self::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ),
            _ => false,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Sqlite(_) => "sqlite",
            Self::Json(_) => "json",
            Self::Owned { .. } => "owned",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyExists { .. } => "already_exists",
            Self::InvalidSuffix { .. } => "invalid_suffix",
            Self::Cancelled => "cancelled",
            Self::Integrity(_) => "integrity",
            Self::UnsupportedSchema { .. } => "unsupported_schema",
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Sqlite(err) => write!(f, "sqlite error: {err}"),
            Self::Json(err) => write!(f, "json error: {err}"),
            Self::Owned { session_id } => {
                write!(f, "session {session_id} is open in another smelt process")
            }
            Self::NotFound { session_id } => write!(f, "session not found: {session_id}"),
            Self::AlreadyExists { session_id } => {
                write!(f, "session already exists: {session_id}")
            }
            Self::InvalidSuffix {
                sequence,
                start,
                len,
            } => write!(
                f,
                "{sequence} suffix starts at {start} beyond the stored length {len}"
            ),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Integrity(message) => write!(f, "integrity error: {message}"),
            Self::UnsupportedSchema { found, expected } if found > expected => write!(
                f,
                "the session database has schema {found}, newer than this build supports ({expected}); update smelt"
            ),
            Self::UnsupportedSchema { .. } => f.write_str(
                "the session database was not written by this smelt release; move it aside to start with an empty one"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<rusqlite::types::FromSqlError> for StoreError {
    fn from(err: rusqlite::types::FromSqlError) -> Self {
        Self::Sqlite(err.into())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub(crate) fn to_sql_error(err: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(err))
}
