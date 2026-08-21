use std::collections::BTreeMap;
use std::io::Write;

use crate::error::{Result, StoreError};
use crate::meta::{self, SessionMetadata};
use crate::session_commit::{
    HistoryIndex, HistoryIndexBound, HistoryLen, NewTurn, SessionCommit, SessionCommitFailure,
    SubmitTurn, TurnId, TurnKind, TurnState, TurnTransition,
};

const MAX_TERMINAL_REASON_BYTES: usize = 1024;

pub(crate) fn validate_session_commit(
    command: &SessionCommit,
) -> std::result::Result<(), SessionCommitFailure> {
    prepare_session_commit(command).map(|_| ())
}

pub fn session_commit_fingerprint(
    command: &SessionCommit,
) -> std::result::Result<String, SessionCommitFailure> {
    let side_tables = prepare_session_commit(command)?;
    canonical_session_commit_fingerprint(command, &side_tables)
        .map_err(commit_failure_from_store_error)
}

pub fn submit_turn_fingerprint(
    command: &SubmitTurn,
) -> std::result::Result<String, SessionCommitFailure> {
    let side_tables = prepare_session_commit(&command.session)?;
    validate_new_turn(&command.turn, command.session.history.final_len)?;
    let mut encoder = CanonicalEncoder::new(b"smelt-submit-turn-v1\0");
    encode_session_commit(&mut encoder, &command.session, &side_tables)
        .map_err(commit_failure_from_store_error)?;
    encode_new_turn(&mut encoder, &command.turn);
    Ok(crate::object::sha256_hex(&encoder.finish()))
}

pub fn turn_transition_fingerprint(
    command: &TurnTransition,
) -> std::result::Result<String, SessionCommitFailure> {
    let side_tables = prepare_session_commit(&command.session)?;
    validate_turn_transition(command)?;
    let mut encoder = CanonicalEncoder::new(b"smelt-turn-transition-v1\0");
    encode_session_commit(&mut encoder, &command.session, &side_tables)
        .map_err(commit_failure_from_store_error)?;
    encoder.u64(command.turn_id.get());
    encoder.string(command.state.as_str());
    encoder.u64(command.at_ms);
    encoder.optional_string(command.terminal_reason.as_deref());
    Ok(crate::object::sha256_hex(&encoder.finish()))
}

pub(crate) fn validate_new_turn(
    turn: &NewTurn,
    final_history_len: HistoryLen,
) -> std::result::Result<(), SessionCommitFailure> {
    validate_coordinate(turn.submitted_history_idx.get(), "submitted history index")?;
    validate_coordinate(turn.created_at_ms, "turn creation timestamp")?;
    if turn.submitted_history_idx.get() >= final_history_len.get() {
        return Err(SessionCommitFailure::InvalidTurn {
            message: format!(
                "submitted history index {} is outside final history length {}",
                turn.submitted_history_idx.get(),
                final_history_len.get()
            ),
        });
    }
    if let Some(continuation_of) = turn.continuation_of {
        validate_turn_id(continuation_of)?;
    }
    if matches!(turn.kind, TurnKind::Continuation) != turn.continuation_of.is_some() {
        return Err(SessionCommitFailure::InvalidTurn {
            message: "continuation turns require exactly one continuation target".into(),
        });
    }
    Ok(())
}

pub(crate) fn validate_turn_transition(
    command: &TurnTransition,
) -> std::result::Result<(), SessionCommitFailure> {
    validate_turn_id(command.turn_id)?;
    validate_coordinate(command.at_ms, "turn transition timestamp")?;
    if command.state == TurnState::Ready {
        return Err(SessionCommitFailure::InvalidTurn {
            message: "a transition cannot target ready".into(),
        });
    }
    if command.state == TurnState::Running && command.terminal_reason.is_some() {
        return Err(SessionCommitFailure::InvalidTurn {
            message: "a running transition cannot have a terminal reason".into(),
        });
    }
    if command
        .terminal_reason
        .as_ref()
        .is_some_and(|reason| reason.len() > MAX_TERMINAL_REASON_BYTES)
    {
        return Err(SessionCommitFailure::InvalidTurn {
            message: format!("terminal reason exceeds {MAX_TERMINAL_REASON_BYTES} UTF-8 bytes"),
        });
    }
    Ok(())
}

