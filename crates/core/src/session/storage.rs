//! Session storage: sessions live as rows in one database under the state directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use protocol::{HistoryItem, ReasoningEffort};
use serde::Serialize;
use serde_json::Value;

use super::snapshots::snapshots_from_rows;
use super::{
    AuthoritativeContextTokens, ContextCheckpoint, ContextCheckpointEvent, DisplayContextTokens,
    Session, SessionContextSnapshotState, SessionListCursor, SessionListMeta, SessionListPage,
    SessionListQuery, SessionMeta,
};
use crate::session_id::{SessionId, SessionPrefix};
use crate::session_store::{SessionStoreError, SessionStoreResult, StoreContext};

/// Where one saved session lives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStoreAddress {
    pub store: smelt_store::Store,
    pub session_id: String,
}

impl SessionStoreAddress {
    pub fn new(store: smelt_store::Store, session_id: String) -> Self {
        Self { store, session_id }
    }

    pub fn reader(&self) -> smelt_store::Result<smelt_store::SessionReader> {
        self.store.reader(&self.session_id)
    }

    pub fn artifact_dir(&self) -> PathBuf {
        self.store.session_artifact_dir(&self.session_id)
    }
}

/// What resuming a session loads: its state, the model context since the last
/// checkpoint, and the transcript records that fill the first screen.
#[derive(Clone, Debug)]
pub struct SessionStoreResume {
    pub meta: SessionMeta,
    /// The session without history; `context` holds the part in memory.
    pub session: Session,
    pub address: SessionStoreAddress,
    pub head: smelt_store::StoreHead,
    /// History index of `context[0]`.
    pub context_start: usize,
    pub context: Vec<HistoryItem>,
    /// Plain user turns before `context_start`.
    pub user_turn_count: usize,
    pub transcript_record_tail: smelt_store::TranscriptRecordSlice,
}

#[derive(Clone, Debug)]
pub struct SessionStorage {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    state_root: PathBuf,
    store: smelt_store::Store,
}

impl SessionStorage {
    pub fn from_env(env: &engine::env::RuntimeEnv) -> Self {
        Self::new(env.state_dir().clone())
    }

    pub fn new(state_root: PathBuf) -> Self {
        let store = smelt_store::Store::new(&state_root);
        Self {
            inner: Arc::new(Inner { state_root, store }),
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.inner.state_root
    }

    pub fn store(&self) -> &smelt_store::Store {
        &self.inner.store
    }

    pub fn create_private_dir_all(&self, path: &Path) -> std::io::Result<()> {
        create_private_dir_all_in(self.state_root(), path)
    }

    pub(crate) fn read_private_file(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        reject_filesystem_symlink_in(self.state_root(), path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(
                "private storage path is not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(std::io::Error::other(
                "private storage file exceeds its size limit",
            ));
        }
        Ok(bytes)
    }

