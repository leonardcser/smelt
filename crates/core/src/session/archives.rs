use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use smelt_store::{
    ArchiveEdit, ArchiveRow, CheckpointEdit, CheckpointEventsEdit, CheckpointRecord,
    CheckpointSummary, CompactSessionArchives, CompactSessionCommit, HistoryIndex, HistorySuffix,
    MetadataArchiveRow, MetadataMessage, SessionAccounting, SessionArchiveBase,
    SessionCommitResult, SessionContextIdentity, SessionScalars, SessionTokenUsage,
    StartupRecoveryResult, StoreError, StoreHead, TranscriptRecordSuffix, ValueEdit,
};

use super::{
    ContextCheckpoint, ContextTokenIdentity, HistorySnapshots, Session,
    SessionContextSnapshotState, SnapshotVersion,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArchiveVersions {
    turn_metas: SnapshotVersion,
    metadata_snapshots: SnapshotVersion,
    context_snapshots: SnapshotVersion,
    checkpoint_events: SnapshotVersion,
}

#[derive(Debug)]
pub(super) struct ArchiveOwner {
    scope: Arc<()>,
    session_id: Option<String>,
    base: Option<SessionArchiveBase>,
    versions: Option<ArchiveVersions>,
    message: Option<Arc<str>>,
    checkpoint: Option<ContextCheckpoint>,
    accounting: Option<SessionContextSnapshotState>,
    checkpoint_events_exact: bool,
}

impl Default for ArchiveOwner {
    fn default() -> Self {
        Self {
            scope: Arc::new(()),
            session_id: None,
            base: None,
            versions: None,
            message: None,
            checkpoint: None,
            accounting: None,
            checkpoint_events_exact: true,
        }
    }
}

impl Clone for ArchiveOwner {
    fn clone(&self) -> Self {
        Self {
            scope: Arc::new(()),
            session_id: self.session_id.clone(),
            base: self.base.clone(),
            versions: self.versions,
            message: self.message.clone(),
            checkpoint: self.checkpoint.clone(),
            accounting: self.accounting.clone(),
            checkpoint_events_exact: self.checkpoint_events_exact,
        }
    }
}

/// A native save and the exact in-memory archive changes it represents.
/// The command is immutable so result acknowledgement cannot be detached from preparation.
#[derive(Clone, Debug)]
pub struct PreparedArchiveSave {
    command: CompactSessionCommit,
    scope: Arc<()>,
    versions: ArchiveVersions,
    message: Option<Arc<str>>,
    checkpoint: Option<ContextCheckpoint>,
    accounting: SessionContextSnapshotState,
}

impl PreparedArchiveSave {
    pub fn command(&self) -> &CompactSessionCommit {
        &self.command
    }

    /// Compare captured document intent independently of runtime archive-base advancement.
    /// Scoped versions and shared body identities avoid traversing unchanged archives.
    pub fn same_snapshot(&self, other: &Self) -> bool {
        let left = &self.command;
        let right = &other.command;
        let SessionScalars {
            title,
            slug,
            cwd,
            mode,
            reasoning_effort,
            model,
            fast_mode,
            accounting: _,
            context_tokens,
            context_tokens_history_len,
            display_context_tokens,
            session_cost_usd,
            updated_at,
        } = &left.scalars;
        let other_scalars = &right.scalars;
        Arc::ptr_eq(&self.scope, &other.scope)
            && self.versions == other.versions
            && same_message(&self.message, &other.message)
            && same_checkpoint(&self.checkpoint, &other.checkpoint)
            && self.accounting == other.accounting
            && left.session_id == right.session_id
            && left.identity == right.identity
            && left.history == right.history
            && left.transcript_records == right.transcript_records
            && title == &other_scalars.title
            && slug == &other_scalars.slug
            && cwd == &other_scalars.cwd
            && mode == &other_scalars.mode
            && reasoning_effort == &other_scalars.reasoning_effort
            && model == &other_scalars.model
            && fast_mode == &other_scalars.fast_mode
            && context_tokens == &other_scalars.context_tokens
            && context_tokens_history_len == &other_scalars.context_tokens_history_len
            && display_context_tokens == &other_scalars.display_context_tokens
            && session_cost_usd == &other_scalars.session_cost_usd
            && updated_at == &other_scalars.updated_at
    }

    pub(crate) fn catalog_message(
        &self,
        result: &SessionCommitResult,
    ) -> Result<Option<Arc<str>>, StoreError> {
        if !self.matches_result(result) {
            return Err(StoreError::Integrity(
                "catalog result does not match its prepared frame".into(),
            ));
        }
        Ok(self.message.clone())
    }

    /// Finalize against a preceding publication from this session instance. The result
    /// must come from publication or exact receipt recovery of `published.command()`.
    /// Publish and acknowledge the returned frame, not the original preparation.
    /// Consuming the frame moves its changed rows and bodies without copying them.
    pub fn finalize_after(
        mut self,
        published: &Self,
        result: &SessionCommitResult,
    ) -> Result<Self, StoreError> {
        let current = result.receipt.current;
        if !Arc::ptr_eq(&self.scope, &published.scope)
            || self.command.session_id != published.command.session_id
            || !published.matches_result(result)
            || current.revision < self.command.expected.revision
            || (current.revision == self.command.expected.revision
                && current != self.command.expected)
            || self.command.archive_base.as_ref().is_some_and(|base| {
                Some(&base.lineage_id) != result.receipt.lineage_id.as_ref()
                    || base.branch_sequence > current.revision
            })
        {
            return Err(StoreError::Integrity(
                "native frame finalization has no matching preceding publication".into(),
            ));
        }
        if current == self.command.expected {
            return Ok(self);
        }
        if self.command.archive_base.is_none() {
            self.command.archive_base = Some(SessionArchiveBase {
                lineage_id: result.receipt.lineage_id.clone().expect("verified lineage"),
                revision_id: result.revision_id.clone(),
                branch_sequence: current.revision,
            });
            // The newly bound source represents the preceding frame's scoped versions
            // and shared bodies. Retain those identities without comparing body bytes.
            let archives = &mut self.command.archives;
            if same_message(&self.message, &published.message) {
                archives.first_user_message = ValueEdit::Retain;
            }
            if same_checkpoint(&self.checkpoint, &published.checkpoint) {
                archives.checkpoint = CheckpointEdit::Retain;
            } else if let (Some(checkpoint), Some(base), CheckpointEdit::ReplaceRecord { record }) = (
                &self.checkpoint,
                &published.checkpoint,
                &mut archives.checkpoint,
            ) {
                if Arc::ptr_eq(&checkpoint.summary, &base.summary) {
                    record.summary = CheckpointSummary::BaseCheckpoint;
                }
            }
            if self.versions.turn_metas == published.versions.turn_metas {
                archives.turn_metas = ArchiveEdit::Retain;
            }
            if self.versions.metadata_snapshots == published.versions.metadata_snapshots {
                archives.metadata_snapshots = ArchiveEdit::Retain;
            }
            if self.versions.context_snapshots == published.versions.context_snapshots {
                archives.context_snapshots = ArchiveEdit::Retain;
            }
            if self.versions.checkpoint_events == published.versions.checkpoint_events {
                archives.checkpoint_events = CheckpointEventsEdit::Retain;
            }
        }
        // Archive references keep their source revision. Scalar retain instead reads
        // the finalized expected head and requires the preceding frame's accounting.
        self.command.scalars.accounting = if self.accounting == published.accounting {
            ValueEdit::Retain
        } else {
            ValueEdit::Replace {
                value: Some(store_accounting(&self.accounting)),
            }
        };
        self.command.expected = current;
        Ok(self)
    }

    fn matches_result(&self, result: &SessionCommitResult) -> bool {
        let command = &self.command;
        let receipt = &result.receipt;
        let Some(record_count) = command.transcript_records.as_ref().map_or(
            Some(command.expected.transcript_record_count.get()),
            |suffix| suffix.start.get().checked_add(suffix.records.len() as u64),
        ) else {
            return false;
        };
        receipt.lineage_id.is_some()
            && receipt.session_id == command.session_id
            && receipt.previous == command.expected
            && receipt.current.history_len == command.history.final_len
            && receipt.current.transcript_record_count.get() == record_count
            && (receipt.previous.revision == receipt.current.revision
                || receipt.previous.revision.checked_add(1) == Some(receipt.current.revision))
            && result.revision_id.len() == 64
            && result
                .revision_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

fn versions(session: &Session) -> ArchiveVersions {
    ArchiveVersions {
        turn_metas: session.turn_metas.version(),
        metadata_snapshots: session.metadata_snapshots.version(),
        context_snapshots: session.context_snapshots.version(),
        checkpoint_events: session.checkpoint_events.version(),
    }
}

fn same_message(left: &Option<Arc<str>>, right: &Option<Arc<str>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn same_checkpoint_header(left: &ContextCheckpoint, right: &ContextCheckpoint) -> bool {
    left.kind == right.kind
        && left.first_live_index == right.first_live_index
        && left.created_at_ms == right.created_at_ms
        && left.tokens_before == right.tokens_before
        && left.tokens_after_estimate == right.tokens_after_estimate
        && left.tokens_after_estimate_history_len == right.tokens_after_estimate_history_len
        && left.pre_checkpoint_context_tokens == right.pre_checkpoint_context_tokens
        && left.pre_checkpoint_context_history_len == right.pre_checkpoint_context_history_len
}

fn same_checkpoint(left: &Option<ContextCheckpoint>, right: &Option<ContextCheckpoint>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            Arc::ptr_eq(&left.summary, &right.summary) && same_checkpoint_header(left, right)
        }
        _ => false,
    }
}

fn checkpoint_fields(checkpoint: &ContextCheckpoint) -> Value {
    json!({
        "kind": checkpoint.kind,
        "first_live_index": checkpoint.first_live_index,
        "created_at_ms": checkpoint.created_at_ms,
        "tokens_before": checkpoint.tokens_before,
        "tokens_after_estimate": checkpoint.tokens_after_estimate,
        "tokens_after_estimate_history_len": checkpoint.tokens_after_estimate_history_len,
        "pre_checkpoint_context_tokens": checkpoint.pre_checkpoint_context_tokens,
        "pre_checkpoint_context_history_len": checkpoint.pre_checkpoint_context_history_len,
    })
}

fn store_context_identity(identity: &ContextTokenIdentity) -> SessionContextIdentity {
    let ContextTokenIdentity {
        model,
        api_base,
        provider_type,
    } = identity;
    SessionContextIdentity {
        model: model.clone(),
        api_base: api_base.clone(),
        provider_type: provider_type.clone(),
    }
}

fn store_accounting(state: &SessionContextSnapshotState) -> SessionAccounting {
    let SessionContextSnapshotState {
        session_usage,
        context_token_identity,
        display_context_token_identity,
    } = state;
    let protocol::TokenUsage {
        context_tokens,
        prompt_tokens,
        completion_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
    } = session_usage;
    SessionAccounting {
        session_usage: SessionTokenUsage {
            context_tokens: *context_tokens,
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            cache_read_tokens: *cache_read_tokens,
            cache_write_tokens: *cache_write_tokens,
            reasoning_tokens: *reasoning_tokens,
        },
        context_token_identity: context_token_identity.as_ref().map(store_context_identity),
        display_context_token_identity: display_context_token_identity
            .as_ref()
            .map(store_context_identity),
    }
}

fn rows<T: Serialize>(
    table: &HistorySnapshots<T>,
    base: Option<SnapshotVersion>,
) -> Result<ArchiveEdit<ArchiveRow>, StoreError> {
    let Some(suffix) = table.suffix_from(base) else {
        return Ok(ArchiveEdit::Retain);
    };
    Ok(ArchiveEdit::ReplaceSuffix {
        retain_records: suffix.retain_records as u64,
        records: suffix
            .records
            .iter()
            .map(|(index, value)| {
                Ok(ArchiveRow {
                    index: HistoryIndex::new(*index as u64),
                    value: serde_json::to_value(value)?,
                })
            })
            .collect::<Result<_, StoreError>>()?,
    })
}

impl Session {
    pub(super) fn rebind_cloned_archives(&mut self, source: &Self) {
        if let Some(base) = self.archive_owner.versions {
            let source = versions(source);
            let cloned = versions(self);
            self.archive_owner.versions = Some(ArchiveVersions {
                turn_metas: base
                    .turn_metas
                    .for_cloned_table(source.turn_metas, cloned.turn_metas),
                metadata_snapshots: base
                    .metadata_snapshots
                    .for_cloned_table(source.metadata_snapshots, cloned.metadata_snapshots),
                context_snapshots: base
                    .context_snapshots
                    .for_cloned_table(source.context_snapshots, cloned.context_snapshots),
                checkpoint_events: base
                    .checkpoint_events
                    .for_cloned_table(source.checkpoint_events, cloned.checkpoint_events),
            });
        }
    }

    pub fn archive_base(&self) -> Option<&SessionArchiveBase> {
        self.archive_owner
            .base
            .as_ref()
            .filter(|_| self.archive_owner.session_id.as_deref() == Some(self.id.as_str()))
    }

    /// Bind copied archive tokens to the target through the canonical SDK fork result.
    /// The captured source must match the copied verified base. Pending edits are not
    /// acknowledged, and the fork keeps its own session/table scopes.
    pub fn bind_fork_archives(&mut self, result: &smelt_store::SessionForkResult) -> bool {
        let owner = &mut self.archive_owner;
        let Some(base) = owner.base.as_mut() else {
            return false;
        };
        let receipt = &result.session.receipt;
        if result.source_session_id == self.id
            || owner.session_id.as_deref() != Some(result.source_session_id.as_str())
            || self.parent_id.as_deref() != Some(result.source_session_id.as_str())
            || base.branch_sequence != result.source_head.revision
            || base.revision_id != result.session.revision_id
            || receipt.lineage_id.as_deref() != Some(base.lineage_id.as_str())
            || receipt.session_id != self.id
            || receipt.previous != StoreHead::default()
            || receipt.current.revision != smelt_store::Revision::new(1)
            || receipt.current.history_len != result.source_head.history_len
            || receipt.current.transcript_record_count != result.source_head.transcript_record_count
        {
            return false;
        }
        owner.session_id = Some(self.id.clone());
        base.branch_sequence = receipt.current.revision;
        true
    }

    /// Prepare only changed archive rows and newly supplied bodies. Retained archives
    /// resolve through the verified snapshot/result base, independently of `expected`.
    pub fn prepare_archive_save(
        &self,
        expected: StoreHead,
        history: HistorySuffix,
        transcript_records: Option<TranscriptRecordSuffix>,
    ) -> Result<PreparedArchiveSave, StoreError> {
        let owner = &self.archive_owner;
        if owner.session_id.as_ref().is_some_and(|id| id != &self.id)
            || (owner.base.is_none() && expected != StoreHead::default())
        {
            return Err(StoreError::Integrity(
                "native save has no matching exact archive owner".into(),
            ));
        }
        if !owner.checkpoint_events_exact {
            return Err(StoreError::Integrity(
                "native save cannot retain ordinals from a filtered checkpoint timeline".into(),
            ));
        }
        let checkpoint = self
            .checkpoint
            .as_ref()
            .filter(|checkpoint| checkpoint.first_live_index as u64 <= history.final_len.get())
            .cloned();
        let checkpoint_edit = match (&checkpoint, &owner.checkpoint) {
            (None, None) if owner.base.is_some() => CheckpointEdit::Retain,
            (Some(current), Some(base))
                if Arc::ptr_eq(&current.summary, &base.summary)
                    && same_checkpoint_header(current, base) =>
            {
                CheckpointEdit::Retain
            }
            (None, _) => CheckpointEdit::Replace { value: None },
            (Some(current), base) => CheckpointEdit::ReplaceRecord {
                record: CheckpointRecord {
                    fields: checkpoint_fields(current),
                    summary: if base
                        .as_ref()
                        .is_some_and(|base| Arc::ptr_eq(&current.summary, &base.summary))
                    {
                        CheckpointSummary::BaseCheckpoint
                    } else {
                        CheckpointSummary::New {
                            text: current.summary.clone(),
                        }
                    },
                },
            },
        };
        let first_user_message =
            if owner.base.is_some() && same_message(&self.first_user_message, &owner.message) {
                ValueEdit::Retain
            } else {
                ValueEdit::Replace {
                    value: self.first_user_message.clone(),
                }
            };
        let metadata_snapshots = match self
            .metadata_snapshots
            .suffix_from(owner.versions.map(|v| v.metadata_snapshots))
        {
            None => ArchiveEdit::Retain,
            Some(suffix) => ArchiveEdit::ReplaceSuffix {
                retain_records: suffix.retain_records as u64,
                records: suffix
                    .records
                    .iter()
                    .map(|(index, snapshot)| {
                        let mut fields = json!({ "title": snapshot.title, "slug": snapshot.slug });
                        let message = match &snapshot.first_user_message {
                            None => {
                                fields["first_user_message"] = Value::Null;
                                MetadataMessage::None
                            }
                            Some(_)
                                if same_message(
                                    &snapshot.first_user_message,
                                    &self.first_user_message,
                                ) =>
                            {
                                MetadataMessage::Active
                            }
                            Some(text) => MetadataMessage::New { text: text.clone() },
                        };
                        MetadataArchiveRow {
                            index: HistoryIndex::new(*index as u64),
                            fields,
                            message,
                        }
                    })
                    .collect(),
            },
        };
        let checkpoint_events = match self
            .checkpoint_events
            .suffix_from(owner.versions.map(|v| v.checkpoint_events))
        {
            None => CheckpointEventsEdit::Retain,
            Some((0, [])) => CheckpointEventsEdit::Clear,
            Some((retain_records, records)) => CheckpointEventsEdit::ReplaceRecordsSuffix {
                retain_records: retain_records as u64,
                records: records
                    .iter()
                    .map(|event| CheckpointRecord {
                        fields: json!({
                            "kind": event.kind,
                            "first_live_index": event.first_live_index,
                            "completed_at_history_len": event.completed_at_history_len,
                            "created_at_ms": event.created_at_ms,
                        }),
                        summary: if checkpoint.as_ref().is_some_and(|checkpoint| {
                            Arc::ptr_eq(&event.summary, &checkpoint.summary)
                        }) {
                            CheckpointSummary::Checkpoint
                        } else {
                            CheckpointSummary::New {
                                text: event.summary.clone(),
                            }
                        },
                    })
                    .collect(),
            },
        };
        let accounting = super::context_snapshot_state_from_session(self);
        // Scalar retain resolves against the expected head, not the archive source base.
        let accounting_edit = if owner.accounting.as_ref() == Some(&accounting)
            && owner
                .base
                .as_ref()
                .is_some_and(|base| base.branch_sequence == expected.revision)
        {
            ValueEdit::Retain
        } else {
            ValueEdit::Replace {
                value: Some(store_accounting(&accounting)),
            }
        };
        let scalars = SessionScalars {
            title: self.title.clone(),
            slug: self.slug.clone(),
            cwd: self.cwd.clone(),
            mode: self.mode.clone(),
            reasoning_effort: self
                .reasoning_effort
                .as_ref()
                .map(|effort| effort.label().to_string()),
            model: self.model.clone(),
            fast_mode: self.fast_mode,
            accounting: accounting_edit,
            context_tokens: self.context_tokens.map(u64::from),
            context_tokens_history_len: self.context_tokens_history_len.map(|index| index as u64),
            display_context_tokens: self.display_context_tokens.map(u64::from),
            session_cost_usd: smelt_store::SessionCostUsd::new(self.session_cost_usd)?,
            updated_at: i64::try_from(self.updated_at_ms).map_err(|_| {
                StoreError::Integrity("session update time exceeds SQLite range".into())
            })?,
        };
        Ok(PreparedArchiveSave {
            command: CompactSessionCommit {
                session_id: self.id.clone(),
                expected,
                identity: super::store_identity_from_session(self)?,
                scalars,
                archive_base: owner.base.clone(),
                archives: CompactSessionArchives {
                    first_user_message,
                    checkpoint: checkpoint_edit,
                    checkpoint_events,
                    turn_metas: rows(&self.turn_metas, owner.versions.map(|v| v.turn_metas))?,
                    metadata_snapshots,
                    context_snapshots: rows(
                        &self.context_snapshots,
                        owner.versions.map(|v| v.context_snapshots),
                    )?,
                },
                history,
                transcript_records,
            },
            scope: owner.scope.clone(),
            versions: versions(self),
            message: self.first_user_message.clone(),
            checkpoint,
            accounting,
        })
    }

    /// Advance a loaded archive base through the SDK's runtime-only startup recovery.
    /// The result must come from the canonical writer. Immutable session content stays
    /// on the same revision, and pending in-memory edits remain unacknowledged.
    pub fn acknowledge_startup_recovery(&mut self, result: &StartupRecoveryResult) -> bool {
        let owner = &mut self.archive_owner;
        let Some(base) = owner.base.as_mut() else {
            return false;
        };
        let receipt = &result.session.receipt;
        if owner.session_id.as_deref() != Some(self.id.as_str())
            || receipt.session_id != self.id
            || receipt.lineage_id.as_deref() != Some(base.lineage_id.as_str())
            || result.session.revision_id != base.revision_id
            || result.interrupted_turns.is_empty()
            || receipt.previous.history_len != receipt.current.history_len
            || receipt.previous.transcript_record_count != receipt.current.transcript_record_count
            || receipt.previous.revision.checked_add(1) != Some(receipt.current.revision)
            || (base.branch_sequence != receipt.previous.revision
                && base.branch_sequence != receipt.current.revision)
        {
            return false;
        }
        base.branch_sequence = receipt.current.revision;
        true
    }

    /// Adopt a verified SDK result for this unchanged command. The caller must obtain
    /// the result from canonical publication or exact fingerprint/receipt recovery.
    /// Archive acknowledgement is independent of provider dispatch acceptance.
    pub fn acknowledge_archive_save(
        &mut self,
        prepared: &PreparedArchiveSave,
        result: &SessionCommitResult,
    ) -> bool {
        let receipt = &result.receipt;
        let command = &prepared.command;
        let Some(lineage_id) = receipt.lineage_id.as_ref() else {
            return false;
        };
        if !Arc::ptr_eq(&self.archive_owner.scope, &prepared.scope)
            || command.session_id != self.id
            || !prepared.matches_result(result)
            || self.archive_owner.base.as_ref().is_some_and(|base| {
                base.lineage_id != *lineage_id
                    || receipt.current.revision < base.branch_sequence
                    || (receipt.current.revision == base.branch_sequence
                        && result.revision_id != base.revision_id)
            })
        {
            return false;
        }
        let same_result = self
            .archive_owner
            .base
            .as_ref()
            .is_some_and(|base| base.branch_sequence == receipt.current.revision);
        let turn_metas_acknowledged = self.turn_metas.acknowledge(prepared.versions.turn_metas);
        let metadata_snapshots_acknowledged = self
            .metadata_snapshots
            .acknowledge(prepared.versions.metadata_snapshots);
        let context_snapshots_acknowledged = self
            .context_snapshots
            .acknowledge(prepared.versions.context_snapshots);
        let checkpoint_events_acknowledged = self
            .checkpoint_events
            .acknowledge(prepared.versions.checkpoint_events);
        let mut versions = prepared.versions;
        // Distinct commands can publish the same revision after logical no-ops.
        // Replaying an older frame must not replace an already acknowledged local base.
        if same_result {
            if let Some(base_versions) = self.archive_owner.versions {
                if !turn_metas_acknowledged {
                    versions.turn_metas = base_versions.turn_metas;
                }
                if !metadata_snapshots_acknowledged {
                    versions.metadata_snapshots = base_versions.metadata_snapshots;
                }
                if !context_snapshots_acknowledged {
                    versions.context_snapshots = base_versions.context_snapshots;
                }
                if !checkpoint_events_acknowledged {
                    versions.checkpoint_events = base_versions.checkpoint_events;
                }
            }
        }
        self.archive_owner.session_id = Some(self.id.clone());
        self.archive_owner.base = Some(SessionArchiveBase {
            lineage_id: lineage_id.clone(),
            revision_id: result.revision_id.clone(),
            branch_sequence: receipt.current.revision,
        });
        self.archive_owner.versions = Some(versions);
        if !same_result || same_message(&self.first_user_message, &prepared.message) {
            self.archive_owner.message = prepared.message.clone();
        }
        if !same_result || same_checkpoint(&self.checkpoint, &prepared.checkpoint) {
            self.archive_owner.checkpoint = prepared.checkpoint.clone();
        }
        if !same_result || super::context_snapshot_state_from_session(self) == prepared.accounting {
            self.archive_owner.accounting = Some(prepared.accounting.clone());
        }
        true
    }

    pub(super) fn bind_loaded_archives(
        &mut self,
        base: SessionArchiveBase,
        metadata: &smelt_store::SessionMetadata,
    ) {
        if let Some(message) = &self.first_user_message {
            let mut snapshots = std::mem::take(&mut self.metadata_snapshots).into_vec();
            for (_, snapshot) in &mut snapshots {
                if snapshot.first_user_message.as_deref() == Some(message.as_ref()) {
                    snapshot.first_user_message = Some(message.clone());
                }
            }
            self.metadata_snapshots = snapshots.into();
        }
        if let Some(checkpoint) = &mut self.checkpoint {
            if let Some(event) = self
                .checkpoint_events
                .iter()
                .find(|event| event.matches(checkpoint))
            {
                checkpoint.summary = event.summary.clone();
            }
        }
        let versions = versions(self);
        self.turn_metas.acknowledge(versions.turn_metas);
        self.metadata_snapshots
            .acknowledge(versions.metadata_snapshots);
        self.context_snapshots
            .acknowledge(versions.context_snapshots);
        self.checkpoint_events
            .acknowledge(versions.checkpoint_events);
        self.archive_owner = ArchiveOwner {
            scope: Arc::new(()),
            session_id: Some(self.id.clone()),
            base: Some(base),
            versions: Some(versions),
            message: self.first_user_message.clone(),
            checkpoint: self.checkpoint.clone(),
            accounting: Some(super::context_snapshot_state_from_session(self)),
            checkpoint_events_exact: metadata
                .checkpoint_events_json
                .as_ref()
                .is_none_or(|value| {
                    value
                        .as_array()
                        .is_some_and(|events| events.len() == self.checkpoint_events.len())
                }),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ContextCheckpointEvent, SessionMetadataSnapshot, SessionStorage};
    use super::*;
    use protocol::{Content, HistoryItem};
    use smelt_store::{HistoryLen, OwnedLineageWriter, TranscriptRecordCount};

    fn session(events: usize, body_bytes: usize) -> Session {
        let mut session = Session::new(4242, "/synthetic".into());
        session.history = vec![HistoryItem::user(Content::text("synthetic"))];
        session.first_user_message = Some("m".repeat(body_bytes).into());
        session.title = Some("initial".into());
        session.snapshot_metadata_at(1);
        for index in 0..events {
            session.checkpoint_events.push(ContextCheckpointEvent {
                kind: "compaction".into(),
                summary: "s".repeat(body_bytes).into(),
                first_live_index: 1,
                completed_at_history_len: 1,
                created_at_ms: index as u64,
            });
        }
        session.checkpoint = Some(ContextCheckpoint {
            summary: "active".repeat(body_bytes / 6).into(),
            first_live_index: 1,
            ..Default::default()
        });
        session
    }

    fn prepare(session: &Session, head: StoreHead) -> PreparedArchiveSave {
        let start = head.history_len.get().min(session.history.len() as u64);
        session
            .prepare_archive_save(
                head,
                HistorySuffix {
                    start: HistoryIndex::new(start),
                    final_len: HistoryLen::new(session.history.len() as u64),
                    items: session.history[start as usize..].to_vec(),
                },
                None,
            )
            .unwrap()
    }

    fn save(
        session: &mut Session,
        writer: &mut OwnedLineageWriter,
        head: StoreHead,
    ) -> SessionCommitResult {
        let prepared = prepare(session, head);
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(session.acknowledge_archive_save(&prepared, &result));
        result
    }

    #[test]
    fn native_accounting_mapping_preserves_exact_wire_bytes() {
        let identities = [
            None,
            Some(ContextTokenIdentity {
                model: None,
                api_base: None,
                provider_type: None,
            }),
            Some(ContextTokenIdentity {
                model: Some("synthetic-model".into()),
                api_base: Some("https://synthetic.invalid".into()),
                provider_type: Some("synthetic-provider".into()),
            }),
        ];
        for mask in 0..64 {
            for amount in [0, u32::MAX] {
                let token = |bit| (mask & (1 << bit) != 0).then_some(amount);
                for context in &identities {
                    for display in &identities {
                        let state = SessionContextSnapshotState {
                            session_usage: protocol::TokenUsage {
                                context_tokens: token(0),
                                prompt_tokens: token(1),
                                completion_tokens: token(2),
                                cache_read_tokens: token(3),
                                cache_write_tokens: token(4),
                                reasoning_tokens: token(5),
                            },
                            context_token_identity: context.clone(),
                            display_context_token_identity: display.clone(),
                        };
                        let expected: SessionAccounting =
                            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
                        let mapped = store_accounting(&state);
                        assert_eq!(mapped, expected);
                        assert_eq!(
                            serde_json::to_vec(&mapped).unwrap(),
                            serde_json::to_vec(&expected).unwrap()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn core_native_title_and_checkpoint_header_commands_do_not_copy_retained_bodies() {
        for (events, body_bytes) in [(0, 0), (32, 32_768), (128, 32_768), (0, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let mut session = session(events, body_bytes);
            let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
            let first = save(&mut session, &mut writer, StoreHead::default());
            session.title = Some("changed".into());
            session.snapshot_metadata_at(1);
            let prepared = prepare(&session, first.receipt.current);
            assert!(matches!(
                prepared.command.archives.first_user_message,
                ValueEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint,
                CheckpointEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint_events,
                CheckpointEventsEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.turn_metas,
                ArchiveEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.context_snapshots,
                ArchiveEdit::Retain
            ));
            let ArchiveEdit::ReplaceSuffix { records, .. } =
                &prepared.command.archives.metadata_snapshots
            else {
                panic!("title snapshot suffix");
            };
            assert_eq!(records.len(), 1);
            assert!(matches!(records[0].message, MetadataMessage::Active));
            let bytes = serde_json::to_vec(prepared.command()).unwrap().len();
            eprintln!("CORE_NATIVE_ARCHIVES events={events} body_bytes={body_bytes} title_command_bytes={bytes}");
            assert!(
                bytes < 4096,
                "retained bodies inflated command to {bytes} bytes"
            );
            let titled = writer.commit_compact_session(prepared.command()).unwrap();
            assert!(session.acknowledge_archive_save(&prepared, &titled));
            session.checkpoint.as_mut().unwrap().tokens_after_estimate = Some(17);
            let header = prepare(&session, titled.receipt.current);
            let CheckpointEdit::ReplaceRecord { record } = &header.command.archives.checkpoint
            else {
                panic!("checkpoint header edit");
            };
            assert!(matches!(record.summary, CheckpointSummary::BaseCheckpoint));
            assert!(serde_json::to_vec(header.command()).unwrap().len() < 4096);
            let edited = writer.commit_compact_session(header.command()).unwrap();
            assert!(session.acknowledge_archive_save(&header, &edited));
            let stored = writer.snapshot().unwrap();
            assert_eq!(
                stored.metadata.first_user_message.as_deref().unwrap().len(),
                body_bytes
            );
            assert_eq!(
                stored.metadata.checkpoint_json.as_ref().unwrap()["tokens_after_estimate"],
                17
            );
            let retained_events = stored
                .metadata
                .checkpoint_events_json
                .as_ref()
                .map_or(0, |events| events.as_array().unwrap().len());
            assert_eq!(retained_events, events);
        }
    }

    fn publish_frame(
        writer: &mut smelt_store::SessionWriter,
        frame: &PreparedArchiveSave,
    ) -> SessionCommitResult {
        let batch = smelt_store::SessionEventBatch::compact_save(
            1,
            frame.command().clone(),
            smelt_store::SessionBatchBarrier::None,
        );
        writer
            .commit_batch(&batch)
            .unwrap()
            .exact_session()
            .unwrap()
            .clone()
    }

    #[test]
    fn core_native_unbound_frame_preparation_and_cloning_share_supplied_bodies() {
        smelt_perf::alloc::enable();
        let mut samples = Vec::new();
        for body_bytes in [128, 32_768, 1_048_576] {
            let root = tempfile::tempdir().unwrap();
            let mut session = session(4, body_bytes);
            session.metadata_snapshots.upsert_truncating_after(
                0,
                SessionMetadataSnapshot {
                    title: Some("historical".into()),
                    slug: None,
                    first_user_message: Some("h".repeat(body_bytes).into()),
                },
            );
            session.snapshot_metadata_at(1);
            let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
            let before = smelt_perf::alloc::thread_snapshot().1;
            let initial = prepare(&session, StoreHead::default());
            let preparation_bytes = smelt_perf::alloc::thread_snapshot().1 - before;
            let before = smelt_perf::alloc::thread_snapshot().1;
            let copied = initial.clone();
            let clone_bytes = smelt_perf::alloc::thread_snapshot().1 - before;
            let before = smelt_perf::alloc::thread_snapshot().1;
            let sdk_command = copied.command().clone();
            let sdk_clone_bytes = smelt_perf::alloc::thread_snapshot().1 - before;
            let ValueEdit::Replace {
                value: Some(message),
            } = &sdk_command.archives.first_user_message
            else {
                panic!("supplied active message");
            };
            assert!(Arc::ptr_eq(
                message,
                session.first_user_message.as_ref().unwrap()
            ));
            let CheckpointEdit::ReplaceRecord { record } = &sdk_command.archives.checkpoint else {
                panic!("supplied checkpoint");
            };
            let CheckpointSummary::New { text } = &record.summary else {
                panic!("supplied active summary");
            };
            assert!(Arc::ptr_eq(
                text,
                &session.checkpoint.as_ref().unwrap().summary
            ));
            let CheckpointEventsEdit::ReplaceRecordsSuffix { records, .. } =
                &sdk_command.archives.checkpoint_events
            else {
                panic!("supplied timeline");
            };
            for (record, event) in records.iter().zip(session.checkpoint_events.iter()) {
                let CheckpointSummary::New { text } = &record.summary else {
                    panic!("supplied event summary");
                };
                assert!(Arc::ptr_eq(text, &event.summary));
            }
            let ArchiveEdit::ReplaceSuffix { records, .. } =
                &sdk_command.archives.metadata_snapshots
            else {
                panic!("supplied metadata snapshots");
            };
            let MetadataMessage::New { text } = &records[0].message else {
                panic!("supplied historical message");
            };
            assert!(Arc::ptr_eq(
                text,
                session.metadata_snapshots[0]
                    .1
                    .first_user_message
                    .as_ref()
                    .unwrap()
            ));
            let wire = serde_json::to_vec(initial.command()).unwrap();
            assert_eq!(serde_json::to_vec(copied.command()).unwrap(), wire);
            let decoded: CompactSessionCommit = serde_json::from_slice(&wire).unwrap();
            assert_eq!(&decoded, initial.command());
            assert_eq!(
                smelt_store::compact_session_commit_fingerprint(copied.command()).unwrap(),
                smelt_store::compact_session_commit_fingerprint(&decoded).unwrap(),
            );
            session.title = Some("queued-title".into());
            let queued = prepare(&session, StoreHead::default());
            let batch = smelt_store::SessionEventBatch::compact_save(
                1,
                sdk_command,
                smelt_store::SessionBatchBarrier::None,
            );
            let saved = writer.commit_batch(&batch).unwrap();
            assert_eq!(writer.recover_batch(&batch).unwrap(), Some(saved.clone()));
            let result = saved.exact_session().unwrap();
            let finalized = queued.finalize_after(&initial, result).unwrap();
            assert!(serde_json::to_vec(finalized.command()).unwrap().len() < 4096);
            assert!(session.acknowledge_archive_save(&copied, result));
            let titled = publish_frame(&mut writer, &finalized);
            assert!(session.acknowledge_archive_save(&finalized, &titled));
            let stored = writer.lineage_writer_mut().snapshot().unwrap();
            assert_eq!(
                stored.metadata.first_user_message.as_deref(),
                session.first_user_message.as_deref()
            );
            assert_eq!(
                stored.metadata.checkpoint_json.as_ref().unwrap()["summary"].as_str(),
                Some(session.checkpoint.as_ref().unwrap().summary.as_ref())
            );
            assert_eq!(
                stored
                    .metadata
                    .checkpoint_events_json
                    .as_ref()
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                4
            );
            assert_eq!(
                stored.side_tables.metadata_snapshots[0].1["first_user_message"].as_str(),
                session.metadata_snapshots[0]
                    .1
                    .first_user_message
                    .as_deref()
            );
            writer.release().unwrap();
            eprintln!("CORE_UNBOUND_BODY_SHARING body_bytes={body_bytes} preparation_bytes={preparation_bytes} clone_bytes={clone_bytes} sdk_clone_bytes={sdk_clone_bytes}");
            samples.push((body_bytes, preparation_bytes, clone_bytes, sdk_clone_bytes));
        }
        let baseline = samples[0];
        for (body_bytes, preparation_bytes, clone_bytes, sdk_clone_bytes) in samples {
            assert!(preparation_bytes <= baseline.1 + 65_536, "{body_bytes}-byte bodies inflate preparation to {preparation_bytes} bytes, baseline {}", baseline.1);
            assert!(clone_bytes <= baseline.2 + 65_536, "{body_bytes}-byte bodies inflate frame cloning to {clone_bytes} bytes, baseline {}", baseline.2);
            assert!(sdk_clone_bytes <= baseline.3 + 65_536, "{body_bytes}-byte bodies inflate SDK command cloning to {sdk_clone_bytes} bytes, baseline {}", baseline.3);
        }
    }

    #[test]
    fn core_native_queued_initial_frames_publish_and_acknowledge_the_actual_frame() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 32_768);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        session.title = Some("queued-title".into());
        session.snapshot_metadata_at(1);
        let queued = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        let finalized = queued
            .clone()
            .finalize_after(&initial, &initial_result)
            .unwrap();
        let result = publish_frame(&mut writer, &finalized);
        assert!(!session.acknowledge_archive_save(&queued, &result));
        assert!(session.acknowledge_archive_save(&finalized, &result));
        let next = prepare(&session, result.receipt.current);
        assert!(matches!(
            next.command.archives.metadata_snapshots,
            ArchiveEdit::Retain
        ));
        assert!(matches!(
            next.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(stored.metadata.title.as_deref(), Some("queued-title"));
        assert_eq!(
            stored
                .metadata
                .checkpoint_events_json
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn core_native_recovered_initial_frame_finalizes_and_acknowledges_a_queued_title() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(32, 32_768);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let batch = smelt_store::SessionEventBatch::compact_save(
            1,
            initial.command().clone(),
            smelt_store::SessionBatchBarrier::None,
        );
        writer.commit_batch(&batch).unwrap();
        session.title = Some("queued-after-lost-reply".into());
        session.snapshot_metadata_at(1);
        let queued = prepare(&session, StoreHead::default());
        let recovered = writer.recover_batch(&batch).unwrap().unwrap();
        let result = recovered.exact_session().unwrap();
        assert!(session.acknowledge_archive_save(&initial, result));
        let finalized = queued.clone().finalize_after(&initial, result).unwrap();
        let bytes = serde_json::to_vec(finalized.command()).unwrap().len();
        eprintln!("CORE_RECOVERED_QUEUED_TITLE events=32 body_bytes=32768 command_bytes={bytes}");
        assert!(
            bytes < 4096,
            "retained bodies inflated command to {bytes} bytes"
        );
        assert!(matches!(
            finalized.command.archives.first_user_message,
            ValueEdit::Retain
        ));
        assert!(matches!(
            finalized.command.archives.checkpoint,
            CheckpointEdit::Retain
        ));
        assert!(matches!(
            finalized.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        let result = publish_frame(&mut writer, &finalized);
        assert!(!session.acknowledge_archive_save(&queued, &result));
        assert!(session.acknowledge_archive_save(&finalized, &result));
        let next = prepare(&session, result.receipt.current);
        assert!(matches!(
            next.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        assert!(matches!(
            next.command.archives.metadata_snapshots,
            ArchiveEdit::Retain
        ));
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(
            stored.metadata.title.as_deref(),
            Some("queued-after-lost-reply")
        );
        assert_eq!(
            stored
                .metadata
                .checkpoint_events_json
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            32
        );
    }

    #[test]
    fn core_native_unbound_queued_header_retains_tables_and_shared_bodies() {
        for (events, body_bytes) in [(0, 128), (32, 32_768), (128, 32_768), (0, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let mut session = session(events, body_bytes);
            session.turn_metas.push((
                1,
                protocol::TurnMeta {
                    elapsed_ms: 7,
                    avg_tps: None,
                    display_tps: None,
                    interrupted: false,
                },
            ));
            session.snapshot_context_at(1);
            let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
            let initial = prepare(&session, StoreHead::default());
            session.checkpoint.as_mut().unwrap().tokens_after_estimate = Some(17);
            let queued = prepare(&session, StoreHead::default());
            let result = publish_frame(&mut writer, &initial);
            let original = writer.lineage_writer_mut().snapshot().unwrap();
            let finalized = queued.finalize_after(&initial, &result).unwrap();
            let archives = &finalized.command.archives;
            assert!(matches!(archives.first_user_message, ValueEdit::Retain));
            assert!(
                matches!(&archives.checkpoint, CheckpointEdit::ReplaceRecord { record }
                if matches!(record.summary, CheckpointSummary::BaseCheckpoint))
            );
            assert!(matches!(
                archives.checkpoint_events,
                CheckpointEventsEdit::Retain
            ));
            assert!(matches!(archives.turn_metas, ArchiveEdit::Retain));
            assert!(matches!(archives.metadata_snapshots, ArchiveEdit::Retain));
            assert!(matches!(archives.context_snapshots, ArchiveEdit::Retain));
            let bytes = serde_json::to_vec(finalized.command()).unwrap().len();
            eprintln!(
                "CORE_QUEUED_HEADER events={events} body_bytes={body_bytes} command_bytes={bytes}"
            );
            assert!(
                bytes < 4096,
                "retained bodies inflated command to {bytes} bytes"
            );
            let result = publish_frame(&mut writer, &finalized);
            assert!(session.acknowledge_archive_save(&finalized, &result));
            let stored = writer.lineage_writer_mut().snapshot().unwrap();
            assert_eq!(stored.side_tables, original.side_tables);
            assert_eq!(
                stored.metadata.first_user_message,
                original.metadata.first_user_message
            );
            assert_eq!(
                stored.metadata.checkpoint_events_json,
                original.metadata.checkpoint_events_json
            );
            assert_eq!(
                stored.metadata.checkpoint_json.as_ref().unwrap()["summary"],
                original.metadata.checkpoint_json.as_ref().unwrap()["summary"]
            );
            assert_eq!(
                stored.metadata.checkpoint_json.as_ref().unwrap()["tokens_after_estimate"],
                17
            );
        }
    }

    #[test]
    fn core_native_unbound_finalization_preserves_changed_bodies_and_each_table() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 128);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        session.first_user_message = Some("new-message".repeat(3276).into());
        session.checkpoint.as_mut().unwrap().summary = "new-summary".repeat(3276).into();
        session.title = Some("changed archives".into());
        session.snapshot_metadata_at(1);
        session.turn_metas.push((
            1,
            protocol::TurnMeta {
                elapsed_ms: 11,
                avg_tps: None,
                display_tps: None,
                interrupted: true,
            },
        ));
        session.context_tokens = Some(17);
        session.context_tokens_history_len = Some(1);
        session.snapshot_context_at(1);
        let event = session.checkpoint_events[0].clone();
        session.checkpoint_events.push(event);
        let queued = prepare(&session, StoreHead::default());
        let ValueEdit::Replace { value: Some(body) } = &queued.command.archives.first_user_message
        else {
            panic!("new message");
        };
        let body_address = body.as_ptr();
        let CheckpointEventsEdit::ReplaceRecordsSuffix { records, .. } =
            &queued.command.archives.checkpoint_events
        else {
            panic!("changed events");
        };
        let rows_address = records.as_ptr();
        let result = publish_frame(&mut writer, &initial);
        let finalized = queued.finalize_after(&initial, &result).unwrap();
        let ValueEdit::Replace { value: Some(body) } =
            &finalized.command.archives.first_user_message
        else {
            panic!("finalized message");
        };
        assert_eq!(body.as_ptr(), body_address);
        assert!(
            matches!(&finalized.command.archives.checkpoint, CheckpointEdit::ReplaceRecord { record }
            if matches!(record.summary, CheckpointSummary::New { .. }))
        );
        assert!(matches!(
            finalized.command.archives.turn_metas,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        assert!(matches!(
            finalized.command.archives.metadata_snapshots,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        assert!(matches!(
            finalized.command.archives.context_snapshots,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        let CheckpointEventsEdit::ReplaceRecordsSuffix { records, .. } =
            &finalized.command.archives.checkpoint_events
        else {
            panic!("finalized events");
        };
        assert_eq!(records.as_ptr(), rows_address);
        let result = publish_frame(&mut writer, &finalized);
        assert!(session.acknowledge_archive_save(&finalized, &result));
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(
            stored.metadata.first_user_message.as_deref(),
            session.first_user_message.as_deref()
        );
        assert_eq!(
            stored.metadata.checkpoint_json.as_ref().unwrap()["summary"].as_str(),
            session
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.as_ref())
        );
        assert_eq!(
            stored.metadata.checkpoint_events_json,
            Some(serde_json::to_value(&session.checkpoint_events).unwrap())
        );
        assert_eq!(
            serde_json::to_value(stored.side_tables.turn_metas).unwrap(),
            serde_json::to_value(&session.turn_metas).unwrap()
        );
        assert_eq!(
            serde_json::to_value(stored.side_tables.metadata_snapshots).unwrap(),
            serde_json::to_value(&session.metadata_snapshots).unwrap()
        );
        assert_eq!(
            serde_json::to_value(stored.side_tables.context_snapshots).unwrap(),
            serde_json::to_value(&session.context_snapshots).unwrap()
        );
    }

    #[test]
    fn core_native_queued_frame_does_not_inherit_changed_scalar_accounting() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        assert!(session.acknowledge_archive_save(&initial, &initial_result));
        session.session_usage.prompt_tokens = Some(7);
        let accounting = prepare(&session, initial_result.receipt.current);
        session.session_usage.prompt_tokens = None;
        session.title = Some("queued-title".into());
        let queued = prepare(&session, initial_result.receipt.current);
        assert!(matches!(
            queued.command.scalars.accounting,
            ValueEdit::Retain
        ));
        let accounting_result = publish_frame(&mut writer, &accounting);
        let finalized = queued
            .clone()
            .finalize_after(&accounting, &accounting_result)
            .unwrap();
        let result = publish_frame(&mut writer, &finalized);
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert!(stored.metadata.accounting_json.unwrap()["session_usage"].get("prompt_tokens").is_none(),
            "queued save must publish its captured accounting, not the intervening head's accounting");
        assert!(!session.acknowledge_archive_save(&queued, &result));
        assert!(session.acknowledge_archive_save(&finalized, &result));
    }

    #[test]
    fn core_native_finalization_preserves_original_archive_sources_and_later_mutations() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 32_768);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        assert!(session.acknowledge_archive_save(&initial, &initial_result));
        session.first_user_message = Some("intervening-message".into());
        session.checkpoint.as_mut().unwrap().summary = "intervening-summary".into();
        let preceding = prepare(&session, initial_result.receipt.current);
        session.first_user_message = initial.message.clone();
        session.checkpoint = initial.checkpoint.clone();
        session.checkpoint.as_mut().unwrap().tokens_after_estimate = Some(17);
        session.title = Some("queued-header".into());
        session.snapshot_metadata_at(1);
        let queued = prepare(&session, initial_result.receipt.current);
        session.title = Some("later-local-title".into());
        session.snapshot_metadata_at(1);
        let preceding_result = publish_frame(&mut writer, &preceding);
        let finalized = queued
            .finalize_after(&preceding, &preceding_result)
            .unwrap();
        assert_eq!(
            finalized.command.archive_base.as_ref().unwrap().revision_id,
            initial_result.revision_id
        );
        assert!(matches!(
            finalized.command.archives.first_user_message,
            ValueEdit::Retain
        ));
        assert!(matches!(&finalized.command.archives.checkpoint,
            CheckpointEdit::ReplaceRecord { record } if matches!(record.summary, CheckpointSummary::BaseCheckpoint)));
        assert!(serde_json::to_vec(finalized.command()).unwrap().len() < 4096);
        let result = publish_frame(&mut writer, &finalized);
        assert!(session.acknowledge_archive_save(&finalized, &result));
        assert!(!session.acknowledge_archive_save(&preceding, &preceding_result));
        assert_eq!(
            session.metadata_snapshots.changed_suffix().unwrap().records[0]
                .1
                .title
                .as_deref(),
            Some("later-local-title")
        );
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(stored.metadata.title.as_deref(), Some("queued-header"));
        assert!(stored.metadata.first_user_message.as_deref() == initial.message.as_deref());
        let checkpoint = stored.metadata.checkpoint_json.unwrap();
        assert!(
            checkpoint["summary"].as_str()
                == initial
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| checkpoint.summary.as_ref())
        );
        assert_eq!(checkpoint["tokens_after_estimate"], 17);
    }

    #[test]
    fn core_native_finalization_moves_changed_bodies_and_rows_without_copying() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        assert!(session.acknowledge_archive_save(&initial, &initial_result));
        session.title = Some("preceding".into());
        let preceding = prepare(&session, initial_result.receipt.current);
        session.first_user_message = Some("new".repeat(349_526).into());
        session.snapshot_metadata_at(1);
        let queued = prepare(&session, initial_result.receipt.current);
        let ValueEdit::Replace { value: Some(body) } = &queued.command.archives.first_user_message
        else {
            panic!("new message body");
        };
        let body_address = body.as_ptr();
        let ArchiveEdit::ReplaceSuffix { records, .. } =
            &queued.command.archives.metadata_snapshots
        else {
            panic!("new metadata row");
        };
        let rows_address = records.as_ptr();
        let preceding_result = publish_frame(&mut writer, &preceding);
        let finalized = queued
            .finalize_after(&preceding, &preceding_result)
            .unwrap();
        let ValueEdit::Replace { value: Some(body) } =
            &finalized.command.archives.first_user_message
        else {
            panic!("finalized new message body");
        };
        assert_eq!(body.as_ptr(), body_address);
        let ArchiveEdit::ReplaceSuffix { records, .. } =
            &finalized.command.archives.metadata_snapshots
        else {
            panic!("finalized metadata row");
        };
        assert_eq!(records.as_ptr(), rows_address);
        let result = publish_frame(&mut writer, &finalized);
        assert!(session.acknowledge_archive_save(&finalized, &result));
        let next = prepare(&session, result.receipt.current);
        assert!(matches!(
            next.command.archives.first_user_message,
            ValueEdit::Retain
        ));
        assert!(serde_json::to_vec(next.command()).unwrap().len() < 4096);
    }

    #[test]
    fn core_native_finalization_preserves_noop_fingerprints_and_exact_replay() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        assert!(session.acknowledge_archive_save(&initial, &initial_result));
        let noop = prepare(&session, initial_result.receipt.current);
        let result = publish_frame(&mut writer, &noop);
        assert_eq!(result.receipt.previous, result.receipt.current);
        let finalized = noop.clone().finalize_after(&noop, &result).unwrap();
        assert!(finalized.command() == noop.command());
        let replay = publish_frame(&mut writer, &finalized);
        assert_eq!(replay, result);
        assert!(session.acknowledge_archive_save(&finalized, &replay));
    }

    #[test]
    fn core_native_finalization_rejects_foreign_stale_and_mismatched_publications() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = smelt_store::SessionWriter::open(root.path(), &session.id).unwrap();
        let initial = prepare(&session, StoreHead::default());
        let queued = prepare(&session, StoreHead::default());
        let initial_result = publish_frame(&mut writer, &initial);
        for boundary in 0..5 {
            let mut invalid = initial_result.clone();
            match boundary {
                0 => invalid.receipt.session_id = "other".into(),
                1 => invalid.receipt.previous.history_len = HistoryLen::new(9),
                2 => {
                    invalid.receipt.current.transcript_record_count = TranscriptRecordCount::new(9)
                }
                3 => invalid.receipt.lineage_id = None,
                _ => invalid.revision_id = "not-an-exact-revision".into(),
            }
            assert!(queued.clone().finalize_after(&initial, &invalid).is_err());
        }
        assert!(session.metadata_snapshots.changed_suffix().is_some());
        assert!(session.acknowledge_archive_save(&initial, &initial_result));
        let cloned = session.clone();
        let foreign = prepare(&cloned, initial_result.receipt.current);
        assert!(foreign.finalize_after(&initial, &initial_result).is_err());
        session.title = Some("advanced".into());
        session.snapshot_metadata_at(1);
        let advanced = prepare(&session, initial_result.receipt.current);
        let advanced_result = publish_frame(&mut writer, &advanced);
        let latest = prepare(&session, advanced_result.receipt.current);
        assert!(latest.finalize_after(&initial, &initial_result).is_err());
        assert!(session.metadata_snapshots.changed_suffix().is_some());
    }

    #[test]
    fn core_native_catalog_publication_shares_message_ownership_and_preserves_exact_values() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let mut session = session(32, 32_768);
        session.first_user_message = Some("m".repeat(1_048_576).into());
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &session.id).unwrap();
        assert!(storage.wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let mut head = StoreHead::default();
        for index in 0..4 {
            session.title = Some(format!("native-title-{index}"));
            session.snapshot_metadata_at(1);
            let prepared = prepare(&session, head);
            let result = writer.commit_compact_session(prepared.command()).unwrap();
            let projected = writer
                .catalog_session_for_result(&result, prepared.catalog_message(&result).unwrap())
                .unwrap();
            assert!(Arc::ptr_eq(
                projected.first_user_message.as_ref().unwrap(),
                session.first_user_message.as_ref().unwrap()
            ));
            assert!(projected.first_user_message_id.is_some());
            storage
                .publish_archive_save_catalog(&writer, &prepared, &result)
                .unwrap();
            assert!(session.acknowledge_archive_save(&prepared, &result));
            head = result.receipt.current;
            assert!(storage.wait_for_session_catalog(std::time::Duration::from_secs(5)));
            let reader = smelt_store::CatalogReader::open_existing(
                smelt_store::SessionStoreLayout::from_state_root(root.path()).catalog_path(),
            )
            .unwrap()
            .unwrap();
            let stored = reader.session(&session.id).unwrap().unwrap();
            assert_eq!(stored.title, session.title);
            assert!(stored.first_user_message.as_deref() == session.first_user_message.as_deref());
            assert_eq!(
                stored.first_user_message_id,
                projected.first_user_message_id
            );
            assert!(
                smelt_store::pending_catalog_session_ids(storage.sessions_dir())
                    .unwrap()
                    .is_empty()
            );
        }
        let prepared = prepare(&session, head);
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_storage = SessionStorage::new(other.path().to_path_buf());
        assert!(other_storage
            .publish_archive_save_catalog(&writer, &prepared, &result)
            .is_err());
        let mut invalid = result.clone();
        invalid.receipt.previous.history_len = HistoryLen::new(99);
        assert!(storage
            .publish_archive_save_catalog(&writer, &prepared, &invalid)
            .is_err());
    }

    #[test]
    fn core_native_partial_acknowledgement_preserves_later_edits_and_replacements() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 32_768);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let first = save(&mut session, &mut writer, StoreHead::default());
        session.title = Some("older".into());
        session.snapshot_metadata_at(1);
        let older = prepare(&session, first.receipt.current);
        session.title = Some("later".into());
        session.snapshot_metadata_at(1);
        let result = writer.commit_compact_session(older.command()).unwrap();
        assert!(session.acknowledge_archive_save(&older, &result));
        assert_eq!(
            session.metadata_snapshots.changed_suffix().unwrap().records[0]
                .1
                .title
                .as_deref(),
            Some("later")
        );
        let later = save(&mut session, &mut writer, result.receipt.current);
        assert!(session.metadata_snapshots.changed_suffix().is_none());

        session.title = Some("prepared".into());
        session.snapshot_metadata_at(1);
        let prepared = prepare(&session, later.receipt.current);
        session.metadata_snapshots = vec![(
            1,
            SessionMetadataSnapshot {
                title: Some("replacement".into()),
                slug: None,
                first_user_message: session.first_user_message.clone(),
            },
        )]
        .into();
        session
            .metadata_snapshots
            .acknowledge(session.metadata_snapshots.version());
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(session.acknowledge_archive_save(&prepared, &result));
        let replacement = prepare(&session, result.receipt.current);
        let ArchiveEdit::ReplaceSuffix {
            retain_records,
            records,
        } = &replacement.command.archives.metadata_snapshots
        else {
            panic!("replacement must not inherit foreign cleanliness");
        };
        assert_eq!(*retain_records, 0);
        assert_eq!(records[0].fields["title"], "replacement");
    }

    #[test]
    fn core_native_summary_is_supplied_once_for_checkpoint_and_timeline() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let first = save(&mut session, &mut writer, StoreHead::default());
        assert!(session.install_context_checkpoint_at_history_index(
            "compaction".into(),
            "s".repeat(1_048_576),
            1,
            None,
            1
        ));
        let prepared = prepare(&session, first.receipt.current);
        let CheckpointEdit::ReplaceRecord { record } = &prepared.command.archives.checkpoint else {
            panic!("new checkpoint");
        };
        assert!(matches!(record.summary, CheckpointSummary::New { .. }));
        let CheckpointEventsEdit::ReplaceRecordsSuffix { records, .. } =
            &prepared.command.archives.checkpoint_events
        else {
            panic!("new timeline row");
        };
        assert!(matches!(records[0].summary, CheckpointSummary::Checkpoint));
        assert!(serde_json::to_vec(prepared.command()).unwrap().len() < 1_048_576 + 8192);
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(session.acknowledge_archive_save(&prepared, &result));
        let stored = writer.snapshot().unwrap();
        assert_eq!(
            stored.metadata.checkpoint_events_json.unwrap()[0]["summary"]
                .as_str()
                .unwrap()
                .len(),
            1_048_576
        );
    }

    #[test]
    fn core_native_archive_suffixes_are_independent_of_dirty_history_and_each_other() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(3, 0);
        session
            .history
            .resize(3, HistoryItem::user(Content::text("synthetic")));
        session.turn_metas.push((
            0,
            protocol::TurnMeta {
                elapsed_ms: 1,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        session.snapshot_context_at(0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let first = save(&mut session, &mut writer, StoreHead::default());
        session.turn_metas.push((
            3,
            protocol::TurnMeta {
                elapsed_ms: 2,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        session.title = Some("new-row".into());
        session.snapshot_metadata_at(3);
        session.context_tokens = Some(17);
        session.snapshot_context_at(2);
        session
            .checkpoint_events
            .retain(|event| event.created_at_ms != 1);
        session.checkpoint_events.push(ContextCheckpointEvent {
            kind: "compaction".into(),
            summary: "new".into(),
            first_live_index: 3,
            completed_at_history_len: 3,
            created_at_ms: 3,
        });
        let prepared = prepare(&session, first.receipt.current);
        assert_eq!(prepared.command.history.start.get(), 3);
        assert!(prepared.command.history.items.is_empty());
        for (edit, index) in [
            (&prepared.command.archives.turn_metas, 3),
            (&prepared.command.archives.context_snapshots, 2),
        ] {
            let ArchiveEdit::ReplaceSuffix {
                retain_records,
                records,
            } = edit
            else {
                panic!("independent changed table");
            };
            assert_eq!(*retain_records, 1);
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].index.get(), index);
        }
        let ArchiveEdit::ReplaceSuffix {
            retain_records,
            records,
        } = &prepared.command.archives.metadata_snapshots
        else {
            panic!("metadata suffix");
        };
        assert_eq!(*retain_records, 1);
        assert_eq!(records[0].index.get(), 3);
        let CheckpointEventsEdit::ReplaceRecordsSuffix {
            retain_records,
            records,
        } = &prepared.command.archives.checkpoint_events
        else {
            panic!("timeline suffix");
        };
        assert_eq!(*retain_records, 1);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].fields["created_at_ms"], 2);
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(session.acknowledge_archive_save(&prepared, &result));
        let stored = writer.snapshot().unwrap();
        assert_eq!(stored.side_tables.turn_metas.len(), 2);
        assert_eq!(stored.side_tables.context_snapshots.len(), 2);
        assert_eq!(stored.side_tables.metadata_snapshots.len(), 2);
        let timeline = stored.metadata.checkpoint_events_json.unwrap();
        assert_eq!(timeline.as_array().unwrap().len(), 3);
        assert_eq!(timeline[1]["created_at_ms"], 2);
    }

    #[test]
    fn core_native_acknowledgement_binds_prepared_bodies_not_later_bodies() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 32_768);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let prepared = prepare(&session, StoreHead::default());
        session.first_user_message = Some("later-message".into());
        session.checkpoint.as_mut().unwrap().summary = "later-summary".into();
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(session.acknowledge_archive_save(&prepared, &result));
        let later = prepare(&session, result.receipt.current);
        assert!(
            matches!(&later.command.archives.first_user_message, ValueEdit::Replace { value: Some(message) } if message.as_ref() == "later-message")
        );
        assert!(
            matches!(&later.command.archives.checkpoint, CheckpointEdit::ReplaceRecord { record } if matches!(&record.summary, CheckpointSummary::New { text } if text.as_ref() == "later-summary"))
        );
    }

    #[test]
    fn core_native_result_rejection_does_not_consume_pending_changes() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let prepared = prepare(&session, StoreHead::default());
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        let mut clone = session.clone();
        let mut fork = session.fork_store_backed(4242);
        assert!(!clone.acknowledge_archive_save(&prepared, &result));
        assert!(!fork.acknowledge_archive_save(&prepared, &result));
        for boundary in 0..5 {
            let mut invalid = result.clone();
            match boundary {
                0 => invalid.receipt.session_id = "other".into(),
                1 => invalid.receipt.previous.history_len = HistoryLen::new(9),
                2 => {
                    invalid.receipt.current.transcript_record_count = TranscriptRecordCount::new(9)
                }
                3 => invalid.receipt.lineage_id = None,
                _ => invalid.revision_id = "not-an-exact-revision".into(),
            }
            assert!(!session.acknowledge_archive_save(&prepared, &invalid));
            assert!(session.metadata_snapshots.changed_suffix().is_some());
            assert!(session.archive_base().is_none());
        }
        assert!(session.acknowledge_archive_save(&prepared, &result));
        assert_eq!(
            session.archive_base().unwrap().revision_id,
            result.revision_id
        );
    }

    #[test]
    fn core_native_same_revision_replay_does_not_regress_acknowledged_tokens_or_body_ownership() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 32_768);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let original = prepare(&session, StoreHead::default());
        let original_result = writer.commit_compact_session(original.command()).unwrap();
        assert!(session.acknowledge_archive_save(&original, &original_result));
        session.first_user_message = Some(
            session
                .first_user_message
                .as_ref()
                .unwrap()
                .to_string()
                .into(),
        );
        session.checkpoint.as_mut().unwrap().summary = session
            .checkpoint
            .as_ref()
            .unwrap()
            .summary
            .to_string()
            .into();
        session.title = Some("temporary".into());
        session.snapshot_metadata_at(1);
        session.title = Some("initial".into());
        session.snapshot_metadata_at(1);
        session.turn_metas.push((
            1,
            protocol::TurnMeta {
                elapsed_ms: 1,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        assert!(session.turn_metas.clear());
        session.snapshot_context_at(1);
        assert!(session.context_snapshots.clear());
        let events = session.checkpoint_events.to_vec();
        session.checkpoint_events.clear();
        for event in events {
            session.checkpoint_events.push(event);
        }
        let no_op = prepare(&session, original_result.receipt.current);
        let result = writer.commit_compact_session(no_op.command()).unwrap();
        assert_eq!(result.receipt.previous, result.receipt.current);
        assert_eq!(result.revision_id, original_result.revision_id);
        assert!(session.acknowledge_archive_save(&no_op, &result));
        assert!(session.acknowledge_archive_save(&original, &original_result));
        let next = prepare(&session, result.receipt.current);
        assert!(
            matches!(
                next.command.archives.metadata_snapshots,
                ArchiveEdit::Retain
            ),
            "replayed older token must not invalidate the acknowledged table base"
        );
        assert!(matches!(
            next.command.archives.turn_metas,
            ArchiveEdit::Retain
        ));
        assert!(matches!(
            next.command.archives.context_snapshots,
            ArchiveEdit::Retain
        ));
        assert!(matches!(
            next.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        assert!(
            matches!(next.command.archives.first_user_message, ValueEdit::Retain),
            "replayed older body pointer must not invalidate the active message base"
        );
        assert!(
            matches!(next.command.archives.checkpoint, CheckpointEdit::Retain),
            "replayed older summary pointer must not invalidate the checkpoint base"
        );
    }

    #[test]
    fn core_native_out_of_order_results_keep_the_newest_base_and_pending_edits() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let first = save(&mut session, &mut writer, StoreHead::default());
        session.title = Some("older".into());
        session.snapshot_metadata_at(1);
        let older = prepare(&session, first.receipt.current);
        let older_result = writer.commit_compact_session(older.command()).unwrap();
        session.title = Some("newer".into());
        session.snapshot_metadata_at(1);
        let newer = prepare(&session, older_result.receipt.current);
        assert_eq!(newer.command.archive_base, older.command.archive_base);
        let newer_result = writer.commit_compact_session(newer.command()).unwrap();
        assert!(session.acknowledge_archive_save(&newer, &newer_result));
        session.title = Some("pending".into());
        session.snapshot_metadata_at(1);
        assert!(!session.acknowledge_archive_save(&older, &older_result));
        assert_eq!(
            session.archive_base().unwrap().revision_id,
            newer_result.revision_id
        );
        assert_eq!(
            session.metadata_snapshots.changed_suffix().unwrap().records[0]
                .1
                .title
                .as_deref(),
            Some("pending")
        );
        let saved = save(&mut session, &mut writer, newer_result.receipt.current);
        for _ in 0..3 {
            let no_op = prepare(&session, saved.receipt.current);
            let result = writer.commit_compact_session(no_op.command()).unwrap();
            assert_eq!(result.receipt.previous, result.receipt.current);
            assert_eq!(result.revision_id, saved.revision_id);
            assert!(session.acknowledge_archive_save(&no_op, &result));
            assert!(session.acknowledge_archive_save(&no_op, &result));
            assert!(session.metadata_snapshots.changed_suffix().is_none());
        }
    }

    #[test]
    fn core_native_accounting_retain_requires_the_exact_expected_scalar_base() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(0, 0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let first = save(&mut session, &mut writer, StoreHead::default());
        session.session_usage.prompt_tokens = Some(7);
        let changed = prepare(&session, first.receipt.current);
        let result = writer.commit_compact_session(changed.command()).unwrap();
        session.session_usage.prompt_tokens = None;
        session.title = Some("reverted-accounting".into());
        let reverted = prepare(&session, result.receipt.current);
        assert!(matches!(
            reverted.command.scalars.accounting,
            ValueEdit::Replace { .. }
        ));
        let result = writer.commit_compact_session(reverted.command()).unwrap();
        assert!(session.acknowledge_archive_save(&reverted, &result));
        assert!(
            writer.snapshot().unwrap().metadata.accounting_json.unwrap()["session_usage"]
                .get("prompt_tokens")
                .is_none()
        );
    }

    #[test]
    fn core_native_logical_equality_and_wire_bytes_exclude_archive_ownership() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 32_768);
        let logical = session.clone();
        let before = serde_json::to_vec(&session).unwrap();
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        save(&mut session, &mut writer, StoreHead::default());
        assert!(session == logical);
        assert!(serde_json::to_vec(&session).unwrap() == before);
        let mut decoded: Session = serde_json::from_slice(&before).unwrap();
        assert!(decoded == session);
        assert!(decoded.archive_base().is_none());
        let own_frame = prepare(&session, writer.snapshot().unwrap().head);
        let result = writer.commit_compact_session(own_frame.command()).unwrap();
        assert!(!decoded.acknowledge_archive_save(&own_frame, &result));
        let wire: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(wire["first_user_message"].as_str().unwrap().len(), 32_768);
        assert_eq!(
            wire["metadata_snapshots"][0][1]["first_user_message"]
                .as_str()
                .unwrap()
                .len(),
            32_768
        );
        assert!(wire["checkpoint_events"][0]["summary"].is_string());
    }

    #[test]
    fn core_native_filtered_timeline_binding_fails_closed_without_changing_legacy_reads() {
        let root = tempfile::tempdir().unwrap();
        let mut session = session(2, 0);
        let mut writer = OwnedLineageWriter::open(root.path(), &session.id).unwrap();
        let result = save(&mut session, &mut writer, StoreHead::default());
        let snapshot = writer.snapshot().unwrap();
        let mut metadata = snapshot.metadata;
        metadata.checkpoint_events_json.as_mut().unwrap()[0]["completed_at_history_len"] = json!(2);
        session.checkpoint_events =
            super::super::checkpoint_events_from_json(metadata.checkpoint_events_json.clone(), 1);
        assert_eq!(session.checkpoint_events.len(), 1);
        session.bind_loaded_archives(
            SessionArchiveBase {
                lineage_id: snapshot.lineage_id,
                revision_id: snapshot.revision_id,
                branch_sequence: result.receipt.current.revision,
            },
            &metadata,
        );
        let error = session
            .prepare_archive_save(
                result.receipt.current,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Integrity(_)));
        assert_eq!(session.checkpoint_events.len(), 1);
        assert_eq!(
            writer
                .snapshot()
                .unwrap()
                .metadata
                .checkpoint_events_json
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn core_native_title_save_preserves_unrecognized_loaded_accounting() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(0, 0);
        storage.save_result(&original).unwrap();
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut legacy =
            super::super::store_commit_from_session(&original, writer.snapshot().unwrap().head, 1)
                .unwrap();
        let accounting =
            json!({"session_usage": {"prompt_tokens": 7}, "future_field": {"opaque": "synthetic"}});
        legacy.metadata.accounting_json = Some(accounting.clone());
        writer.commit_session(&legacy).unwrap();
        let mut resumed = storage
            .load_store_resume_result(&original.id, 80, 24)
            .unwrap()
            .unwrap();
        resumed.session.title = Some("title-only".into());
        let prepared = resumed
            .session
            .prepare_archive_save(
                resumed.head,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap();
        resumed.session.title = Some("queued-title-only".into());
        let queued = resumed
            .session
            .prepare_archive_save(
                resumed.head,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap();
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(
            writer.snapshot().unwrap().metadata.accounting_json == Some(accounting.clone()),
            "title-only save must preserve opaque accounting fields"
        );
        let finalized = queued.finalize_after(&prepared, &result).unwrap();
        assert!(matches!(
            finalized.command.scalars.accounting,
            ValueEdit::Retain
        ));
        let final_result = writer.commit_compact_session(finalized.command()).unwrap();
        assert!(resumed
            .session
            .acknowledge_archive_save(&finalized, &final_result));
        assert!(
            writer.snapshot().unwrap().metadata.accounting_json == Some(accounting),
            "finalized title-only save must preserve opaque accounting fields"
        );
    }

    #[test]
    fn core_native_materialized_title_save_retains_verified_archives() {
        for (events, body_bytes) in [(32, 32_768), (0, 128), (128, 32_768), (0, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let storage = SessionStorage::new(root.path().to_path_buf());
            let original = session(events, body_bytes);
            storage.save_result(&original).unwrap();
            let resumed = storage
                .load_store_resume_result(&original.id, 80, 24)
                .unwrap()
                .unwrap();
            let live = crate::session_runtime::LiveSession::from_store(
                resumed.header,
                resumed.store_address,
            );
            let mut materialized = live
                .materialize_full_session(&resumed.session, "synthetic:materialize")
                .unwrap();
            assert_eq!(materialized.history, original.history);
            assert_eq!(materialized.archive_base(), resumed.session.archive_base());
            assert!(Arc::ptr_eq(
                materialized.first_user_message.as_ref().unwrap(),
                resumed.session.first_user_message.as_ref().unwrap()
            ));
            assert_ne!(
                materialized.metadata_snapshots.version(),
                resumed.session.metadata_snapshots.version()
            );
            materialized.title = Some("materialized-title".into());
            materialized.snapshot_metadata_at(1);
            let prepared = prepare(&materialized, resumed.head);
            let bytes = serde_json::to_vec(prepared.command()).unwrap().len();
            eprintln!("CORE_MATERIALIZED_ARCHIVES events={events} body_bytes={body_bytes} title_command_bytes={bytes}");
            assert!(
                bytes < 4096,
                "materialized title command inflated to {bytes} bytes"
            );
            assert!(matches!(
                prepared.command.archives.first_user_message,
                ValueEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint,
                CheckpointEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint_events,
                CheckpointEventsEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.turn_metas,
                ArchiveEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.context_snapshots,
                ArchiveEdit::Retain
            ));
            let mut writer =
                OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
            let before = writer.snapshot().unwrap();
            let result = writer.commit_compact_session(prepared.command()).unwrap();
            assert!(materialized.acknowledge_archive_save(&prepared, &result));
            assert!(!resumed
                .session
                .clone()
                .acknowledge_archive_save(&prepared, &result));
            let after = writer.snapshot().unwrap();
            assert_eq!(after.metadata.title.as_deref(), Some("materialized-title"));
            assert_eq!(
                after.metadata.first_user_message,
                before.metadata.first_user_message
            );
            assert_eq!(
                after.metadata.checkpoint_json,
                before.metadata.checkpoint_json
            );
            assert_eq!(
                after.metadata.checkpoint_events_json,
                before.metadata.checkpoint_events_json
            );
            assert_eq!(
                after.metadata.accounting_json,
                before.metadata.accounting_json
            );
            assert_eq!(after.side_tables.turn_metas, before.side_tables.turn_metas);
            assert_eq!(
                after.side_tables.context_snapshots,
                before.side_tables.context_snapshots
            );
            assert_eq!(writer.history_range(0, 1).unwrap(), original.history);
            assert!(resumed
                .session
                .metadata_snapshots
                .changed_suffix()
                .is_none());
        }
    }

    #[test]
    fn core_native_cloned_archives_keep_pending_suffixes_and_partial_acknowledgements() {
        let root = tempfile::tempdir().unwrap();
        let mut source = session(2, 32_768);
        source
            .history
            .resize(3, HistoryItem::user(Content::text("synthetic")));
        source.turn_metas.push((
            1,
            protocol::TurnMeta {
                elapsed_ms: 1,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        source.snapshot_context_at(1);
        let mut writer = OwnedLineageWriter::open(root.path(), &source.id).unwrap();
        let initial = save(&mut source, &mut writer, StoreHead::default());
        source.title = Some("pending-before-clone".into());
        source.snapshot_metadata_at(3);
        source.turn_metas.push((
            3,
            protocol::TurnMeta {
                elapsed_ms: 2,
                avg_tps: None,
                display_tps: None,
                interrupted: true,
            },
        ));
        source.context_tokens = Some(17);
        source.context_tokens_history_len = Some(3);
        source.snapshot_context_at(3);
        source.checkpoint_events.push(ContextCheckpointEvent {
            kind: "compaction".into(),
            summary: "pending-summary".into(),
            first_live_index: 1,
            completed_at_history_len: 3,
            created_at_ms: 3,
        });
        let mut cloned = source.clone();
        assert_eq!(
            serde_json::to_vec(&cloned).unwrap(),
            serde_json::to_vec(&source).unwrap()
        );
        assert!(!cloned.turn_metas.acknowledge(source.turn_metas.version()));
        assert!(!cloned
            .metadata_snapshots
            .acknowledge(source.metadata_snapshots.version()));
        assert!(!cloned
            .context_snapshots
            .acknowledge(source.context_snapshots.version()));
        assert!(!cloned
            .checkpoint_events
            .acknowledge(source.checkpoint_events.version()));
        let prepared = prepare(&cloned, initial.receipt.current);
        assert!(matches!(&prepared.command.archives.turn_metas,
            ArchiveEdit::ReplaceSuffix { retain_records: 1, records } if records.len() == 1));
        assert!(matches!(&prepared.command.archives.metadata_snapshots,
            ArchiveEdit::ReplaceSuffix { retain_records: 1, records } if records.len() == 1));
        assert!(matches!(&prepared.command.archives.context_snapshots,
            ArchiveEdit::ReplaceSuffix { retain_records: 1, records } if records.len() == 1));
        assert!(matches!(&prepared.command.archives.checkpoint_events,
            CheckpointEventsEdit::ReplaceRecordsSuffix { retain_records: 2, records } if records.len() == 1));
        assert!(serde_json::to_vec(prepared.command()).unwrap().len() < 4096);
        cloned.title = Some("later-clone-title".into());
        cloned.snapshot_metadata_at(3);
        cloned.checkpoint_events.push(ContextCheckpointEvent {
            kind: "compaction".into(),
            summary: "later-summary".into(),
            first_live_index: 1,
            completed_at_history_len: 3,
            created_at_ms: 4,
        });
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(!source.acknowledge_archive_save(&prepared, &result));
        assert!(cloned.acknowledge_archive_save(&prepared, &result));
        assert_eq!(
            source.archive_base().unwrap().revision_id,
            initial.revision_id
        );
        assert!(source.turn_metas.changed_suffix().is_some());
        assert!(source.metadata_snapshots.changed_suffix().is_some());
        assert!(source.context_snapshots.changed_suffix().is_some());
        assert_eq!(source.checkpoint_events.changed_suffix().unwrap().0, 2);
        assert!(cloned.turn_metas.changed_suffix().is_none());
        assert!(cloned.context_snapshots.changed_suffix().is_none());
        assert_eq!(
            cloned.metadata_snapshots.changed_suffix().unwrap().records[0]
                .1
                .title
                .as_deref(),
            Some("later-clone-title")
        );
        assert_eq!(cloned.checkpoint_events.changed_suffix().unwrap().0, 3);
        let stored = writer.snapshot().unwrap();
        assert_eq!(
            stored.metadata.title.as_deref(),
            Some("pending-before-clone")
        );
        assert_eq!(stored.side_tables.turn_metas[1].1["elapsed_ms"], 2);
        assert_eq!(
            stored.side_tables.context_snapshots[1].1["context_tokens"],
            17
        );
        assert_eq!(
            stored
                .metadata
                .checkpoint_events_json
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let next = prepare(&cloned, result.receipt.current);
        assert!(matches!(&next.command.archives.checkpoint_events,
            CheckpointEventsEdit::ReplaceRecordsSuffix { retain_records: 3, records } if records.len() == 1));
        let result = writer.commit_compact_session(next.command()).unwrap();
        assert!(cloned.acknowledge_archive_save(&next, &result));
        assert_eq!(
            writer.snapshot().unwrap().metadata.title.as_deref(),
            Some("later-clone-title")
        );
    }

    #[test]
    fn core_native_cloned_archives_do_not_bind_foreign_clean_table_replacements() {
        let root = tempfile::tempdir().unwrap();
        let mut source = session(2, 128);
        let mut writer = OwnedLineageWriter::open(root.path(), &source.id).unwrap();
        let initial = save(&mut source, &mut writer, StoreHead::default());
        source.title = Some("foreign-table-title".into());
        source.snapshot_metadata_at(1);
        source.metadata_snapshots = source.metadata_snapshots.clone();
        source
            .metadata_snapshots
            .acknowledge(source.metadata_snapshots.version());
        source.turn_metas.push((
            1,
            protocol::TurnMeta {
                elapsed_ms: 77,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        source.turn_metas = source.turn_metas.clone();
        source.turn_metas.acknowledge(source.turn_metas.version());
        source.context_tokens = Some(99);
        source.context_tokens_history_len = Some(1);
        source.snapshot_context_at(1);
        source.context_snapshots = source.context_snapshots.clone();
        source
            .context_snapshots
            .acknowledge(source.context_snapshots.version());
        source.checkpoint_events.push(ContextCheckpointEvent {
            kind: "compaction".into(),
            summary: "foreign-summary".into(),
            first_live_index: 1,
            completed_at_history_len: 1,
            created_at_ms: 3,
        });
        source.checkpoint_events = source.checkpoint_events.clone();
        source
            .checkpoint_events
            .acknowledge(source.checkpoint_events.version());
        let mut cloned = source.clone();
        let prepared = prepare(&cloned, initial.receipt.current);
        assert!(matches!(&prepared.command.archives.turn_metas,
            ArchiveEdit::ReplaceSuffix { retain_records: 0, records } if records.len() == 1));
        assert!(matches!(&prepared.command.archives.metadata_snapshots,
            ArchiveEdit::ReplaceSuffix { retain_records: 0, records } if records[0].fields["title"] == "foreign-table-title"));
        assert!(matches!(&prepared.command.archives.context_snapshots,
            ArchiveEdit::ReplaceSuffix { retain_records: 0, records } if records[0].value["context_tokens"] == 99));
        assert!(matches!(&prepared.command.archives.checkpoint_events,
            CheckpointEventsEdit::ReplaceRecordsSuffix { retain_records: 0, records } if records.len() == 3));
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(cloned.acknowledge_archive_save(&prepared, &result));
        let stored = writer.snapshot().unwrap();
        assert_eq!(
            serde_json::to_value(stored.side_tables.turn_metas).unwrap(),
            serde_json::to_value(&cloned.turn_metas).unwrap()
        );
        assert_eq!(
            serde_json::to_value(stored.side_tables.metadata_snapshots).unwrap(),
            serde_json::to_value(&cloned.metadata_snapshots).unwrap()
        );
        assert_eq!(
            serde_json::to_value(stored.side_tables.context_snapshots).unwrap(),
            serde_json::to_value(&cloned.context_snapshots).unwrap()
        );
        assert_eq!(
            stored.metadata.checkpoint_events_json,
            Some(serde_json::to_value(&cloned.checkpoint_events).unwrap())
        );
    }

    #[test]
    fn core_native_cloned_noop_keeps_exact_commands_and_unknown_accounting() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(2, 128);
        storage.save_result(&original).unwrap();
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut legacy =
            super::super::store_commit_from_session(&original, writer.snapshot().unwrap().head, 1)
                .unwrap();
        let accounting =
            json!({"session_usage": {"prompt_tokens": 7}, "future_field": {"opaque": "synthetic"}});
        legacy.metadata.accounting_json = Some(accounting.clone());
        writer.commit_session(&legacy).unwrap();
        let mut source = storage.load_full_result(&original.id).unwrap().unwrap();
        let head = writer.snapshot().unwrap().head;
        let mut cloned = source.clone();
        let source_frame = prepare(&source, head);
        let cloned_frame = prepare(&cloned, head);
        assert_eq!(source_frame.command(), cloned_frame.command());
        assert_eq!(
            serde_json::to_vec(&source).unwrap(),
            serde_json::to_vec(&cloned).unwrap()
        );
        let result = writer
            .commit_compact_session(cloned_frame.command())
            .unwrap();
        assert_eq!(result.receipt.current, head);
        assert_eq!(
            result.revision_id,
            source.archive_base().unwrap().revision_id
        );
        assert!(!source.acknowledge_archive_save(&cloned_frame, &result));
        assert!(!cloned.acknowledge_archive_save(&source_frame, &result));
        assert!(cloned.acknowledge_archive_save(&cloned_frame, &result));
        cloned.title = Some("cloned-title".into());
        cloned.snapshot_metadata_at(1);
        let titled = prepare(&cloned, head);
        assert!(matches!(
            titled.command.scalars.accounting,
            ValueEdit::Retain
        ));
        let result = writer.commit_compact_session(titled.command()).unwrap();
        assert!(cloned.acknowledge_archive_save(&titled, &result));
        assert_eq!(
            writer.snapshot().unwrap().metadata.accounting_json,
            Some(accounting)
        );
    }

    #[test]
    fn core_native_full_load_title_save_retains_verified_archives() {
        for (events, body_bytes) in [(0, 128), (32, 32_768), (128, 32_768), (0, 1_048_576)] {
            let root = tempfile::tempdir().unwrap();
            let storage = SessionStorage::new(root.path().to_path_buf());
            let mut original = session(events, body_bytes);
            if let Some(event) = original.checkpoint_events.last() {
                original.checkpoint = Some(event.to_checkpoint());
            }
            storage.save_result(&original).unwrap();
            let mut writer =
                OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
            let before = writer.snapshot().unwrap();
            let mut loaded = storage.load_full_result(&original.id).unwrap().unwrap();
            assert_eq!(loaded.history, original.history);
            assert!(Arc::ptr_eq(
                loaded.first_user_message.as_ref().unwrap(),
                loaded
                    .metadata_snapshots
                    .last()
                    .unwrap()
                    .1
                    .first_user_message
                    .as_ref()
                    .unwrap(),
            ));
            if let Some(event) = loaded.checkpoint_events.last() {
                assert!(Arc::ptr_eq(
                    &loaded.checkpoint.as_ref().unwrap().summary,
                    &event.summary
                ));
            }
            loaded.title = Some("full-load-title".into());
            loaded.snapshot_metadata_at(1);
            let prepared = loaded
                .prepare_archive_save(
                    before.head,
                    HistorySuffix {
                        start: HistoryIndex::new(1),
                        final_len: HistoryLen::new(1),
                        items: Vec::new(),
                    },
                    None,
                )
                .expect("a verified full load must support a native title save");
            let base = loaded.archive_base().unwrap();
            assert_eq!(base.lineage_id, before.lineage_id);
            assert_eq!(base.revision_id, before.revision_id);
            assert_eq!(base.branch_sequence, before.head.revision);
            assert!(matches!(
                prepared.command.archives.first_user_message,
                ValueEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint,
                CheckpointEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint_events,
                CheckpointEventsEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.turn_metas,
                ArchiveEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.context_snapshots,
                ArchiveEdit::Retain
            ));
            let ArchiveEdit::ReplaceSuffix { records, .. } =
                &prepared.command.archives.metadata_snapshots
            else {
                panic!("title snapshot suffix");
            };
            assert_eq!(records.len(), 1);
            assert!(matches!(records[0].message, MetadataMessage::Active));
            let bytes = serde_json::to_vec(prepared.command()).unwrap().len();
            eprintln!("CORE_FULL_LOAD_ARCHIVES events={events} body_bytes={body_bytes} title_command_bytes={bytes}");
            assert!(
                bytes < 4096,
                "full-load title command inflated to {bytes} bytes"
            );
            let result = writer.commit_compact_session(prepared.command()).unwrap();
            assert!(loaded.acknowledge_archive_save(&prepared, &result));
            let after = writer.snapshot().unwrap();
            assert_eq!(after.metadata.title.as_deref(), Some("full-load-title"));
            assert_eq!(
                after.metadata.first_user_message,
                before.metadata.first_user_message
            );
            assert_eq!(
                after.metadata.checkpoint_json,
                before.metadata.checkpoint_json
            );
            assert_eq!(
                after.metadata.checkpoint_events_json,
                before.metadata.checkpoint_events_json
            );
            assert_eq!(
                after.metadata.accounting_json,
                before.metadata.accounting_json
            );
            assert_eq!(writer.history_range(0, 1).unwrap(), original.history);
        }
    }

    #[test]
    fn core_native_startup_interruption_title_save_preserves_verified_archives_and_accounting() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(32, 32_768);
        storage.save_result(&original).unwrap();
        let mut writer =
            smelt_store::SessionWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut legacy =
            super::super::store_commit_from_session(&original, writer.store_head().unwrap(), 1)
                .unwrap();
        let accounting =
            json!({"session_usage": {"prompt_tokens": 7}, "future": {"opaque": [7, "retained"]}});
        legacy.metadata.accounting_json = Some(accounting.clone());
        writer
            .submit_turn(&smelt_store::SubmitTurn {
                session: legacy,
                turn: smelt_store::NewTurn {
                    kind: smelt_store::TurnKind::Command,
                    submitted_history_idx: HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: original.updated_at_ms,
                },
            })
            .unwrap();
        writer.release().unwrap();
        let mut resumed = storage
            .load_store_resume_result(&original.id, 80, 24)
            .unwrap()
            .unwrap();
        let mut writer =
            smelt_store::SessionWriter::open_existing(storage.sessions_dir(), &original.id)
                .unwrap();
        let recovery = writer.startup_recovery().unwrap().clone();
        assert_eq!(recovery.session.receipt.previous, resumed.head);
        assert_eq!(
            recovery.session.receipt.previous.history_len,
            recovery.session.receipt.current.history_len
        );
        resumed.session.title = Some("title after startup interruption".into());
        resumed.session.snapshot_metadata_at(1);

        let base = resumed.session.archive_base().unwrap().clone();
        let mut rejected = Vec::new();
        let mut foreign = recovery.clone();
        foreign.session.receipt.session_id = "foreign".into();
        rejected.push(foreign);
        let mut foreign = recovery.clone();
        foreign.session.receipt.lineage_id = Some("foreign".into());
        rejected.push(foreign);
        let mut foreign = recovery.clone();
        foreign.session.receipt.lineage_id = None;
        rejected.push(foreign);
        let mut foreign = recovery.clone();
        foreign.session.revision_id = "0".repeat(64);
        rejected.push(foreign);
        let mut empty = recovery.clone();
        empty.interrupted_turns.clear();
        rejected.push(empty);
        let mut malformed = recovery.clone();
        malformed.session.receipt.current.history_len = HistoryLen::new(2);
        rejected.push(malformed);
        let mut malformed = recovery.clone();
        malformed.session.receipt.current.transcript_record_count = TranscriptRecordCount::new(1);
        rejected.push(malformed);
        let mut malformed = recovery.clone();
        malformed.session.receipt.current.revision = recovery.session.receipt.previous.revision;
        rejected.push(malformed);
        let mut stale = recovery.clone();
        stale.session.receipt.previous.revision = smelt_store::Revision::ZERO;
        stale.session.receipt.current.revision = smelt_store::Revision::new(1);
        rejected.push(stale);
        let pending_versions = versions(&resumed.session);
        for result in rejected {
            assert!(!resumed.session.acknowledge_startup_recovery(&result));
            assert_eq!(resumed.session.archive_base(), Some(&base));
            assert_eq!(versions(&resumed.session), pending_versions);
        }
        assert!(!original.clone().acknowledge_startup_recovery(&recovery));
        let mut renamed = resumed.session.clone();
        renamed.id = "foreign".into();
        assert!(!renamed.acknowledge_startup_recovery(&recovery));

        let prepare_pending = |session: &Session, head: StoreHead| {
            session
                .prepare_archive_save(
                    head,
                    HistorySuffix {
                        start: HistoryIndex::new(head.history_len.get()),
                        final_len: head.history_len,
                        items: Vec::new(),
                    },
                    None,
                )
                .unwrap()
        };
        let mut dirty = resumed.session.clone();
        dirty.turn_metas.push((
            1,
            protocol::TurnMeta {
                elapsed_ms: 9,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));
        dirty.session_usage.prompt_tokens = Some(9);
        dirty.context_tokens = Some(17);
        dirty.context_tokens_history_len = Some(1);
        assert!(dirty.snapshot_context_at(1));
        dirty.checkpoint_events.push(ContextCheckpointEvent {
            kind: "manual".into(),
            summary: "pending summary".into(),
            first_live_index: 1,
            completed_at_history_len: 1,
            created_at_ms: original.updated_at_ms,
        });
        dirty.first_user_message = Some("pending message".into());
        dirty.checkpoint.as_mut().unwrap().summary = "pending active summary".into();
        let before = prepare_pending(&dirty, recovery.session.receipt.current);
        assert!(matches!(
            before.command.archives.turn_metas,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        assert!(matches!(
            before.command.archives.metadata_snapshots,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        assert!(matches!(
            before.command.archives.context_snapshots,
            ArchiveEdit::ReplaceSuffix { .. }
        ));
        assert!(matches!(
            before.command.archives.checkpoint_events,
            CheckpointEventsEdit::ReplaceRecordsSuffix { .. }
        ));
        let dirty_versions = versions(&dirty);
        assert!(dirty.acknowledge_startup_recovery(&recovery));
        assert!(dirty.acknowledge_startup_recovery(&recovery));
        let after = prepare_pending(&dirty, recovery.session.receipt.current);
        assert_eq!(before.command.archives, after.command.archives);
        assert_eq!(before.command.scalars, after.command.scalars);
        assert_eq!(dirty_versions, versions(&dirty));
        assert!(matches!(
            after.command.scalars.accounting,
            ValueEdit::Replace { .. }
        ));

        assert!(resumed.session.acknowledge_startup_recovery(&recovery));
        assert!(resumed.session.acknowledge_startup_recovery(&recovery));
        assert_eq!(versions(&resumed.session), pending_versions);
        assert_eq!(
            resumed.session.archive_base().unwrap().revision_id,
            base.revision_id
        );
        assert_eq!(
            resumed.session.archive_base().unwrap().branch_sequence,
            recovery.head().revision
        );
        let prepared = resumed
            .session
            .prepare_archive_save(
                recovery.session.receipt.current,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap();
        assert!(serde_json::to_vec(prepared.command()).unwrap().len() < 4096);
        assert!(matches!(
            prepared.command.scalars.accounting,
            ValueEdit::Retain
        ));
        let saved = publish_frame(&mut writer, &prepared);
        assert!(resumed.session.acknowledge_archive_save(&prepared, &saved));
        let advanced_base = resumed.session.archive_base().unwrap().clone();
        assert!(!resumed.session.acknowledge_startup_recovery(&recovery));
        assert_eq!(resumed.session.archive_base(), Some(&advanced_base));
        let snapshot = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(snapshot.metadata.accounting_json, Some(accounting));
        assert_eq!(
            snapshot.metadata.first_user_message.as_deref(),
            original.first_user_message.as_deref()
        );
        assert_eq!(
            snapshot
                .metadata
                .checkpoint_events_json
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            32
        );

        let pending = prepare_pending(&dirty, saved.receipt.current);
        let result = publish_frame(&mut writer, &pending);
        assert!(dirty.acknowledge_archive_save(&pending, &result));
        let stored = writer.lineage_writer_mut().snapshot().unwrap();
        assert_eq!(
            stored.metadata.first_user_message.as_deref(),
            Some("pending message")
        );
        assert_eq!(
            stored.metadata.checkpoint_json.as_ref().unwrap()["summary"],
            "pending active summary"
        );
        let events = stored
            .metadata
            .checkpoint_events_json
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(events.len(), 33);
        assert_eq!(
            &events[..32],
            snapshot
                .metadata
                .checkpoint_events_json
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
        );
        assert_eq!(events[32]["summary"], "pending summary");
        assert_eq!(
            stored.side_tables.turn_metas.len(),
            snapshot.side_tables.turn_metas.len() + 1
        );
        assert_eq!(
            stored.side_tables.context_snapshots.len(),
            dirty.context_snapshots.len()
        );
        assert_eq!(
            stored.side_tables.context_snapshots.last().unwrap().1["context_tokens"],
            17
        );
        assert_eq!(
            stored.side_tables.metadata_snapshots.last().unwrap().1["title"],
            "title after startup interruption"
        );
        assert_eq!(
            stored.metadata.accounting_json.as_ref().unwrap()["session_usage"]["prompt_tokens"],
            9
        );
        writer.release().unwrap();
    }

    #[test]
    fn core_native_full_load_title_save_preserves_unrecognized_accounting() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(0, 128);
        storage.save_result(&original).unwrap();
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut legacy =
            super::super::store_commit_from_session(&original, writer.store_head().unwrap(), 1)
                .unwrap();
        let accounting = json!({"future": {"opaque": [7, "retained"]}});
        legacy.metadata.accounting_json = Some(accounting.clone());
        writer.commit_session(&legacy).unwrap();
        let before = writer.snapshot().unwrap();
        let mut loaded = storage.load_full_result(&original.id).unwrap().unwrap();
        loaded.title = Some("full-load-opaque-accounting".into());
        let prepared = prepare(&loaded, before.head);
        assert!(matches!(
            prepared.command.scalars.accounting,
            ValueEdit::Retain
        ));
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(loaded.acknowledge_archive_save(&prepared, &result));
        assert_eq!(
            writer.snapshot().unwrap().metadata.accounting_json,
            Some(accounting)
        );
    }

    #[test]
    fn core_native_pending_fork_preserves_verified_archive_prefixes() {
        for events in [0, 32, 128] {
            let root = tempfile::tempdir().unwrap();
            let mut source = session(events, 32_768);
            source
                .history
                .resize(3, HistoryItem::user(Content::text("synthetic")));
            source.turn_metas.push((
                1,
                protocol::TurnMeta {
                    elapsed_ms: 1,
                    avg_tps: None,
                    display_tps: None,
                    interrupted: false,
                },
            ));
            source.snapshot_context_at(1);
            let mut writer = OwnedLineageWriter::open(root.path(), &source.id).unwrap();
            let first = save(&mut source, &mut writer, StoreHead::default());
            source.title = Some("published-source".into());
            source.snapshot_metadata_at(2);
            let published = save(&mut source, &mut writer, first.receipt.current);
            let original = writer.snapshot().unwrap();
            source.title = Some("pending-fork".into());
            source.snapshot_metadata_at(3);
            source.turn_metas.push((
                3,
                protocol::TurnMeta {
                    elapsed_ms: 2,
                    avg_tps: None,
                    display_tps: None,
                    interrupted: true,
                },
            ));
            source.context_tokens = Some(17);
            source.context_tokens_history_len = Some(3);
            source.snapshot_context_at(3);
            source.checkpoint_events.push(ContextCheckpointEvent {
                kind: "compaction".into(),
                summary: "pending-summary".into(),
                first_live_index: 1,
                completed_at_history_len: 3,
                created_at_ms: 3,
            });
            let mut forked = source.fork_store_backed(4242);
            assert!(forked.archive_base().is_none());
            let (mut destination, imported) = OwnedLineageWriter::fork_from(
                root.path(),
                &source.id,
                &forked.id,
                forked.created_at_ms,
                Some(published.receipt.current),
                &|| false,
            )
            .unwrap();
            assert_eq!(imported.session.receipt.current.revision.get(), 1);
            assert_eq!(published.receipt.current.revision.get(), 2);
            assert!(forked.bind_fork_archives(&imported));
            assert!(!forked.bind_fork_archives(&imported));
            let prepared = forked.prepare_archive_save(imported.session.receipt.current, HistorySuffix {
                start: HistoryIndex::new(3), final_len: HistoryLen::new(3), items: Vec::new(),
            }, None).expect("a verified root-copy fork must preserve pending native edits without reloading archives");
            assert!(matches!(&prepared.command.archives.turn_metas,
                ArchiveEdit::ReplaceSuffix { retain_records: 1, records } if records.len() == 1));
            assert!(matches!(&prepared.command.archives.metadata_snapshots,
                ArchiveEdit::ReplaceSuffix { retain_records: 2, records } if records.len() == 1));
            assert!(matches!(&prepared.command.archives.context_snapshots,
                ArchiveEdit::ReplaceSuffix { retain_records: 1, records } if records.len() == 1));
            assert!(matches!(&prepared.command.archives.checkpoint_events,
                CheckpointEventsEdit::ReplaceRecordsSuffix { retain_records, records }
                    if *retain_records == events as u64 && records.len() == 1));
            assert!(matches!(
                prepared.command.archives.first_user_message,
                ValueEdit::Retain
            ));
            assert!(matches!(
                prepared.command.archives.checkpoint,
                CheckpointEdit::Retain
            ));
            let bytes = serde_json::to_vec(prepared.command()).unwrap().len();
            eprintln!("NATIVE_PENDING_FORK events={events} command_bytes={bytes}");
            assert!(
                bytes < 4096,
                "retained fork archives inflated command to {bytes} bytes"
            );
            forked.title = Some("later-fork-title".into());
            forked.snapshot_metadata_at(3);
            let result = destination
                .commit_compact_session(prepared.command())
                .unwrap();
            assert!(!source.acknowledge_archive_save(&prepared, &result));
            assert!(forked.acknowledge_archive_save(&prepared, &result));
            assert!(source.turn_metas.changed_suffix().is_some());
            assert!(source.metadata_snapshots.changed_suffix().is_some());
            assert!(forked.turn_metas.changed_suffix().is_none());
            assert!(forked.context_snapshots.changed_suffix().is_none());
            assert!(forked.checkpoint_events.changed_suffix().is_none());
            assert!(forked.metadata_snapshots.changed_suffix().is_some());
            let stored = destination.snapshot().unwrap();
            assert_eq!(stored.metadata.title.as_deref(), Some("pending-fork"));
            assert_eq!(
                stored.metadata.first_user_message,
                original.metadata.first_user_message
            );
            assert_eq!(
                stored.metadata.checkpoint_json,
                original.metadata.checkpoint_json
            );
            assert_eq!(stored.side_tables.turn_metas[1].1["elapsed_ms"], 2);
            assert_eq!(
                stored.side_tables.context_snapshots[1].1["context_tokens"],
                17
            );
            assert_eq!(
                stored.side_tables.metadata_snapshots[2].1["title"],
                "pending-fork"
            );
            assert_eq!(
                stored
                    .metadata
                    .checkpoint_events_json
                    .as_ref()
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                events + 1
            );
            assert_eq!(writer.snapshot().unwrap(), original);
        }
    }

    #[test]
    fn core_native_fork_binding_rejects_foreign_captures_without_acknowledging_edits() {
        let root = tempfile::tempdir().unwrap();
        let mut source = session(2, 128);
        let mut writer = OwnedLineageWriter::open(root.path(), &source.id).unwrap();
        let first = save(&mut source, &mut writer, StoreHead::default());
        source.title = Some("second publication".into());
        source.snapshot_metadata_at(1);
        let published = save(&mut source, &mut writer, first.receipt.current);
        source.title = Some("pending fork title".into());
        source.snapshot_metadata_at(1);
        let mut forked = source.fork_store_backed(4243);
        let (_destination, result) = OwnedLineageWriter::fork_from(
            root.path(),
            &source.id,
            &forked.id,
            forked.created_at_ms,
            Some(published.receipt.current),
            &|| false,
        )
        .unwrap();
        let pending = versions(&forked);
        let capture = forked.archive_owner.base.clone();
        let logical = serde_json::to_vec(&forked).unwrap();
        let mut rejected = Vec::new();
        let mut invalid = result.clone();
        invalid.source_session_id = "foreign-source".into();
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.source_head.revision = smelt_store::Revision::new(1);
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.source_head.history_len = HistoryLen::new(2);
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.source_head.transcript_record_count = TranscriptRecordCount::new(1);
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.session.revision_id = "f".repeat(64);
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.session.receipt.lineage_id = Some("f".repeat(64));
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.session.receipt.session_id = source.id.clone();
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.session.receipt.previous = published.receipt.current;
        rejected.push(invalid);
        let mut invalid = result.clone();
        invalid.session.receipt.current.revision = smelt_store::Revision::new(2);
        rejected.push(invalid);
        for invalid in rejected {
            assert!(!forked.bind_fork_archives(&invalid));
            assert!(forked.archive_base().is_none());
            assert_eq!(forked.archive_owner.base, capture);
            assert_eq!(versions(&forked), pending);
            assert_eq!(serde_json::to_vec(&forked).unwrap(), logical);
            assert!(forked.metadata_snapshots.changed_suffix().is_some());
        }
        assert!(!source.bind_fork_archives(&result));
        let parent = forked.parent_id.take();
        assert!(!forked.bind_fork_archives(&result));
        forked.parent_id = parent;
        assert!(forked.bind_fork_archives(&result));
        assert_eq!(versions(&forked), pending);
        assert_eq!(serde_json::to_vec(&forked).unwrap(), logical);
        assert!(forked.metadata_snapshots.changed_suffix().is_some());
        assert_eq!(
            forked.archive_base().unwrap().branch_sequence,
            smelt_store::Revision::new(1)
        );
        assert_eq!(
            source.archive_base().unwrap().branch_sequence,
            smelt_store::Revision::new(2)
        );
        assert!(!forked.bind_fork_archives(&result));
        let mut unbound = session(0, 128).fork_store_backed(4244);
        unbound.id.clone_from(&forked.id);
        unbound.parent_id.clone_from(&forked.parent_id);
        assert!(!unbound.bind_fork_archives(&result));
    }

    #[test]
    fn core_native_fork_binding_preserves_opaque_accounting_and_foreign_clean_tables() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(32, 32_768);
        storage.save_result(&original).unwrap();
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut legacy =
            super::super::store_commit_from_session(&original, writer.store_head().unwrap(), 1)
                .unwrap();
        let accounting = json!({"future": {"opaque": [7, "retained"]}});
        legacy.metadata.accounting_json = Some(accounting.clone());
        writer.commit_session(&legacy).unwrap();
        let before = writer.snapshot().unwrap();
        let mut loaded = storage.load_full_result(&original.id).unwrap().unwrap();
        loaded.title = Some("foreign-table-fork".into());
        loaded.metadata_snapshots = vec![(
            1,
            SessionMetadataSnapshot {
                title: Some("foreign-row-title".into()),
                slug: None,
                first_user_message: Some("foreign-row-message".into()),
            },
        )]
        .into();
        loaded
            .metadata_snapshots
            .acknowledge(loaded.metadata_snapshots.version());
        assert!(loaded.metadata_snapshots.changed_suffix().is_none());
        let mut forked = loaded.fork_store_backed(4243);
        let (mut destination, copied) = OwnedLineageWriter::fork_from(
            storage.sessions_dir(),
            &loaded.id,
            &forked.id,
            forked.created_at_ms,
            Some(before.head),
            &|| false,
        )
        .unwrap();
        assert!(forked.bind_fork_archives(&copied));
        let frame = forked
            .prepare_archive_save(
                copied.session.receipt.current,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap();
        assert!(matches!(
            frame.command.scalars.accounting,
            ValueEdit::Retain
        ));
        assert!(matches!(&frame.command.archives.metadata_snapshots,
            ArchiveEdit::ReplaceSuffix { retain_records: 0, records } if records.len() == 1));
        assert!(matches!(
            frame.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        assert!(serde_json::to_vec(frame.command()).unwrap().len() < 4096);
        let saved = destination.commit_compact_session(frame.command()).unwrap();
        assert!(forked.acknowledge_archive_save(&frame, &saved));
        let snapshot = destination.snapshot().unwrap();
        assert_eq!(snapshot.metadata.accounting_json, Some(accounting));
        assert_eq!(
            snapshot.metadata.first_user_message,
            before.metadata.first_user_message
        );
        assert_eq!(
            snapshot.metadata.checkpoint_events_json,
            before.metadata.checkpoint_events_json
        );
        assert_eq!(snapshot.side_tables.metadata_snapshots.len(), 1);
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0].1["title"],
            "foreign-row-title"
        );
        assert_eq!(
            snapshot.side_tables.metadata_snapshots[0].1["first_user_message"],
            "foreign-row-message"
        );
        assert_eq!(writer.snapshot().unwrap(), before);
        assert!(!forked.bind_fork_archives(&copied));
    }

    #[test]
    fn core_native_full_load_of_canonical_fork_binds_the_target_branch() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let original = session(32, 32_768);
        storage.save_result(&original).unwrap();
        let mut source = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let mut update =
            super::super::store_commit_from_session(&original, source.store_head().unwrap(), 1)
                .unwrap();
        update.metadata.title = Some("source-title".into());
        source.commit_session(&update).unwrap();
        let source_before = source.snapshot().unwrap();
        let fork = original.fork_target(4243);
        source.fork_current(&fork.id, fork.created_at_ms).unwrap();
        let mut target = OwnedLineageWriter::open(storage.sessions_dir(), &fork.id).unwrap();
        let target_before = target.snapshot().unwrap();
        assert_ne!(target_before.head.revision, source_before.head.revision);
        let mut loaded = storage.load_full_result(&fork.id).unwrap().unwrap();
        assert_eq!(loaded.id, fork.id);
        assert_eq!(loaded.parent_id.as_deref(), Some(original.id.as_str()));
        assert_eq!(
            loaded.archive_base().unwrap().lineage_id,
            target_before.lineage_id
        );
        assert_eq!(
            loaded.archive_base().unwrap().revision_id,
            target_before.revision_id
        );
        assert_eq!(
            loaded.archive_base().unwrap().branch_sequence,
            target_before.head.revision
        );
        loaded.title = Some("fork-title".into());
        loaded.updated_at_ms = fork.created_at_ms;
        loaded.snapshot_metadata_at(1);
        let prepared = prepare(&loaded, target_before.head);
        assert!(serde_json::to_vec(prepared.command()).unwrap().len() < 4096);
        let result = target.commit_compact_session(prepared.command()).unwrap();
        assert!(loaded.acknowledge_archive_save(&prepared, &result));
        let target_after = target.snapshot().unwrap();
        assert_eq!(target_after.metadata.title.as_deref(), Some("fork-title"));
        assert_eq!(
            target_after.metadata.checkpoint_events_json,
            target_before.metadata.checkpoint_events_json
        );
        let source_after = source.snapshot().unwrap();
        assert_eq!(source_after.head, source_before.head);
        assert_eq!(source_after.revision_id, source_before.revision_id);
        assert_eq!(source_after.metadata, source_before.metadata);
        assert!(loaded.fork_store_backed(4244).archive_base().is_none());
    }

    #[test]
    fn core_resume_binds_verified_snapshot_and_shares_loaded_message_bytes() {
        let root = tempfile::tempdir().unwrap();
        let storage = SessionStorage::new(root.path().to_path_buf());
        let mut original = session(2, 32_768);
        assert!(original.install_context_checkpoint_at_history_index(
            "compaction".into(),
            "shared".repeat(8192),
            1,
            None,
            1,
        ));
        storage.save_result(&original).unwrap();
        let mut resumed = storage
            .load_store_resume_result(&original.id, 80, 24)
            .unwrap()
            .unwrap();
        let reader = smelt_store::LineageSessionReader::open_existing_in_lineage(
            storage.sessions_dir(),
            &resumed.store_address.lineage_id,
            &original.id,
        )
        .unwrap();
        let snapshot = reader.snapshot().unwrap();
        assert_eq!(
            resumed.session.archive_base().unwrap().revision_id,
            snapshot.revision_id
        );
        assert_eq!(
            resumed.session.archive_base().unwrap().lineage_id,
            snapshot.lineage_id
        );
        assert_eq!(
            resumed.session.archive_base().unwrap().branch_sequence,
            snapshot.head.revision
        );
        assert!(Arc::ptr_eq(
            resumed.session.first_user_message.as_ref().unwrap(),
            resumed
                .session
                .metadata_snapshots
                .last()
                .unwrap()
                .1
                .first_user_message
                .as_ref()
                .unwrap()
        ));
        assert!(Arc::ptr_eq(
            &resumed.session.checkpoint.as_ref().unwrap().summary,
            &resumed.session.checkpoint_events.last().unwrap().summary,
        ));
        resumed.session.title = Some("resumed".into());
        resumed.session.snapshot_metadata_at(1);
        let prepared = resumed
            .session
            .prepare_archive_save(
                resumed.head,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new(),
                },
                None,
            )
            .unwrap();
        assert!(matches!(
            prepared.command.archives.checkpoint_events,
            CheckpointEventsEdit::Retain
        ));
        assert!(matches!(
            prepared.command.archives.first_user_message,
            ValueEdit::Retain
        ));
        let mut writer = OwnedLineageWriter::open(storage.sessions_dir(), &original.id).unwrap();
        let result = writer.commit_compact_session(prepared.command()).unwrap();
        assert!(resumed.session.acknowledge_archive_save(&prepared, &result));
        assert_eq!(
            reader.snapshot().unwrap().metadata.title.as_deref(),
            Some("resumed")
        );
        original.id = "unbound".into();
        assert!(original
            .prepare_archive_save(
                resumed.head,
                HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: HistoryLen::new(1),
                    items: Vec::new()
                },
                None
            )
            .is_err());
    }
}
