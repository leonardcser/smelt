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
    JournalRecovery {
        failure: Box<crate::SessionCommitFailure>,
    },
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
    pub fn is_recoverable_derived_corruption(&self) -> bool {
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
        ) || matches!(self, Self::JournalRecovery { failure } if failure.invalidates_connection())
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
            StoreError::JournalRecovery { failure } => {
                write!(f, "session journal recovery failed: {failure:?}")
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_recovery_rejects_operational_failures() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_READONLY,
            rusqlite::ffi::SQLITE_PERM,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_CANTOPEN,
            rusqlite::ffi::SQLITE_AUTH,
            rusqlite::ffi::SQLITE_ERROR,
        ] {
            let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(!error.is_recoverable_derived_corruption(), "{error:?}");
        }
        assert!(
            !StoreError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .is_recoverable_derived_corruption()
        );
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(error.is_recoverable_derived_corruption(), "{error:?}");
        }
    }

    #[test]
    fn journal_recovery_failure_preserves_structure_and_connection_disposition() {
        for failure in [
            crate::SessionCommitFailure::Busy {
                operation: "journal commit".into(),
                attempts: 3,
                waited_ms: 10,
            },
            crate::SessionCommitFailure::Sqlite {
                message: "fixture failure".into(),
            },
            crate::SessionCommitFailure::Integrity {
                message: "fixture corruption".into(),
            },
        ] {
            let error = StoreError::JournalRecovery {
                failure: Box::new(failure.clone()),
            };
            assert_eq!(
                error.invalidates_connection(),
                failure.invalidates_connection()
            );
            assert!(!error.is_recoverable_derived_corruption());
            assert!(error
                .to_string()
                .starts_with("session journal recovery failed:"));
            assert_eq!(
                crate::session_command::commit_failure_from_store_error(error),
                failure
            );
        }
    }
}