    pub fn write_private_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        write_private_file_in(self.state_root(), path, contents)
    }

    /// Artifact directory of an in-memory session, whose id is always valid.
    pub fn artifact_dir_for_id(&self, id: &str) -> PathBuf {
        let id = SessionId::parse(id)
            .unwrap_or_else(|err| panic!("invalid in-memory session id {id:?}: {err}"));
        self.session_artifact_dir(&id)
    }

    pub fn artifact_dir_for(&self, session: &Session) -> PathBuf {
        self.artifact_dir_for_id(&session.id)
    }

    pub fn session_artifact_dir(&self, id: &SessionId) -> PathBuf {
        self.store().session_artifact_dir(id.as_str())
    }

    pub fn address(&self, session_id: impl Into<String>) -> SessionStoreAddress {
        SessionStoreAddress::new(self.store().clone(), session_id.into())
    }

    /// Resolves an exact id or a unique prefix of a saved session.
    pub fn resolve(&self, id_or_prefix: &str) -> SessionStoreResult<SessionStoreAddress> {
        let id = match SessionId::parse(id_or_prefix) {
            Ok(id) => {
                if !self.store().exists(id.as_str()).context("find session")? {
                    return Err(SessionStoreError::SessionNotFound {
                        id: id.into_string(),
                    });
                }
                id.into_string()
            }
            Err(_) => self.resolve_prefix(id_or_prefix)?.into_string(),
        };
        Ok(self.address(id))
    }

    pub fn resolve_prefix(&self, prefix: &str) -> SessionStoreResult<SessionId> {
        let prefix =
            SessionPrefix::parse(prefix).map_err(|error| SessionStoreError::InvalidSessionId {
                value: prefix.to_string(),
                message: error.to_string(),
            })?;
        let ids = self
            .store()
            .ids_with_prefix(prefix.as_str(), 2)
            .context("resolve session prefix")?;
        match ids.as_slice() {
            [] => Err(SessionStoreError::SessionNotFound {
                id: prefix.as_str().to_string(),
            }),
            [id] => SessionId::parse(id).map_err(|error| SessionStoreError::Corrupt {
                context: format!("stored session id {id:?}: {error}"),
            }),
            _ => Err(SessionStoreError::AmbiguousPrefix {
                prefix: prefix.as_str().to_string(),
                matches: ids.len(),
            }),
        }
    }

    pub fn reader(&self, id_or_prefix: &str) -> SessionStoreResult<smelt_store::SessionReader> {
        self.resolve(id_or_prefix)?.reader().context("open session")
    }

    pub fn load_meta(&self, id_or_prefix: &str) -> SessionStoreResult<SessionMeta> {
        let _perf = smelt_perf::perf::begin("session:load_meta");
        let state = self.reader(id_or_prefix)?.state().context("read session")?;
        Ok(session_from_state(state)?.0)
    }

    /// Loads a session with its whole history. Keep this out of hot paths: it
    /// costs time and memory in proportion to the session.
    pub fn load_full(&self, id_or_prefix: &str) -> SessionStoreResult<Session> {
        let _perf = smelt_perf::perf::begin("session:load_full");
        let reader = self.reader(id_or_prefix)?;
        let (state, history) = reader
            .read_snapshot(|reader| {
                let state = reader.state()?;
                let history = reader.history_range(0, state.head.history_len.get())?;
                Ok((state, history))
            })
            .context("read session")?;
        let (_, mut session) = session_from_state(state)?;
        session.history = history;
        Ok(session)
    }

    /// Loads what resuming needs: state, model context and the transcript tail
    /// that fills `record_target_rows` rows at `record_width`.
    pub fn load_resume(
        &self,
        id_or_prefix: &str,
        record_width: u16,
        record_target_rows: u16,
    ) -> SessionStoreResult<SessionStoreResume> {
        let _perf = smelt_perf::perf::begin("session:load_resume");
        let address = self.resolve(id_or_prefix)?;
        let reader = address.reader().context("open session")?;
        let (state, context_start, context, user_turn_count, transcript_record_tail) = reader
            .read_snapshot(|reader| {
                let state = reader.state()?;
                let history_len = state.head.history_len.get();
                let context_start = state
                    .checkpoint
                    .as_ref()
                    .and_then(|value| value.get("first_live_index"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(history_len);
                let context = reader.history_range(context_start, history_len)?;
                let user_turn_count = reader.history_user_turn_count_before(context_start)?;
                let tail = reader.transcript_tail_for_rows_with_total(
                    state.head.transcript_record_count.as_usize(),
                    record_width,
                    record_target_rows,
                )?;
                Ok((state, context_start, context, user_turn_count, tail))
            })
            .context("read session")?;
        let head = state.head;
        let (meta, session) = session_from_state(state)?;
        Ok(SessionStoreResume {
            meta,
            session,
            address,
            head,
            context_start: context_start as usize,
            context,
            user_turn_count: user_turn_count as usize,
            transcript_record_tail,
        })
    }

    /// Writes a whole session. For tools, tests and side sessions; interactive
    /// sessions save only what changed.
    pub fn save(&self, session: &Session) -> SessionStoreResult<smelt_store::StoreHead> {
        let _perf = smelt_perf::perf::begin("session:write");
        SessionId::parse(&session.id).map_err(|error| SessionStoreError::InvalidSessionId {
            value: session.id.clone(),
            message: error.to_string(),
        })?;
        let commit = full_commit(session)?;
        self.store()
            .writer(&session.id)
            .and_then(|mut writer| writer.commit(&store_identity(session), &commit))
            .context("save session")
    }

    pub fn list_page(&self, query: SessionListQuery) -> SessionStoreResult<SessionListPage> {
        self.list(query, None)
    }

    /// Sessions whose parent is `parent_id`, newest first.
    pub fn children(&self, parent_id: &str) -> SessionStoreResult<Vec<SessionListMeta>> {
        self.list_all(Some(parent_id))
    }

    pub fn list_all_sessions(&self) -> SessionStoreResult<Vec<SessionListMeta>> {
        self.list_all(None)
    }

    fn list_all(&self, parent_id: Option<&str>) -> SessionStoreResult<Vec<SessionListMeta>> {
        let mut sessions = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list(
                SessionListQuery {
                    limit: 1000,
                    cursor,
                    cwd: None,
                },
                parent_id,
            )?;
            sessions.extend(page.sessions);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(sessions);
            }
        }
    }

    fn list(
        &self,
        query: SessionListQuery,
        parent_id: Option<&str>,
    ) -> SessionStoreResult<SessionListPage> {
        let _perf = smelt_perf::perf::begin("session:list_page");
        let page = self
            .store()
            .list(&smelt_store::CatalogQuery {
                limit: query.limit,
                cursor: query.cursor.map(|cursor| smelt_store::CatalogCursor {
                    updated_at: cursor.updated_at_ms.min(i64::MAX as u64) as i64,
                    id: cursor.id,
                }),
                cwd: query.cwd,
                parent_id: parent_id.map(str::to_owned),
            })
            .context("list sessions")?;
        Ok(SessionListPage {
            sessions: page.sessions.into_iter().map(list_meta).collect(),
            next_cursor: page.next_cursor.map(|cursor| SessionListCursor {
                updated_at_ms: cursor.updated_at.max(0) as u64,
                id: cursor.id,
            }),
        })
    }

    pub fn delete(&self, id_or_prefix: &str) -> SessionStoreResult<()> {
        let address = self.resolve(id_or_prefix)?;
        self.store()
            .delete(&address.session_id)
            .context("delete session")
    }

    /// The text of a session's conversation, for search.
    pub fn load_conversation_text(&self, id_or_prefix: &str) -> SessionStoreResult<String> {
        let _perf = smelt_perf::perf::begin("session:load_conversation_text");
        self.reader(id_or_prefix)?
            .conversation_text()
            .context("read session text")
    }

    /// Ids of the sessions whose conversation text contains `query`, ignoring
    /// case, from the full-text index. Queries shorter than
    /// [`smelt_store::SEARCH_MIN_CHARS`] match nothing.
    pub fn search(&self, query: &str) -> SessionStoreResult<Vec<String>> {
        let _perf = smelt_perf::perf::begin("session:search");
        self.store()
            .search_sessions(query)
            .context("search sessions")
    }

    pub fn export_history_jsonl(
        &self,
        id_or_prefix: &str,
        out: impl std::io::Write,
    ) -> SessionStoreResult<()> {
        self.reader(id_or_prefix)?
            .export_history_jsonl(out, &|| false)
            .context("export session history")
    }

    pub fn export_requests_jsonl(
        &self,
        id_or_prefix: &str,
        out: impl std::io::Write,
    ) -> SessionStoreResult<()> {
        self.reader(id_or_prefix)?
            .export_requests_jsonl(out)
            .context("export session requests")
    }
}

