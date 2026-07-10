#[cfg(any(test, feature = "test-util"))]
use std::fs;
#[cfg(any(test, feature = "test-util"))]
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{to_sql_error, Result, StoreError};
use crate::object::checked_i64;
use crate::schema::SCHEMA_VERSION;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionState {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    pub parent_id: Option<String>,
    pub accounting_json: Option<serde_json::Value>,
    pub checkpoint_json: Option<serde_json::Value>,
    pub context_tokens: Option<u64>,
    pub context_tokens_history_len: Option<u64>,
    pub display_context_tokens: Option<u64>,
    pub session_cost_usd: f64,
    pub revision: u64,
    pub history_len: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    pub parent_id: Option<String>,
    pub context_tokens: Option<u64>,
    pub revision: u64,
    pub history_len: u64,
    pub updated_at: i64,
    pub schema_version: i32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriterOwner {
    pub hostname: String,
    pub pid: u32,
    pub process_start_id: String,
    pub app_version: String,
    pub claimed_at: i64,
}

impl WriterOwner {
    pub fn summary(&self) -> String {
        format!(
            "pid {} on {} (process {}, claimed at {})",
            self.pid, self.hostname, self.process_start_id, self.claimed_at
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedWriterOwner {
    token: String,
    owner: WriterOwner,
}

const WRITER_OWNER_KEY: &str = "writer_owner";

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES (?1, ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (key, value),
    )?;
    Ok(())
}

pub(crate) fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM store_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn claim_writer_owner(
    conn: &Connection,
    token: &str,
    owner: &WriterOwner,
) -> Result<()> {
    let persisted = PersistedWriterOwner {
        token: token.to_string(),
        owner: owner.clone(),
    };
    set_meta(conn, WRITER_OWNER_KEY, &serde_json::to_string(&persisted)?)?;
    // COMPAT(session-writer-lease-metadata): remove with pre-lock session metadata support.
    conn.execute("DELETE FROM store_meta WHERE key = 'writer_lease'", [])?;
    Ok(())
}

pub(crate) fn writer_owner(conn: &Connection) -> Result<Option<WriterOwner>> {
    Ok(persisted_writer_owner(conn)?.map(|persisted| persisted.owner))
}

pub(crate) fn verify_writer_owner(conn: &Connection, token: &str) -> Result<()> {
    match persisted_writer_owner(conn)? {
        Some(owner) if owner.token == token => Ok(()),
        _ => Err(StoreError::OwnershipLost),
    }
}

pub(crate) fn clear_writer_owner(conn: &Connection, token: &str) -> Result<()> {
    if persisted_writer_owner(conn)?.is_some_and(|owner| owner.token == token) {
        conn.execute("DELETE FROM store_meta WHERE key = ?1", [WRITER_OWNER_KEY])?;
    }
    Ok(())
}

fn persisted_writer_owner(conn: &Connection) -> Result<Option<PersistedWriterOwner>> {
    meta(conn, WRITER_OWNER_KEY)?
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

pub(crate) fn upsert_session_state(conn: &Connection, state: &SessionState) -> Result<()> {
    validate_session_state_checkpoint(state)?;
    let accounting_json = optional_json_string(&state.accounting_json)?;
    let checkpoint_json = optional_json_string(&state.checkpoint_json)?;
    conn.execute(
        "INSERT INTO session_state (
            singleton, id, title, slug, first_user_message, cwd, mode, reasoning_effort,
            model, fast_mode, parent_id, accounting_json, checkpoint_json, context_tokens,
            context_tokens_history_len, display_context_tokens, session_cost_usd,
            revision, history_len, created_at, updated_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
         ON CONFLICT(singleton) DO UPDATE SET
            id = excluded.id,
            title = excluded.title,
            slug = excluded.slug,
            first_user_message = excluded.first_user_message,
            cwd = excluded.cwd,
            mode = excluded.mode,
            reasoning_effort = excluded.reasoning_effort,
            model = excluded.model,
            fast_mode = excluded.fast_mode,
            parent_id = excluded.parent_id,
            accounting_json = excluded.accounting_json,
            checkpoint_json = excluded.checkpoint_json,
            context_tokens = excluded.context_tokens,
            context_tokens_history_len = excluded.context_tokens_history_len,
            display_context_tokens = excluded.display_context_tokens,
            session_cost_usd = excluded.session_cost_usd,
            revision = excluded.revision,
            history_len = excluded.history_len,
            updated_at = excluded.updated_at",
        params![
            &state.id,
            &state.title,
            &state.slug,
            &state.first_user_message,
            &state.cwd,
            &state.mode,
            &state.reasoning_effort,
            &state.model,
            state.fast_mode,
            &state.parent_id,
            &accounting_json,
            &checkpoint_json,
            state.context_tokens.map(|tokens| checked_i64(tokens, "context_tokens")).transpose()?,
            state
                .context_tokens_history_len
                .map(|len| checked_i64(len, "context_tokens_history_len"))
                .transpose()?,
            state
                .display_context_tokens
                .map(|tokens| checked_i64(tokens, "display_context_tokens"))
                .transpose()?,
            state.session_cost_usd,
            checked_i64(state.revision, "revision")?,
            checked_i64(state.history_len, "history_len")?,
            state.created_at,
            state.updated_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn session_state(conn: &Connection) -> Result<Option<SessionState>> {
    conn.query_row(
        "SELECT id, title, slug, first_user_message, cwd, mode, reasoning_effort,
                model, fast_mode, parent_id, accounting_json, checkpoint_json, context_tokens,
                context_tokens_history_len, display_context_tokens, session_cost_usd,
                revision, history_len, created_at, updated_at
         FROM session_state
         WHERE singleton = 1",
        [],
        |row| {
            let accounting_json: Option<String> = row.get(10)?;
            let checkpoint_json: Option<String> = row.get(11)?;
            Ok(SessionState {
                id: row.get(0)?,
                title: row.get(1)?,
                slug: row.get(2)?,
                first_user_message: row.get(3)?,
                cwd: row.get(4)?,
                mode: row.get(5)?,
                reasoning_effort: row.get(6)?,
                model: row.get(7)?,
                fast_mode: row.get(8)?,
                parent_id: row.get(9)?,
                accounting_json: parse_optional_json(accounting_json)?,
                checkpoint_json: parse_optional_json(checkpoint_json)?,
                context_tokens: row.get::<_, Option<i64>>(12)?.map(|value| value as u64),
                context_tokens_history_len: row
                    .get::<_, Option<i64>>(13)?
                    .map(|value| value as u64),
                display_context_tokens: row.get::<_, Option<i64>>(14)?.map(|value| value as u64),
                session_cost_usd: row.get(15)?,
                revision: row.get::<_, i64>(16)? as u64,
                history_len: row.get::<_, i64>(17)? as u64,
                created_at: row.get(18)?,
                updated_at: row.get(19)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn session_meta(conn: &Connection) -> Result<Option<SessionMeta>> {
    Ok(session_state(conn)?.map(|state| SessionMeta {
        id: state.id,
        title: state.title,
        slug: state.slug,
        first_user_message: state.first_user_message,
        cwd: state.cwd,
        mode: state.mode,
        reasoning_effort: state.reasoning_effort,
        model: state.model,
        fast_mode: state.fast_mode,
        parent_id: state.parent_id,
        context_tokens: state.context_tokens,
        revision: state.revision,
        history_len: state.history_len,
        updated_at: state.updated_at,
        schema_version: SCHEMA_VERSION,
    }))
}

// COMPAT(session-checkpoint-live-index-past-history): repair checkpoints saved
// with a live tail start beyond the retained SQLite history. Keeping the summary
// and replaying all retained rows preserves the most context available.
pub(crate) fn repair_checkpoint_first_live_index_past_history(conn: &Connection) -> Result<usize> {
    let Some(state) = session_state(conn)? else {
        return Ok(0);
    };
    let Some(mut checkpoint_json) = state.checkpoint_json else {
        return Ok(0);
    };
    let Some(first_live_index) = checkpoint_first_live_index(&checkpoint_json) else {
        return Ok(0);
    };
    let retained_history_len = state.history_len.min(history_item_count(conn)?);
    if first_live_index <= retained_history_len {
        return Ok(0);
    }
    let Some(object) = checkpoint_json.as_object_mut() else {
        return Ok(0);
    };
    object.insert("first_live_index".to_string(), serde_json::json!(0));
    let checkpoint_json = serde_json::to_string(&checkpoint_json)?;
    conn.execute(
        "UPDATE session_state SET checkpoint_json = ?1 WHERE singleton = 1",
        [checkpoint_json],
    )?;
    smelt_perf::perf::record_value("store:session:checkpoint_live_index_repaired", 1);
    Ok(1)
}

fn history_item_count(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM history_items", [], |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

fn validate_session_state_checkpoint(state: &SessionState) -> Result<()> {
    let Some(first_live_index) = state
        .checkpoint_json
        .as_ref()
        .and_then(checkpoint_first_live_index)
    else {
        return Ok(());
    };
    if first_live_index > state.history_len {
        return Err(StoreError::Integrity(format!(
            "checkpoint first_live_index {first_live_index} exceeds history_len {}",
            state.history_len
        )));
    }
    Ok(())
}

fn checkpoint_first_live_index(value: &serde_json::Value) -> Option<u64> {
    value
        .get("first_live_index")
        .and_then(serde_json::Value::as_u64)
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) fn write_meta_sidecar(
    conn: &Connection,
    path: impl AsRef<Path>,
) -> Result<Option<SessionMeta>> {
    let meta = {
        let _perf = smelt_perf::perf::begin("store:session:meta_sidecar_query");
        let Some(meta) = session_meta(conn)? else {
            return Ok(None);
        };
        meta
    };
    let bytes = {
        let _perf = smelt_perf::perf::begin("store:session:meta_sidecar_encode");
        serde_json::to_vec_pretty(&meta)?
    };
    {
        let _perf = smelt_perf::perf::begin("store:session:meta_sidecar_write");
        fs::write(path, bytes)?;
    }
    Ok(Some(meta))
}

fn optional_json_string(value: &Option<serde_json::Value>) -> Result<Option<String>> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn parse_optional_json(value: Option<String>) -> rusqlite::Result<Option<serde_json::Value>> {
    value
        .map(|text| serde_json::from_str(&text).map_err(to_sql_error))
        .transpose()
}