pub(crate) fn commit_failure_from_store_error(error: StoreError) -> SessionCommitFailure {
    match error {
        StoreError::OwnershipLost | StoreError::OwnershipConflict { .. } => {
            SessionCommitFailure::OwnershipLost
        }
        StoreError::Cancelled => SessionCommitFailure::InvalidCommand {
            message: "operation cancelled".into(),
        },
        StoreError::Busy {
            operation,
            attempts,
            waited_ms,
        } => SessionCommitFailure::Busy {
            operation: operation.to_owned(),
            attempts,
            waited_ms,
        },
        StoreError::UnsupportedSchema { found, expected } => {
            SessionCommitFailure::UnsupportedSchema { found, expected }
        }
        StoreError::Json(error) => SessionCommitFailure::InvalidCommand {
            message: error.to_string(),
        },
        StoreError::ObjectTooLarge { size, max } => SessionCommitFailure::InvalidCommand {
            message: format!("session object is too large: {size} bytes exceeds {max}"),
        },
        StoreError::Integrity(message) | StoreError::MissingObject { reference: message } => {
            SessionCommitFailure::Integrity { message }
        }
        StoreError::Io(error) => SessionCommitFailure::Io {
            message: error.to_string(),
        },
        StoreError::Sqlite(error) => SessionCommitFailure::Sqlite {
            message: error.to_string(),
        },
        StoreError::TransactionCleanup { operation, message } => SessionCommitFailure::Sqlite {
            message: format!("transaction cleanup failed during {operation}: {message}"),
        },
        StoreError::OperationCleanup {
            operation,
            primary,
            cleanup,
        } => SessionCommitFailure::Sqlite {
            message: format!(
                "{operation} failed: {primary}; cleanup also failed: {}",
                cleanup
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        },
    }
}

#[derive(Debug)]
struct PreparedSideTables {
    start: u64,
    turn_metas: Vec<(u64, serde_json::Value)>,
    metadata_snapshots: Vec<(u64, serde_json::Value)>,
    context_snapshots: Vec<(u64, serde_json::Value)>,
}

fn prepare_session_commit(
    command: &SessionCommit,
) -> std::result::Result<PreparedSideTables, SessionCommitFailure> {
    if command.identity.id != command.session_id {
        return Err(SessionCommitFailure::SessionMismatch {
            expected: command.session_id.clone(),
            actual: Some(command.identity.id.clone()),
        });
    }
    for (value, field) in [
        (command.expected.revision.get(), "expected revision"),
        (
            command.expected.history_len.get(),
            "expected history length",
        ),
        (
            command.expected.transcript_record_count.get(),
            "expected record length",
        ),
        (command.history.start.get(), "history start"),
        (command.history.final_len.get(), "history final length"),
    ] {
        validate_coordinate(value, field)?;
    }
    validate_metadata_coordinates(&command.metadata)?;
    meta::validate_session_checkpoint(&command.metadata, command.history.final_len.get())
        .map_err(commit_failure_from_store_error)?;

    let start =
        command
            .history
            .start
            .as_usize()
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: format!(
                    "history index {} does not fit usize",
                    command.history.start.get()
                ),
            })?;
    let final_len =
        command
            .history
            .final_len
            .as_usize()
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: format!(
                    "history length {} does not fit usize",
                    command.history.final_len.get()
                ),
            })?;
    if command.history.start.get() > command.expected.history_len.get() {
        return Err(SessionCommitFailure::InvalidHistorySuffixStart {
            start: command.history.start,
            current_len: command.expected.history_len,
        });
    }
    if start.checked_add(command.history.items.len()) != Some(final_len) {
        return Err(SessionCommitFailure::InvalidHistorySuffix {
            start: command.history.start,
            final_len: command.history.final_len,
            item_count: u64::try_from(command.history.items.len()).unwrap_or(u64::MAX),
        });
    }

    let side_tables = prepare_side_tables(command)?;
    if let Some(suffix) = &command.transcript_records {
        validate_coordinate(suffix.start.get(), "record start")?;
        if suffix.start.get() > command.expected.transcript_record_count.get() {
            return Err(SessionCommitFailure::InvalidTranscriptRecordSuffix {
                start: suffix.start,
                current_len: command.expected.transcript_record_count,
            });
        }
        let final_len = suffix
            .start
            .get()
            .checked_add(u64::try_from(suffix.records.len()).map_err(|_| {
                SessionCommitFailure::Integrity {
                    message: "record item count exceeds u64".into(),
                }
            })?)
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: "record suffix length overflows u64".into(),
            })?;
        validate_coordinate(final_len, "record final length")?;
        for record in &suffix.records {
            validate_coordinate(record.block_idx, "record block index")?;
            if let Some(history_idx) = record.history_idx {
                validate_coordinate(history_idx, "record history index")?;
                if history_idx >= command.history.final_len.get() {
                    return Err(SessionCommitFailure::Integrity {
                        message: format!(
                            "transcript record history link {history_idx} is outside final history length {}",
                            command.history.final_len.get()
                        ),
                    });
                }
            }
            validate_coordinate(record.estimated_text_bytes, "record estimated text bytes")?;
            serde_json::from_str::<serde_json::Value>(&record.block_json)
                .map_err(StoreError::from)
                .map_err(commit_failure_from_store_error)?;
            if let Some(origin_json) = &record.origin_json {
                serde_json::from_str::<serde_json::Value>(origin_json)
                    .map_err(StoreError::from)
                    .map_err(commit_failure_from_store_error)?;
            }
            if let Some(tool_state_json) = &record.tool_state_json {
                serde_json::from_str::<serde_json::Value>(tool_state_json)
                    .map_err(StoreError::from)
                    .map_err(commit_failure_from_store_error)?;
            }
        }
    }
    Ok(side_tables)
}