/// Storage under the configured state directory, shared by the process.
pub fn storage() -> SessionStorage {
    static STORAGE: OnceLock<Mutex<Option<SessionStorage>>> = OnceLock::new();
    let state_root = crate::config::state_dir();
    let mut current = STORAGE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    match current.as_ref() {
        Some(storage) if storage.state_root() == state_root => storage.clone(),
        _ => current.insert(SessionStorage::new(state_root)).clone(),
    }
}

// Conversions between sessions and stored rows.

pub fn store_identity(session: &Session) -> smelt_store::SessionIdentity {
    smelt_store::SessionIdentity {
        id: session.id.clone(),
        created_at: session.created_at_ms.min(i64::MAX as u64) as i64,
        parent_id: session.parent_id.clone(),
    }
}

pub fn store_metadata(session: &Session) -> SessionStoreResult<smelt_store::SessionMetadata> {
    let accounting = SessionContextSnapshotState {
        session_usage: session.session_usage.clone(),
        context_token_identity: session.context_token_identity.clone(),
        display_context_token_identity: session.display_context_token_identity.clone(),
    };
    Ok(smelt_store::SessionMetadata {
        title: session.title.clone(),
        slug: session.slug.clone(),
        cwd: session.cwd.clone(),
        mode: session.mode.clone(),
        reasoning_effort: session
            .reasoning_effort
            .as_ref()
            .map(|effort| effort.label().to_string()),
        model: session.model.clone(),
        fast_mode: session.fast_mode,
        accounting: Some(json_value(&accounting)?),
        context_tokens: session.context_tokens.map(u64::from),
        context_tokens_history_len: session.context_tokens_history_len.map(|len| len as u64),
        display_context_tokens: session.display_context_tokens.map(u64::from),
        cost_usd: session.session_cost_usd,
        last_turn_id: session.last_turn_id,
        updated_at: session.updated_at_ms.min(i64::MAX as u64) as i64,
    })
}

