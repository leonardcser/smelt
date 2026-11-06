use std::fmt;

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
    /// Another smelt process has the session open.
    Owned {
        id: String,
    },
    Corrupt {
        context: String,
    },
    Store {
        operation: &'static str,
        code: &'static str,
        transient: bool,
        message: String,
    },
}

impl SessionStoreError {
    pub fn store(operation: &'static str, error: smelt_store::StoreError) -> Self {
        match error {
            smelt_store::StoreError::NotFound { session_id } => {
                Self::SessionNotFound { id: session_id }
            }
            smelt_store::StoreError::Owned { session_id } => Self::Owned { id: session_id },
            smelt_store::StoreError::Integrity(message) => Self::Corrupt {
                context: format!("{operation}: {message}"),
            },
            error => Self::Store {
                operation,
                code: error.code(),
                transient: error.is_transient(),
                message: error.to_string(),
            },
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSessionId { .. } => "invalid_session_id",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::AmbiguousPrefix { .. } => "ambiguous_prefix",
            Self::Owned { .. } => "owned",
            Self::Corrupt { .. } => "corrupt",
            Self::Store { code, .. } => code,
        }
    }

    /// Busy or locked storage that can succeed when retried.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Store {
                transient: true,
                ..
            }
        )
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
            Self::Owned { id } => write!(f, "session {id} is open in another smelt process"),
            Self::Corrupt { context } => write!(f, "corrupt session: {context}"),
            Self::Store {
                operation, message, ..
            } => write!(f, "failed to {operation}: {message}"),
        }
    }
}

impl std::error::Error for SessionStoreError {}

pub type SessionStoreResult<T> = Result<T, SessionStoreError>;

/// Maps a store result into a session result for `operation`.
pub(crate) trait StoreContext<T> {
    fn context(self, operation: &'static str) -> SessionStoreResult<T>;
}

impl<T> StoreContext<T> for smelt_store::Result<T> {
    fn context(self, operation: &'static str) -> SessionStoreResult<T> {
        self.map_err(|error| SessionStoreError::store(operation, error))
    }
}
