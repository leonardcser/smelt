use std::{io::Write, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    HistoryIndex, HistorySuffix, SessionCommitFailure, SessionCostUsd, SessionIdentity,
    SessionMetadata, StoreHead, TranscriptRecordSuffix,
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueEdit<T> {
    #[default]
    Retain,
    Replace {
        value: Option<T>,
    },
}

/// Edits one archive independently. Retained extents count archive rows, not history items.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArchiveEdit<T> {
    #[default]
    Retain,
    ReplaceSuffix {
        retain_records: u64,
        records: Vec<T>,
    },
}

/// A string summary supplied once or shared from a verified checkpoint record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointSummary {
    New {
        text: Arc<str>,
    },
    BaseCheckpoint,
    /// Ordinal in the exact archive base, independently of the retained prefix.
    BaseEvent {
        record: u64,
    },
    /// The prepared active checkpoint. Only timeline records may use this source.
    Checkpoint,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRecord {
    /// Object fields excluding `summary`, including any unknown header fields.
    pub fields: Value,
    pub summary: CheckpointSummary,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointEdit {
    #[default]
    Retain,
    Replace {
        value: Option<Value>,
    },
    ReplaceRecord {
        record: CheckpointRecord,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointEventsEdit {
    #[default]
    Retain,
    Clear,
    ReplaceSuffix {
        retain_records: u64,
        records: Vec<Value>,
    },
    ReplaceRecordsSuffix {
        retain_records: u64,
        records: Vec<CheckpointRecord>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionArchiveBase {
    pub lineage_id: String,
    pub revision_id: String,
    /// Sequence in the destination branch, including for revisions inherited by a fork.
    pub branch_sequence: crate::Revision,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContextIdentity {
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub provider_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAccounting {
    pub session_usage: SessionTokenUsage,
    pub context_token_identity: Option<SessionContextIdentity>,
    pub display_context_token_identity: Option<SessionContextIdentity>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionScalars {
    pub title: Option<String>,
    pub slug: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub fast_mode: Option<bool>,
    pub accounting: ValueEdit<SessionAccounting>,
    pub context_tokens: Option<u64>,
    pub context_tokens_history_len: Option<u64>,
    pub display_context_tokens: Option<u64>,
    pub session_cost_usd: SessionCostUsd,
    pub updated_at: i64,
}

impl SessionScalars {
    pub(crate) fn metadata(
        &self,
        retained_accounting: Option<Value>,
    ) -> crate::Result<SessionMetadata> {
        let accounting_json = match &self.accounting {
            ValueEdit::Retain => retained_accounting,
            ValueEdit::Replace { value } => value.as_ref().map(serde_json::to_value).transpose()?,
        };
        Ok(SessionMetadata {
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: None,
            cwd: self.cwd.clone(),
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            model: self.model.clone(),
            fast_mode: self.fast_mode,
            accounting_json,
            checkpoint_json: None,
            checkpoint_events_json: None,
            context_tokens: self.context_tokens,
            context_tokens_history_len: self.context_tokens_history_len,
            display_context_tokens: self.display_context_tokens,
            session_cost_usd: self.session_cost_usd,
            updated_at: self.updated_at,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRow {
    pub index: HistoryIndex,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataMessage {
    None,
    /// Shares the prepared active-message payload without reading it. An absent active
    /// message remains absent; a present empty message remains present.
    Active,
    New {
        text: Arc<str>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataArchiveRow {
    pub index: HistoryIndex,
    pub fields: Value,
    pub message: MetadataMessage,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactSessionArchives {
    pub first_user_message: ValueEdit<Arc<str>>,
    pub checkpoint: CheckpointEdit,
    pub checkpoint_events: CheckpointEventsEdit,
    pub turn_metas: ArchiveEdit<ArchiveRow>,
    pub metadata_snapshots: ArchiveEdit<MetadataArchiveRow>,
    pub context_snapshots: ArchiveEdit<ArchiveRow>,
}

/// Archive edits always refer to `archive_base`, independently of the expected head.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactSessionCommit {
    pub session_id: String,
    pub expected: StoreHead,
    pub identity: SessionIdentity,
    pub scalars: SessionScalars,
    pub archive_base: Option<SessionArchiveBase>,
    pub archives: CompactSessionArchives,
    pub history: HistorySuffix,
    pub transcript_records: Option<TranscriptRecordSuffix>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactSubmitTurn {
    pub session: CompactSessionCommit,
    pub turn: crate::NewTurn,
}

/// The submission receipt owns the exact session result, independently of later heads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactSubmitTurnResult {
    pub session: crate::SessionCommitResult,
    pub turn_id: crate::TurnId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactTurnTransition {
    pub session: CompactSessionCommit,
    pub turn_id: crate::TurnId,
    pub state: crate::TurnState,
    pub at_ms: u64,
    pub terminal_reason: Option<String>,
}

/// The transition receipt owns the exact session result, including for no-op saves.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactTurnTransitionResult {
    pub session: crate::SessionCommitResult,
    pub turn_id: crate::TurnId,
    pub state: crate::TurnState,
}

pub(crate) fn validate_compact_session_commit(
    command: &CompactSessionCommit,
) -> std::result::Result<(), SessionCommitFailure> {
    use crate::session_command::{commit_failure_from_store_error, validate_coordinate};
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
    for (value, field) in [
        (command.scalars.context_tokens, "context_tokens"),
        (
            command.scalars.context_tokens_history_len,
            "context_tokens_history_len",
        ),
        (
            command.scalars.display_context_tokens,
            "display_context_tokens",
        ),
    ] {
        if let Some(value) = value {
            validate_coordinate(value, field)?;
        }
    }
    if command.identity.created_at < 0 || command.scalars.updated_at < 0 {
        return Err(SessionCommitFailure::InvalidCommand {
            message: "session timestamps must be nonnegative".into(),
        });
    }
    if let Some(base) = &command.archive_base {
        crate::lineage::validate_lower_hex(&base.lineage_id, 32, "archive base lineage")
            .map_err(commit_failure_from_store_error)?;
        crate::lineage::validate_lower_hex(&base.revision_id, 64, "archive base revision")
            .map_err(commit_failure_from_store_error)?;
        validate_coordinate(base.branch_sequence.get(), "archive base branch sequence")?;
        if base.branch_sequence == crate::Revision::ZERO {
            return Err(SessionCommitFailure::InvalidCommand {
                message: "archive base branch sequence must be nonzero".into(),
            });
        }
    }
    crate::session_command::validate_history_suffix(command.expected, &command.history)?;
    crate::session_command::validate_transcript_suffix(
        command.expected,
        command.history.final_len,
        command.transcript_records.as_ref(),
    )?;
    let final_len = command.history.final_len.get();
    for edit in [
        &command.archives.turn_metas,
        &command.archives.context_snapshots,
    ] {
        if let ArchiveEdit::ReplaceSuffix {
            retain_records,
            records,
        } = edit
        {
            validate_coordinate(*retain_records, "retained archive rows")?;
            validate_indices(records.iter().map(|row| row.index), final_len)?;
        }
    }
    if let ArchiveEdit::ReplaceSuffix {
        retain_records,
        records,
    } = &command.archives.metadata_snapshots
    {
        validate_coordinate(*retain_records, "retained metadata rows")?;
        validate_indices(records.iter().map(|row| row.index), final_len)?;
        for row in records {
            let inline = row.fields.get("first_user_message");
            if (!matches!(row.message, MetadataMessage::None) && inline.is_some())
                || inline.is_some_and(Value::is_string)
            {
                return Err(SessionCommitFailure::InvalidCommand {
                    message: "metadata archive fields contain an inline first message".into(),
                });
            }
            if !matches!(row.message, MetadataMessage::None) && !row.fields.is_object() {
                return Err(SessionCommitFailure::InvalidCommand {
                    message: "metadata message requires object fields".into(),
                });
            }
        }
    }
    let checkpoint_fields = match &command.archives.checkpoint {
        CheckpointEdit::Replace { value } => value.as_ref(),
        CheckpointEdit::ReplaceRecord { record } => {
            validate_checkpoint_record(record)?;
            if matches!(record.summary, CheckpointSummary::Checkpoint) {
                return Err(SessionCommitFailure::InvalidCommand {
                    message: "active checkpoint cannot reference itself".into(),
                });
            }
            Some(&record.fields)
        }
        CheckpointEdit::Retain => None,
    };
    if let Some(value) = checkpoint_fields {
        if value
            .get("first_live_index")
            .and_then(Value::as_u64)
            .is_some_and(|index| index > final_len)
        {
            return Err(SessionCommitFailure::InvalidCommand {
                message: "checkpoint exceeds final history length".into(),
            });
        }
    }
    if let CheckpointEventsEdit::ReplaceSuffix {
        retain_records,
        records,
    } = &command.archives.checkpoint_events
    {
        validate_coordinate(*retain_records, "retained checkpoint events")?;
        crate::meta::validate_checkpoint_events(records, final_len)
            .map_err(commit_failure_from_store_error)?;
    }
    if let CheckpointEventsEdit::ReplaceRecordsSuffix {
        retain_records,
        records,
    } = &command.archives.checkpoint_events
    {
        validate_coordinate(*retain_records, "retained checkpoint events")?;
        for record in records {
            validate_checkpoint_record(record)?;
        }
        crate::meta::validate_checkpoint_event_fields(
            records.iter().map(|record| &record.fields),
            final_len,
        )
        .map_err(commit_failure_from_store_error)?;
    }
    Ok(())
}

fn validate_checkpoint_record(
    record: &CheckpointRecord,
) -> std::result::Result<(), SessionCommitFailure> {
    if !record.fields.is_object() || record.fields.get("summary").is_some() {
        return Err(SessionCommitFailure::InvalidCommand {
            message: "checkpoint record requires object fields without an inline summary".into(),
        });
    }
    if let CheckpointSummary::BaseEvent { record } = record.summary {
        crate::session_command::validate_coordinate(record, "checkpoint summary source ordinal")?;
    }
    Ok(())
}

fn validate_indices(
    indices: impl Iterator<Item = HistoryIndex>,
    final_len: u64,
) -> std::result::Result<(), SessionCommitFailure> {
    let mut previous = None;
    for index in indices {
        crate::session_command::validate_coordinate(index.get(), "archive index")?;
        if index.get() > final_len || previous.is_some_and(|previous| previous >= index) {
            return Err(SessionCommitFailure::InvalidCommand { message: "archive suffix indices must be strictly increasing and fit final history length".into() });
        }
        previous = Some(index);
    }
    Ok(())
}

struct FingerprintWriter {
    hash: Sha256,
    bytes: u64,
}

impl Write for FingerprintWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.hash.update(bytes);
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn compact_session_commit_fingerprint(
    command: &CompactSessionCommit,
) -> std::result::Result<String, SessionCommitFailure> {
    validate_compact_session_commit(command)?;
    compact_fingerprint(
        command,
        b"smelt-session-commit-v2\0",
        "store:compact_commit:fingerprint_bytes",
    )
}

pub fn compact_submit_turn_fingerprint(
    command: &CompactSubmitTurn,
) -> std::result::Result<String, SessionCommitFailure> {
    validate_compact_session_commit(&command.session)?;
    crate::session_command::validate_new_turn(&command.turn, command.session.history.final_len)?;
    compact_fingerprint(
        command,
        b"smelt-submit-turn-v2\0",
        "store:compact_submit:fingerprint_bytes",
    )
}

pub fn compact_turn_transition_fingerprint(
    command: &CompactTurnTransition,
) -> std::result::Result<String, SessionCommitFailure> {
    validate_compact_session_commit(&command.session)?;
    crate::session_command::validate_turn_transition_fields(
        command.turn_id,
        command.state,
        command.at_ms,
        command.terminal_reason.as_deref(),
    )?;
    compact_fingerprint(
        command,
        b"smelt-turn-transition-v2\0",
        "store:compact_transition:fingerprint_bytes",
    )
}

fn compact_fingerprint(
    command: &impl Serialize,
    domain: &[u8],
    metric: &'static str,
) -> std::result::Result<String, SessionCommitFailure> {
    let mut writer = FingerprintWriter {
        hash: Sha256::new(),
        bytes: 0,
    };
    writer.hash.update(domain);
    let value = serde_json::to_value(command)
        .map_err(crate::StoreError::from)
        .map_err(crate::session_command::commit_failure_from_store_error)?;
    crate::session_command::write_canonical_json(&value, &mut writer)
        .map_err(crate::session_command::commit_failure_from_store_error)?;
    smelt_perf::perf::record_value(metric, writer.bytes);
    Ok(crate::object::hex_lower(&writer.hash.finalize()))
}