/// The checkpoint as stored, if it lies within `history_len`.
pub fn store_checkpoint(
    checkpoint: Option<&ContextCheckpoint>,
    history_len: usize,
) -> SessionStoreResult<Option<Value>> {
    checkpoint
        .filter(|checkpoint| checkpoint.first_live_index <= history_len)
        .map(json_value)
        .transpose()
}

/// A commit that writes all of `session`.
pub fn full_commit(session: &Session) -> SessionStoreResult<smelt_store::SessionCommit> {
    fn all_rows<'a, T: Serialize + 'a>(
        rows: impl Iterator<Item = (u64, &'a T)>,
    ) -> SessionStoreResult<Option<smelt_store::SideRows>> {
        Ok(Some(smelt_store::SideRows {
            start: 0,
            rows: rows
                .map(|(key, value)| Ok((key, json_value(value)?)))
                .collect::<SessionStoreResult<_>>()?,
        }))
    }
    Ok(smelt_store::SessionCommit {
        metadata: store_metadata(session)?,
        first_user_message: Some(session.first_user_message.as_deref().map(str::to_owned)),
        checkpoint: Some(store_checkpoint(
            session.checkpoint.as_ref(),
            session.history.len(),
        )?),
        history: Some(smelt_store::HistorySuffix {
            start: 0,
            items: session.history.clone(),
        }),
        turn_metas: all_rows(session.turn_metas.iter().map(keyed))?,
        metadata_snapshots: all_rows(session.metadata_snapshots.iter().map(keyed))?,
        context_snapshots: all_rows(session.context_snapshots.iter().map(keyed))?,
        checkpoint_events: all_rows(
            session
                .checkpoint_events
                .iter()
                .enumerate()
                .map(|(row, event)| (row as u64, event)),
        )?,
        transcript: None,
    })
}

fn keyed<T>((key, value): &(usize, T)) -> (u64, &T) {
    (*key as u64, value)
}

fn json_value(value: &impl Serialize) -> SessionStoreResult<Value> {
    serde_json::to_value(value).map_err(|error| SessionStoreError::Corrupt {
        context: format!("encode session value: {error}"),
    })
}

fn corrupt(what: &str) -> impl Fn(String) -> SessionStoreError + '_ {
    move |error| SessionStoreError::Corrupt {
        context: format!("{what}: {error}"),
    }
}

