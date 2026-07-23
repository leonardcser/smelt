use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{to_sql_error, Result, StoreError};
use crate::object::checked_i64;
use crate::schema::SCHEMA_VERSION;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionIdentity {
    pub id: String,
    pub created_at: i64,
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionCostUsd(f64);

impl SessionCostUsd {
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() || value < 0.0 {
            return Err(StoreError::Integrity(format!(
                "session_cost_usd must be finite and nonnegative, got {value}"
            )));
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }

    pub const fn get(self) -> f64 {
        self.0
    }

    pub const fn normalized_bits(self) -> u64 {
        self.0.to_bits()
    }
}

impl Serialize for SessionCostUsd {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for SessionCostUsd {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    pub accounting_json: Option<serde_json::Value>,
    pub checkpoint_json: Option<serde_json::Value>,
    pub context_tokens: Option<u64>,
    pub context_tokens_history_len: Option<u64>,
    pub display_context_tokens: Option<u64>,
    pub session_cost_usd: SessionCostUsd,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PersistedSession {
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub revision: u64,
    pub history_len: u64,
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

pub(crate) fn write_session(
    conn: &Connection,
    identity: &SessionIdentity,
    metadata: &SessionMetadata,
    revision: u64,
    history_len: u64,
    transcript_record_count: u64,
) -> Result<()> {
    validate_session_checkpoint(metadata, history_len)?;
    let accounting_json = optional_json_string(&metadata.accounting_json)?;
    let checkpoint_json = optional_json_string(&metadata.checkpoint_json)?;
    conn.execute(
        "INSERT INTO session_state (
            singleton, id, title, slug, first_user_message, cwd, mode, reasoning_effort,
            model, fast_mode, parent_id, accounting_json, checkpoint_json, context_tokens,
            context_tokens_history_len, display_context_tokens, session_cost_usd,
            revision, history_len, transcript_record_count, created_at, updated_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
         ON CONFLICT(singleton) DO UPDATE SET
            title = excluded.title,
            slug = excluded.slug,
            first_user_message = excluded.first_user_message,
            cwd = excluded.cwd,
            mode = excluded.mode,
            reasoning_effort = excluded.reasoning_effort,
            model = excluded.model,
            fast_mode = excluded.fast_mode,
            accounting_json = excluded.accounting_json,
            checkpoint_json = excluded.checkpoint_json,
            context_tokens = excluded.context_tokens,
            context_tokens_history_len = excluded.context_tokens_history_len,
            display_context_tokens = excluded.display_context_tokens,
            session_cost_usd = excluded.session_cost_usd,
            revision = excluded.revision,
            history_len = excluded.history_len,
            transcript_record_count = excluded.transcript_record_count,
            updated_at = excluded.updated_at",
        params![
            &identity.id,
            &metadata.title,
            &metadata.slug,
            &metadata.first_user_message,
            &metadata.cwd,
            &metadata.mode,
            &metadata.reasoning_effort,
            &metadata.model,
            metadata.fast_mode,
            &identity.parent_id,
            &accounting_json,
            &checkpoint_json,
            metadata
                .context_tokens
                .map(|tokens| checked_i64(tokens, "context_tokens"))
                .transpose()?,
            metadata
                .context_tokens_history_len
                .map(|len| checked_i64(len, "context_tokens_history_len"))
                .transpose()?,
            metadata
                .display_context_tokens
                .map(|tokens| checked_i64(tokens, "display_context_tokens"))
                .transpose()?,
            metadata.session_cost_usd.get(),
            checked_i64(revision, "revision")?,
            checked_i64(history_len, "history_len")?,
            checked_i64(transcript_record_count, "transcript_record_count")?,
            identity.created_at,
            metadata.updated_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn stored_session(conn: &Connection) -> Result<Option<PersistedSession>> {
    let persisted = conn
        .query_row(
            "SELECT id, title, slug, first_user_message, cwd, mode, reasoning_effort,
                    model, fast_mode, parent_id, accounting_json, checkpoint_json, context_tokens,
                    context_tokens_history_len, display_context_tokens, session_cost_usd,
                    revision, history_len, created_at, updated_at
             FROM session_state
             WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<bool>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, f64>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        title,
        slug,
        first_user_message,
        cwd,
        mode,
        reasoning_effort,
        model,
        fast_mode,
        parent_id,
        accounting_json,
        checkpoint_json,
        context_tokens,
        context_tokens_history_len,
        display_context_tokens,
        session_cost_usd,
        revision,
        history_len,
        created_at,
        updated_at,
    )) = persisted
    else {
        return Ok(None);
    };
    let revision = nonnegative_u64(revision, "revision")?;
    let history_len = nonnegative_u64(history_len, "history_len")?;
    let stored = PersistedSession {
        identity: SessionIdentity {
            id,
            created_at,
            parent_id,
        },
        metadata: SessionMetadata {
            title,
            slug,
            first_user_message,
            cwd,
            mode,
            reasoning_effort,
            model,
            fast_mode,
            accounting_json: parse_optional_json(accounting_json).map_err(StoreError::from)?,
            checkpoint_json: parse_optional_json(checkpoint_json).map_err(StoreError::from)?,
            context_tokens: optional_nonnegative_u64(context_tokens, "context_tokens")?,
            context_tokens_history_len: optional_nonnegative_u64(
                context_tokens_history_len,
                "context_tokens_history_len",
            )?,
            display_context_tokens: optional_nonnegative_u64(
                display_context_tokens,
                "display_context_tokens",
            )?,
            session_cost_usd: SessionCostUsd::new(session_cost_usd)?,
            updated_at,
        },
        revision,
        history_len,
    };
    Ok(Some(stored))
}

pub(crate) fn session_meta(conn: &Connection) -> Result<Option<SessionMeta>> {
    Ok(stored_session(conn)?.map(|session| SessionMeta {
        id: session.identity.id,
        title: session.metadata.title,
        slug: session.metadata.slug,
        first_user_message: session.metadata.first_user_message,
        cwd: session.metadata.cwd,
        mode: session.metadata.mode,
        reasoning_effort: session.metadata.reasoning_effort,
        model: session.metadata.model,
        fast_mode: session.metadata.fast_mode,
        parent_id: session.identity.parent_id,
        context_tokens: session.metadata.context_tokens,
        revision: session.revision,
        history_len: session.history_len,
        updated_at: session.metadata.updated_at,
        schema_version: SCHEMA_VERSION,
    }))
}

// COMPAT(session-checkpoint-live-index-past-history): repair checkpoints saved
// with a live tail start beyond the retained SQLite history. Keeping the summary
// and replaying all retained rows preserves the most context available.
pub(crate) fn repaired_checkpoint_metadata(
    conn: &Connection,
) -> Result<Option<(PersistedSession, SessionMetadata)>> {
    let Some(session) = stored_session(conn)? else {
        return Ok(None);
    };
    let Some(mut checkpoint_json) = session.metadata.checkpoint_json.clone() else {
        return Ok(None);
    };
    let Some(first_live_index) = checkpoint_first_live_index(&checkpoint_json) else {
        return Ok(None);
    };
    let retained_history_len = session.history_len.min(history_item_count(conn)?);
    if first_live_index <= retained_history_len {
        return Ok(None);
    }
    let Some(object) = checkpoint_json.as_object_mut() else {
        return Ok(None);
    };
    object.insert("first_live_index".to_string(), serde_json::json!(0));
    let mut metadata = session.metadata.clone();
    metadata.checkpoint_json = Some(checkpoint_json);
    Ok(Some((session, metadata)))
}

fn history_item_count(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM history_items", [], |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

pub(crate) fn validate_session_checkpoint(
    metadata: &SessionMetadata,
    history_len: u64,
) -> Result<()> {
    let Some(first_live_index) = metadata
        .checkpoint_json
        .as_ref()
        .and_then(checkpoint_first_live_index)
    else {
        return Ok(());
    };
    if first_live_index > history_len {
        return Err(StoreError::Integrity(format!(
            "checkpoint first_live_index {first_live_index} exceeds history_len {history_len}"
        )));
    }
    Ok(())
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("{field} must be nonnegative, got {value}")))
}

fn optional_nonnegative_u64(value: Option<i64>, field: &str) -> Result<Option<u64>> {
    value.map(|value| nonnegative_u64(value, field)).transpose()
}

fn checkpoint_first_live_index(value: &serde_json::Value) -> Option<u64> {
    value
        .get("first_live_index")
        .and_then(serde_json::Value::as_u64)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cost_rejects_negative_and_non_finite_values() {
        for value in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(SessionCostUsd::new(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn session_cost_normalizes_negative_zero() {
        let negative_zero = SessionCostUsd::new(-0.0).unwrap();
        let positive_zero = SessionCostUsd::new(0.0).unwrap();

        assert_eq!(negative_zero, positive_zero);
        assert_eq!(negative_zero.get().to_bits(), 0.0f64.to_bits());
        assert_eq!(negative_zero.normalized_bits(), 0.0f64.to_bits());
    }
}