fn prepare_side_tables(
    command: &SessionCommit,
) -> std::result::Result<PreparedSideTables, SessionCommitFailure> {
    let start = command.side_tables.start;
    validate_coordinate(start.get(), "side-table start")?;
    if start.get() > command.history.final_len.get() {
        return Err(SessionCommitFailure::InvalidSideTableSuffix {
            start,
            final_len: command.history.final_len,
        });
    }
    for (name, rows) in [
        ("turn_metas", command.side_tables.turn_metas.as_slice()),
        (
            "metadata_snapshots",
            command.side_tables.metadata_snapshots.as_slice(),
        ),
        (
            "accounting_snapshots",
            command.side_tables.context_snapshots.as_slice(),
        ),
    ] {
        validate_side_table_rows(name, rows, command.history.final_len)?;
    }
    Ok(PreparedSideTables {
        start: start.get(),
        turn_metas: normalize_side_table_rows(&command.side_tables.turn_metas, start.get())
            .map_err(commit_failure_from_store_error)?,
        metadata_snapshots: normalize_side_table_rows(
            &command.side_tables.metadata_snapshots,
            start.get(),
        )
        .map_err(commit_failure_from_store_error)?,
        context_snapshots: normalize_side_table_rows(
            &command.side_tables.context_snapshots,
            start.get(),
        )
        .map_err(commit_failure_from_store_error)?,
    })
}