fn list_meta(session: smelt_store::CatalogSession) -> SessionListMeta {
    SessionListMeta {
        id: session.id,
        title: session.title,
        slug: session.slug,
        first_user_message: session.first_user_message.map(Into::into),
        created_at_ms: session.created_at.max(0) as u64,
        updated_at_ms: session.updated_at.max(0) as u64,
        mode: session.mode,
        reasoning_effort: session
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse),
        model: session.model,
        fast_mode: session.fast_mode,
        cwd: session.cwd,
        parent_id: session.parent_id,
        display_context_tokens: session
            .context_tokens
            .and_then(|tokens| u32::try_from(tokens).ok()),
        history_len: Some(session.history_len as usize),
        text_bytes: Some(session.text_bytes),
    }
}

/// A stored session as metadata and as a session without history.
fn session_from_state(
    state: smelt_store::SessionState,
) -> SessionStoreResult<(SessionMeta, Session)> {
    let smelt_store::SessionState {
        identity,
        metadata,
        first_user_message,
        checkpoint,
        head,
        history_text_bytes,
        turn_metas,
        metadata_snapshots,
        context_snapshots,
        checkpoint_events,
    } = state;
    let history_len = head.history_len.as_usize();
    let checkpoint = checkpoint
        .map(serde_json::from_value::<ContextCheckpoint>)
        .transpose()
        .map_err(|error| corrupt("checkpoint")(error.to_string()))?
        .filter(|checkpoint| checkpoint.first_live_index <= history_len);
    let checkpoint_events = checkpoint_events
        .into_iter()
        .map(|(_, value)| serde_json::from_value::<ContextCheckpointEvent>(value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| corrupt("checkpoint events")(error.to_string()))?;
    let accounting: SessionContextSnapshotState = metadata
        .accounting
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| corrupt("session accounting")(error.to_string()))?
        .unwrap_or_default();
    let context_token_identity = accounting.context_token_identity.clone();
    let display_context_token_identity = accounting
        .display_context_token_identity
        .clone()
        .or_else(|| context_token_identity.clone());
    let context_tokens = metadata
        .context_tokens
        .and_then(|tokens| u32::try_from(tokens).ok());
    let context_tokens_history_len = metadata.context_tokens_history_len.map(|len| len as usize);
    let display_context_tokens = metadata
        .display_context_tokens
        .or(metadata.context_tokens)
        .and_then(|tokens| u32::try_from(tokens).ok());
    let reasoning_effort = metadata
        .reasoning_effort
        .as_deref()
        .and_then(ReasoningEffort::parse);
    let created_at_ms = identity.created_at.max(0) as u64;
    let updated_at_ms = metadata.updated_at.max(0) as u64;
    let first_user_message: Option<Arc<str>> = first_user_message.map(Into::into);

    let meta = SessionMeta {
        id: identity.id.clone(),
        title: metadata.title.clone(),
        slug: metadata.slug.clone(),
        first_user_message: first_user_message.clone(),
        created_at_ms,
        updated_at_ms,
        mode: metadata.mode.clone(),
        reasoning_effort: reasoning_effort.clone(),
        model: metadata.model.clone(),
        fast_mode: metadata.fast_mode,
        cwd: metadata.cwd.clone(),
        parent_id: identity.parent_id.clone(),
        authoritative_context_tokens: context_tokens
            .zip(context_tokens_history_len)
            .zip(context_token_identity.clone())
            .map(
                |((tokens, history_len), identity)| AuthoritativeContextTokens {
                    tokens,
                    history_len,
                    identity,
                },
            ),
        display_context_tokens: display_context_tokens.map(|tokens| DisplayContextTokens {
            tokens,
            identity: display_context_token_identity.clone(),
        }),
        history_len: Some(history_len),
        checkpoint: checkpoint.clone(),
        checkpoint_events: checkpoint_events.clone().into(),
        text_bytes: Some(history_text_bytes),
    };
    let session = Session {
        id: identity.id,
        title: metadata.title,
        slug: metadata.slug,
        first_user_message,
        metadata_snapshots: snapshots_from_rows(metadata_snapshots)
            .map_err(corrupt("metadata snapshots"))?,
        created_at_ms,
        updated_at_ms,
        mode: metadata.mode,
        reasoning_effort,
        model: metadata.model,
        fast_mode: metadata.fast_mode,
        cwd: metadata.cwd,
        parent_id: identity.parent_id,
        history: Vec::new(),
        checkpoint,
        checkpoint_events: checkpoint_events.into(),
        context_tokens,
        context_tokens_history_len,
        context_token_identity,
        display_context_tokens,
        display_context_token_identity,
        turn_metas: snapshots_from_rows(turn_metas).map_err(corrupt("turn metadata"))?,
        context_snapshots: snapshots_from_rows(context_snapshots)
            .map_err(corrupt("context snapshots"))?,
        session_cost_usd: metadata.cost_usd,
        session_usage: accounting.session_usage,
        last_turn_id: metadata.last_turn_id,
    };
    Ok((meta, session))
}

