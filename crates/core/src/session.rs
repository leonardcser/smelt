use protocol::{
    history_item_message_count, HistoryItem, Message, ReasoningEffort, TokenUsage, TurnMeta,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    #[serde(default = "default_checkpoint_kind")]
    pub kind: String,
    pub summary: Arc<str>,
    pub first_live_index: usize,
    pub created_at_ms: u64,
    pub tokens_before: Option<u32>,
    pub tokens_after_estimate: Option<u32>,
    #[serde(default)]
    pub tokens_after_estimate_history_len: Option<usize>,
    /// Token baseline that was active just before this checkpoint was
    /// installed. Restored when the session is rewound past the checkpoint.
    #[serde(default)]
    pub pre_checkpoint_context_tokens: Option<u32>,
    #[serde(default)]
    pub pre_checkpoint_context_history_len: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCheckpointEvent {
    #[serde(default = "default_checkpoint_kind")]
    pub kind: String,
    pub summary: Arc<str>,
    /// Canonical history boundary retained verbatim in model context.
    pub first_live_index: usize,
    /// Canonical history length when compaction completed and its transcript marker appeared.
    pub completed_at_history_len: usize,
    pub created_at_ms: u64,
}

impl ContextCheckpointEvent {
    fn matches(&self, checkpoint: &ContextCheckpoint) -> bool {
        self.created_at_ms == checkpoint.created_at_ms
            && self.first_live_index == checkpoint.first_live_index
            && self.kind == checkpoint.kind
            && self.summary == checkpoint.summary
    }

    fn to_checkpoint(&self) -> ContextCheckpoint {
        ContextCheckpoint {
            kind: self.kind.clone(),
            summary: self.summary.clone(),
            first_live_index: self.first_live_index,
            created_at_ms: self.created_at_ms,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshotKey {
    pub kind: String,
    pub first_live_index: usize,
    pub created_at_ms: u64,
}

impl From<&ContextCheckpoint> for ContextSnapshotKey {
    fn from(cp: &ContextCheckpoint) -> Self {
        Self {
            kind: cp.kind.clone(),
            first_live_index: cp.first_live_index,
            created_at_ms: cp.created_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTokenIdentity {
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub provider_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthoritativeContextTokens {
    pub tokens: u32,
    pub history_len: usize,
    pub identity: ContextTokenIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayContextTokens {
    pub tokens: u32,
    pub identity: Option<ContextTokenIdentity>,
}

#[derive(Debug, Clone)]
struct ContextTokenReading {
    tokens: u32,
    history_len: Option<usize>,
    identity: Option<ContextTokenIdentity>,
}

impl ContextTokenReading {
    fn matches(&self, identity: &ContextTokenIdentity) -> bool {
        self.identity.as_ref() == Some(identity)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub context_tokens: Option<u32>,
    pub context_tokens_history_len: Option<usize>,
    #[serde(default)]
    pub context_token_identity: Option<ContextTokenIdentity>,
    /// Sticky display reading at this history point. This can differ from the
    /// authoritative baseline after checkpointing clears the next-request estimate.
    #[serde(default)]
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub display_context_token_identity: Option<ContextTokenIdentity>,
    pub checkpoint: Option<ContextSnapshotKey>,
}

impl ContextSnapshot {
    fn from_session(session: &Session) -> Self {
        Self {
            context_tokens: session.context_tokens,
            context_tokens_history_len: session.context_tokens_history_len,
            context_token_identity: session.context_token_identity.clone(),
            display_context_tokens: session.display_context_tokens,
            display_context_token_identity: session.display_context_token_identity.clone(),
            checkpoint: session.checkpoint_snapshot_key(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadataSnapshot {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<Arc<str>>,
}

impl SessionMetadataSnapshot {
    fn from_session(session: &Session) -> Self {
        Self {
            title: session.title.clone(),
            slug: session.slug.clone(),
            first_user_message: session.first_user_message.clone(),
        }
    }
}

fn default_checkpoint_kind() -> String {
    "compaction".to_string()
}

impl Default for ContextCheckpoint {
    fn default() -> Self {
        Self {
            kind: default_checkpoint_kind(),
            summary: Arc::from(""),
            first_live_index: 0,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkTarget {
    pub id: String,
    pub created_at_ms: u64,
}

/// In-memory conversation state.
///
/// Storage shape is `Vec<HistoryItem>` (the sum-type history that makes
/// orphan tool_calls impossible).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<Arc<str>>,
    /// Title/slug snapshots keyed by semantic history length. Rewind restores
    /// the latest snapshot at or before the retained history boundary.
    pub metadata_snapshots: HistorySnapshots<SessionMetadataSnapshot>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub mode: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub model: Option<String>,
    pub fast_mode: Option<bool>,
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
    pub history: Vec<HistoryItem>,
    pub checkpoint: Option<ContextCheckpoint>,
    pub checkpoint_events: CheckpointEvents,
    pub context_tokens: Option<u32>,
    /// History length at the time `context_tokens` was recorded. Used to
    /// decide whether the provider baseline exactly covers the current
    /// history or needs a delta estimate for appended messages.
    pub context_tokens_history_len: Option<usize>,
    pub context_token_identity: Option<ContextTokenIdentity>,
    /// Last non-background provider context-token reading surfaced to the UI.
    /// It may lag the current history while a new request is in flight.
    pub display_context_tokens: Option<u32>,
    pub display_context_token_identity: Option<ContextTokenIdentity>,
    /// Per-turn metadata, keyed by `history.len()` at turn-complete time.
    pub turn_metas: HistorySnapshots<TurnMeta>,
    /// Context snapshots keyed by `history.len()` at turn-complete time.
    /// Rewind uses these to restore context baselines; session cost and
    /// cumulative usage remain spent counters for the whole session.
    pub context_snapshots: HistorySnapshots<ContextSnapshot>,
    /// Running session cost in USD; updated incrementally as token usage events arrive.
    pub session_cost_usd: f64,
    /// Cumulative token usage across every turn this session has made;
    /// distinct from the per-turn `context_tokens` snapshot.
    pub session_usage: TokenUsage,
    /// The newest turn id, so ids keep growing across resumes.
    pub last_turn_id: u64,
}

pub use crate::session_store::{SessionStoreError, SessionStoreResult};

mod snapshots;
mod storage;
pub use snapshots::{CheckpointEvents, HistorySnapshots};
pub use storage::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<Arc<str>>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub authoritative_context_tokens: Option<AuthoritativeContextTokens>,
    #[serde(default)]
    pub display_context_tokens: Option<DisplayContextTokens>,
    #[serde(default)]
    pub history_len: Option<usize>,
    #[serde(default)]
    pub checkpoint: Option<ContextCheckpoint>,
    #[serde(default)]
    pub checkpoint_events: CheckpointEvents,
    /// Approximate text byte size (message bodies, reasoning, tool-call args).
    /// Projected into the catalog so list consumers avoid opening session databases.
    #[serde(default)]
    pub text_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<Arc<str>>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default, rename = "context_tokens")]
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub history_len: Option<usize>,
    /// Approximate text byte size (message bodies, reasoning, tool-call args).
    #[serde(default)]
    pub text_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListCursor {
    pub updated_at_ms: u64,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListQuery {
    pub limit: u32,
    pub cursor: Option<SessionListCursor>,
    pub cwd: Option<String>,
}

impl Default for SessionListQuery {
    fn default() -> Self {
        Self {
            limit: 200,
            cursor: None,
            cwd: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionListPage {
    pub sessions: Vec<SessionListMeta>,
    pub next_cursor: Option<SessionListCursor>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct SessionContextSnapshotState {
    #[serde(default)]
    session_usage: TokenUsage,
    #[serde(default)]
    context_token_identity: Option<ContextTokenIdentity>,
    #[serde(default)]
    display_context_token_identity: Option<ContextTokenIdentity>,
}

impl Session {
    /// Create a fresh session. `pid` mixes into the session id; `cwd`
    /// is recorded as the session's working directory.
    pub fn new(pid: u32, cwd: std::path::PathBuf) -> Self {
        let now = now_ms();
        let id = new_session_id(now, pid);
        let cwd = cwd.to_str().map(String::from);
        Self {
            id,
            title: None,
            slug: None,
            first_user_message: None,
            metadata_snapshots: HistorySnapshots::default(),
            created_at_ms: now,
            updated_at_ms: now,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            cwd,
            parent_id: None,
            history: Vec::new(),
            checkpoint: None,
            checkpoint_events: CheckpointEvents::default(),
            context_tokens: None,
            context_tokens_history_len: None,
            context_token_identity: None,
            display_context_tokens: None,
            display_context_token_identity: None,
            turn_metas: HistorySnapshots::default(),
            context_snapshots: HistorySnapshots::default(),
            session_cost_usd: 0.0,
            session_usage: TokenUsage::default(),
            last_turn_id: 0,
        }
    }

    fn context_token_reading(&self) -> Option<ContextTokenReading> {
        self.context_tokens.map(|tokens| ContextTokenReading {
            tokens,
            history_len: self.context_tokens_history_len,
            identity: self.context_token_identity.clone(),
        })
    }

    fn display_context_token_reading(&self) -> Option<ContextTokenReading> {
        self.display_context_tokens
            .map(|tokens| ContextTokenReading {
                tokens,
                history_len: None,
                identity: self.display_context_token_identity.clone(),
            })
            .or_else(|| self.context_token_reading())
    }

    fn set_context_token_reading(&mut self, reading: ContextTokenReading) {
        self.context_tokens = Some(reading.tokens);
        self.context_tokens_history_len = reading.history_len;
        self.context_token_identity = reading.identity.clone();
    }

    fn set_display_context_token_reading(&mut self, reading: ContextTokenReading) {
        self.display_context_tokens = Some(reading.tokens);
        self.display_context_token_identity = reading.identity;
    }

    pub fn record_context_tokens(
        &mut self,
        tokens: u32,
        history_len: usize,
        identity: ContextTokenIdentity,
    ) {
        let reading = ContextTokenReading {
            tokens,
            history_len: Some(history_len),
            identity: Some(identity),
        };
        self.set_context_token_reading(reading.clone());
        self.set_display_context_token_reading(reading);
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
        self.context_token_identity = None;
        self.display_context_tokens = None;
        self.display_context_token_identity = None;
    }

    pub fn clear_context_tokens_baseline(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
        self.context_token_identity = None;
    }

    pub fn reset_context_tokens_for_checkpoint(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
        self.context_token_identity = None;
        self.display_context_tokens = Some(0);
        self.display_context_token_identity = None;
    }

    /// Provider-reported context size, when it was taken at exactly
    /// `history_len` live history items.
    pub fn current_context_tokens(&self, history_len: usize) -> Option<u32> {
        self.context_token_reading()
            .filter(|reading| reading.history_len == Some(history_len))
            .map(|reading| reading.tokens)
    }

    pub fn context_tokens_for(&self, identity: &ContextTokenIdentity) -> Option<u32> {
        self.context_token_reading()
            .filter(|reading| reading.matches(identity))
            .map(|reading| reading.tokens)
    }

    pub fn display_context_tokens(&self) -> Option<u32> {
        self.display_context_token_reading()
            .map(|reading| reading.tokens)
    }

    pub fn display_context_tokens_stale(&self, identity: &ContextTokenIdentity) -> bool {
        self.display_context_token_reading()
            .filter(|reading| reading.tokens > 0)
            .is_some_and(|reading| !reading.matches(identity))
    }

    pub fn clear_context_tokens_baseline_if_mismatched(&mut self, identity: &ContextTokenIdentity) {
        if self
            .context_token_identity
            .as_ref()
            .is_some_and(|current| current != identity)
        {
            self.clear_context_tokens_baseline();
        }
    }

    pub fn checkpoint_snapshot_key(&self) -> Option<ContextSnapshotKey> {
        self.checkpoint.as_ref().map(ContextSnapshotKey::from)
    }

    pub fn record_checkpoint_tokens_after_estimate(
        &mut self,
        tokens: u32,
        history_len: usize,
    ) -> bool {
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            return false;
        };
        if checkpoint.tokens_after_estimate == Some(tokens)
            && checkpoint.tokens_after_estimate_history_len == Some(history_len)
        {
            return false;
        }
        checkpoint.tokens_after_estimate = Some(tokens);
        checkpoint.tokens_after_estimate_history_len = Some(history_len);
        true
    }

    pub fn snapshot_context(&mut self) {
        self.snapshot_context_at(self.history.len());
    }

    pub fn snapshot_context_at(&mut self, hist_idx: usize) -> bool {
        let snapshot = ContextSnapshot::from_session(self);
        self.context_snapshots
            .upsert_truncating_after(hist_idx, snapshot)
    }

    pub fn finish_turn_state(
        &mut self,
        history_len: usize,
        meta: TurnMeta,
        update_context_token_history_len: bool,
    ) -> bool {
        let mut changed = self.turn_metas.upsert_truncating_after(history_len, meta);
        if update_context_token_history_len && self.context_tokens.is_some() {
            changed |= self.context_tokens_history_len != Some(history_len);
            self.context_tokens_history_len = Some(history_len);
        }
        changed | self.snapshot_context_at(history_len)
    }

    pub fn snapshot_metadata_at(&mut self, hist_idx: usize) -> bool {
        let snapshot = SessionMetadataSnapshot::from_session(self);
        self.metadata_snapshots
            .upsert_truncating_after(hist_idx, snapshot)
    }

    pub fn restore_metadata_after_rewind(&mut self, hist_idx: usize) {
        self.metadata_snapshots.truncate_after(hist_idx);
        if let Some(snapshot) = self.metadata_snapshots.last_value_cloned() {
            self.apply_metadata_snapshot(snapshot);
        } else {
            self.clear_metadata();
        }
    }

    pub fn prune_metadata_snapshots(&mut self, hist_idx: usize) {
        self.metadata_snapshots.truncate_after(hist_idx);
        if let Some(snapshot) = self.metadata_snapshots.last_value_cloned() {
            self.apply_metadata_snapshot(snapshot);
        }
    }

    pub fn clear_metadata_snapshots(&mut self) {
        self.metadata_snapshots.clear();
        self.clear_metadata();
    }

    pub fn restore_rewindable_snapshots_after_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) -> Option<TurnMeta> {
        self.turn_metas.truncate_after(hist_idx);
        self.restore_context_after_rewind(hist_idx, keep_checkpoint_at_boundary);
        self.restore_metadata_after_rewind(hist_idx);
        self.turn_metas.last_value_cloned()
    }

    pub fn prune_rewindable_snapshots(&mut self, hist_idx: usize) -> Option<TurnMeta> {
        self.turn_metas.truncate_after(hist_idx);
        self.prune_context_snapshots(hist_idx);
        self.prune_metadata_snapshots(hist_idx);
        self.turn_metas.last_value_cloned()
    }

    fn apply_metadata_snapshot(&mut self, snapshot: SessionMetadataSnapshot) {
        self.title = snapshot.title;
        self.slug = snapshot.slug;
        self.first_user_message = snapshot.first_user_message;
    }

    fn clear_metadata(&mut self) {
        self.title = None;
        self.slug = None;
        self.first_user_message = None;
    }

    fn truncate_checkpoint_events(
        &mut self,
        history_len: usize,
        keep_checkpoint_at_boundary: bool,
    ) -> Option<(Option<u32>, Option<usize>)> {
        let had_events = !self.checkpoint_events.is_empty();
        let retained_checkpoint = self.checkpoint.as_ref().filter(|checkpoint| {
            keep_checkpoint_at_boundary && checkpoint.first_live_index == history_len
        });
        self.checkpoint_events.retain(|event| {
            (event.completed_at_history_len <= history_len && event.first_live_index <= history_len)
                || retained_checkpoint.is_some_and(|checkpoint| event.matches(checkpoint))
        });
        if !had_events
            || self.checkpoint.as_ref().is_some_and(|checkpoint| {
                self.checkpoint_events
                    .iter()
                    .any(|event| event.matches(checkpoint))
            })
        {
            return None;
        }

        let fallback = self.checkpoint.take().map(|checkpoint| {
            (
                checkpoint.pre_checkpoint_context_tokens,
                checkpoint.pre_checkpoint_context_history_len,
            )
        });
        self.checkpoint = self
            .checkpoint_events
            .last()
            .map(ContextCheckpointEvent::to_checkpoint);
        fallback
    }

    fn remove_checkpoint_event(&mut self, checkpoint: &ContextCheckpoint) {
        self.checkpoint_events
            .retain(|event| !event.matches(checkpoint));
    }

    pub fn clear_context_snapshots(&mut self) {
        self.context_snapshots.clear();
    }

    pub fn restore_context_after_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) {
        let event_fallback = self.truncate_checkpoint_events(hist_idx, keep_checkpoint_at_boundary);
        let checkpoint_fallback = self
            .clear_checkpoint_for_rewind(hist_idx, keep_checkpoint_at_boundary)
            .or(event_fallback);
        self.context_snapshots.truncate_after(hist_idx);
        self.restore_context_tokens_after_rewind(hist_idx, checkpoint_fallback);
    }

    pub fn prune_context_snapshots(&mut self, hist_idx: usize) {
        let event_fallback = self.truncate_checkpoint_events(hist_idx, true);
        let checkpoint_fallback = self
            .clear_checkpoint_for_rewind(hist_idx, true)
            .or(event_fallback);
        self.context_snapshots.truncate_after(hist_idx);
        if !self.context_snapshots.is_empty() || checkpoint_fallback.is_some() {
            self.restore_context_tokens_after_rewind(hist_idx, checkpoint_fallback);
        } else if self
            .context_tokens_history_len
            .is_some_and(|len| len > hist_idx)
        {
            self.clear_context_tokens();
        }
    }

    fn restore_context_tokens_after_rewind(
        &mut self,
        hist_idx: usize,
        checkpoint_fallback: Option<(Option<u32>, Option<usize>)>,
    ) {
        let checkpoint = self.checkpoint_snapshot_key();
        let snapshot = self
            .context_snapshots
            .iter()
            .rev()
            .find(|(_, snapshot)| {
                snapshot.checkpoint == checkpoint
                    && snapshot
                        .context_tokens_history_len
                        .is_none_or(|len| len <= hist_idx)
            })
            .map(|(_, snapshot)| snapshot.clone());

        if let Some(snapshot) = snapshot {
            self.context_tokens = snapshot.context_tokens;
            self.context_tokens_history_len = snapshot.context_tokens_history_len;
            self.context_token_identity = snapshot.context_token_identity;
            self.display_context_tokens =
                snapshot.display_context_tokens.or(snapshot.context_tokens);
            self.display_context_token_identity = snapshot.display_context_token_identity;
        } else if let Some((tokens, Some(history_len))) = checkpoint_fallback {
            if history_len <= hist_idx {
                self.context_tokens = tokens;
                self.context_tokens_history_len = Some(history_len);
                self.context_token_identity = None;
                self.display_context_tokens = tokens;
                self.display_context_token_identity = None;
            } else {
                self.clear_context_tokens();
            }
        } else if self.context_snapshots.is_empty()
            && self
                .context_tokens_history_len
                .is_some_and(|len| len <= hist_idx)
        {
            // Keep a live baseline that still fits the rewound history when no
            // checkpoint-specific snapshot was recorded.
        } else {
            self.clear_context_tokens();
        }
    }

    fn clear_checkpoint_for_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) -> Option<(Option<u32>, Option<usize>)> {
        let should_clear = self.checkpoint.as_ref().is_some_and(|checkpoint| {
            checkpoint.first_live_index > hist_idx
                || (!keep_checkpoint_at_boundary && checkpoint.first_live_index == hist_idx)
        });
        if !should_clear {
            return None;
        }
        let checkpoint = self.checkpoint.take()?;
        self.remove_checkpoint_event(&checkpoint);
        Some((
            checkpoint.pre_checkpoint_context_tokens,
            checkpoint.pre_checkpoint_context_history_len,
        ))
    }

    pub fn model_history_range(&self) -> (Vec<HistoryItem>, usize, usize) {
        let end_index = self.history.len();
        let Some(cp) = &self.checkpoint else {
            return (Vec::new(), 0, end_index);
        };
        (
            vec![HistoryItem::user(protocol::compaction_summary_content(
                &cp.summary,
            ))],
            cp.first_live_index,
            end_index,
        )
    }

    pub fn model_history(&self) -> Vec<HistoryItem> {
        let (mut out, first_live_index, _) = self.model_history_range();
        out.extend(self.history.iter().skip(first_live_index).cloned());
        out
    }

    pub fn install_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        if summary.trim().is_empty() || self.history.is_empty() {
            return false;
        }
        let Some(first_live_index) =
            self.first_live_history_index_for_model_message(first_live_message_index)
        else {
            return false;
        };
        self.install_context_checkpoint_at_history_index(
            kind,
            summary,
            first_live_index,
            tokens_before,
            self.history.len(),
        )
    }

    pub fn install_context_checkpoint_at_history_index(
        &mut self,
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        context_snapshot_index: usize,
    ) -> bool {
        // Store-backed sessions keep history in SQLite, so an empty in-memory
        // history can still have a nonzero checkpoint boundary. For materialized
        // sessions, keep checkpoint coordinates within loaded history.
        if summary.trim().is_empty()
            || first_live_index > context_snapshot_index
            || (!self.history.is_empty() && context_snapshot_index > self.history.len())
        {
            return false;
        }
        let created_at_ms = now_ms();
        let summary: Arc<str> = summary.into();
        self.checkpoint_events.push(ContextCheckpointEvent {
            kind: kind.clone(),
            summary: summary.clone(),
            first_live_index,
            completed_at_history_len: context_snapshot_index,
            created_at_ms,
        });
        self.checkpoint = Some(ContextCheckpoint {
            kind,
            summary,
            first_live_index,
            created_at_ms,
            tokens_before,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: self.context_tokens,
            pre_checkpoint_context_history_len: self.context_tokens_history_len,
        });
        // The next provider response is the first authoritative baseline
        // token reading for checkpointed model history. Show zero until then
        // so the prompt bar reflects that the full old context was compacted.
        self.reset_context_tokens_for_checkpoint();
        self.snapshot_context_at(context_snapshot_index);
        true
    }

    fn first_live_history_index_for_model_message(
        &self,
        first_live_message_index: usize,
    ) -> Option<usize> {
        if first_live_message_index == 0 {
            return None;
        }

        let mut message_index = 0usize;
        let first_history_index = if let Some(cp) = &self.checkpoint {
            // The checkpoint summary is one synthetic model-visible user message.
            message_index = 1;
            cp.first_live_index
        } else {
            0
        };

        for (history_index, item) in self.history.iter().enumerate().skip(first_history_index) {
            if first_live_message_index == message_index {
                return Some(history_index);
            }
            let next_message_index = message_index.saturating_add(history_item_message_count(item));
            if first_live_message_index < next_message_index {
                return None;
            }
            message_index = next_message_index;
        }

        (first_live_message_index == message_index).then_some(self.history.len())
    }

    fn restore_checkpoint_fallback(
        &mut self,
        hist_idx: usize,
        fallback: Option<(Option<u32>, Option<usize>)>,
    ) {
        match fallback {
            Some((tokens, Some(history_len))) if history_len <= hist_idx => {
                self.context_tokens = tokens;
                self.context_tokens_history_len = Some(history_len);
                self.context_token_identity = None;
                self.display_context_tokens = tokens;
                self.display_context_token_identity = None;
            }
            _ => self.clear_context_tokens(),
        }
    }

    pub fn clear_checkpoint_if_rewound_to(&mut self, hist_idx: usize) {
        let event_fallback = self.truncate_checkpoint_events(hist_idx, false);
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|cp| cp.first_live_index >= hist_idx)
        {
            if let Some(cp) = self.checkpoint.take() {
                self.remove_checkpoint_event(&cp);
                let fallback = (
                    cp.pre_checkpoint_context_tokens,
                    cp.pre_checkpoint_context_history_len,
                );
                self.restore_checkpoint_fallback(hist_idx, Some(fallback).or(event_fallback));
            }
        } else if event_fallback.is_some() {
            self.restore_checkpoint_fallback(hist_idx, event_fallback);
        }
    }

    pub fn fork_target(&self, pid: u32) -> SessionForkTarget {
        let created_at_ms = now_ms();
        SessionForkTarget {
            id: new_session_id(created_at_ms, pid),
            created_at_ms,
        }
    }

    pub fn fork(&self, pid: u32) -> Self {
        self.fork_with_history(pid, self.history.clone())
    }

    /// Create metadata for a store-backed fork without materializing history.
    pub fn fork_store_backed(&self, pid: u32) -> Self {
        self.fork_with_history(pid, Vec::new())
    }

    fn fork_with_history(&self, pid: u32, history: Vec<HistoryItem>) -> Self {
        let target = self.fork_target(pid);
        Self {
            id: target.id,
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            metadata_snapshots: self.metadata_snapshots.clone(),
            created_at_ms: target.created_at_ms,
            updated_at_ms: target.created_at_ms,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            model: self.model.clone(),
            fast_mode: self.fast_mode,
            cwd: self.cwd.clone(),
            parent_id: Some(self.id.clone()),
            history,
            checkpoint: self.checkpoint.clone(),
            checkpoint_events: self.checkpoint_events.clone(),
            context_tokens: self.context_tokens,
            context_tokens_history_len: self.context_tokens_history_len,
            context_token_identity: self.context_token_identity.clone(),
            display_context_tokens: self.display_context_tokens,
            display_context_token_identity: self.display_context_token_identity.clone(),
            turn_metas: self.turn_metas.clone(),
            context_snapshots: self.context_snapshots.clone(),
            session_cost_usd: self.session_cost_usd,
            session_usage: self.session_usage.clone(),
            last_turn_id: self.last_turn_id,
        }
    }
}

/// Internal heuristic used for compaction and request-preparation estimates.
/// This must not be surfaced to users as authoritative provider usage.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    let mut chars = 0usize;
    for msg in messages {
        if let Some(ref content) = msg.content {
            chars += content.text_content().len();
            chars += content.image_count() * 4800;
        }
        if let Some(ref reasoning) = msg.reasoning_content {
            chars += reasoning.len();
        }
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                chars += call.function.name.len();
                chars += call.function.arguments.len();
            }
        }
        if let Some(ref id) = msg.tool_call_id {
            chars += id.len();
        }
    }
    chars.div_ceil(4).min(u32::MAX as usize) as u32
}

pub fn is_context_checkpoint_summary(item: &HistoryItem) -> bool {
    matches!(
        item,
        HistoryItem::User { content, .. }
            if matches!(
                protocol::classify_user_history_content(content),
                protocol::UserHistoryContent::CompactionSummary { .. }
            )
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_session_id(now_ms: u64, pid: u32) -> String {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(now_ms.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    crate::utils::hex_lower(&hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_full_sha256_hex() {
        let id = new_session_id(123456789, 4242);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_ids_are_unique() {
        let id1 = new_session_id(100, 4242);
        let id2 = new_session_id(100, 4242);
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_id_is_deterministic_for_fixed_inputs() {
        // Same (now_ms, pid) at adjacent counter values produces stable
        // ids - so a sim scenario that scripts both replays bit-identical.
        let now = 1_700_000_000_000;
        let pid = 7;
        let a = new_session_id(now, pid);
        let b = new_session_id(now, pid);
        assert_ne!(a, b, "counter should still vary within a process");
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn shortest_prefix_with_no_others() {
        // When the sessions dir doesn't exist or is empty, returns 4.
        let id = "abcdef1234567890";
        let prefix = &id[..id.len().min(4)];
        assert_eq!(prefix, "abcd");
    }

    use protocol::{AssistantStep, Content, HistoryItem, ToolInvocation, ToolOutcome};

    fn user_item(text: &str) -> HistoryItem {
        HistoryItem::User {
            content: Content::Text(text.into()),
            display: None,
            command: false,
            sent_at_ms: None,
            image_placement: None,
        }
    }
    fn assistant_text_item(text: &str) -> HistoryItem {
        HistoryItem::Assistant(AssistantStep::terminal(
            Some(Content::Text(text.into())),
            None,
            Vec::new(),
        ))
    }
    fn checkpoint(summary: &str, first_live_index: usize) -> ContextCheckpoint {
        ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: summary.into(),
            first_live_index,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }
    }

    fn test_context_identity() -> ContextTokenIdentity {
        ContextTokenIdentity {
            model: Some("test-model".into()),
            api_base: Some("https://test.example".into()),
            provider_type: Some("test-provider".into()),
        }
    }

    #[test]
    fn finish_turn_state_records_turn_meta_and_rewindable_context() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user_item("hello")];
        session.record_context_tokens(123, 1, test_context_identity());
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: true,
        };

        session.finish_turn_state(7, meta, true);

        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
    }

    // ── Context checkpoints ──────────────────────────────────────────

    #[test]
    fn install_checkpoint_uses_explicit_message_boundary_without_checkpoint() {
        let history = vec![
            user_item("first"),
            assistant_text_item("reply 1"),
            user_item("second"),
            assistant_text_item("reply 2"),
            user_item("third"),
        ];
        let mut s = fixture_session();
        s.history = history;

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        assert_eq!(s.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn install_checkpoint_uses_explicit_message_boundary_with_tool_group() {
        let tool_heavy_turn = HistoryItem::Assistant(AssistantStep::with_invocations(
            Some(Content::Text("done".into())),
            None,
            Vec::new(),
            vec![ToolInvocation {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
                result: ToolOutcome::new("x".repeat(100), false, None),
                elapsed_ms: None,
                called_at_ms: None,
                provider: None,
            }],
        ));
        let history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            tool_heavy_turn,
        ];
        let mut s = fixture_session();
        s.history = history;

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(installed);
        assert_eq!(s.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn install_checkpoint_rejects_boundary_inside_assistant_tool_group() {
        let tool_heavy_turn = HistoryItem::Assistant(AssistantStep::with_invocations(
            Some(Content::Text("done".into())),
            None,
            Vec::new(),
            vec![ToolInvocation {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
                result: ToolOutcome::new("x".repeat(100), false, None),
                elapsed_ms: None,
                called_at_ms: None,
                provider: None,
            }],
        ));
        let mut s = fixture_session();
        s.history = vec![user_item("recent"), tool_heavy_turn];

        // Message index 2 is the tool output inside the collapsed assistant item.
        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(100));

        assert!(!installed);
        assert!(s.checkpoint.is_none());
    }

    #[test]
    fn install_checkpoint_maps_model_message_boundary_through_existing_checkpoint() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("kept user"),
            assistant_text_item("kept reply"),
            user_item("newest user"),
        ];
        s.checkpoint = Some(checkpoint("older summary", 2));

        let installed =
            s.install_context_checkpoint("compaction".into(), "new summary".into(), 2, Some(100));

        assert!(installed);
        assert_eq!(s.checkpoint.as_ref().unwrap().first_live_index, 3);
    }

    #[test]
    fn install_context_checkpoint_refuses_noop_checkpoint() {
        let mut s = fixture_session();
        s.history = vec![user_item("only recent"), assistant_text_item("reply")];
        s.context_tokens = Some(100);

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 0, Some(100));

        assert!(!installed);
        assert!(s.checkpoint.is_none());
        assert_eq!(s.context_tokens, Some(100));
    }

    #[test]
    fn install_context_checkpoint_at_history_index_rejects_past_history() {
        let mut s = fixture_session();
        s.history = vec![user_item("only recent"), assistant_text_item("reply")];

        let installed = s.install_context_checkpoint_at_history_index(
            "compaction".into(),
            "summary".into(),
            3,
            Some(100),
            2,
        );

        assert!(!installed);
        assert!(s.checkpoint.is_none());
    }

    #[test]
    fn install_context_checkpoint_clears_authoritative_context_tokens_and_resets_display() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.record_context_tokens(500, s.history.len(), test_context_identity());

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(500));

        assert!(installed);
        assert!(s.context_tokens.is_none());
        assert!(s.context_tokens_history_len.is_none());
        assert_eq!(s.display_context_tokens, Some(0));
        assert_eq!(s.checkpoint.as_ref().unwrap().first_live_index, 2);
        assert_eq!(
            s.checkpoint.as_ref().unwrap().pre_checkpoint_context_tokens,
            Some(500)
        );
        assert_eq!(
            s.checkpoint
                .as_ref()
                .unwrap()
                .pre_checkpoint_context_history_len,
            Some(4)
        );
    }

    #[test]
    fn restore_context_after_rewind_restores_authoritative_context_snapshot() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.context_tokens = Some(710);
        s.context_tokens_history_len = Some(2);
        s.snapshot_context();

        s.history.extend([user_item("c"), assistant_text_item("d")]);
        s.context_tokens = Some(700);
        s.context_tokens_history_len = Some(4);
        s.snapshot_context();

        s.history.truncate(2);
        s.restore_context_after_rewind(2, false);

        assert_eq!(s.context_tokens, Some(710));
        assert_eq!(s.context_tokens_history_len, Some(2));
        assert_eq!(s.display_context_tokens, Some(710));
    }

    #[test]
    fn restore_context_after_rewind_restores_display_context_snapshot() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.record_context_tokens(710, 2, test_context_identity());
        s.clear_context_tokens_baseline();
        s.snapshot_context();

        s.history.extend([user_item("c"), assistant_text_item("d")]);
        s.record_context_tokens(700, 4, test_context_identity());
        s.snapshot_context();

        s.history.truncate(2);
        s.restore_context_after_rewind(2, false);

        assert_eq!(s.context_tokens, None);
        assert_eq!(s.context_tokens_history_len, None);
        assert_eq!(s.display_context_tokens(), Some(710));
    }

    #[test]
    fn pruning_history_clears_only_checkpoints_past_the_new_boundary() {
        let mut s = fixture_session();
        s.checkpoint = Some(checkpoint("past", 3));
        s.prune_rewindable_snapshots(2);
        assert!(s.checkpoint.is_none());

        s.checkpoint = Some(checkpoint("at boundary", 2));
        s.prune_rewindable_snapshots(2);
        assert_eq!(
            s.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.first_live_index),
            Some(2)
        );
    }

    #[test]
    fn pruning_between_compactions_restores_the_previous_checkpoint() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("one"),
            assistant_text_item("one reply"),
            user_item("two"),
            assistant_text_item("two reply"),
            user_item("three"),
            assistant_text_item("three reply"),
        ];
        assert!(s.install_context_checkpoint_at_history_index(
            "compaction".into(),
            "first summary".into(),
            2,
            Some(100),
            4,
        ));
        assert!(s.install_context_checkpoint_at_history_index(
            "compaction".into(),
            "second summary".into(),
            4,
            Some(80),
            6,
        ));

        let mut boundary_update = s.clone();
        boundary_update.prune_rewindable_snapshots(4);
        assert_eq!(boundary_update.checkpoint_events.len(), 2);
        assert_eq!(
            boundary_update
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.as_ref()),
            Some("second summary")
        );

        s.restore_rewindable_snapshots_after_rewind(4, false);

        assert_eq!(s.checkpoint_events.len(), 1);
        assert_eq!(s.checkpoint_events[0].summary.as_ref(), "first summary");
        assert_eq!(
            s.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.as_ref()),
            Some("first summary")
        );

        s.restore_rewindable_snapshots_after_rewind(1, false);

        assert!(s.checkpoint_events.is_empty());
        assert!(s.checkpoint.is_none());
    }

    #[test]
    fn model_history_with_checkpoint_prepends_summary_and_tail() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.checkpoint = Some(checkpoint("summary text", 2));

        let model = s.model_history();

        assert_eq!(model.len(), 3);
        assert!(
            matches!(&model[0], HistoryItem::User { content, .. } if content.text_content().contains("summary text"))
        );
        assert_eq!(model[1..], s.history[2..]);
    }

    #[test]
    fn clear_checkpoint_if_rewound_to_drops_checkpoint_at_or_before_boundary() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
        ];
        s.checkpoint = Some(checkpoint("summary", 2));
        s.clear_checkpoint_if_rewound_to(2);
        assert!(s.checkpoint.is_none());

        s.checkpoint = Some(checkpoint("summary", 2));
        s.clear_checkpoint_if_rewound_to(3);
        assert!(s.checkpoint.is_some());
    }

    #[test]
    fn clear_checkpoint_if_rewound_to_clears_pre_checkpoint_baseline_from_later_history() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.context_tokens = Some(100);
        s.context_tokens_history_len = Some(4);
        s.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".into(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: Some(100),
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: Some(100),
            pre_checkpoint_context_history_len: Some(4),
        });

        s.history.truncate(2);
        s.clear_checkpoint_if_rewound_to(2);

        assert!(s.checkpoint.is_none());
        assert!(s.context_tokens.is_none());
        assert!(s.context_tokens_history_len.is_none());
    }

    #[test]
    fn clear_checkpoint_if_rewound_to_without_pre_checkpoint_baseline_clears_tokens() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
        ];
        s.checkpoint = Some(ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: "summary".into(),
            first_live_index: 2,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            tokens_after_estimate_history_len: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        });

        s.history.truncate(1);
        s.clear_checkpoint_if_rewound_to(1);

        assert!(s.checkpoint.is_none());
        assert!(s.context_tokens.is_none());
        assert!(s.context_tokens_history_len.is_none());
    }

    // ── Session::new / fork / meta ───────────────────────────────────

    fn fixture_session() -> Session {
        Session::new(4242, std::path::PathBuf::from("/work"))
    }

    #[test]
    fn new_initializes_empty_fields_and_matches_created_updated() {
        let s = fixture_session();
        assert!(!s.id.is_empty());
        assert_eq!(s.id.len(), 64);
        assert_eq!(s.created_at_ms, s.updated_at_ms);
        assert!(s.history.is_empty());
        assert_eq!(s.session_cost_usd, 0.0);
        assert!(s.parent_id.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn current_context_tokens_requires_exact_history_length() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.record_context_tokens(100, 2, test_context_identity());
        assert_eq!(s.current_context_tokens(s.history.len()), Some(100));
        assert_eq!(s.display_context_tokens, Some(100));

        s.history.push(user_item("c"));
        assert_eq!(s.current_context_tokens(s.history.len()), None);
        assert_eq!(s.display_context_tokens, Some(100));
    }

    #[test]
    fn clear_context_tokens_baseline_preserves_visible_reading() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.record_context_tokens(100, 2, test_context_identity());

        s.clear_context_tokens_baseline();

        assert_eq!(s.context_tokens, None);
        assert_eq!(s.context_tokens_history_len, None);
        assert_eq!(s.display_context_tokens, Some(100));
    }

    #[test]
    fn display_tokens_without_identity_are_stale_unless_zero() {
        let mut s = fixture_session();
        s.display_context_tokens = Some(100);
        s.display_context_token_identity = None;

        assert!(s.display_context_tokens_stale(&test_context_identity()));

        s.display_context_tokens = Some(0);
        assert!(!s.display_context_tokens_stale(&test_context_identity()));
    }

    #[test]
    fn fork_clones_history_and_links_parent_with_fresh_id() {
        let mut s = fixture_session();
        s.history.push(user_item("q1"));
        s.history.push(assistant_text_item("a1"));
        s.title = Some("kept".into());
        s.record_context_tokens(500, s.history.len(), test_context_identity());
        s.session_cost_usd = 1.25;

        let forked = s.fork(4242);
        assert_ne!(forked.id, s.id);
        assert_eq!(forked.parent_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(forked.title.as_deref(), Some("kept"));
        assert_eq!(forked.history.len(), s.history.len());
        assert_eq!(forked.context_tokens, Some(500));
        assert_eq!(forked.context_tokens_history_len, Some(2));
        assert_eq!(forked.display_context_tokens, Some(500));
        assert_eq!(forked.session_cost_usd, 1.25);
        assert!(forked.created_at_ms >= s.created_at_ms);
    }
}
