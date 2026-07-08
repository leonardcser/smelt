use std::fmt;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStoreError {
    SessionNotFound { id: String },
    MissingDatabase { id: String },
    OpenDatabase { message: String },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionStoreError::SessionNotFound { id } => {
                write!(f, "session not found or prefix is ambiguous: {id}")
            }
            SessionStoreError::MissingDatabase { id } => {
                write!(f, "session {id} has no sqlite database")
            }
            SessionStoreError::OpenDatabase { message } => {
                write!(f, "failed to open sqlite database: {message}")
            }
        }
    }
}

impl std::error::Error for SessionStoreError {}

pub type SessionStoreResult<T> = Result<T, SessionStoreError>;

pub fn ensure_session_db_read_only(dir_path: &Path) -> SessionStoreResult<()> {
    open_session_db(dir_path, smelt_store::SessionDb::open_read_only)
}

pub fn ensure_session_db_writable(dir_path: &Path) -> SessionStoreResult<()> {
    open_session_db(dir_path, smelt_store::SessionDb::open)
}

fn open_session_db(
    dir_path: &Path,
    open: impl FnOnce(std::path::PathBuf) -> smelt_store::Result<smelt_store::SessionDb>,
) -> SessionStoreResult<()> {
    let db_path = dir_path.join("session.db");
    if !db_path.is_file() {
        return Err(SessionStoreError::MissingDatabase {
            id: session_dir_id(dir_path),
        });
    }
    open(db_path)
        .map(|_| ())
        .map_err(|err| SessionStoreError::OpenDatabase {
            message: err.to_string(),
        })
}

fn session_dir_id(dir_path: &Path) -> String {
    dir_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<unknown>")
        .to_string()
}

pub fn export_history_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    let db = db_for_export(id_or_prefix).map_err(|err| err.to_string())?;
    db.export_history_jsonl(out).map_err(|err| err.to_string())
}

pub fn export_requests_jsonl(id_or_prefix: &str, out: impl Write) -> Result<(), String> {
    let db = db_for_export(id_or_prefix).map_err(|err| err.to_string())?;
    db.export_requests_jsonl(out).map_err(|err| err.to_string())
}

fn db_for_export(id_or_prefix: &str) -> SessionStoreResult<smelt_store::SessionDb> {
    let id = crate::session::resolve_prefix(id_or_prefix).ok_or_else(|| {
        SessionStoreError::SessionNotFound {
            id: id_or_prefix.to_string(),
        }
    })?;
    let dir = crate::session::sessions_dir().join(&id);
    ensure_session_db_read_only(&dir)?;
    smelt_store::SessionDb::open_read_only(dir.join("session.db")).map_err(|err| {
        SessionStoreError::OpenDatabase {
            message: err.to_string(),
        }
    })
}