// Private files and directories under the state directory.

fn reject_filesystem_symlink_in(state_root: &Path, path: &Path) -> std::io::Result<()> {
    if !path.starts_with(state_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "storage path {} escaped root {}",
                path.display(),
                state_root.display()
            ),
        ));
    }
    for candidate in path.ancestors() {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refusing symlinked storage path {}", candidate.display()),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        if candidate == state_root {
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "storage root {} was not an ancestor of {}",
            state_root.display(),
            path.display()
        ),
    ))
}

fn create_private_dir(state_root: &Path, path: &Path, recursive: bool) -> std::io::Result<()> {
    reject_filesystem_symlink_in(state_root, path)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(err) if !recursive && err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    reject_filesystem_symlink_in(state_root, path)?;
    if !fs::metadata(path)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "storage directory path is not a directory: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let state_root = smelt_paths::state_dir();
    let root = if path.starts_with(&state_root) {
        state_root.as_path()
    } else {
        path
    };
    create_private_dir_all_in(root, path)
}

pub fn create_private_dir_all_in(state_root: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path.strip_prefix(state_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "storage path {} escaped root {}",
                path.display(),
                state_root.display()
            ),
        )
    })?;
    create_private_dir(state_root, state_root, true)?;
    let mut current = state_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid storage directory path: {}", path.display()),
            ));
        };
        current.push(component);
        create_private_dir(state_root, &current, false)?;
    }
    Ok(())
}

pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let state_root = smelt_paths::state_dir();
    let root = if path.starts_with(&state_root) {
        state_root.as_path()
    } else {
        path
    };
    write_private_file_in(root, path, contents)
}

