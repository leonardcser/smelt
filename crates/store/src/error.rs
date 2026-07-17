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
    OwnershipConflict {
        owner: Option<String>,
    },
    OwnershipLost,
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
    pub fn is_database_locked(&self) -> bool {
        matches!(self, StoreError::Busy { .. })
            || matches!(
                self,
                StoreError::Sqlite(rusqlite::Error::SqliteFailure(err, _))
                    if matches!(
                        err.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    )
            )
    }

    pub fn invalidates_connection(&self) -> bool {
        matches!(self, Self::Sqlite(_) | Self::TransactionCleanup { .. })
    }

    pub fn session_persistence_disposition(&self) -> crate::SessionPersistenceDisposition {
        use crate::SessionPersistenceDisposition::{OwnershipLost, ReadOnly, Reopen, Retry};

        match self {
            Self::Busy { .. } => Retry,
            Self::Io(err) => io_persistence_disposition(err.kind()),
            Self::Sqlite(err) => sqlite_persistence_disposition(err),
            Self::OwnershipConflict { .. } | Self::OwnershipLost => OwnershipLost,
            Self::TransactionCleanup { .. } => Reopen,
            Self::Json(_)
            | Self::MissingObject { .. }
            | Self::ObjectTooLarge { .. }
            | Self::Integrity(_)
            | Self::UnsupportedSchema { .. } => ReadOnly,
        }
    }
}

fn io_persistence_disposition(kind: std::io::ErrorKind) -> crate::SessionPersistenceDisposition {
    use crate::SessionPersistenceDisposition::{ReadOnly, Reopen, Retry};
    use std::io::ErrorKind;

    match kind {
        ErrorKind::Interrupted
        | ErrorKind::WouldBlock
        | ErrorKind::TimedOut
        | ErrorKind::ResourceBusy
        | ErrorKind::Deadlock
        | ErrorKind::StaleNetworkFileHandle
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::HostUnreachable
        | ErrorKind::NetworkUnreachable
        | ErrorKind::NetworkDown => Retry,
        ErrorKind::PermissionDenied
        | ErrorKind::ReadOnlyFilesystem
        | ErrorKind::InvalidInput
        | ErrorKind::InvalidData => ReadOnly,
        _ => Reopen,
    }
}

fn sqlite_persistence_disposition(err: &rusqlite::Error) -> crate::SessionPersistenceDisposition {
    use crate::SessionPersistenceDisposition::{ReadOnly, Reopen, Retry};
    use rusqlite::ErrorCode;

    let rusqlite::Error::SqliteFailure(err, _) = err else {
        return ReadOnly;
    };
    match err.code {
        ErrorCode::DatabaseBusy
        | ErrorCode::DatabaseLocked
        | ErrorCode::OperationInterrupted
        | ErrorCode::FileLockingProtocolFailed
        | ErrorCode::SchemaChanged => Retry,
        ErrorCode::PermissionDenied
        | ErrorCode::ReadOnly
        | ErrorCode::DatabaseCorrupt
        | ErrorCode::NotADatabase
        | ErrorCode::ConstraintViolation
        | ErrorCode::ApiMisuse => ReadOnly,
        _ => Reopen,
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
            StoreError::OwnershipConflict { owner } => match owner {
                Some(owner) => write!(f, "session is owned by another writer: {owner}"),
                None => f.write_str("session is owned by another writer"),
            },
            StoreError::OwnershipLost => f.write_str("session writer ownership was lost"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionPersistenceDisposition::{ReadOnly, Reopen, Retry};

    fn sqlite_failure(result_code: i32) -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(result_code),
            None,
        ))
    }

    #[test]
    fn io_persistence_disposition_only_retries_known_transient_kinds() {
        assert_eq!(
            StoreError::Io(std::io::Error::from(std::io::ErrorKind::WouldBlock))
                .session_persistence_disposition(),
            Retry
        );
        assert_eq!(
            StoreError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .session_persistence_disposition(),
            ReadOnly
        );
        assert_eq!(
            StoreError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull))
                .session_persistence_disposition(),
            Reopen
        );
        assert_eq!(
            StoreError::Io(std::io::Error::from(std::io::ErrorKind::NotADirectory))
                .session_persistence_disposition(),
            Reopen
        );
    }

    #[test]
    fn sqlite_persistence_disposition_uses_structured_result_codes() {
        assert_eq!(
            sqlite_failure(rusqlite::ffi::SQLITE_BUSY).session_persistence_disposition(),
            Retry
        );
        assert_eq!(
            sqlite_failure(rusqlite::ffi::SQLITE_READONLY).session_persistence_disposition(),
            ReadOnly
        );
        assert_eq!(
            sqlite_failure(rusqlite::ffi::SQLITE_CORRUPT).session_persistence_disposition(),
            ReadOnly
        );
        assert_eq!(
            sqlite_failure(rusqlite::ffi::SQLITE_FULL).session_persistence_disposition(),
            Reopen
        );
        assert_eq!(
            sqlite_failure(rusqlite::ffi::SQLITE_CANTOPEN).session_persistence_disposition(),
            Reopen
        );
    }
}