fn normalize_side_table_rows(
    rows: &[(HistoryIndex, serde_json::Value)],
    start: u64,
) -> Result<Vec<(u64, serde_json::Value)>> {
    rows.iter()
        .map(|(index, value)| {
            i64::try_from(index.get()).map_err(|_| {
                StoreError::Integrity("side-table row index exceeds SQLite integer range".into())
            })?;
            Ok((index.get(), value.clone()))
        })
        .collect::<Result<BTreeMap<_, _>>>()
        .map(|rows| {
            rows.into_iter()
                .filter(|(index, _)| *index >= start)
                .collect()
        })
}

fn validate_side_table_rows(
    table: &str,
    rows: &[(HistoryIndex, serde_json::Value)],
    final_len: HistoryLen,
) -> std::result::Result<(), SessionCommitFailure> {
    for (index, _) in rows {
        if index.get() > final_len.get() {
            return Err(SessionCommitFailure::InvalidSideTableRow {
                table: table.to_owned(),
                index: *index,
                final_len,
                bound: HistoryIndexBound::AtOrBeforeFinalLen,
            });
        }
    }
    Ok(())
}

fn validate_metadata_coordinates(
    metadata: &SessionMetadata,
) -> std::result::Result<(), SessionCommitFailure> {
    for (value, field) in [
        (metadata.context_tokens, "context_tokens"),
        (
            metadata.context_tokens_history_len,
            "context_tokens_history_len",
        ),
        (metadata.display_context_tokens, "display_context_tokens"),
    ] {
        if let Some(value) = value {
            validate_coordinate(value, field)?;
        }
    }
    Ok(())
}

fn validate_coordinate(value: u64, field: &str) -> std::result::Result<(), SessionCommitFailure> {
    i64::try_from(value).map_err(|_| SessionCommitFailure::Integrity {
        message: format!("{field} exceeds SQLite integer range"),
    })?;
    usize::try_from(value).map_err(|_| SessionCommitFailure::Integrity {
        message: format!("{field} exceeds platform limits"),
    })?;
    Ok(())
}

fn validate_turn_id(turn_id: TurnId) -> std::result::Result<(), SessionCommitFailure> {
    let value = i64::try_from(turn_id.get()).map_err(|_| SessionCommitFailure::InvalidTurn {
        message: "turn ID exceeds SQLite integer range".into(),
    })?;
    if value <= 0 {
        return Err(SessionCommitFailure::InvalidTurn {
            message: "turn ID must be positive".into(),
        });
    }
    Ok(())
}

fn canonical_session_commit_fingerprint(
    command: &SessionCommit,
    side_tables: &PreparedSideTables,
) -> Result<String> {
    let mut encoder = CanonicalEncoder::new(b"smelt-session-commit-v1\0");
    encode_session_commit(&mut encoder, command, side_tables)?;
    Ok(crate::object::sha256_hex(&encoder.finish()))
}

fn encode_new_turn(encoder: &mut CanonicalEncoder, turn: &NewTurn) {
    encoder.string(turn.kind.as_str());
    encoder.u64(turn.submitted_history_idx.get());
    encoder.optional_u64(turn.continuation_of.map(TurnId::get));
    encoder.u64(turn.created_at_ms);
}

fn encode_session_commit(
    encoder: &mut CanonicalEncoder,
    command: &SessionCommit,
    side_tables: &PreparedSideTables,
) -> Result<()> {
    encoder.string(&command.session_id);
    encoder.u64(command.expected.revision.get());
    encoder.u64(command.expected.history_len.get());
    encoder.u64(command.expected.transcript_record_count.get());
    encoder.string(&command.identity.id);
    encoder.i64(command.identity.created_at);
    encoder.optional_string(command.identity.parent_id.as_deref());
    encode_session_metadata(encoder, &command.metadata)?;
    encoder.u64(command.history.start.get());
    encoder.u64(command.history.final_len.get());
    encoder.u64(command.history.items.len() as u64);
    for item in &command.history.items {
        encoder.json(item)?;
    }
    encoder.u64(side_tables.start);
    encode_side_table_rows(encoder, &side_tables.turn_metas)?;
    encode_side_table_rows(encoder, &side_tables.metadata_snapshots)?;
    encode_side_table_rows(encoder, &side_tables.context_snapshots)?;
    match &command.transcript_records {
        Some(suffix) => {
            encoder.bool(true);
            encoder.u64(suffix.start.get());
            encoder.u64(suffix.records.len() as u64);
            for record in &suffix.records {
                encoder.u64(record.block_idx);
                encoder.optional_u64(record.history_idx);
                encoder.string(&record.kind);
                encoder.optional_string(record.tool_call_id.as_deref());
                encoder.optional_string(record.tool_name.as_deref());
                encoder.string(&record.content_hash);
                encoder.u64(record.estimated_text_bytes);
                encoder.string(&record.preview_text);
                encoder.string(&record.indexed_text);
                encoder.json_text(&record.block_json)?;
                encoder.optional_json_text(record.origin_json.as_deref())?;
                encoder.optional_json_text(record.tool_state_json.as_deref())?;
            }
        }
        None => encoder.bool(false),
    }
    Ok(())
}

