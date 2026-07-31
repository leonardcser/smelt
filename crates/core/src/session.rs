use crate::config;
use protocol::{
    history_item_message_count, HistoryItem, Message, ReasoningEffort, TokenUsage, TurnMeta,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    #[serde(default = "default_checkpoint_kind")]
    pub kind: String,
    pub summary: String,
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
    pub summary: String,
    pub first_live_index: usize,
    pub completed_at_history_len: usize,
    pub created_at_ms: u64,
}

impl ContextCheckpointEvent {
    fn from_checkpoint(checkpoint: &ContextCheckpoint, completed_at_history_len: usize) -> Self {
        Self {
            kind: checkpoint.kind.clone(),
            summary: checkpoint.summary.clone(),
            first_live_index: checkpoint.first_live_index,
            completed_at_history_len,
            created_at_ms: checkpoint.created_at_ms,
        }
    }

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
    pub first_user_message: Option<String>,
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
            summary: String::new(),
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

/// In-memory conversation state.
///
/// Storage shape is `Vec<HistoryItem>` (the sum-type history that makes
/// orphan tool_calls impossible).
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
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
    pub checkpoint_events: Vec<ContextCheckpointEvent>,
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
}

const CURRENT_SESSION_SCHEMA_VERSION: u32 = 2;

