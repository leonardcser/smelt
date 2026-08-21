use std::fmt;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStoreError {
    InvalidSessionId {
        value: String,
        message: String,
    },
    SessionNotFound {
        id: String,
    },
    AmbiguousPrefix {
        prefix: String,
        matches: usize,
    },
    MissingDatabase {
        id: String,
    },
    InvalidListQuery {
        message: String,
    },
    CatalogUnavailable {
        kind: String,
        summary: String,
    },
    SymlinkNotAllowed {
        operation: &'static str,
        path: String,
    },
    ReadOnlyOwnerConflict {
        owner: String,
    },
    Busy {
        operation: String,
        attempts: u32,
        waited_ms: u64,
    },
    Io {
        operation: &'static str,
        path: String,
        message: String,
    },
    UnsupportedSchema {
        found: i32,
        supported: i32,
    },
    MissingObject {
        reference: String,
    },
    ObjectTooLarge {
        size: u64,
        max: u64,
    },
    Corrupt {
        context: String,
    },
    Sqlite {
        operation: &'static str,
        path: String,
        message: String,
    },
}

impl SessionStoreError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidSessionId { .. } => "invalid_session_id",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::AmbiguousPrefix { .. } => "ambiguous_prefix",
            Self::MissingDatabase { .. } => "missing_database",
            Self::InvalidListQuery { .. } => "invalid_list_query",
            Self::CatalogUnavailable { kind, .. } => kind,
            Self::SymlinkNotAllowed { .. } => "symlink_not_allowed",
            Self::ReadOnlyOwnerConflict { .. } => "read_only_owner_conflict",
            Self::Busy { .. } => "busy",
            Self::Io { .. } => "io",
            Self::UnsupportedSchema { .. } => "unsupported_schema",
            Self::MissingObject { .. } => "missing_object",
            Self::ObjectTooLarge { .. } => "object_too_large",
            Self::Corrupt { .. } => "corrupt",
            Self::Sqlite { .. } => "sqlite",
        }
    }
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId { value, message } => {
                write!(f, "invalid session id {value:?}: {message}")
            }
            Self::SessionNotFound { id } => write!(f, "session not found: {id}"),
            Self::AmbiguousPrefix { prefix, matches } => {
                write!(
                    f,
                    "session prefix {prefix:?} is ambiguous ({matches} matches)"
                )
            }
            Self::MissingDatabase { id } => {
                write!(f, "session {id} has no sqlite database")
            }
            Self::InvalidListQuery { message } => {
                write!(f, "invalid session list query: {message}")
            }
            Self::CatalogUnavailable { summary, .. } => f.write_str(summary),
            Self::SymlinkNotAllowed { operation, path } => {
                write!(f, "cannot {operation} symlinked session path {path}")
            }
            Self::ReadOnlyOwnerConflict { owner } => {
                write!(f, "session is owned by another writer: {owner}")
            }
            Self::Busy {
                operation,
                attempts,
                waited_ms,
            } => write!(
                f,
                "session database busy during {operation} after {attempts} attempts over {waited_ms}ms"
            ),
            Self::Io {
                operation,
                path,
                message,
            } => write!(f, "failed to {operation} {path}: {message}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "unsupported session schema version {found}; this build supports {supported}"
            ),
            Self::MissingObject { reference } => {
                write!(f, "session attachment or object is missing: {reference}")
            }
            Self::ObjectTooLarge { size, max } => write!(
                f,
                "session attachment or object is too large: {size} bytes exceeds {max}"
            ),
            Self::Corrupt { context } => write!(f, "corrupt session: {context}"),
            Self::Sqlite {
                operation,
                path,
                message,
            } => write!(f, "sqlite {operation} failed for {path}: {message}"),
        }
    }
}

impl std::error::Error for SessionStoreError {}

pub type SessionStoreResult<T> = Result<T, SessionStoreError>;