fn encode_session_metadata(
    encoder: &mut CanonicalEncoder,
    metadata: &SessionMetadata,
) -> Result<()> {
    encoder.optional_string(metadata.title.as_deref());
    encoder.optional_string(metadata.slug.as_deref());
    encoder.optional_string(metadata.first_user_message.as_deref());
    encoder.optional_string(metadata.cwd.as_deref());
    encoder.optional_string(metadata.mode.as_deref());
    encoder.optional_string(metadata.reasoning_effort.as_deref());
    encoder.optional_string(metadata.model.as_deref());
    match metadata.fast_mode {
        Some(value) => {
            encoder.bool(true);
            encoder.bool(value);
        }
        None => encoder.bool(false),
    }
    encoder.optional_json(metadata.accounting_json.as_ref())?;
    encoder.optional_json(metadata.checkpoint_json.as_ref())?;
    encoder.optional_json(metadata.checkpoint_events_json.as_ref())?;
    encoder.optional_u64(metadata.context_tokens);
    encoder.optional_u64(metadata.context_tokens_history_len);
    encoder.optional_u64(metadata.display_context_tokens);
    encoder.u64(metadata.session_cost_usd.normalized_bits());
    encoder.i64(metadata.updated_at);
    Ok(())
}

fn encode_side_table_rows(
    encoder: &mut CanonicalEncoder,
    rows: &[(u64, serde_json::Value)],
) -> Result<()> {
    encoder.u64(rows.len() as u64);
    for (index, value) in rows {
        encoder.u64(*index);
        encoder.json(value)?;
    }
    Ok(())
}

struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn new(version: &[u8]) -> Self {
        Self {
            bytes: version.to_vec(),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.string(value);
            }
            None => self.bool(false),
        }
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.u64(value);
            }
            None => self.bool(false),
        }
    }

    fn json(&mut self, value: &impl serde::Serialize) -> Result<()> {
        let value = serde_json::to_value(value)?;
        let mut bytes = Vec::new();
        write_canonical_json(&value, &mut bytes)?;
        self.u64(bytes.len() as u64);
        self.bytes.extend_from_slice(&bytes);
        Ok(())
    }

    fn optional_json(&mut self, value: Option<&serde_json::Value>) -> Result<()> {
        match value {
            Some(value) => {
                self.bool(true);
                self.json(value)
            }
            None => {
                self.bool(false);
                Ok(())
            }
        }
    }

    fn json_text(&mut self, value: &str) -> Result<()> {
        self.json(&serde_json::from_str::<serde_json::Value>(value)?)
    }

    fn optional_json_text(&mut self, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => {
                self.bool(true);
                self.json_text(value)
            }
            None => {
                self.bool(false);
                Ok(())
            }
        }
    }
}

fn write_canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        serde_json::Value::Number(value) => write!(out, "{value}")?,
        serde_json::Value::String(value) => serde_json::to_writer(out, value)?,
        serde_json::Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(values) => {
            out.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_canonical_json(value, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}
