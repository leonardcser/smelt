use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Busy {
        operation: &'static str,
        attempts: u32,
        waited_ms: u64,
    },
    TransactionCleanup {
        operation: &'static str,
        message: String,
    },
    OperationCleanup {
        operation: &'static str,
        primary: Box<StoreError>,
        cleanup: Vec<StoreError>,
    },
    OwnershipConflict {
        owner: Option<String>,
    },
    OwnershipLost,
    Cancelled,
    MissingObject {
        reference: String,
    },
    ObjectTooLarge {
        size: u64,
        max: u64,
    },
    Integrity(String),
    UnsupportedSchema {
        found: i32,
        expected: i32,
    },
}

impl StoreError {
    pub fn is_recoverable_catalog_corruption(&self) -> bool {
        matches!(self, Self::UnsupportedSchema { .. } | Self::Integrity(_))
            || matches!(
                self,
                Self::Sqlite(rusqlite::Error::SqliteFailure(error, _))
                    if matches!(
                        error.code,
                        rusqlite::ErrorCode::DatabaseCorrupt
                            | rusqlite::ErrorCode::NotADatabase
                    )
            )
    }

    pub fn invalidates_connection(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(_) | Self::TransactionCleanup { .. } | Self::OperationCleanup { .. }
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(err) => write!(f, "io error: {err}"),
            StoreError::Sqlite(err) => write!(f, "sqlite error: {err}"),
            StoreError::Json(err) => write!(f, "json error: {err}"),
            StoreError::Busy {
                operation,
                attempts,
                waited_ms,
            } => write!(
                f,
                "database busy during {operation} after {attempts} attempts over {waited_ms}ms"
            ),
            StoreError::TransactionCleanup { operation, message } => {
                write!(
                    f,
                    "transaction cleanup failed during {operation}: {message}"
                )
            }
            StoreError::OperationCleanup {
                operation,
                primary,
                cleanup,
            } => {
                write!(f, "{operation} failed: {primary}")?;
                for error in cleanup {
                    write!(f, "; cleanup also failed: {error}")?;
                }
                Ok(())
            }
            StoreError::OwnershipConflict { owner } => match owner {
                Some(owner) => write!(f, "session is owned by another writer: {owner}"),
                None => f.write_str("session is owned by another writer"),
            },
            StoreError::OwnershipLost => f.write_str("session writer ownership was lost"),
            StoreError::Cancelled => f.write_str("operation cancelled"),
            StoreError::MissingObject { reference } => {
                write!(f, "session object is missing: {reference}")
            }
            StoreError::ObjectTooLarge { size, max } => {
                write!(f, "session object is too large: {size} bytes exceeds {max}")
            }
            StoreError::Integrity(message) => write!(f, "integrity error: {message}"),
            StoreError::UnsupportedSchema { found, expected } => {
                write!(f, "unsupported schema version {found}; expected {expected}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        StoreError::Io(err)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        StoreError::Sqlite(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        StoreError::Json(err)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub(crate) fn to_sql_error(err: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(err))
}