pub fn store_error(
    operation: &'static str,
    path: &Path,
    err: smelt_store::StoreError,
) -> SessionStoreError {
    match err {
        smelt_store::StoreError::Io(err) => SessionStoreError::Io {
            operation,
            path: path.display().to_string(),
            message: err.to_string(),
        },
        smelt_store::StoreError::Sqlite(err) => SessionStoreError::Sqlite {
            operation,
            path: path.display().to_string(),
            message: err.to_string(),
        },
        smelt_store::StoreError::Json(err) => SessionStoreError::Corrupt {
            context: format!("{operation} {}: {err}", path.display()),
        },
        smelt_store::StoreError::Busy {
            operation,
            attempts,
            waited_ms,
        } => SessionStoreError::Busy {
            operation: operation.to_string(),
            attempts,
            waited_ms,
        },
        smelt_store::StoreError::TransactionCleanup {
            operation: transaction_operation,
            message,
        } => SessionStoreError::Sqlite {
            operation,
            path: path.display().to_string(),
            message: format!(
                "transaction cleanup failed during {transaction_operation}: {message}"
            ),
        },
        smelt_store::StoreError::OperationCleanup {
            operation: failed_operation,
            primary,
            cleanup,
        } => SessionStoreError::Sqlite {
            operation,
            path: path.display().to_string(),
            message: format!(
                "{failed_operation} failed: {primary}; cleanup also failed: {}",
                cleanup
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        },
        smelt_store::StoreError::OwnershipConflict { owner } => {
            SessionStoreError::ReadOnlyOwnerConflict {
                owner: owner.unwrap_or_else(|| "unknown owner".into()),
            }
        }
        smelt_store::StoreError::OwnershipLost => SessionStoreError::ReadOnlyOwnerConflict {
            owner: "writer ownership was lost".into(),
        },
        smelt_store::StoreError::Cancelled => SessionStoreError::Corrupt {
            context: format!("{operation} {}: operation cancelled", path.display()),
        },
        smelt_store::StoreError::MissingObject { reference } => {
            SessionStoreError::MissingObject { reference }
        }
        smelt_store::StoreError::ObjectTooLarge { size, max } => {
            SessionStoreError::ObjectTooLarge { size, max }
        }
        smelt_store::StoreError::Integrity(message) => SessionStoreError::Corrupt {
            context: format!("{operation} {}: {message}", path.display()),
        },
        smelt_store::StoreError::UnsupportedSchema { found, expected } => {
            SessionStoreError::UnsupportedSchema {
                found,
                supported: expected,
            }
        }
    }
}

pub(crate) fn reject_symlink_in(
    state_root: &Path,
    path: &Path,
    operation: &'static str,
) -> SessionStoreResult<()> {
    if !path.starts_with(state_root) {
        return Err(SessionStoreError::Io {
            operation: "confine session path beneath",
            path: state_root.display().to_string(),
            message: format!("{} escaped its storage root", path.display()),
        });
    }
    for candidate in path.ancestors() {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SessionStoreError::SymlinkNotAllowed {
                    operation,
                    path: candidate.display().to_string(),
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(SessionStoreError::Io {
                    operation: "inspect",
                    path: candidate.display().to_string(),
                    message: err.to_string(),
                });
            }
        }
        if candidate == state_root {
            return Ok(());
        }
    }
    Err(SessionStoreError::Io {
        operation: "inspect storage root for",
        path: path.display().to_string(),
        message: format!("storage root {} was not an ancestor", state_root.display()),
    })
}

pub fn export_history_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    reader_for_export(id_or_prefix)
        .and_then(|reader| {
            reader.export_history_jsonl(out).map_err(|error| {
                store_error("export lineage history", reader.database_path(), error)
            })
        })
        .map_err(|error| error.to_string())
}

pub fn export_requests_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    reader_for_export(id_or_prefix)
        .and_then(|reader| {
            reader.export_requests_jsonl(out).map_err(|error| {
                store_error("export lineage requests", reader.database_path(), error)
            })
        })
        .map_err(|error| error.to_string())
}

fn reader_for_export(id_or_prefix: &str) -> SessionStoreResult<smelt_store::LineageSessionReader> {
    let resolved = crate::session::resolve_session_for_read_result(id_or_prefix)?;
    smelt_store::LineageSessionReader::open_existing_in_lineage(
        &resolved.sessions_root,
        &resolved.lineage_id,
        &resolved.id,
    )
    .map_err(|error| {
        store_error(
            "open lineage export database",
            &resolved.sessions_root,
            error,
        )
    })
}