pub use crate::session_store::{
    ensure_session_db_read_only, export_history_jsonl, export_requests_jsonl, SessionStoreError,
    SessionStoreResult,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionWireV2 {
    pub schema_version: u32,
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub metadata_snapshots: HistorySnapshots<SessionMetadataSnapshot>,
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
    pub history: Vec<HistoryItem>,
    #[serde(default)]
    pub checkpoint: Option<ContextCheckpoint>,
    #[serde(default)]
    pub checkpoint_events: Vec<ContextCheckpointEvent>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub context_tokens_history_len: Option<usize>,
    #[serde(default)]
    pub context_token_identity: Option<ContextTokenIdentity>,
    #[serde(default)]
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub display_context_token_identity: Option<ContextTokenIdentity>,
    #[serde(default)]
    pub turn_metas: HistorySnapshots<TurnMeta>,
    #[serde(default, alias = "accounting_snapshots")]
    pub context_snapshots: HistorySnapshots<ContextSnapshot>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub session_usage: TokenUsage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistorySnapshots<T>(Vec<(usize, T)>);

impl<T> Default for HistorySnapshots<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> HistorySnapshots<T> {
    pub fn from_vec(entries: Vec<(usize, T)>) -> Self {
        Self(entries)
    }

    pub fn into_vec(self) -> Vec<(usize, T)> {
        self.0
    }

    pub fn as_slice(&self) -> &[(usize, T)] {
        &self.0
    }

    pub fn push(&mut self, entry: (usize, T)) {
        self.0.push(entry);
    }

    pub fn upsert_truncating_after(&mut self, len: usize, value: T) {
        self.truncate_after(len);
        if let Some((existing_len, existing)) = self.0.last_mut() {
            if *existing_len == len {
                *existing = value;
                return;
            }
        }
        self.push((len, value));
    }

    pub fn truncate_after(&mut self, len: usize) {
        while self.0.last().is_some_and(|(entry_len, _)| *entry_len > len) {
            self.0.pop();
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn last(&self) -> Option<&(usize, T)> {
        self.0.last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (usize, T)> {
        self.0.iter()
    }
}

impl<T: Clone> HistorySnapshots<T> {
    pub fn last_value_cloned(&self) -> Option<T> {
        self.0.last().map(|(_, value)| value.clone())
    }
}

impl<'a, T> IntoIterator for &'a HistorySnapshots<T> {
    type Item = &'a (usize, T);
    type IntoIter = std::slice::Iter<'a, (usize, T)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T> std::ops::Index<usize> for HistorySnapshots<T> {
    type Output = (usize, T);

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl<T> From<Vec<(usize, T)>> for HistorySnapshots<T> {
    fn from(value: Vec<(usize, T)>) -> Self {
        Self::from_vec(value)
    }
}

impl From<SessionWireV2> for Session {
    fn from(w: SessionWireV2) -> Self {
        let context_tokens = w.context_tokens;
        let display_context_tokens = w.display_context_tokens.or(context_tokens);
        let metadata_snapshots = w.metadata_snapshots;
        let context_snapshots = w.context_snapshots;
        let checkpoint = w.checkpoint;
        let mut checkpoint_events = w.checkpoint_events;
        checkpoint_events.retain(|event| {
            event.first_live_index <= event.completed_at_history_len
                && event.completed_at_history_len <= w.history.len()
        });
        // COMPAT(session-checkpoint-event-backfill): legacy session JSON retained
        // only the active checkpoint.
        if checkpoint_events.is_empty() {
            if let Some(checkpoint) = &checkpoint {
                checkpoint_events.push(ContextCheckpointEvent::from_checkpoint(
                    checkpoint,
                    checkpoint.first_live_index,
                ));
            }
        }
        Self {
            id: w.id,
            title: w.title,
            slug: w.slug,
            first_user_message: w.first_user_message,
            metadata_snapshots,
            created_at_ms: w.created_at_ms,
            updated_at_ms: w.updated_at_ms,
            mode: w.mode,
            reasoning_effort: w.reasoning_effort,
            model: w.model,
            fast_mode: w.fast_mode,
            cwd: w.cwd,
            parent_id: w.parent_id,
            history: w.history,
            checkpoint,
            checkpoint_events,
            context_tokens,
            context_tokens_history_len: w.context_tokens_history_len,
            context_token_identity: w.context_token_identity,
            display_context_tokens,
            display_context_token_identity: w.display_context_token_identity,
            turn_metas: w.turn_metas,
            context_snapshots,
            session_cost_usd: w.session_cost_usd,
            session_usage: w.session_usage,
        }
    }
}

impl From<&Session> for SessionWireV2 {
    fn from(s: &Session) -> Self {
        SessionWireV2 {
            schema_version: CURRENT_SESSION_SCHEMA_VERSION,
            id: s.id.clone(),
            title: s.title.clone(),
            slug: s.slug.clone(),
            first_user_message: s.first_user_message.clone(),
            metadata_snapshots: s.metadata_snapshots.clone(),
            created_at_ms: s.created_at_ms,
            updated_at_ms: s.updated_at_ms,
            mode: s.mode.clone(),
            reasoning_effort: s.reasoning_effort,
            model: s.model.clone(),
            fast_mode: s.fast_mode,
            cwd: s.cwd.clone(),
            parent_id: s.parent_id.clone(),
            history: s.history.clone(),
            checkpoint: s.checkpoint.clone(),
            checkpoint_events: s.checkpoint_events.clone(),
            context_tokens: s.context_tokens,
            context_tokens_history_len: s.context_tokens_history_len,
            context_token_identity: s.context_token_identity.clone(),
            display_context_tokens: s.display_context_tokens,
            display_context_token_identity: s.display_context_token_identity.clone(),
            turn_metas: s.turn_metas.clone(),
            context_snapshots: s.context_snapshots.clone(),
            session_cost_usd: s.session_cost_usd,
            session_usage: s.session_usage.clone(),
        }
    }
}

impl Serialize for Session {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        SessionWireV2::from(self).serialize(ser)
    }
}

impl<'de> Deserialize<'de> for Session {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let wire = SessionWireV2::deserialize(de)?;
        if wire.schema_version != CURRENT_SESSION_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unsupported session schema version {}",
                wire.schema_version
            )));
        }
        Ok(Session::from(wire))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
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
    pub checkpoint_events: Vec<ContextCheckpointEvent>,
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
    pub first_user_message: Option<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct SessionListEntry {
    pub id: String,
    pub status: SessionListStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionListStatus {
    Available(Box<SessionListMeta>),
    Upgradeable { reason: String },
    Unavailable(SessionStoreError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionListAvailability {
    Available,
    Unavailable,
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
    pub availability: Option<SessionListAvailability>,
}

impl Default for SessionListQuery {
    fn default() -> Self {
        Self {
            limit: 200,
            cursor: None,
            cwd: None,
            availability: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCatalogState {
    Reconciling,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCatalogStatus {
    pub state: SessionCatalogState,
    pub completed_scan_id: u64,
    pub reconciled_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionListPage {
    pub entries: Vec<SessionListEntry>,
    pub next_cursor: Option<SessionListCursor>,
    pub catalog: SessionCatalogStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
            checkpoint_events: Vec::new(),
            context_tokens: None,
            context_tokens_history_len: None,
            context_token_identity: None,
            display_context_tokens: None,
            display_context_token_identity: None,
            turn_metas: HistorySnapshots::default(),
            context_snapshots: HistorySnapshots::default(),
            session_cost_usd: 0.0,
            session_usage: TokenUsage::default(),
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

    pub fn current_context_tokens(&self) -> Option<u32> {
        self.context_token_reading()
            .filter(|reading| reading.history_len == Some(self.history.len()))
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

    pub fn snapshot_context_at(&mut self, hist_idx: usize) {
        let snapshot = ContextSnapshot::from_session(self);
        self.context_snapshots.push((hist_idx, snapshot));
    }

    pub fn finish_turn_state(
        &mut self,
        history_len: usize,
        meta: TurnMeta,
        snapshot_context: bool,
        update_context_token_history_len: bool,
    ) {
        self.turn_metas.push((history_len, meta));
        if snapshot_context {
            if update_context_token_history_len && self.context_tokens.is_some() {
                self.context_tokens_history_len = Some(history_len);
            }
            self.snapshot_context_at(history_len);
        }
    }

    pub fn snapshot_metadata_at(&mut self, hist_idx: usize) {
        let snapshot = SessionMetadataSnapshot::from_session(self);
        self.metadata_snapshots
            .upsert_truncating_after(hist_idx, snapshot);
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
        // COMPAT(session-checkpoint-event-backfill): materialized legacy sessions
        // may carry an active checkpoint without canonical event history.
        if self.checkpoint_events.is_empty() {
            if let Some(checkpoint) = &self.checkpoint {
                self.checkpoint_events
                    .push(ContextCheckpointEvent::from_checkpoint(
                        checkpoint,
                        checkpoint.first_live_index,
                    ));
            }
        }
        let created_at_ms = now_ms();
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

    pub fn fork(&self, pid: u32) -> Self {
        let now = now_ms();
        Self {
            id: new_session_id(now, pid),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            metadata_snapshots: self.metadata_snapshots.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            fast_mode: self.fast_mode,
            cwd: self.cwd.clone(),
            parent_id: Some(self.id.clone()),
            history: self.history.clone(),
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

// ── Save / Load / Delete ─────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SessionStorage {
    inner: Arc<SessionStorageInner>,
}

struct SessionStorageInner {
    state_root: PathBuf,
    catalog: OnceLock<Result<crate::session_catalog::SessionCatalog, String>>,
}

impl SessionStorage {
    pub fn from_env(env: &engine::env::RuntimeEnv) -> Self {
        Self::new(env.xdg_state().join("smelt"))
    }

    pub fn new(state_root: PathBuf) -> Self {
        Self {
            inner: Arc::new(SessionStorageInner {
                state_root,
                catalog: OnceLock::new(),
            }),
        }
    }

    pub fn state_root(&self) -> &Path {
        &self.inner.state_root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.inner.state_root.join("sessions")
    }

    fn reject_symlink(&self, path: &Path, operation: &'static str) -> SessionStoreResult<()> {
        crate::session_store::reject_symlink_in(self.state_root(), path, operation)
    }

    pub fn ensure_session_db_read_only(&self, dir: &Path) -> SessionStoreResult<()> {
        crate::session_store::ensure_session_db_read_only_in(self.state_root(), dir)
    }

    pub fn create_private_dir_all(&self, path: &Path) -> std::io::Result<()> {
        create_private_dir_all_in(self.state_root(), path)
    }

    pub fn write_private_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        write_private_file_in(self.state_root(), path, contents)
    }

    pub fn dir_for(&self, session: &Session) -> PathBuf {
        self.dir_for_id(&session.id)
    }

    pub fn dir_for_id(&self, id: &str) -> PathBuf {
        let id = crate::session_id::SessionId::parse(id)
            .unwrap_or_else(|err| panic!("invalid in-memory session id {id:?}: {err}"));
        self.session_dir(&id)
    }

    pub fn session_dir(&self, id: &crate::session_id::SessionId) -> PathBuf {
        self.sessions_dir().join(id.as_str())
    }

    fn catalog(&self) -> Result<&crate::session_catalog::SessionCatalog, &str> {
        self.inner
            .catalog
            .get_or_init(|| {
                crate::session_catalog::SessionCatalog::open(self.inner.state_root.clone())
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

fn process_storage() -> SessionStorage {
    static STORAGE: OnceLock<Mutex<Option<(PathBuf, SessionStorage)>>> = OnceLock::new();
    let state_root = config::state_dir();
    let mut current = STORAGE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if current.as_ref().is_none_or(|(root, _)| root != &state_root) {
        *current = Some((state_root.clone(), SessionStorage::new(state_root)));
    }
    current
        .as_ref()
        .map(|(_, storage)| storage.clone())
        .expect("process session storage initialized")
}

pub fn dir_for(session: &Session) -> PathBuf {
    process_storage().dir_for(session)
}

/// Resolve a path for an in-memory session whose ID must satisfy the persisted ID invariant.
/// User-provided IDs and prefixes must go through [`resolve_prefix`] first.
pub fn dir_for_id(id: &str) -> PathBuf {
    process_storage().dir_for_id(id)
}

pub fn session_dir(id: &crate::session_id::SessionId) -> PathBuf {
    process_storage().session_dir(id)
}

const ARTIFACT_CLEANUP_BATCH: usize = 64;

impl SessionStorage {
    pub fn cleanup_abandoned_session_artifacts(&self) {
        let root = self.sessions_dir();
        let _ = smelt_store::cleanup_abandoned_session_artifacts(&root, ARTIFACT_CLEANUP_BATCH);
    }
}

pub fn cleanup_abandoned_session_artifacts() {
    process_storage().cleanup_abandoned_session_artifacts();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDirKind {
    Store,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSessionDir {
    pub id: String,
    pub dir: PathBuf,
    pub kind: SessionDirKind,
}

#[derive(Clone, Debug)]
pub struct SessionStoreRef {
    pub session_dir: PathBuf,
    pub db_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SessionHeader {
    pub meta: SessionMeta,
    pub history_len: usize,
    pub revision: u64,
    pub degraded_warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SessionStoreResume {
    pub header: SessionHeader,
    pub store_ref: SessionStoreRef,
    pub head: smelt_store::StoreHead,
    pub transcript_record_tail: smelt_store::TranscriptRecordSlice,
}

impl SessionStorage {
    pub fn save(&self, session: &Session) {
        if let Err(err) = self.save_result(session) {
            eprintln!("smelt: failed to save session {}: {err}", session.id);
        }
    }

    /// Synchronous save entry point for offline tools and tests.
    /// Interactive sessions persist through the worker-owned writer lifecycle.
    pub fn save_result(
        &self,
        session: &Session,
    ) -> Result<smelt_store::SaveReceipt, smelt_store::StoreError> {
        let _perf = smelt_perf::perf::begin("session:write");
        let session_id = crate::session_id::SessionId::parse(&session.id)
            .map_err(|err| smelt_store::StoreError::Integrity(err.to_string()))?;
        let sessions_dir = self.sessions_dir();
        create_private_dir_all_in(self.state_root(), &sessions_dir)?;
        let mut writer = smelt_store::OwnedSessionWriter::open(&sessions_dir, session_id.as_str())?;
        let expected = writer.store_head()?;
        let command = store_commit_from_session(session, expected, 0)?;
        let receipt = writer
            .commit_session(&command)
            .map_err(session_commit_failure_to_store_error)?;
        if writer.is_staged() {
            writer.publish()?;
        }
        self.publish_session_catalog_commit(&command, &receipt, true);
        writer.release()?;
        Ok(receipt)
    }
}

pub fn save(session: &Session) {
    process_storage().save(session);
}

pub fn save_result(session: &Session) -> Result<smelt_store::SaveReceipt, smelt_store::StoreError> {
    process_storage().save_result(session)
}

pub fn initial_store_commit_from_session(
    session: &Session,
) -> Result<smelt_store::SessionCommit, smelt_store::StoreError> {
    store_commit_from_session(session, smelt_store::StoreHead::default(), 0)
}

pub fn store_commit_from_session(
    session: &Session,
    expected: smelt_store::StoreHead,
    history_start_idx: usize,
) -> Result<smelt_store::SessionCommit, smelt_store::StoreError> {
    let history_len = session.history.len();
    let history_start_idx = history_start_idx.min(history_len);
    let history_start = u64::try_from(history_start_idx)
        .map_err(|_| smelt_store::StoreError::Integrity("history start exceeds u64".into()))?;
    let history_len_u64 = u64::try_from(history_len)
        .map_err(|_| smelt_store::StoreError::Integrity("history length exceeds u64".into()))?;
    Ok(smelt_store::SessionCommit {
        session_id: session.id.clone(),
        expected,
        identity: store_identity_from_session(session)?,
        metadata: store_metadata_from_session(session, history_len)?,
        history: smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(history_start),
            final_len: smelt_store::HistoryLen::new(history_len_u64),
            items: session.history[history_start_idx..].to_vec(),
        },
        side_tables: store_side_table_suffixes_from_session_at(
            session,
            history_start_idx,
            history_len,
        )?,
        transcript_records: None,
    })
}

pub fn store_side_table_suffixes_from_session(
    session: &Session,
    history_start_idx: usize,
) -> Result<smelt_store::SideTableSuffixes, smelt_store::StoreError> {
    store_side_table_suffixes_from_session_at(session, history_start_idx, session.history.len())
}

pub fn store_side_table_suffixes_from_session_at(
    session: &Session,
    history_start_idx: usize,
    history_len: usize,
) -> Result<smelt_store::SideTableSuffixes, smelt_store::StoreError> {
    let history_start_idx = history_start_idx.min(history_len);
    Ok(smelt_store::SideTableSuffixes {
        start: smelt_store::HistoryIndex::new(history_start_idx as u64),
        turn_metas: typed_store_values(turn_meta_values_from(
            &session.turn_metas,
            history_start_idx,
            history_len,
        )?),
        metadata_snapshots: typed_store_values(snapshot_values_from(
            &session.metadata_snapshots,
            history_start_idx,
            history_len,
        )?),
        context_snapshots: typed_store_values(snapshot_values_from(
            &session.context_snapshots,
            history_start_idx,
            history_len,
        )?),
    })
}

fn typed_store_values(rows: Vec<(u64, Value)>) -> Vec<(smelt_store::HistoryIndex, Value)> {
    rows.into_iter()
        .map(|(idx, value)| (smelt_store::HistoryIndex::new(idx), value))
        .collect()
}

fn session_commit_failure_to_store_error(
    failure: smelt_store::SessionCommitFailure,
) -> smelt_store::StoreError {
    smelt_store::StoreError::Integrity(format!("session commit failed: {failure:?}"))
}

fn context_snapshot_state_from_session(session: &Session) -> SessionContextSnapshotState {
    SessionContextSnapshotState {
        session_usage: session.session_usage.clone(),
        context_token_identity: session.context_token_identity.clone(),
        display_context_token_identity: session.display_context_token_identity.clone(),
    }
}

fn context_snapshot_state_from_json(value: Option<Value>) -> SessionContextSnapshotState {
    let Some(value) = value else {
        return SessionContextSnapshotState::default();
    };
    if value
        .as_object()
        .is_some_and(|object| object.contains_key("session_usage"))
    {
        serde_json::from_value(value).unwrap_or_default()
    } else {
        SessionContextSnapshotState {
            session_usage: serde_json::from_value(value).unwrap_or_default(),
            ..SessionContextSnapshotState::default()
        }
    }
}

fn checkpoint_from_json(
    value: Option<Value>,
    retained_history_len: usize,
) -> Option<ContextCheckpoint> {
    let mut checkpoint: ContextCheckpoint = serde_json::from_value(value?).ok()?;
    // COMPAT(session-checkpoint-live-index-past-history): read-only load paths
    // cannot rewrite the store, but they still must not expose impossible
    // checkpoint coordinates to model-history construction.
    if checkpoint.first_live_index > retained_history_len {
        checkpoint.first_live_index = 0;
    }
    Some(checkpoint)
}

fn checkpoint_json_for_history_len(
    checkpoint: Option<&ContextCheckpoint>,
    history_len: usize,
) -> Result<Option<Value>, smelt_store::StoreError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    if checkpoint.first_live_index > history_len {
        smelt_perf::perf::record_value("session:save:dropped_checkpoint_past_history", 1);
        return Ok(None);
    }
    debug_assert!(checkpoint.first_live_index <= history_len);
    Ok(Some(serde_json::to_value(checkpoint)?))
}

fn checkpoint_events_from_json(
    value: Option<Value>,
    checkpoint: Option<&ContextCheckpoint>,
    retained_history_len: usize,
) -> Vec<ContextCheckpointEvent> {
    let mut events: Vec<ContextCheckpointEvent> = value
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    events.retain(|event: &ContextCheckpointEvent| {
        event.first_live_index <= event.completed_at_history_len
            && event.completed_at_history_len <= retained_history_len
    });
    if events.is_empty() {
        // COMPAT(session-checkpoint-event-backfill): checkpoint metadata written
        // before canonical event history retained only the active checkpoint.
        if let Some(checkpoint) = checkpoint {
            events.push(ContextCheckpointEvent::from_checkpoint(
                checkpoint,
                checkpoint.first_live_index,
            ));
        }
    }
    events
}

fn checkpoint_events_json_for_history_len(
    events: &[ContextCheckpointEvent],
    history_len: usize,
) -> Result<Option<Value>, smelt_store::StoreError> {
    if let Some(event) = events.iter().find(|event| {
        event.first_live_index > event.completed_at_history_len
            || event.completed_at_history_len > history_len
    }) {
        return Err(smelt_store::StoreError::Integrity(format!(
            "checkpoint event boundary {} and completion {} must fit history length {history_len}",
            event.first_live_index, event.completed_at_history_len
        )));
    }
    if events.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_value(events)?))
    }
}

pub fn store_identity_from_session(
    session: &Session,
) -> Result<smelt_store::SessionIdentity, smelt_store::StoreError> {
    Ok(smelt_store::SessionIdentity {
        id: session.id.clone(),
        created_at: i64::try_from(session.created_at_ms).map_err(|_| {
            smelt_store::StoreError::Integrity("session creation time exceeds SQLite range".into())
        })?,
        parent_id: session.parent_id.clone(),
    })
}

pub fn store_metadata_from_session(
    session: &Session,
    history_len: usize,
) -> Result<smelt_store::SessionMetadata, smelt_store::StoreError> {
    Ok(smelt_store::SessionMetadata {
        title: session.title.clone(),
        slug: session.slug.clone(),
        first_user_message: session.first_user_message.clone(),
        cwd: session.cwd.clone(),
        mode: session.mode.clone(),
        reasoning_effort: session
            .reasoning_effort
            .map(|effort| effort.label().to_string()),
        model: session.model.clone(),
        fast_mode: session.fast_mode,
        accounting_json: Some(serde_json::to_value(context_snapshot_state_from_session(
            session,
        ))?),
        checkpoint_json: checkpoint_json_for_history_len(session.checkpoint.as_ref(), history_len)?,
        checkpoint_events_json: checkpoint_events_json_for_history_len(
            &session.checkpoint_events,
            history_len,
        )?,
        context_tokens: session.context_tokens.map(u64::from),
        context_tokens_history_len: session
            .context_tokens_history_len
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                smelt_store::StoreError::Integrity(
                    "context token history length exceeds u64".into(),
                )
            })?,
        display_context_tokens: session.display_context_tokens.map(u64::from),
        session_cost_usd: smelt_store::SessionCostUsd::new(session.session_cost_usd)?,
        updated_at: i64::try_from(session.updated_at_ms).map_err(|_| {
            smelt_store::StoreError::Integrity("session update time exceeds SQLite range".into())
        })?,
    })
}

fn turn_meta_values_from<T: Serialize>(
    snapshots: &HistorySnapshots<T>,
    history_start_idx: usize,
    history_len: usize,
) -> Result<Vec<(u64, Value)>, smelt_store::StoreError> {
    if let Some((idx, _)) = snapshots.iter().find(|(idx, _)| *idx > history_len) {
        return Err(smelt_store::StoreError::Integrity(format!(
            "turn metadata index {idx} must be at or before final history length {history_len}"
        )));
    }
    snapshots
        .iter()
        .filter(|(idx, _)| *idx >= history_start_idx && *idx <= history_len)
        .map(|(idx, value)| Ok((*idx as u64, serde_json::to_value(value)?)))
        .collect()
}

fn snapshot_values_from<T: Serialize>(
    snapshots: &HistorySnapshots<T>,
    history_start_idx: usize,
    history_len: usize,
) -> Result<Vec<(u64, Value)>, smelt_store::StoreError> {
    let mut values = Vec::new();
    for (idx, value) in snapshots.iter() {
        if *idx > history_len {
            return Err(smelt_store::StoreError::Integrity(format!(
                "snapshot index {idx} must be at or before final history length {history_len}"
            )));
        }
        if *idx >= history_start_idx {
            values.push((*idx as u64, serde_json::to_value(value)?));
        }
    }
    Ok(values)
}

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
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
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
    let state_root = engine::state_dir();
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
    let state_root = engine::state_dir();
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

impl SessionStorage {
    /// Load the full semantic session by exact ID or unique prefix.
    ///
    /// This materializes all history rows and should stay out of normal resume,
    /// preview, render, search, save, and provider-dispatch hot paths.
    pub fn load_full(&self, id_or_prefix: &str) -> Option<Session> {
        self.load_full_result(id_or_prefix).ok().flatten()
    }

    pub fn load_full_result(&self, id_or_prefix: &str) -> SessionStoreResult<Option<Session>> {
        let _perf = smelt_perf::perf::begin("session:load_full");
        let id = {
            let _perf = smelt_perf::perf::begin("session:load_full:resolve");
            self.resolve_prefix(id_or_prefix)?
        };
        let dir = self.session_dir(&id);
        self.ensure_session_db_read_only(&dir)?;
        load_db_session_result(&dir)
    }

    pub fn load_meta(&self, id_or_prefix: &str) -> Option<SessionMeta> {
        self.load_meta_result(id_or_prefix).ok().flatten()
    }

    pub fn load_meta_result(&self, id_or_prefix: &str) -> SessionStoreResult<Option<SessionMeta>> {
        let _perf = smelt_perf::perf::begin("session:load_meta");
        let dir = self.prepare_session_dir_for_read_result(id_or_prefix)?;
        load_meta_from_db_result(&dir)
    }

    pub fn session_schema_status_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<smelt_store::SessionSchemaStatus> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        self.session_schema_status_for_resolved(&resolved)
    }

    pub fn migrate_session_schema_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<smelt_store::SessionSchemaMigration> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        self.migrate_session_schema_for_resolved(&resolved)
    }

    pub fn quarantine_orphaned_session_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<Option<smelt_store::SessionOrphanQuarantine>> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        if let Ok(catalog) = self.catalog() {
            catalog.begin_delete(&resolved.id);
        }
        let result = smelt_store::quarantine_orphaned_session(self.sessions_dir(), &resolved.id)
            .map_err(|error| {
                crate::session_store::store_error(
                    "quarantine orphaned session",
                    &resolved.dir.join("session.db"),
                    error,
                )
            });
        match &result {
            Ok(Some(_)) => {
                if let Ok(catalog) = self.catalog() {
                    catalog.complete_delete(&resolved.id);
                }
            }
            Ok(None) | Err(_) => {
                if let Ok(catalog) = self.catalog() {
                    catalog.cancel_delete(&resolved.id);
                }
            }
        }
        result
    }

    fn session_schema_status_for_resolved(
        &self,
        resolved: &ResolvedSessionDir,
    ) -> SessionStoreResult<smelt_store::SessionSchemaStatus> {
        smelt_store::session_schema_status(self.sessions_dir(), &resolved.id).map_err(|error| {
            crate::session_store::store_error(
                "inspect session schema",
                &resolved.dir.join("session.db"),
                error,
            )
        })
    }

    fn migrate_session_schema_for_resolved(
        &self,
        resolved: &ResolvedSessionDir,
    ) -> SessionStoreResult<smelt_store::SessionSchemaMigration> {
        let db_path = resolved.dir.join("session.db");
        let migration = smelt_store::migrate_session_schema(self.sessions_dir(), &resolved.id)
            .map_err(|error| {
                crate::session_store::store_error("migrate session schema", &db_path, error)
            })?;
        if migration.migrated {
            self.request_session_catalog_projection(&resolved.id, migration.store_head.revision);
        }
        Ok(migration)
    }

    fn ensure_current_schema_for_explicit_open(
        &self,
        resolved: &ResolvedSessionDir,
    ) -> SessionStoreResult<()> {
        match self.session_schema_status_for_resolved(resolved)? {
            smelt_store::SessionSchemaStatus::Current { .. } => Ok(()),
            smelt_store::SessionSchemaStatus::Upgradeable { .. } => self
                .migrate_session_schema_for_resolved(resolved)
                .map(|_| ()),
            smelt_store::SessionSchemaStatus::Future { found, supported } => {
                Err(SessionStoreError::UnsupportedSchema { found, supported })
            }
            smelt_store::SessionSchemaStatus::Unrecognized { found, supported } => {
                Err(SessionStoreError::Corrupt {
                    context: format!(
                        "unrecognized session schema version {found}; supported migrations start at 1 and target {supported}"
                    ),
                })
            }
            smelt_store::SessionSchemaStatus::Orphaned { found } => {
                Err(SessionStoreError::Orphaned { found })
            }
            smelt_store::SessionSchemaStatus::Corrupt { reason, .. } => {
                Err(SessionStoreError::Corrupt { context: reason })
            }
        }
    }

    pub fn resolve_session_dir_for_read(&self, id_or_prefix: &str) -> Option<ResolvedSessionDir> {
        self.resolve_session_dir_for_read_result(id_or_prefix).ok()
    }

    pub fn resolve_session_dir_for_read_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<ResolvedSessionDir> {
        let id = self.resolve_prefix(id_or_prefix)?;
        let dir = self.session_dir(&id);
        self.reject_symlink(&dir, "read")?;
        self.reject_symlink(&dir.join("session.db"), "read")?;
        let kind = session_dir_kind(&dir)
            .ok_or_else(|| SessionStoreError::MissingDatabase { id: id.to_string() })?;
        Ok(ResolvedSessionDir {
            id: id.into_string(),
            dir,
            kind,
        })
    }

    pub fn prepare_session_dir_for_read(&self, id_or_prefix: &str) -> Option<PathBuf> {
        self.prepare_session_dir_for_read_result(id_or_prefix).ok()
    }

    pub fn prepare_session_dir_for_read_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<PathBuf> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        self.ensure_session_db_read_only(&resolved.dir)?;
        Ok(resolved.dir)
    }

    pub fn load_store_resume_result(
        &self,
        id_or_prefix: &str,
        record_width: u16,
        record_target_rows: u16,
    ) -> SessionStoreResult<Option<SessionStoreResume>> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        self.ensure_current_schema_for_explicit_open(&resolved)?;
        load_store_resume_from_resolved(resolved, record_width, record_target_rows)
    }

    pub fn load_store_header(
        &self,
        id_or_prefix: &str,
    ) -> Option<(SessionHeader, SessionStoreRef)> {
        self.load_store_header_result(id_or_prefix).ok().flatten()
    }

    pub fn load_store_header_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<Option<(SessionHeader, SessionStoreRef)>> {
        let resolved = self.resolve_session_dir_for_read_result(id_or_prefix)?;
        self.load_store_header_for_dir_result(resolved.dir)
    }

    pub fn load_store_header_for_dir(
        &self,
        session_dir: PathBuf,
    ) -> Option<(SessionHeader, SessionStoreRef)> {
        self.load_store_header_for_dir_result(session_dir)
            .ok()
            .flatten()
    }

    pub fn load_store_header_for_dir_result(
        &self,
        session_dir: PathBuf,
    ) -> SessionStoreResult<Option<(SessionHeader, SessionStoreRef)>> {
        self.reject_symlink(&session_dir, "read")?;
        self.reject_symlink(&session_dir.join("session.db"), "read")?;
        load_store_header_from_prepared_dir_result(session_dir)
    }
}

/// Load the full semantic session by exact ID or unique prefix (git-style short ID).
///
/// This materializes all history rows and should stay out of normal resume,
/// preview, render, search, save, and provider-dispatch hot paths.
pub fn load_full(id_or_prefix: &str) -> Option<Session> {
    process_storage().load_full(id_or_prefix)
}

pub fn load_full_result(id_or_prefix: &str) -> SessionStoreResult<Option<Session>> {
    process_storage().load_full_result(id_or_prefix)
}

pub fn load_meta(id_or_prefix: &str) -> Option<SessionMeta> {
    process_storage().load_meta(id_or_prefix)
}

pub fn load_meta_result(id_or_prefix: &str) -> SessionStoreResult<Option<SessionMeta>> {
    process_storage().load_meta_result(id_or_prefix)
}

pub fn session_schema_status_result(
    id_or_prefix: &str,
) -> SessionStoreResult<smelt_store::SessionSchemaStatus> {
    process_storage().session_schema_status_result(id_or_prefix)
}

pub fn migrate_session_schema_result(
    id_or_prefix: &str,
) -> SessionStoreResult<smelt_store::SessionSchemaMigration> {
    process_storage().migrate_session_schema_result(id_or_prefix)
}

pub fn quarantine_orphaned_session_result(
    id_or_prefix: &str,
) -> SessionStoreResult<Option<smelt_store::SessionOrphanQuarantine>> {
    process_storage().quarantine_orphaned_session_result(id_or_prefix)
}

pub fn resolve_session_dir_for_read(id_or_prefix: &str) -> Option<ResolvedSessionDir> {
    process_storage().resolve_session_dir_for_read(id_or_prefix)
}

pub fn resolve_session_dir_for_read_result(
    id_or_prefix: &str,
) -> SessionStoreResult<ResolvedSessionDir> {
    process_storage().resolve_session_dir_for_read_result(id_or_prefix)
}

pub fn prepare_session_dir_for_read(id_or_prefix: &str) -> Option<PathBuf> {
    process_storage().prepare_session_dir_for_read(id_or_prefix)
}

pub fn prepare_session_dir_for_read_result(id_or_prefix: &str) -> SessionStoreResult<PathBuf> {
    process_storage().prepare_session_dir_for_read_result(id_or_prefix)
}

pub fn load_store_resume_result(
    id_or_prefix: &str,
    record_width: u16,
    record_target_rows: u16,
) -> SessionStoreResult<Option<SessionStoreResume>> {
    process_storage().load_store_resume_result(id_or_prefix, record_width, record_target_rows)
}

fn load_store_resume_from_resolved(
    resolved: ResolvedSessionDir,
    record_width: u16,
    record_target_rows: u16,
) -> SessionStoreResult<Option<SessionStoreResume>> {
    let session_dir = resolved.dir;
    let db_path = session_dir.join("session.db");
    let db = smelt_store::SessionReader::open_database(&db_path)
        .map_err(|err| crate::session_store::store_error("open", &db_path, err))?;
    let Some(snapshot) = db
        .load_session_resume_snapshot(record_width, record_target_rows)
        .map_err(|err| {
            crate::session_store::store_error("read coherent session snapshot", &db_path, err)
        })?
    else {
        return Ok(None);
    };
    let history_len = snapshot
        .session
        .head
        .history_len
        .as_usize()
        .ok_or_else(|| SessionStoreError::Corrupt {
            context: "session history length exceeds platform limits".into(),
        })?;
    let meta = session_meta_from_stored_session(
        &session_dir,
        snapshot.session.clone(),
        snapshot.history_text_bytes,
        snapshot.retained_history_len.min(history_len),
    )?;
    let header = SessionHeader {
        meta,
        history_len,
        revision: snapshot.session.head.revision.get(),
        degraded_warnings: snapshot
            .missing_object_references
            .iter()
            .map(|reference| format!("missing SQLite object {reference}"))
            .collect(),
    };
    Ok(Some(SessionStoreResume {
        header,
        store_ref: SessionStoreRef {
            session_dir,
            db_path,
        },
        head: snapshot.session.head,
        transcript_record_tail: snapshot.transcript_record_tail,
    }))
}

pub fn load_store_header(id_or_prefix: &str) -> Option<(SessionHeader, SessionStoreRef)> {
    process_storage().load_store_header(id_or_prefix)
}

pub fn load_store_header_result(
    id_or_prefix: &str,
) -> SessionStoreResult<Option<(SessionHeader, SessionStoreRef)>> {
    process_storage().load_store_header_result(id_or_prefix)
}

pub fn load_store_header_for_dir(session_dir: PathBuf) -> Option<(SessionHeader, SessionStoreRef)> {
    load_store_header_for_dir_result(session_dir).ok().flatten()
}

pub fn load_store_header_for_dir_result(
    session_dir: PathBuf,
) -> SessionStoreResult<Option<(SessionHeader, SessionStoreRef)>> {
    crate::session_store::reject_symlink(&session_dir, "read")?;
    crate::session_store::reject_symlink(&session_dir.join("session.db"), "read")?;
    load_store_header_from_prepared_dir_result(session_dir)
}

fn load_store_header_from_prepared_dir_result(
    session_dir: PathBuf,
) -> SessionStoreResult<Option<(SessionHeader, SessionStoreRef)>> {
    let db_path = session_dir.join("session.db");
    let db = smelt_store::SessionReader::open_database(&db_path)
        .map_err(|err| crate::session_store::store_error("open", &db_path, err))?;
    let Some(stored) = db
        .stored_session()
        .map_err(|err| crate::session_store::store_error("read session metadata", &db_path, err))?
    else {
        return Ok(None);
    };
    let Some(meta) = load_meta_from_db_result(&session_dir)? else {
        return Ok(None);
    };
    let history_len =
        stored
            .head
            .history_len
            .as_usize()
            .ok_or_else(|| SessionStoreError::Corrupt {
                context: "session history length exceeds platform limits".into(),
            })?;
    let degraded_warnings = db.degraded_warnings().map_err(|err| {
        crate::session_store::store_error("inspect session objects", &db_path, err)
    })?;
    let header = SessionHeader {
        meta,
        history_len,
        revision: stored.head.revision.get(),
        degraded_warnings,
    };
    Ok(Some((
        header,
        SessionStoreRef {
            session_dir,
            db_path,
        },
    )))
}

fn session_dir_kind(dir: &Path) -> Option<SessionDirKind> {
    dir.join("session.db")
        .is_file()
        .then_some(SessionDirKind::Store)
}

pub fn load_meta_for_prepared_dir(dir: PathBuf) -> Option<SessionMeta> {
    load_meta_for_dir_result(dir).ok().flatten()
}

fn load_db_session_result(dir_path: &std::path::Path) -> SessionStoreResult<Option<Session>> {
    let db_path = dir_path.join("session.db");
    if !db_path.is_file() {
        return Ok(None);
    }
    let db = smelt_store::SessionReader::open_database(&db_path)
        .map_err(|err| crate::session_store::store_error("open", &db_path, err))?;
    let Some(session) = db
        .load_full_session()
        .map_err(|err| crate::session_store::store_error("load session", &db_path, err))?
    else {
        return Ok(None);
    };
    let expected_id = dir_path.file_name().and_then(|name| name.to_str());
    if expected_id != Some(session.session.identity.id.as_str()) {
        return Err(SessionStoreError::Corrupt {
            context: format!(
                "session id {:?} does not match directory {:?}",
                session.session.identity.id, expected_id
            ),
        });
    }
    session_from_full_store(session).map(Some)
}

fn session_from_full_store(snapshot: smelt_store::FullSession) -> SessionStoreResult<Session> {
    let identity = snapshot.session.identity;
    let metadata = snapshot.session.metadata;
    crate::session_id::SessionId::parse(&identity.id).map_err(|err| {
        SessionStoreError::Corrupt {
            context: format!("invalid persisted session id: {err}"),
        }
    })?;
    let turn_metas =
        snapshots_from_values(snapshot.turn_metas).map_err(|err| SessionStoreError::Corrupt {
            context: format!("invalid turn metadata: {err}"),
        })?;
    let metadata_snapshots = snapshots_from_values(snapshot.metadata_snapshots).map_err(|err| {
        SessionStoreError::Corrupt {
            context: format!("invalid metadata snapshot: {err}"),
        }
    })?;
    let context_snapshots = snapshots_from_values(snapshot.context_snapshots).map_err(|err| {
        SessionStoreError::Corrupt {
            context: format!("invalid accounting snapshot: {err}"),
        }
    })?;
    let context_state = context_snapshot_state_from_json(metadata.accounting_json.clone());
    let session_usage = context_state.session_usage.clone();
    let context_token_identity = context_state.context_token_identity;
    let display_context_token_identity = context_state
        .display_context_token_identity
        .or_else(|| context_token_identity.clone());
    let checkpoint = checkpoint_from_json(metadata.checkpoint_json.clone(), snapshot.history.len());
    let checkpoint_events = checkpoint_events_from_json(
        metadata.checkpoint_events_json.clone(),
        checkpoint.as_ref(),
        snapshot.history.len(),
    );
    let context_tokens = metadata
        .context_tokens
        .and_then(|tokens| u32::try_from(tokens).ok());
    let created_at_ms =
        u64::try_from(identity.created_at).map_err(|_| SessionStoreError::Corrupt {
            context: "negative session creation time".into(),
        })?;
    let updated_at_ms =
        u64::try_from(metadata.updated_at).map_err(|_| SessionStoreError::Corrupt {
            context: "negative session update time".into(),
        })?;
    Ok(Session {
        id: identity.id,
        title: metadata.title,
        slug: metadata.slug,
        first_user_message: metadata.first_user_message,
        metadata_snapshots,
        created_at_ms,
        updated_at_ms,
        mode: metadata.mode,
        reasoning_effort: metadata
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse),
        model: metadata.model,
        fast_mode: metadata.fast_mode,
        cwd: metadata.cwd,
        parent_id: identity.parent_id,
        history: snapshot.history,
        checkpoint,
        checkpoint_events,
        context_tokens,
        context_tokens_history_len: metadata
            .context_tokens_history_len
            .and_then(|len| usize::try_from(len).ok()),
        context_token_identity,
        display_context_tokens: metadata
            .display_context_tokens
            .and_then(|tokens| u32::try_from(tokens).ok())
            .or(context_tokens),
        display_context_token_identity,
        turn_metas,
        context_snapshots,
        session_cost_usd: metadata.session_cost_usd.get(),
        session_usage,
    })
}

fn snapshots_from_values<T: for<'de> Deserialize<'de>>(
    rows: Vec<(u64, Value)>,
) -> Result<HistorySnapshots<T>, serde_json::Error> {
    rows.into_iter()
        .map(|(idx, value)| serde_json::from_value(value).map(|value| (idx as usize, value)))
        .collect::<Result<Vec<_>, _>>()
        .map(HistorySnapshots::from_vec)
}

pub fn backfill_transcript_block_records_from_history_range(
    session_dir: &Path,
    history_range: std::ops::Range<usize>,
    record_start_idx: usize,
    block_start_idx: u64,
) -> Result<usize, smelt_store::StoreError> {
    let session_id = session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| smelt_store::StoreError::Integrity("session directory has no id".into()))?;
    let root = session_dir.parent().ok_or_else(|| {
        smelt_store::StoreError::Integrity("session directory has no parent".into())
    })?;
    let mut maintenance = smelt_store::SessionMaintenance::open(root, session_id)?;
    let reader = smelt_store::SessionReader::open_existing(session_dir)?;
    let history = reader.read_history_items_range(history_range.clone())?;
    let records = transcript_block_records_from_history_items(
        history_range.start,
        block_start_idx,
        &history,
    )?;
    let written = records.len();
    maintenance.replace_transcript_record_suffix(record_start_idx, &records)?;
    Ok(written)
}

pub fn backfill_transcript_block_records_in_history_chunks(
    session_dir: &Path,
    chunk_items: usize,
    max_chunks: Option<usize>,
) -> Result<usize, smelt_store::StoreError> {
    let chunk_items = chunk_items.max(1);
    let db = smelt_store::SessionReader::open_existing(session_dir)?;
    let history_len = db.history_item_count()?;
    let mut history_start = db
        .transcript_record_max_history_idx()?
        .map_or(0, |idx| idx.saturating_add(1))
        .min(history_len);
    drop(db);

    let mut total_written = 0usize;
    let mut chunks = 0usize;
    while history_start < history_len && max_chunks.is_none_or(|max| chunks < max) {
        let history_end = history_start.saturating_add(chunk_items).min(history_len);
        let record_start = {
            let db = smelt_store::SessionReader::open_existing(session_dir)?;
            db.transcript_record_count()?
        };
        let written = backfill_transcript_block_records_from_history_range(
            session_dir,
            history_start..history_end,
            record_start,
            record_start as u64,
        )?;
        if written == 0 {
            break;
        }
        total_written += written;
        history_start = history_end;
        chunks += 1;
        smelt_perf::perf::record_value("session:block_record_backfill:chunks", chunks as u64);
        smelt_perf::perf::record_value(
            "session:block_record_backfill:records",
            total_written as u64,
        );
    }
    Ok(total_written)
}

fn transcript_block_records_from_history_items(
    first_history_idx: usize,
    block_start_idx: u64,
    history: &[HistoryItem],
) -> Result<Vec<smelt_store::StoredTranscriptBlock>, smelt_store::StoreError> {
    let mut records = Vec::new();
    for (offset, item) in history.iter().enumerate() {
        push_history_item_block_rows(&mut records, first_history_idx + offset, item)?;
    }
    for (offset, record) in records.iter_mut().enumerate() {
        record.block_idx = block_start_idx.saturating_add(offset as u64);
    }
    Ok(records)
}

fn push_history_item_block_rows(
    records: &mut Vec<smelt_store::StoredTranscriptBlock>,
    history_idx: usize,
    item: &HistoryItem,
) -> Result<(), smelt_store::StoreError> {
    let origin = Some(crate::transcript_model::BlockOrigin::History(history_idx));
    match item {
        HistoryItem::User {
            content,
            display,
            command,
        } => {
            let block = match protocol::classify_user_history_content(content) {
                protocol::UserHistoryContent::CompactionSummary { summary } => {
                    crate::transcript_model::Block::Compacted { summary }
                }
                protocol::UserHistoryContent::ModeChange { text } => {
                    crate::transcript_model::Block::Mode {
                        text,
                        icon: String::new(),
                        hl_group: "SmeltAccent".to_string(),
                    }
                }
                protocol::UserHistoryContent::ProcessStatus { text } => {
                    crate::transcript_model::Block::ProcessStatus { text, event: None }
                }
                protocol::UserHistoryContent::Plain => {
                    let text = content.text_content();
                    let image_labels = content.image_labels();
                    let display_source = display.as_deref().unwrap_or(&text);
                    let display_text = if image_labels.is_empty() {
                        display_source.to_string()
                    } else {
                        let suffix = image_labels.join(" ");
                        if display_source.is_empty() {
                            suffix
                        } else {
                            format!("{display_source} {suffix}")
                        }
                    };
                    crate::transcript_model::Block::User {
                        text: display_text,
                        image_labels,
                        command: *command,
                    }
                }
            };
            records.push(transcript_block_record(records.len(), block, origin, None)?);
        }
        HistoryItem::Assistant(turn) => {
            if let Some(reasoning) = turn.reasoning.as_ref().filter(|text| !text.is_empty()) {
                records.push(transcript_block_record(
                    records.len(),
                    crate::transcript_model::Block::Thinking {
                        title: None,
                        summary_titles: Vec::new(),
                        content: reasoning.clone(),
                        kind: protocol::ReasoningKind::Raw,
                    },
                    origin,
                    None,
                )?);
            }
            if let Some(content) = &turn.content {
                records.push(transcript_block_record(
                    records.len(),
                    crate::transcript_model::Block::Text {
                        content: content.text_content().into_owned(),
                    },
                    origin,
                    None,
                )?);
            }
            for inv in &turn.invocations {
                let args: std::collections::HashMap<String, serde_json::Value> =
                    serde_json::from_str(&inv.arguments).unwrap_or_default();
                let status = if inv.result.content.contains("denied this tool call")
                    || inv.result.content.contains("blocked this tool call")
                {
                    crate::transcript_model::ToolStatus::Denied
                } else if inv.result.is_error {
                    crate::transcript_model::ToolStatus::Err
                } else {
                    crate::transcript_model::ToolStatus::Ok
                };
                let elapsed_ms = inv.elapsed_ms;
                let tool_state = crate::transcript_model::ToolState {
                    status,
                    elapsed: elapsed_ms.map(std::time::Duration::from_millis),
                    called_at_ms: inv.called_at_ms,
                    elapsed_active: false,
                    output: Some(Box::new(crate::transcript_model::ToolOutput {
                        content: inv.result.content.clone(),
                        is_error: inv.result.is_error,
                        metadata: inv.result.metadata.clone(),
                    })),
                    user_message: None,
                    preview_output: None,
                };
                records.push(transcript_block_record(
                    records.len(),
                    crate::transcript_model::Block::ToolCall {
                        call_id: inv.call_id.clone(),
                        name: inv.name.clone(),
                        summary: crate::mcp::args_summary(&args),
                        args,
                    },
                    origin,
                    Some(tool_state),
                )?);
            }
        }
        HistoryItem::Note(note) => match note.kind() {
            protocol::HistoryNoteKind::Context => {}
            protocol::HistoryNoteKind::ModeChange => records.push(transcript_block_record(
                records.len(),
                crate::transcript_model::Block::Mode {
                    text: note.text().to_string(),
                    icon: String::new(),
                    hl_group: "SmeltAccent".to_string(),
                },
                origin,
                None,
            )?),
            protocol::HistoryNoteKind::ProcessStatus => records.push(transcript_block_record(
                records.len(),
                crate::transcript_model::Block::ProcessStatus {
                    text: note.text().to_string(),
                    event: note.process_status_event_ref().cloned(),
                },
                origin,
                None,
            )?),
        },
        HistoryItem::System { .. } => {}
    }
    Ok(())
}

fn transcript_block_record(
    record_idx: usize,
    block: crate::transcript_model::Block,
    origin: Option<crate::transcript_model::BlockOrigin>,
    tool_state: Option<crate::transcript_model::ToolState>,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    let content_hash = crate::utils::hash_serializable(&block);
    let record = crate::transcript_model::TranscriptBlockRecord {
        block,
        content_hash,
        origin,
        tool_state,
    };
    crate::transcript_model::transcript_block_row(record_idx, &record)
}

impl SessionStorage {
    pub fn session_ids_result(&self) -> SessionStoreResult<Vec<String>> {
        let root = self.sessions_dir();
        crate::session_store::reject_symlink_in(self.state_root(), &root, "list sessions")?;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(SessionStoreError::Io {
                    operation: "list sessions in",
                    path: root.display().to_string(),
                    message: error.to_string(),
                })
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| SessionStoreError::Io {
                operation: "read session entry in",
                path: root.display().to_string(),
                message: error.to_string(),
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if crate::session_id::SessionId::parse(&name).is_ok() {
                ids.push(name);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn resolve_prefix(&self, prefix: &str) -> SessionStoreResult<crate::session_id::SessionId> {
        resolve_prefix_in(self.state_root(), &self.sessions_dir(), prefix)
    }

    pub fn delete(&self, id_or_prefix: &str) -> SessionStoreResult<()> {
        let id = self.resolve_prefix(id_or_prefix)?;
        let root = self.sessions_dir();
        let session_dir = self.session_dir(&id);
        debug_assert_eq!(session_dir.parent(), Some(root.as_path()));
        if session_dir.parent() != Some(root.as_path()) {
            return Err(SessionStoreError::Io {
                operation: "confine session path beneath",
                path: root.display().to_string(),
                message: "resolved session path escaped its root".into(),
            });
        }
        self.reject_symlink(&session_dir, "delete")?;
        self.reject_symlink(&session_dir.join("session.db"), "delete")?;
        if let Ok(catalog) = self.catalog() {
            catalog.begin_delete(id.as_str());
        }
        match smelt_store::SessionMaintenance::delete_session(&root, id.as_str())
            .map_err(|err| crate::session_store::store_error("delete session", &session_dir, err))
        {
            Ok(()) => {
                if let Ok(catalog) = self.catalog() {
                    catalog.complete_delete(id.as_str());
                }
                Ok(())
            }
            Err(error) => {
                if let Ok(catalog) = self.catalog() {
                    catalog.cancel_delete(id.as_str());
                }
                Err(error)
            }
        }
    }
}

pub fn session_ids_result() -> SessionStoreResult<Vec<String>> {
    process_storage().session_ids_result()
}

pub fn resolve_prefix(prefix: &str) -> SessionStoreResult<crate::session_id::SessionId> {
    process_storage().resolve_prefix(prefix)
}

fn resolve_prefix_in(
    state_root: &Path,
    root: &Path,
    prefix: &str,
) -> SessionStoreResult<crate::session_id::SessionId> {
    let prefix = crate::session_id::SessionPrefix::parse(prefix).map_err(|err| {
        SessionStoreError::InvalidSessionId {
            value: prefix.to_string(),
            message: err.to_string(),
        }
    })?;
    crate::session_store::reject_symlink_in(state_root, root, "resolve")?;
    if let Ok(id) = crate::session_id::SessionId::parse(prefix.as_str()) {
        return match fs::symlink_metadata(root.join(id.as_str())) {
            Ok(_) => Ok(id),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(SessionStoreError::SessionNotFound {
                    id: prefix.as_str().to_string(),
                })
            }
            Err(error) => Err(SessionStoreError::Io {
                operation: "inspect session in",
                path: root.display().to_string(),
                message: error.to_string(),
            }),
        };
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionStoreError::SessionNotFound {
                id: prefix.as_str().to_string(),
            });
        }
        Err(err) => {
            return Err(SessionStoreError::Io {
                operation: "list sessions in",
                path: root.display().to_string(),
                message: err.to_string(),
            });
        }
    };
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| SessionStoreError::Io {
            operation: "read session entry in",
            path: root.display().to_string(),
            message: err.to_string(),
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = crate::session_id::SessionId::parse(&name) else {
            continue;
        };
        if !id.as_str().starts_with(prefix.as_str()) {
            continue;
        }
        matches.push(id);
    }
    match matches.len() {
        0 => Err(SessionStoreError::SessionNotFound {
            id: prefix.as_str().to_string(),
        }),
        1 => Ok(matches.pop().expect("one session match")),
        count => Err(SessionStoreError::AmbiguousPrefix {
            prefix: prefix.as_str().to_string(),
            matches: count,
        }),
    }
}

pub fn delete(id_or_prefix: &str) -> SessionStoreResult<()> {
    process_storage().delete(id_or_prefix)
}

impl SessionStorage {
    pub fn list_sessions(&self) -> Vec<SessionListMeta> {
        self.list_session_entries()
            .into_iter()
            .filter_map(|entry| match entry.status {
                SessionListStatus::Available(meta) => Some(*meta),
                SessionListStatus::Upgradeable { .. } | SessionListStatus::Unavailable(_) => None,
            })
            .collect()
    }

    pub fn list_session_entries(&self) -> Vec<SessionListEntry> {
        self.list_session_entries_result().unwrap_or_default()
    }

    pub fn list_session_entries_result(&self) -> SessionStoreResult<Vec<SessionListEntry>> {
        list_session_entries_result_in(self)
    }

    pub fn list_session_page_result(
        &self,
        query: SessionListQuery,
    ) -> SessionStoreResult<SessionListPage> {
        list_session_page_result_in(self, query)
    }

    pub fn request_session_catalog_reconciliation(&self) {
        if let Ok(catalog) = self.catalog() {
            catalog.request_reconciliation();
        }
    }

    pub fn wait_for_session_catalog(&self, timeout: std::time::Duration) -> bool {
        self.catalog()
            .is_ok_and(|catalog| catalog.wait_for_queued_work(timeout))
    }

    pub fn request_session_catalog_projection(&self, id: &str, revision: smelt_store::Revision) {
        if let Ok(catalog) = self.catalog() {
            catalog.request_projection(id, revision);
        }
    }

    pub fn publish_session_catalog_commit(
        &self,
        command: &smelt_store::SessionCommit,
        receipt: &smelt_store::SaveReceipt,
        schedule_projection: bool,
    ) {
        if let Ok(catalog) = self.catalog() {
            catalog.publish_commit(command, receipt, schedule_projection);
        }
    }

    fn read_catalog_page(
        &self,
        query: &smelt_store::CatalogQuery,
    ) -> crate::session_catalog::ReadPage {
        match self.catalog() {
            Ok(catalog) => catalog.read_page(query),
            Err(error) => crate::session_catalog::unavailable_read_page(error),
        }
    }
}

pub fn list_sessions() -> Vec<SessionListMeta> {
    process_storage().list_sessions()
}

pub fn list_session_entries() -> Vec<SessionListEntry> {
    process_storage().list_session_entries()
}

pub fn list_session_entries_result() -> SessionStoreResult<Vec<SessionListEntry>> {
    process_storage().list_session_entries_result()
}

fn list_session_entries_result_in(
    storage: &SessionStorage,
) -> SessionStoreResult<Vec<SessionListEntry>> {
    let _perf = smelt_perf::perf::begin("session:list");
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = storage.list_session_page_result(SessionListQuery {
            limit: 512,
            cursor,
            cwd: None,
            availability: None,
        })?;
        entries.extend(page.entries);
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    Ok(entries)
}

pub fn list_session_page_result(query: SessionListQuery) -> SessionStoreResult<SessionListPage> {
    process_storage().list_session_page_result(query)
}

fn list_session_page_result_in(
    storage: &SessionStorage,
    query: SessionListQuery,
) -> SessionStoreResult<SessionListPage> {
    let _perf = smelt_perf::perf::begin("session:list_page");
    if query.limit == 0 || query.limit > 1_000 {
        return Err(SessionStoreError::InvalidListQuery {
            message: "page limit must be between 1 and 1000".into(),
        });
    }
    let cursor = query
        .cursor
        .map(|cursor| {
            Ok(smelt_store::CatalogCursor {
                updated_at: i64::try_from(cursor.updated_at_ms).map_err(|_| {
                    SessionStoreError::InvalidListQuery {
                        message: "cursor update time exceeds SQLite integer range".into(),
                    }
                })?,
                id: cursor.id,
            })
        })
        .transpose()?;
    let catalog_query = smelt_store::CatalogQuery {
        limit: query.limit,
        cursor,
        cwd: query.cwd,
        availability: query.availability.map(|availability| match availability {
            SessionListAvailability::Available => smelt_store::CatalogAvailability::Available,
            SessionListAvailability::Unavailable => smelt_store::CatalogAvailability::Unavailable,
        }),
    };
    let page = storage.read_catalog_page(&catalog_query);
    let entries = page
        .sessions
        .into_iter()
        .map(session_list_entry_from_catalog)
        .collect::<SessionStoreResult<Vec<_>>>()?;
    let next_cursor = page
        .next_cursor
        .map(|cursor| {
            Ok(SessionListCursor {
                updated_at_ms: u64::try_from(cursor.updated_at).map_err(|_| {
                    SessionStoreError::Corrupt {
                        context: "catalog session update time is negative".into(),
                    }
                })?,
                id: cursor.id,
            })
        })
        .transpose()?;
    let state = match page.status.state {
        crate::session_catalog::ServiceState::Reconciling => SessionCatalogState::Reconciling,
        crate::session_catalog::ServiceState::Ready => SessionCatalogState::Ready,
        crate::session_catalog::ServiceState::Degraded => SessionCatalogState::Degraded,
    };
    Ok(SessionListPage {
        entries,
        next_cursor,
        catalog: SessionCatalogStatus {
            state,
            completed_scan_id: page.status.completed_scan_id,
            reconciled_at_ms: page
                .status
                .reconciled_at
                .map(|at| {
                    u64::try_from(at).map_err(|_| SessionStoreError::Corrupt {
                        context: "catalog reconciliation time is negative".into(),
                    })
                })
                .transpose()?,
            last_error: page.status.last_error,
        },
    })
}

pub fn request_session_catalog_reconciliation() {
    process_storage().request_session_catalog_reconciliation();
}

/// Wait until all catalog work queued before this call has completed.
pub fn wait_for_session_catalog(timeout: std::time::Duration) -> bool {
    process_storage().wait_for_session_catalog(timeout)
}

pub fn request_session_catalog_projection(id: &str, revision: smelt_store::Revision) {
    process_storage().request_session_catalog_projection(id, revision);
}

pub fn publish_session_catalog_commit(
    command: &smelt_store::SessionCommit,
    receipt: &smelt_store::SaveReceipt,
    schedule_projection: bool,
) {
    process_storage().publish_session_catalog_commit(command, receipt, schedule_projection);
}

fn session_list_entry_from_catalog(
    session: smelt_store::CatalogSession,
) -> SessionStoreResult<SessionListEntry> {
    let id = session.id.clone();
    let status = match session.availability {
        smelt_store::CatalogAvailability::Available => {
            let created_at_ms =
                u64::try_from(session.created_at).map_err(|_| SessionStoreError::Corrupt {
                    context: format!("catalog session {id} has a negative creation time"),
                })?;
            let updated_at_ms =
                u64::try_from(session.updated_at).map_err(|_| SessionStoreError::Corrupt {
                    context: format!("catalog session {id} has a negative update time"),
                })?;
            SessionListStatus::Available(Box::new(SessionListMeta {
                id: id.clone(),
                title: session.title,
                slug: session.slug,
                first_user_message: session.first_user_message,
                created_at_ms,
                updated_at_ms,
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
                history_len: session
                    .history_len
                    .and_then(|length| usize::try_from(length).ok()),
                text_bytes: session.text_bytes,
            }))
        }
        smelt_store::CatalogAvailability::Unavailable => {
            let kind = session.error_kind.unwrap_or_else(|| "unavailable".into());
            let summary = session
                .error_summary
                .unwrap_or_else(|| format!("session {id} is unavailable"));
            if kind == "upgrade_required" {
                SessionListStatus::Upgradeable { reason: summary }
            } else {
                let error = match kind.as_str() {
                    "missing_database" => SessionStoreError::MissingDatabase { id: id.clone() },
                    "corrupt" => SessionStoreError::Corrupt { context: summary },
                    _ => SessionStoreError::CatalogUnavailable { kind, summary },
                };
                SessionListStatus::Unavailable(error)
            }
        }
    };
    Ok(SessionListEntry { id, status })
}

fn load_meta_for_dir_result(path: PathBuf) -> SessionStoreResult<Option<SessionMeta>> {
    crate::session_store::reject_symlink(&path, "read")?;
    let db_path = path.join("session.db");
    crate::session_store::reject_symlink(&db_path, "read")?;
    if !db_path.is_file() {
        return Ok(None);
    }
    load_meta_from_db_result(&path)
}

fn load_meta_from_db_result(path: &Path) -> SessionStoreResult<Option<SessionMeta>> {
    let db_path = path.join("session.db");
    if !db_path.is_file() {
        return Ok(None);
    }
    let db = smelt_store::SessionReader::open_database(&db_path)
        .map_err(|err| crate::session_store::store_error("open", &db_path, err))?;
    let Some(session) = db
        .stored_session()
        .map_err(|err| crate::session_store::store_error("read session metadata", &db_path, err))?
    else {
        return Ok(None);
    };
    let text_bytes = db
        .history_text_bytes()
        .map_err(|err| crate::session_store::store_error("read history size", &db_path, err))?;
    let retained_history_len = db
        .history_item_count()
        .map_err(|err| crate::session_store::store_error("read history length", &db_path, err))?;
    session_meta_from_stored_session(path, session, text_bytes, retained_history_len).map(Some)
}

fn session_meta_from_stored_session(
    path: &Path,
    session: smelt_store::StoredSession,
    text_bytes: u64,
    retained_history_len: usize,
) -> SessionStoreResult<SessionMeta> {
    let identity = session.identity;
    let metadata = session.metadata;
    crate::session_id::SessionId::parse(&identity.id).map_err(|err| {
        SessionStoreError::Corrupt {
            context: format!("invalid persisted session id: {err}"),
        }
    })?;
    let expected_id = path.file_name().and_then(|name| name.to_str());
    if expected_id != Some(identity.id.as_str()) {
        return Err(SessionStoreError::Corrupt {
            context: format!(
                "session id {:?} does not match directory {:?}",
                identity.id, expected_id
            ),
        });
    }
    let history_len =
        session
            .head
            .history_len
            .as_usize()
            .ok_or_else(|| SessionStoreError::Corrupt {
                context: "session history length exceeds platform limits".into(),
            })?;
    let checkpoint = checkpoint_from_json(
        metadata.checkpoint_json.clone(),
        retained_history_len.min(history_len),
    );
    let context_state = context_snapshot_state_from_json(metadata.accounting_json.clone());
    let context_token_identity = context_state.context_token_identity;
    let display_context_token_identity = context_state
        .display_context_token_identity
        .or_else(|| context_token_identity.clone());
    let authoritative_context_tokens = metadata
        .context_tokens
        .and_then(|tokens| u32::try_from(tokens).ok())
        .zip(
            metadata
                .context_tokens_history_len
                .and_then(|len| usize::try_from(len).ok()),
        )
        .zip(context_token_identity)
        .map(
            |((tokens, history_len), identity)| AuthoritativeContextTokens {
                tokens,
                history_len,
                identity,
            },
        );
    let display_context_tokens = metadata
        .display_context_tokens
        .or(metadata.context_tokens)
        .and_then(|tokens| u32::try_from(tokens).ok())
        .map(|tokens| DisplayContextTokens {
            tokens,
            identity: display_context_token_identity,
        });
    let checkpoint_events = checkpoint_events_from_json(
        metadata.checkpoint_events_json.clone(),
        checkpoint.as_ref(),
        retained_history_len.min(history_len),
    );
    let created_at_ms =
        u64::try_from(identity.created_at).map_err(|_| SessionStoreError::Corrupt {
            context: "negative session creation time".into(),
        })?;
    let updated_at_ms =
        u64::try_from(metadata.updated_at).map_err(|_| SessionStoreError::Corrupt {
            context: "negative session update time".into(),
        })?;
    Ok(SessionMeta {
        id: identity.id,
        title: metadata.title,
        slug: metadata.slug,
        first_user_message: metadata.first_user_message,
        created_at_ms,
        updated_at_ms,
        mode: metadata.mode,
        reasoning_effort: metadata
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse),
        model: metadata.model,
        fast_mode: metadata.fast_mode,
        cwd: metadata.cwd,
        parent_id: identity.parent_id,
        authoritative_context_tokens,
        display_context_tokens,
        history_len: Some(history_len),
        checkpoint,
        checkpoint_events,
        text_bytes: Some(text_bytes),
    })
}

impl SessionStorage {
    /// Read searchable text from canonical SQLite without refreshing derived files.
    pub fn load_search_blob(&self, id_or_prefix: &str) -> Option<String> {
        self.load_search_blob_result(id_or_prefix).ok().flatten()
    }

    pub fn load_search_blob_result(
        &self,
        id_or_prefix: &str,
    ) -> SessionStoreResult<Option<String>> {
        let _perf = smelt_perf::perf::begin("session:load_search_blob");
        let id = self.resolve_prefix(id_or_prefix)?;
        let session_dir = self.session_dir(&id);
        self.ensure_session_db_read_only(&session_dir)?;
        let db_path = session_dir.join("session.db");
        let db = smelt_store::SessionReader::open_database(&db_path)
            .map_err(|err| crate::session_store::store_error("open", &db_path, err))?;
        db.search_blob()
            .map(Some)
            .map_err(|err| crate::session_store::store_error("read search text", &db_path, err))
    }

    /// Parallel batch read of search blobs. Missing or unavailable sessions are omitted.
    pub fn load_search_blobs(&self, ids: Vec<String>) -> Vec<(String, String)> {
        let _perf = smelt_perf::perf::begin("session:load_search_blobs");
        let storage = self.clone();
        crate::utils::parallel_filter_map(ids, move |id| {
            storage.load_search_blob(&id).map(|blob| (id, blob))
        })
    }
}

/// Read searchable text from canonical SQLite without refreshing derived files.
pub fn load_search_blob(id_or_prefix: &str) -> Option<String> {
    process_storage().load_search_blob(id_or_prefix)
}

pub fn load_search_blob_result(id_or_prefix: &str) -> SessionStoreResult<Option<String>> {
    process_storage().load_search_blob_result(id_or_prefix)
}

/// Parallel batch read of search blobs. Returns `(id, blob)` pairs; missing
/// or unavailable sessions are omitted. Output order is not stable.
pub fn load_search_blobs(ids: Vec<String>) -> Vec<(String, String)> {
    process_storage().load_search_blobs(ids)
}

#[cfg(test)]
fn session_updated_at(meta: &SessionMeta) -> u64 {
    if meta.updated_at_ms > 0 {
        meta.updated_at_ms
    } else {
        meta.created_at_ms
    }
}

#[cfg(test)]
pub(crate) fn sessions_dir() -> PathBuf {
    process_storage().sessions_dir()
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

    const TEST_SESSION_ID: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn numbered_session_id(value: u64) -> String {
        format!("{value:064x}")
    }

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

    #[test]
    fn delete_rejects_untrusted_paths_and_malformed_ids() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let sessions = sessions_dir();
        fs::create_dir_all(&sessions).expect("create sessions dir");
        let absolute_target = state.path().join("absolute-target");
        fs::create_dir_all(&absolute_target).expect("create absolute target");
        let parent_target = sessions.parent().unwrap().join("parent-target");
        fs::create_dir_all(&parent_target).expect("create parent target");

        let absolute = absolute_target.to_str().expect("utf-8 temp path");
        for invalid in [absolute, "../parent-target", "a/b", "", "ABCD", "xyz1"] {
            assert!(
                matches!(
                    delete(invalid),
                    Err(SessionStoreError::InvalidSessionId { .. })
                ),
                "unexpected delete result for {invalid:?}"
            );
        }
        assert!(absolute_target.exists());
        assert!(parent_target.exists());
    }

    #[test]
    fn delete_accepts_valid_exact_id_and_unique_prefix_but_rejects_ambiguity() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let first = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let second = "0123ffff89abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for id in [first, second] {
            let mut session = fixture_session();
            session.id = id.into();
            session.history.push(user_item("deletable"));
            save_result(&session).expect("create valid session");
        }

        assert!(matches!(
            delete("0123"),
            Err(SessionStoreError::AmbiguousPrefix { matches: 2, .. })
        ));
        delete("01234567").expect("delete unique prefix");
        assert!(!sessions_dir().join(first).exists());
        delete(second).expect("delete exact id");
        assert!(!sessions_dir().join(second).exists());
    }

    #[cfg(unix)]
    #[test]
    fn delete_rejects_symlinked_session_directory() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let target = state.path().join("outside-target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("session.db"), "fixture").unwrap();
        fs::create_dir_all(sessions_dir()).unwrap();
        symlink(&target, sessions_dir().join(id)).unwrap();

        assert!(matches!(
            delete(id),
            Err(SessionStoreError::SymlinkNotAllowed { .. })
        ));
        assert!(target.join("session.db").exists());
    }

    #[cfg(unix)]
    #[test]
    fn session_resolution_rejects_symlinked_sessions_root() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let target_root = state.path().join("outside-sessions");
        let target_session = target_root.join(id);
        fs::create_dir_all(&target_session).unwrap();
        fs::write(target_session.join("session.db"), "fixture").unwrap();
        fs::create_dir_all(engine::state_dir()).unwrap();
        symlink(&target_root, sessions_dir()).unwrap();

        assert!(matches!(
            resolve_prefix(id),
            Err(SessionStoreError::SymlinkNotAllowed { .. })
        ));
        assert!(target_session.join("session.db").exists());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_storage_rejects_a_symlinked_state_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("runtime root");
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let target_root = root.path().join("target-state");
        let target_session = target_root.join("sessions").join(id);
        fs::create_dir_all(&target_session).unwrap();
        fs::write(target_session.join("session.db"), "fixture").unwrap();
        let state_root = root.path().join("runtime-state");
        symlink(&target_root, &state_root).unwrap();
        let storage = SessionStorage::new(state_root);

        assert!(matches!(
            storage.resolve_prefix(id),
            Err(SessionStoreError::SymlinkNotAllowed { .. })
        ));
        assert!(target_session.join("session.db").exists());
    }

    use protocol::{AssistantStep, Content, ContentPart, HistoryItem, ToolInvocation, ToolOutcome};

    fn user_item(text: &str) -> HistoryItem {
        HistoryItem::User {
            content: Content::Text(text.into()),
            display: None,
            command: false,
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
            summary: summary.to_string(),
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

        session.finish_turn_state(7, meta, true, true);

        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
    }

    #[test]
    fn side_table_suffix_includes_turn_meta_at_final_history_boundary() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user_item("hello")];
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: false,
        };
        session.finish_turn_state(session.history.len(), meta.clone(), false, false);

        let suffix = store_side_table_suffixes_from_session(&session, session.history.len())
            .expect("serialize final-boundary turn metadata");

        assert_eq!(suffix.turn_metas.len(), 1);
        assert_eq!(suffix.turn_metas[0].0, smelt_store::HistoryIndex::new(1));
        assert_eq!(
            suffix.turn_metas[0].1,
            serde_json::to_value(meta).expect("serialize expected turn metadata")
        );
    }

    #[test]
    fn side_table_suffix_rejects_snapshots_past_final_history_len() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user_item("hello")];
        session.snapshot_context_at(1);
        session.snapshot_context_at(24);

        let err = store_side_table_suffixes_from_session_at(&session, 0, 1).unwrap_err();

        assert!(err
            .to_string()
            .contains("snapshot index 24 must be at or before final history length 1"));
    }

    #[test]
    fn load_store_header_for_dir_does_not_repair_transcript_links() {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join(TEST_SESSION_ID);
        fs::create_dir(&dir).expect("create session dir");
        let mut s = fixture_session();
        s.id = TEST_SESSION_ID.into();
        s.history
            .push(HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            )));
        let mut db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        db.apply_session_commit(&initial_store_commit_from_session(&s).unwrap())
            .unwrap();
        let block_json = serde_json::json!({
            "kind": "user",
            "text": "continue",
            "image_labels": [],
        })
        .to_string();
        let origin_json = serde_json::json!({ "History": 0 }).to_string();
        db.connection()
            .execute(
                "UPDATE transcript_blocks
                 SET record_idx = 0, kind = 'user', content_hash = 'bad-user-link',
                     estimated_text_bytes = ?1, preview_text = 'continue',
                     block_json = ?2, origin_json = ?3
                 WHERE block_idx = 0",
                ("continue".len() as i64, block_json, origin_json),
            )
            .unwrap();
        drop(db);

        let (_header, _store_ref) =
            load_store_header_for_dir(dir.clone()).expect("store header loads without repair");

        let db = smelt_store::SessionDb::open_read_only(dir.join("session.db")).unwrap();
        let rows = db.read_all_transcript_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].history_idx, Some(0));
        assert!(rows[0].origin_json.is_some());
    }

    #[test]
    fn store_metadata_drops_checkpoint_past_target_history_len() {
        let mut s = fixture_session();
        s.history = vec![user_item("kept")];
        s.checkpoint = Some(checkpoint("stale summary", 3));

        let metadata = store_metadata_from_session(&s, 1).unwrap();

        assert!(metadata.checkpoint_json.is_none());
    }

    #[test]
    fn load_store_header_tolerates_checkpoint_without_repairing_database() {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join(TEST_SESSION_ID);
        fs::create_dir(&dir).expect("create session dir");
        let mut s = fixture_session();
        s.id = TEST_SESSION_ID.into();
        s.history = vec![user_item("old prompt"), assistant_text_item("recent reply")];
        let mut db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        db.apply_session_commit(&initial_store_commit_from_session(&s).unwrap())
            .unwrap();
        let checkpoint = serde_json::json!({
            "kind": "compaction",
            "summary": "retained summary",
            "first_live_index": 177,
            "created_at_ms": 1,
        });
        db.connection()
            .execute(
                "UPDATE session_state SET checkpoint_json = ?1 WHERE singleton = 1",
                [checkpoint.to_string()],
            )
            .unwrap();
        drop(db);

        let (header, store_ref) =
            load_store_header_for_dir(dir.clone()).expect("store header loads without repair");

        assert_eq!(header.history_len, 2);
        assert_eq!(
            header
                .meta
                .checkpoint
                .as_ref()
                .map(|cp| cp.first_live_index),
            Some(0)
        );
        let checkpoint = header.meta.checkpoint.clone();
        let live = crate::session_runtime::LiveSession::from_store(header, store_ref);
        match live.model_history_source(checkpoint.as_ref()) {
            protocol::ModelHistorySource::Store {
                prefix,
                first_live_index,
                end_index,
                suffix,
                ..
            } => {
                assert_eq!(prefix.len(), 1);
                assert_eq!(first_live_index, 0);
                assert_eq!(end_index, 2);
                assert!(suffix.is_empty());
            }
            protocol::ModelHistorySource::Items { .. } => {
                panic!("expected store-backed model history")
            }
        }

        let db = smelt_store::SessionDb::open_read_only(dir.join("session.db")).unwrap();
        let persisted = db
            .stored_session()
            .unwrap()
            .unwrap()
            .metadata
            .checkpoint_json
            .unwrap();
        assert_eq!(persisted["first_live_index"].as_u64(), Some(177));
    }

    #[test]
    fn list_sessions_reads_catalog_without_opening_session_databases() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut s = fixture_session();
            s.id = numbered_session_id(i + 1);
            s.title = Some(format!("catalog session {i}"));
            s.updated_at_ms = 1_700_000_000_000 + i;
            s.history.push(user_item(&format!("prompt {i}")));
            save_result(&s).unwrap();
            ids.push(s.id);
        }

        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let listed = list_sessions();
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        for id in &ids {
            let meta = listed
                .iter()
                .find(|meta| meta.id == *id)
                .expect("listed catalog metadata");
            assert_eq!(meta.history_len, Some(1));
            assert!(meta.text_bytes.is_some_and(|bytes| bytes > 0));
        }
        let read_only_count = snapshot
            .durations
            .iter()
            .find(|row| row.label == "store:db:open_read_only")
            .map(|row| row.count)
            .unwrap_or(0);
        assert_eq!(
            read_only_count, 0,
            "catalog listing must not open canonical session databases"
        );
        let read_write_count = snapshot
            .durations
            .iter()
            .find(|row| row.label == "store:db:open_read_write")
            .map(|row| row.count)
            .unwrap_or(0);
        assert_eq!(
            read_write_count, 0,
            "list_sessions should not open databases read-write"
        );

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let loaded = load_meta(&ids[0]).expect("load exact meta");
        let exact_snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        assert_eq!(loaded.history_len, Some(1));
        assert!(loaded.text_bytes.is_some_and(|bytes| bytes > 0));
        assert!(
            exact_snapshot
                .durations
                .iter()
                .any(|row| row.label == "store:db:open_read_only" && row.count > 0),
            "exact load should still enrich stale metadata from sqlite"
        );
    }

    #[test]
    fn listing_classifies_old_schema_and_explicit_resume_migrates_and_reprojects() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut session = fixture_session();
        session.id = numbered_session_id(4);
        session.history.push(user_item("upgrade me"));
        save_result(&session).unwrap();
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));

        let session_dir = dir_for(&session);
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        db.connection()
            .pragma_update(None, "user_version", 9)
            .unwrap();
        drop(db);
        request_session_catalog_reconciliation();
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));

        let entry = list_session_entries()
            .into_iter()
            .find(|entry| entry.id == session.id)
            .expect("old session remains listed");
        assert!(matches!(
            entry.status,
            SessionListStatus::Upgradeable { .. }
        ));
        assert_eq!(
            session_schema_status_result(&session.id).unwrap(),
            smelt_store::SessionSchemaStatus::Upgradeable {
                found: 9,
                target: smelt_store::SCHEMA_VERSION,
            },
            "listing must not migrate canonical storage"
        );

        let resumed = load_store_resume_result(&session.id, 80, 40)
            .unwrap()
            .expect("bounded resume");
        assert_eq!(resumed.header.history_len, 1);
        assert_eq!(
            session_schema_status_result(&session.id).unwrap(),
            smelt_store::SessionSchemaStatus::Current {
                version: smelt_store::SCHEMA_VERSION,
            }
        );
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));
        let entry = list_session_entries()
            .into_iter()
            .find(|entry| entry.id == session.id)
            .expect("migrated session remains listed");
        assert!(matches!(entry.status, SessionListStatus::Available(_)));
    }

    #[test]
    fn explicit_migration_reuses_the_validated_store_head_for_catalog_projection() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut session = fixture_session();
        session.id = numbered_session_id(5);
        session.history.push(user_item("upgrade without reopening"));
        save_result(&session).unwrap();
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));

        let session_dir = dir_for(&session);
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        let expected_head = db.store_head().unwrap();
        db.connection()
            .pragma_update(None, "user_version", 9)
            .unwrap();
        drop(db);

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let migration = migrate_session_schema_result(&session.id).unwrap();
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        assert!(migration.migrated);
        assert_eq!(migration.store_head, expected_head);
        let read_only_count = snapshot
            .durations
            .iter()
            .find(|row| row.label == "store:db:open_read_only")
            .map(|row| row.count)
            .unwrap_or(0);
        assert_eq!(
            read_only_count, 0,
            "migration must project from its receipt instead of reopening canonical storage"
        );
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));
    }

    #[test]
    fn ordinary_reads_do_not_modify_the_canonical_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut session = fixture_session();
        session.id = TEST_SESSION_ID.into();
        session.history.push(user_item("read only"));
        save_result(&session).unwrap();
        let db_path = dir_for(&session).join("session.db");
        let modified = fs::metadata(&db_path).unwrap().modified().unwrap();

        assert!(load_store_header(&session.id).is_some());
        assert!(load_meta(&session.id).is_some());
        assert!(load_search_blob(&session.id).is_some_and(|text| text.contains("read only")));
        assert_eq!(list_sessions().len(), 1);

        assert_eq!(
            fs::metadata(&db_path).unwrap().modified().unwrap(),
            modified
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_state_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut session = fixture_session();
        session.id = TEST_SESSION_ID.into();
        session.history.push(user_item("private"));
        save_result(&session).unwrap();
        let session_dir = dir_for(&session);
        for dir in [engine::state_dir(), sessions_dir(), session_dir.clone()] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(session_dir.join("session.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn list_session_entries_keeps_corrupt_session_visible() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut healthy = fixture_session();
        healthy.id = numbered_session_id(20);
        healthy.history.push(user_item("healthy"));
        save(&healthy);
        let corrupt_id = numbered_session_id(21);
        let corrupt_dir = sessions_dir().join(&corrupt_id);
        fs::create_dir_all(&corrupt_dir).unwrap();
        fs::write(corrupt_dir.join("session.db"), b"not sqlite").unwrap();
        let missing_db_id = numbered_session_id(22);
        fs::create_dir_all(sessions_dir().join(&missing_db_id)).unwrap();
        request_session_catalog_reconciliation();
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));

        let entries = list_session_entries();

        assert!(entries.iter().any(|entry| {
            entry.id == healthy.id && matches!(entry.status, SessionListStatus::Available(_))
        }));
        assert!(entries.iter().any(|entry| {
            entry.id == corrupt_id && matches!(entry.status, SessionListStatus::Unavailable(_))
        }));
        assert!(entries.iter().any(|entry| {
            entry.id == missing_db_id
                && matches!(
                    entry.status,
                    SessionListStatus::Unavailable(SessionStoreError::MissingDatabase { .. })
                )
        }));
    }

    #[test]
    fn exact_load_preserves_invalid_missing_and_unsupported_errors() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        assert!(matches!(
            load_meta_result("../escape"),
            Err(SessionStoreError::InvalidSessionId { .. })
        ));
        assert!(matches!(
            load_meta_result("abcd"),
            Err(SessionStoreError::SessionNotFound { .. })
        ));
        let missing_db_id = numbered_session_id(29);
        fs::create_dir_all(sessions_dir().join(&missing_db_id)).unwrap();
        assert!(matches!(
            load_meta_result(&missing_db_id),
            Err(SessionStoreError::MissingDatabase { .. })
        ));

        let id = numbered_session_id(30);
        let dir = sessions_dir().join(&id);
        let db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        db.connection()
            .execute_batch("PRAGMA user_version = 999")
            .unwrap();
        drop(db);
        assert!(matches!(
            load_meta_result(&id),
            Err(SessionStoreError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn session_listing_rejects_invalid_page_queries() {
        assert!(matches!(
            list_session_page_result(SessionListQuery {
                limit: 0,
                ..SessionListQuery::default()
            }),
            Err(SessionStoreError::InvalidListQuery { .. })
        ));
        assert!(matches!(
            list_session_page_result(SessionListQuery {
                cursor: Some(SessionListCursor {
                    updated_at_ms: u64::MAX,
                    id: TEST_SESSION_ID.into(),
                }),
                ..SessionListQuery::default()
            }),
            Err(SessionStoreError::InvalidListQuery { .. })
        ));
    }

    #[test]
    fn session_listing_reports_root_io_errors_without_synchronous_scanning() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        create_private_dir_all(&engine::state_dir()).unwrap();
        fs::write(sessions_dir(), "not a directory").unwrap();

        request_session_catalog_reconciliation();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let page = list_session_page_result(SessionListQuery {
                limit: 10,
                ..SessionListQuery::default()
            })
            .unwrap();
            if page.catalog.state == SessionCatalogState::Degraded {
                assert!(page.catalog.last_error.is_some());
                assert!(page.entries.is_empty());
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn persisted_session_id_must_match_directory_id() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let directory_id = numbered_session_id(31);
        let persisted_id = numbered_session_id(32);
        let dir = sessions_dir().join(&directory_id);
        let mut session = fixture_session();
        session.id = persisted_id;
        session.history.push(user_item("mismatched id"));
        let mut db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        db.apply_session_commit(&initial_store_commit_from_session(&session).unwrap())
            .unwrap();
        drop(db);

        assert!(matches!(
            load_full_result(&directory_id),
            Err(SessionStoreError::Corrupt { .. })
        ));
        request_session_catalog_reconciliation();
        assert!(wait_for_session_catalog(std::time::Duration::from_secs(2)));
        assert!(list_session_entries().iter().any(|entry| {
            entry.id == directory_id
                && matches!(
                    entry.status,
                    SessionListStatus::Unavailable(SessionStoreError::Corrupt { .. })
                )
        }));

        assert!(matches!(
            load_meta_result(&directory_id),
            Err(SessionStoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn resolve_session_dir_for_read_ignores_directories_without_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let stale_dir = sessions_dir().join("legacy-resolve");
        fs::create_dir_all(&stale_dir).expect("create stale session dir");
        fs::write(stale_dir.join("session.json"), "{}").expect("write stale marker");

        let mut session = fixture_session();
        session.id = TEST_SESSION_ID.into();
        session.history.push(user_item("hello"));
        save(&session);

        assert!(resolve_session_dir_for_read("abcd").is_none());
        assert!(!stale_dir.join("session.db").exists());

        let store = resolve_session_dir_for_read("01234567").expect("resolve store prefix");
        assert_eq!(store.id, session.id);
        assert_eq!(store.kind, SessionDirKind::Store);

        let (header, store_ref) = load_store_header("01234567").expect("load store header");
        assert_eq!(header.meta.id, session.id);
        assert_eq!(header.history_len, 1);
        assert_eq!(header.meta.history_len, Some(1));
        assert!(header.revision > 0);
        assert_eq!(store_ref.session_dir, dir_for(&session));
        assert!(store_ref.db_path.is_file());
    }

    fn stale_session_dir_without_db(id: &str) -> std::path::PathBuf {
        let dir = sessions_dir().join(id);
        fs::create_dir_all(&dir).expect("create stale session dir");
        fs::write(dir.join("session.json"), "{}").expect("write stale marker");
        dir
    }

    #[test]
    fn prepare_session_dir_for_read_does_not_create_missing_session_db() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = numbered_session_id(100);
        let dir = stale_session_dir_without_db(&id);

        assert!(prepare_session_dir_for_read(&id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn load_meta_does_not_create_missing_session_db() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = numbered_session_id(101);
        let dir = stale_session_dir_without_db(&id);

        assert!(load_meta(&id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn list_sessions_ignores_directory_without_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = numbered_session_id(102);
        let dir = stale_session_dir_without_db(&id);

        assert!(list_sessions().is_empty());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn load_search_blob_does_not_create_missing_session_db() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = numbered_session_id(103);
        let dir = stale_session_dir_without_db(&id);

        assert!(load_search_blob(&id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn db_session_loads_without_history_jsonl() {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join(TEST_SESSION_ID);
        fs::create_dir(&dir).expect("create session dir");
        let mut s = fixture_session();
        s.id = TEST_SESSION_ID.into();
        s.title = Some("DB".into());
        s.first_user_message = Some("hello sqlite".into());
        s.reasoning_effort = Some(ReasoningEffort::High);
        s.parent_id = Some("parent-session".into());
        s.session_cost_usd = 1.5;
        s.history.push(user_item("hello sqlite"));
        s.record_context_tokens(42, 1, test_context_identity());
        let mut db = smelt_store::SessionDb::open(dir.join("session.db")).unwrap();
        db.apply_session_commit(&initial_store_commit_from_session(&s).unwrap())
            .unwrap();

        let loaded = load_db_session_result(&dir)
            .expect("read db session")
            .expect("load db session");
        assert_eq!(loaded.id, TEST_SESSION_ID);
        assert_eq!(loaded.title.as_deref(), Some("DB"));
        assert_eq!(loaded.first_user_message.as_deref(), Some("hello sqlite"));
        assert_eq!(loaded.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(loaded.parent_id.as_deref(), Some("parent-session"));
        assert_eq!(loaded.context_tokens, Some(42));
        assert_eq!(loaded.display_context_tokens, Some(42));
        assert_eq!(loaded.session_cost_usd, 1.5);
        assert_eq!(loaded.history.len(), 1);
        assert!(!dir.join("history.jsonl").exists());
    }

    #[test]
    fn session_serializes_history_native_with_user_display() {
        let mut s = fixture_session();
        s.history.push(HistoryItem::User {
            content: Content::Text("expanded command body".into()),
            display: Some("/reflect".into()),
            command: true,
        });

        let json = serde_json::to_value(&s).expect("serialize session");
        assert_eq!(json["schema_version"], CURRENT_SESSION_SCHEMA_VERSION);
        assert!(json.get("messages").is_none());
        assert_eq!(json["history"][0]["display"], "/reflect");
        assert_eq!(json["history"][0]["command"], true);

        let loaded: Session = serde_json::from_value(json).expect("deserialize session");
        assert!(matches!(
            &loaded.history[0],
            HistoryItem::User {
                content,
                display: Some(display),
                command: true,
            } if content.text_content() == "expanded command body" && display == "/reflect"
        ));
    }

    #[test]
    fn unsupported_session_schema_version_is_rejected() {
        let json = serde_json::json!({
            "schema_version": 99,
            "id": "future",
            "history": []
        });

        let err = serde_json::from_value::<Session>(json).expect_err("future schema should fail");
        assert!(err
            .to_string()
            .contains("unsupported session schema version 99"));
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
                result: ToolOutcome {
                    content: "x".repeat(100),
                    is_error: false,
                    metadata: None,
                },
                elapsed_ms: None,
                called_at_ms: None,
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
                result: ToolOutcome {
                    content: "x".repeat(100),
                    is_error: false,
                    metadata: None,
                },
                elapsed_ms: None,
                called_at_ms: None,
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
                .map(|checkpoint| checkpoint.summary.as_str()),
            Some("second summary")
        );

        s.restore_rewindable_snapshots_after_rewind(4, false);

        assert_eq!(s.checkpoint_events.len(), 1);
        assert_eq!(s.checkpoint_events[0].summary, "first summary");
        assert_eq!(
            s.checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.as_str()),
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
            summary: "summary".to_string(),
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
            summary: "summary".to_string(),
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
        assert_eq!(s.current_context_tokens(), Some(100));
        assert_eq!(s.display_context_tokens, Some(100));

        s.history.push(user_item("c"));
        assert_eq!(s.current_context_tokens(), None);
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
    fn legacy_unknown_context_tokens_are_stale_for_display() {
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

    // ── session_updated_at ────────────────────────────────────────────

    #[test]
    fn session_updated_at_prefers_updated_falls_back_to_created() {
        let m = SessionMeta {
            id: "x".into(),
            title: None,
            slug: None,
            first_user_message: None,
            created_at_ms: 100,
            updated_at_ms: 200,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            cwd: None,
            parent_id: None,
            authoritative_context_tokens: None,
            display_context_tokens: None,
            history_len: None,
            checkpoint: None,
            checkpoint_events: Vec::new(),
            text_bytes: None,
        };
        assert_eq!(session_updated_at(&m), 200);
        let m2 = SessionMeta {
            updated_at_ms: 0,
            ..m
        };
        assert_eq!(session_updated_at(&m2), 100);
    }

    // ── externalize / internalize blobs ───────────────────────────────

    fn image_item(url: &str) -> HistoryItem {
        HistoryItem::User {
            content: Content::Parts(vec![ContentPart::ImageUrl {
                url: url.to_string(),
                label: None,
            }]),
            display: None,
            command: false,
        }
    }

    fn first_image_url(item: &HistoryItem) -> &str {
        match item {
            HistoryItem::User { content, .. } | HistoryItem::System { content } => match content {
                Content::Parts(parts) => match &parts[0] {
                    ContentPart::ImageUrl { url, .. } => url,
                    _ => panic!("expected image part"),
                },
                _ => panic!("expected parts content"),
            },
            _ => panic!("expected user/system item"),
        }
    }

    #[test]
    fn save_stores_attachment_with_canonical_commit_and_round_trips() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let data_url = "data:image/png;base64,AAAA";
        let mut session = Session::new(1, PathBuf::from("/tmp"));
        session.id = TEST_SESSION_ID.into();
        session.history = vec![image_item(data_url)];
        save(&session);

        let session_dir = dir_for(&session);
        assert!(!session_dir.join("blobs").exists());
        let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")).unwrap();
        assert_eq!(
            db.connection()
                .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.connection()
                .query_row("SELECT role FROM history_object_refs", [], |row| row
                    .get::<_, String>(0),)
                .unwrap(),
            "attachment_image"
        );
        let reader = smelt_store::SessionReader::open_existing(&session_dir).unwrap();
        let stored = reader.read_history_items_range(0..1).unwrap();
        assert_eq!(first_image_url(&stored[0]), data_url);
        let loaded = load_full_result(TEST_SESSION_ID).unwrap().unwrap();
        assert_eq!(first_image_url(&loaded.history[0]), data_url);
    }

    #[test]
    fn session_round_trips_through_wire_form_preserving_history_and_snapshots() {
        // Verify lossless save → load → save: native history rows,
        // snapshot keys, session cost, and context tokens all survive a
        // round-trip through the current `history: Vec<HistoryItem>`
        // on-disk JSON shape.
        let inv_ok = ToolInvocation {
            call_id: "c1".into(),
            name: "read".into(),
            arguments: "{\"p\":\"a\"}".into(),
            result: ToolOutcome {
                content: "ok".into(),
                is_error: false,
                metadata: None,
            },
            elapsed_ms: None,
            called_at_ms: Some(1_742_573_823_000),
        };
        let inv_err = ToolInvocation {
            call_id: "c2".into(),
            name: "write".into(),
            arguments: "{}".into(),
            result: ToolOutcome {
                content: "denied".into(),
                is_error: true,
                metadata: None,
            },
            elapsed_ms: None,
            called_at_ms: Some(1_742_573_824_000),
        };
        let mut original = Session::new(123, std::path::PathBuf::from("/w"));
        original.history.push(user_item("hi"));
        original.history.push(assistant_text_item("hello"));
        original
            .history
            .push(HistoryItem::Assistant(AssistantStep::with_invocations(
                Some(Content::Text("doing work".into())),
                None,
                Vec::new(),
                vec![inv_ok, inv_err],
            )));
        original.context_tokens = Some(200);
        original.context_tokens_history_len = Some(3);
        original.session_cost_usd = 1.25;
        original.session_usage.prompt_tokens = Some(10);
        original.session_usage.completion_tokens = Some(2);
        original.snapshot_context();

        let json = serde_json::to_string(&original).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let snapshot = &value["context_snapshots"][0][1];
        assert!(snapshot.get("cost_usd").is_none());
        assert!(snapshot.get("session_usage").is_none());
        let round: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(round.history, original.history);
        assert_eq!(
            round.context_snapshots.len(),
            original.context_snapshots.len()
        );
        assert_eq!(round.context_tokens, original.context_tokens);
        assert_eq!(
            round.context_tokens_history_len,
            original.context_tokens_history_len
        );
        assert_eq!(round.context_snapshots.len(), 1);
        assert_eq!(round.context_snapshots[0].0, 3);
        assert_eq!(round.context_snapshots[0].1.context_tokens, Some(200));
        assert_eq!(round.session_cost_usd, original.session_cost_usd);
        assert_eq!(round.id, original.id);
    }

    #[test]
    fn round_trip_preserves_invocation_timing() {
        let mut original = Session::new(7, std::path::PathBuf::from("/w"));
        original
            .history
            .push(HistoryItem::Assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "c1".into(),
                    name: "f".into(),
                    arguments: "{}".into(),
                    result: ToolOutcome {
                        content: "ok".into(),
                        is_error: false,
                        metadata: None,
                    },
                    elapsed_ms: Some(42),
                    called_at_ms: Some(1_742_573_823_000),
                }],
            )));
        let round: Session =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        let restored_inv = &round
            .history
            .iter()
            .find_map(|i| i.as_assistant())
            .unwrap()
            .invocations[0];
        assert_eq!(
            restored_inv.elapsed_ms,
            Some(42),
            "native history should preserve invocation elapsed telemetry"
        );
        assert_eq!(restored_inv.called_at_ms, Some(1_742_573_823_000));
    }

    #[test]
    fn staged_session_is_hidden_until_atomic_publication() {
        let state = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let mut session = fixture_session();
        session.id = TEST_SESSION_ID.into();
        let mut writer =
            smelt_store::OwnedSessionWriter::open(sessions_dir(), TEST_SESSION_ID).unwrap();
        let staged_path = writer.session_dir().to_path_buf();
        let destination = dir_for_id(TEST_SESSION_ID);
        writer
            .commit_session(&initial_store_commit_from_session(&session).unwrap())
            .unwrap();

        assert!(!destination.exists());
        let published = writer.publish().unwrap();

        assert_eq!(published, destination);
        assert!(!staged_path.exists());
        assert!(destination.join("session.db").is_file());
    }

    #[test]
    fn lifecycle_cleanup_removes_abandoned_but_not_active_staging() {
        let state = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let staging_root = sessions_dir().join(".staging");
        create_private_dir_all(&staging_root).unwrap();

        let mut abandoned = fixture_session();
        abandoned.id = TEST_SESSION_ID.into();
        let abandoned_path = staging_root.join(format!("{TEST_SESSION_ID}.1.0.orphan"));
        create_private_dir_all(&abandoned_path).unwrap();
        let mut abandoned_db =
            smelt_store::SessionDb::open(abandoned_path.join("session.db")).unwrap();
        abandoned_db
            .apply_session_commit(&initial_store_commit_from_session(&abandoned).unwrap())
            .unwrap();
        drop(abandoned_db);

        let active_id = crate::session_id::SessionId::parse(&numbered_session_id(2)).unwrap();
        let active =
            smelt_store::OwnedSessionWriter::open(sessions_dir(), active_id.as_str()).unwrap();
        let active_path = active.session_dir().to_path_buf();

        cleanup_abandoned_session_artifacts();

        assert!(!abandoned_path.exists());
        assert!(active_path.exists());
        active.release().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_cleanup_never_follows_storage_symlinks() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        create_private_dir_all(&sessions_dir()).unwrap();
        let external = tempfile::tempdir().unwrap();
        let sentinel = external.path().join("sentinel");
        fs::write(&sentinel, "keep").unwrap();
        symlink(external.path(), sessions_dir().join(".staging")).unwrap();
        symlink(external.path(), sessions_dir().join(".trash")).unwrap();
        symlink(external.path(), sessions_dir().join(TEST_SESSION_ID)).unwrap();

        cleanup_abandoned_session_artifacts();

        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    }
}