pub fn write_private_file_in(
    state_root: &Path,
    path: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        reject_filesystem_symlink_in(state_root, parent)?;
    }
    reject_filesystem_symlink_in(state_root, path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut file, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> (tempfile::TempDir, SessionStorage) {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().join("state"));
        (root, storage)
    }

    fn user(text: &str) -> HistoryItem {
        HistoryItem::user(protocol::Content::text(text))
    }

    fn session(history: Vec<HistoryItem>) -> Session {
        let mut session = Session::new(1, PathBuf::from("/work"));
        session.title = Some("title".into());
        session.first_user_message = Some("first".into());
        session.history = history;
        session.last_turn_id = 7;
        session
    }

    #[test]
    fn saved_session_loads_back_whole() {
        let (_root, storage) = storage();
        let mut saved = session(vec![user("one"), user("two")]);
        saved.metadata_snapshots.push((
            1,
            super::super::SessionMetadataSnapshot {
                title: Some("title".into()),
                slug: None,
                first_user_message: None,
            },
        ));
        storage.save(&saved).unwrap();

        let loaded = storage.load_full(&saved.id).unwrap();
        assert_eq!(loaded, saved);
        let meta = storage.load_meta(&saved.id[..8]).unwrap();
        assert_eq!(meta.history_len, Some(2));
        assert_eq!(meta.first_user_message.as_deref(), Some("first"));
    }

    #[test]
    fn resume_loads_only_the_context_after_the_checkpoint() {
        let (_root, storage) = storage();
        let mut saved = session((0..5).map(|i| user(&i.to_string())).collect());
        saved.checkpoint = Some(ContextCheckpoint {
            summary: "summary".into(),
            first_live_index: 3,
            ..Default::default()
        });
        storage.save(&saved).unwrap();

        let resume = storage.load_resume(&saved.id, 80, 24).unwrap();
        assert_eq!(resume.context_start, 3);
        assert_eq!(resume.context, saved.history[3..]);
        assert_eq!(resume.user_turn_count, 3);
        assert!(resume.session.history.is_empty());
        assert_eq!(resume.session.last_turn_id, 7);
        assert_eq!(resume.head.history_len.get(), 5);
    }

    #[test]
    fn prefixes_resolve_only_when_unique() {
        let (_root, storage) = storage();
        let mut first = session(Vec::new());
        first.id = format!("abcdef{}", "0".repeat(58));
        let mut second = session(Vec::new());
        second.id = format!("abcdef{}", "1".repeat(58));
        storage.save(&first).unwrap();
        storage.save(&second).unwrap();

        assert_eq!(
            storage.resolve("abcdef0").unwrap().session_id,
            first.id.as_str()
        );
        assert!(matches!(
            storage.resolve("abcdef"),
            Err(SessionStoreError::AmbiguousPrefix { matches: 2, .. })
        ));
        assert!(matches!(
            storage.resolve("cdcdcd"),
            Err(SessionStoreError::SessionNotFound { .. })
        ));
        assert!(matches!(
            storage.resolve("../x"),
            Err(SessionStoreError::InvalidSessionId { .. })
        ));
    }

    #[test]
    fn listing_pages_newest_first_and_finds_children() {
        let (_root, storage) = storage();
        let parent = session(Vec::new());
        storage.save(&parent).unwrap();
        let mut ids = vec![parent.id.clone()];
        for at in 1..=3 {
            let mut child = parent.fork(1);
            child.updated_at_ms = parent.updated_at_ms + at;
            storage.save(&child).unwrap();
            ids.push(child.id);
        }

        let first = storage
            .list_page(SessionListQuery {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        let second = storage
            .list_page(SessionListQuery {
                limit: 2,
                cursor: first.next_cursor.clone(),
                ..Default::default()
            })
            .unwrap();
        let listed: Vec<_> = first
            .sessions
            .iter()
            .chain(&second.sessions)
            .map(|meta| meta.id.clone())
            .collect();
        ids.reverse();
        assert_eq!(listed, ids);
        assert!(second.next_cursor.is_none());
        assert_eq!(storage.children(&parent.id).unwrap().len(), 3);
    }

    #[test]
    fn deleted_sessions_are_gone() {
        let (_root, storage) = storage();
        let saved = session(vec![user("one")]);
        storage.save(&saved).unwrap();
        storage.delete(&saved.id).unwrap();
        assert!(matches!(
            storage.load_meta(&saved.id),
            Err(SessionStoreError::SessionNotFound { .. })
        ));
    }
}
