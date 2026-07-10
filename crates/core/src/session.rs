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
    pub model: String,
    pub api_base: String,
    pub provider_type: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone)]
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
    ensure_session_db_read_only, ensure_session_db_writable, export_history_jsonl,
    export_requests_jsonl, SessionStoreError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAccessDecision {
    Owned,
    ReadOnly { reason: String },
}

#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub session: Session,
    pub access: SessionAccessDecision,
}

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
            checkpoint: w.checkpoint,
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
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub context_token_identity: Option<ContextTokenIdentity>,
    #[serde(default)]
    pub display_context_token_identity: Option<ContextTokenIdentity>,
    #[serde(default)]
    pub history_len: Option<usize>,
    #[serde(default)]
    pub checkpoint: Option<ContextCheckpoint>,
    /// Approximate text byte size (message bodies, reasoning, tool-call args).
    /// Populated in `meta.json` so the resume dialog avoids loading session history.
    #[serde(default)]
    pub text_bytes: Option<u64>,
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

    pub fn record_context_tokens(&mut self, tokens: u32, identity: ContextTokenIdentity) {
        let reading = ContextTokenReading {
            tokens,
            history_len: Some(self.history.len()),
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

    pub fn clear_context_snapshots(&mut self) {
        self.context_snapshots.clear();
    }

    pub fn restore_context_after_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) {
        let checkpoint_fallback =
            self.clear_checkpoint_for_rewind(hist_idx, keep_checkpoint_at_boundary);
        self.context_snapshots.truncate_after(hist_idx);
        self.restore_context_tokens_after_rewind(hist_idx, checkpoint_fallback);
    }

    pub fn prune_context_snapshots(&mut self, hist_idx: usize) {
        let checkpoint_fallback = self.clear_checkpoint_for_rewind(hist_idx, true);
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
        self.checkpoint.take().map(|cp| {
            (
                cp.pre_checkpoint_context_tokens,
                cp.pre_checkpoint_context_history_len,
            )
        })
    }

    pub fn model_history_range(&self, summary_prefix: &str) -> (Vec<HistoryItem>, usize, usize) {
        let end_index = self.history.len();
        let Some(cp) = &self.checkpoint else {
            return (Vec::new(), 0, end_index);
        };
        (
            vec![HistoryItem::user(protocol::Content::text(format!(
                "{}\n{}",
                summary_prefix.trim_end(),
                cp.summary
            )))],
            cp.first_live_index,
            end_index,
        )
    }

    pub fn model_history(&self, summary_prefix: &str) -> Vec<HistoryItem> {
        let (mut out, first_live_index, _) = self.model_history_range(summary_prefix);
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
        self.checkpoint = Some(ContextCheckpoint {
            kind,
            summary,
            first_live_index,
            created_at_ms: now_ms(),
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

    pub fn clear_checkpoint_if_rewound_to(&mut self, hist_idx: usize) {
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|cp| cp.first_live_index >= hist_idx)
        {
            if let Some(cp) = self.checkpoint.take() {
                match cp.pre_checkpoint_context_history_len {
                    Some(len) if len <= hist_idx => {
                        self.context_tokens = cp.pre_checkpoint_context_tokens;
                        self.context_tokens_history_len = Some(len);
                        self.context_token_identity = None;
                        self.display_context_tokens = cp.pre_checkpoint_context_tokens;
                        self.display_context_token_identity = None;
                    }
                    _ => self.clear_context_tokens(),
                }
            }
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

pub fn is_context_checkpoint_summary(item: &HistoryItem, summary_prefix: &str) -> bool {
    let HistoryItem::User { content, .. } = item else {
        return false;
    };
    content
        .text_content()
        .trim_start()
        .starts_with(summary_prefix.trim_end())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Save / Load / Delete ─────────────────────────────────────────────────────

pub fn dir_for(session: &Session) -> PathBuf {
    dir_for_id(&session.id)
}

pub fn dir_for_id(id: &str) -> PathBuf {
    sessions_dir().join(id)
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
}

pub fn save(session: &Session, store: &crate::attachment::AttachmentStore) {
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = store.save_blobs(&blob_dir);
    save_with_blobs(session, &url_to_blob);
}

/// Write the canonical SQLite session store. Assumes blobs are already flushed.
/// Safe to call from a background thread.
pub fn save_with_blobs(session: &Session, url_to_blob: &std::collections::HashMap<String, String>) {
    if let Err(err) = save_with_blobs_result(session, url_to_blob) {
        eprintln!("smelt: failed to save session {}: {err}", session.id);
    }
}

pub fn save_with_blobs_result(
    session: &Session,
    url_to_blob: &std::collections::HashMap<String, String>,
) -> Result<smelt_store::SaveReceipt, smelt_store::StoreError> {
    save_with_blobs_result_with_history_start(session, url_to_blob, 0)
}

pub fn save_with_blobs_result_with_history_start(
    session: &Session,
    url_to_blob: &std::collections::HashMap<String, String>,
    history_start_idx: usize,
) -> Result<smelt_store::SaveReceipt, smelt_store::StoreError> {
    let _perf = smelt_perf::perf::begin("session:write");
    let session_dir = dir_for(session);
    fs::create_dir_all(&session_dir)?;

    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.history, url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    let db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let current_state = db.session_state()?;
    let base_revision = current_state.as_ref().map_or(0, |state| state.revision);
    let base_history_len = current_state.as_ref().map_or(0, |state| state.history_len);
    let base_descriptor_len = db.transcript_descriptor_count()? as u64;
    let history_len = session_out.history.len();
    let history_start_idx = history_start_idx.min(history_len);
    let command = smelt_store::SessionCommit {
        session_id: session_out.id.clone(),
        save_id: smelt_store::SaveId::ZERO,
        base_revision: smelt_store::Revision::new(base_revision),
        base_history_len: smelt_store::HistoryLen::new(base_history_len),
        base_descriptor_len: smelt_store::DescriptorLen::new(base_descriptor_len),
        state: store_state_from_session(session_out.as_ref(), history_len)?,
        history: smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(history_start_idx as u64),
            final_len: smelt_store::HistoryLen::new(history_len as u64),
            items: session_out.history[history_start_idx..].to_vec(),
        },
        side_tables: store_side_table_suffixes_from_session_at(
            session_out.as_ref(),
            history_start_idx,
            history_len,
        )?,
        descriptors: None,
    };
    let receipt = db
        .commit_session(&command)
        .map_err(session_commit_failure_to_store_error)?;
    if let Err(err) = refresh_derived_files(&session_dir) {
        eprintln!(
            "smelt: failed to refresh derived files for session {}: {err}",
            session_out.id
        );
    }
    Ok(receipt)
}

pub fn store_snapshot_from_session(
    session: &Session,
    history_start_idx: usize,
) -> Result<smelt_store::SessionSnapshot, smelt_store::StoreError> {
    let history_start_idx = history_start_idx.min(session.history.len());
    Ok(smelt_store::SessionSnapshot {
        state: store_state_from_session(session, session.history.len())?,
        history_start_idx,
        history_len: session.history.len(),
        history: session.history[history_start_idx..].to_vec(),
        turn_metas: turn_meta_values_from(
            &session.turn_metas,
            history_start_idx,
            session.history.len(),
        )?,
        metadata_snapshots: snapshot_values_from(
            &session.metadata_snapshots,
            history_start_idx,
            session.history.len(),
        )?,
        context_snapshots: snapshot_values_from(
            &session.context_snapshots,
            history_start_idx,
            session.history.len(),
        )?,
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

pub fn store_state_from_session(
    session: &Session,
    history_len: usize,
) -> Result<smelt_store::SessionState, smelt_store::StoreError> {
    Ok(smelt_store::SessionState {
        id: session.id.clone(),
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
        parent_id: session.parent_id.clone(),
        accounting_json: Some(serde_json::to_value(context_snapshot_state_from_session(
            session,
        ))?),
        checkpoint_json: checkpoint_json_for_history_len(session.checkpoint.as_ref(), history_len)?,
        context_tokens: session.context_tokens.map(u64::from),
        context_tokens_history_len: session.context_tokens_history_len.map(|len| len as u64),
        display_context_tokens: session.display_context_tokens.map(u64::from),
        session_cost_usd: session.session_cost_usd,
        revision: 0,
        history_len: history_len as u64,
        created_at: session.created_at_ms as i64,
        updated_at: session.updated_at_ms as i64,
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
    // A turn completed exactly at the current boundary remains staged until a
    // subsequent history item makes it a valid `turn_idx` row in the store.
    snapshots
        .iter()
        .filter(|(idx, _)| *idx >= history_start_idx && *idx < history_len)
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

/// Write `contents` to `path` atomically via a tmp file + rename.
pub fn atomic_write(path: &std::path::Path, contents: &[u8], ts: u64) {
    let Some(dir) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let tmp = dir.join(format!("{name}.{ts}.tmp"));
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Load the full semantic session by exact ID or unique prefix (git-style short ID).
///
/// This materializes all history rows and should stay out of normal resume,
/// preview, render, search, save, and provider-dispatch hot paths.
pub fn load_full(id_or_prefix: &str) -> Option<Session> {
    let _perf = smelt_perf::perf::begin("session:load_full");
    let id = {
        let _perf = smelt_perf::perf::begin("session:load_full:resolve");
        resolve_prefix(id_or_prefix)?
    };
    load_full_exact(&id)
}

pub fn load_full_for_resume(id_or_prefix: &str) -> Option<LoadedSession> {
    let _perf = smelt_perf::perf::begin("session:load_full_for_resume");
    let id = {
        let _perf = smelt_perf::perf::begin("session:load_full:resolve");
        resolve_prefix(id_or_prefix)?
    };
    load_session_files_for_resume(&sessions_dir().join(id))
}

pub fn load_meta(id_or_prefix: &str) -> Option<SessionMeta> {
    let _perf = smelt_perf::perf::begin("session:load_meta");
    load_meta_for_prepared_dir(prepare_session_dir_for_read(id_or_prefix)?)
}

pub fn resolve_session_dir_for_read(id_or_prefix: &str) -> Option<ResolvedSessionDir> {
    let id = resolve_prefix(id_or_prefix)?;
    let dir = dir_for_id(&id);
    let kind = session_dir_kind(&dir)?;
    Some(ResolvedSessionDir { id, dir, kind })
}

pub fn prepare_session_dir_for_read(id_or_prefix: &str) -> Option<PathBuf> {
    let resolved = resolve_session_dir_for_read(id_or_prefix)?;
    if writer_lease_conflict_for_session_dir(&resolved.dir).is_some() {
        return Some(resolved.dir);
    }
    if let Err(err) = crate::session_store::ensure_session_db_read_only(&resolved.dir) {
        log_session_open_error(&resolved.dir, &err);
        return None;
    }
    Some(resolved.dir)
}

pub fn load_store_header(id_or_prefix: &str) -> Option<(SessionHeader, SessionStoreRef)> {
    let resolved = resolve_session_dir_for_read(id_or_prefix)?;
    load_store_header_for_dir(resolved.dir)
}

pub fn load_store_header_for_dir(session_dir: PathBuf) -> Option<(SessionHeader, SessionStoreRef)> {
    let db_path = session_dir.join("session.db");
    // COMPAT(transcript-descriptor-history-link-mismatch): repair broken
    // descriptor/history links before sparse resume builds its tail window.
    // COMPAT(session-checkpoint-live-index-past-history): repair impossible
    // checkpoint live starts before constructing store-backed model history.
    if let Ok(db) = smelt_store::SessionDb::open(&db_path) {
        let _ = db.repair_mismatched_transcript_descriptor_history_links();
        let _ = db.repair_checkpoint_first_live_index_past_history();
    }
    let db = smelt_store::SessionDb::open_read_only(&db_path).ok()?;
    let state = db.session_state().ok()??;
    let meta = load_meta_from_db(&session_dir)?;
    let history_len = state.history_len as usize;
    let header = SessionHeader {
        meta,
        history_len,
        revision: state.revision,
    };
    Some((
        header,
        SessionStoreRef {
            session_dir,
            db_path,
        },
    ))
}

fn session_dir_kind(dir: &Path) -> Option<SessionDirKind> {
    dir.join("session.db")
        .is_file()
        .then_some(SessionDirKind::Store)
}

pub fn load_meta_for_prepared_dir(dir: PathBuf) -> Option<SessionMeta> {
    load_meta_for_dir(dir, MetaLoadMode::Full)
}

fn load_full_exact(id: &str) -> Option<Session> {
    let _perf = smelt_perf::perf::begin("session:load_full:exact");
    load_session_files(&sessions_dir().join(id))
}

fn load_session_files(dir_path: &std::path::Path) -> Option<Session> {
    if let Err(err) = crate::session_store::ensure_session_db_read_only(dir_path) {
        log_session_open_error(dir_path, &err);
        return None;
    }
    load_db_session(dir_path).and_then(|session| internalize_session_blobs(dir_path, session))
}

fn writer_lease_conflict_for_session_dir(dir_path: &Path) -> Option<SessionAccessDecision> {
    let db_path = dir_path.join("session.db");
    if !db_path.is_file() {
        return None;
    }
    let db = smelt_store::SessionDb::open_read_only(&db_path).ok()?;
    let lease = db.active_writer_lease_for_current_process().ok()??;
    Some(SessionAccessDecision::ReadOnly {
        reason: format!(
            "session has active writer lease from pid {} on {}",
            lease.pid, lease.hostname
        ),
    })
}

pub fn claim_access_for_session_dir(dir_path: &Path) -> SessionAccessDecision {
    let db_path = dir_path.join("session.db");
    if !db_path.is_file() {
        return SessionAccessDecision::Owned;
    }
    if let Some(access) = writer_lease_conflict_for_session_dir(dir_path) {
        return access;
    }
    match smelt_store::SessionDb::open(&db_path)
        .and_then(|db| db.acquire_current_process_writer_lease())
    {
        Ok(_) => SessionAccessDecision::Owned,
        Err(err) => SessionAccessDecision::ReadOnly {
            reason: err.to_string(),
        },
    }
}

fn load_session_files_for_resume(dir_path: &std::path::Path) -> Option<LoadedSession> {
    if let Some(access) = writer_lease_conflict_for_session_dir(dir_path) {
        let session = load_db_session(dir_path)
            .and_then(|session| internalize_session_blobs(dir_path, session))?;
        return Some(LoadedSession { session, access });
    }

    if let Err(err) = crate::session_store::ensure_session_db_writable(dir_path) {
        log_session_open_error(dir_path, &err);
        return None;
    }
    let access = claim_access_for_session_dir(dir_path);
    let session = load_db_session(dir_path)
        .and_then(|session| internalize_session_blobs(dir_path, session))?;
    Some(LoadedSession { session, access })
}

fn log_session_open_error(dir_path: &std::path::Path, err: &SessionStoreError) {
    engine::log::entry(
        engine::log::Level::Warn,
        "session_open_failed",
        &serde_json::json!({
            "session_dir": dir_path.display().to_string(),
            "error": err.to_string(),
        }),
    );
}

fn internalize_session_blobs(dir_path: &std::path::Path, mut session: Session) -> Option<Session> {
    let blob_dir = dir_path.join("blobs");
    if blob_dir.is_dir() {
        let blob_to_url = {
            let _perf = smelt_perf::perf::begin("session:load_full:read_blobs");
            crate::attachment::AttachmentStore::load_blobs(&blob_dir)
        };
        smelt_perf::perf::record_value("session:load_full:blobs", blob_to_url.len() as u64);
        if !blob_to_url.is_empty() {
            let _perf = smelt_perf::perf::begin("session:load_full:internalize_blobs");
            internalize_blobs(&mut session.history, &blob_to_url);
        }
    }
    Some(session)
}

fn load_db_session(dir_path: &std::path::Path) -> Option<Session> {
    let db_path = dir_path.join("session.db");
    if !db_path.is_file() {
        return None;
    }
    let db = smelt_store::SessionDb::open_read_only(&db_path).ok()?;
    let mut snapshot = db.load_full_session_snapshot().ok()??;
    let history = std::mem::take(&mut snapshot.history);
    Some(session_from_store_snapshot(snapshot, history))
}

fn session_from_store_snapshot(
    snapshot: smelt_store::SessionSnapshot,
    history: Vec<HistoryItem>,
) -> Session {
    let state = snapshot.state;
    let turn_metas = snapshots_from_values(snapshot.turn_metas).unwrap_or_default();
    let metadata_snapshots = snapshots_from_values(snapshot.metadata_snapshots).unwrap_or_default();
    let context_snapshots = snapshots_from_values(snapshot.context_snapshots).unwrap_or_default();
    let context_state = context_snapshot_state_from_json(state.accounting_json.clone());
    let session_usage = context_state.session_usage.clone();
    let context_token_identity = context_state.context_token_identity;
    let display_context_token_identity = context_state
        .display_context_token_identity
        .or_else(|| context_token_identity.clone());
    let checkpoint = checkpoint_from_json(state.checkpoint_json.clone(), history.len());
    let context_tokens = state
        .context_tokens
        .and_then(|tokens| u32::try_from(tokens).ok());
    Session {
        id: state.id,
        title: state.title,
        slug: state.slug,
        first_user_message: state.first_user_message,
        metadata_snapshots,
        created_at_ms: state.created_at as u64,
        updated_at_ms: state.updated_at as u64,
        mode: state.mode,
        reasoning_effort: state
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse),
        model: state.model,
        fast_mode: state.fast_mode,
        cwd: state.cwd,
        parent_id: state.parent_id,
        history,
        checkpoint,
        context_tokens,
        context_tokens_history_len: state
            .context_tokens_history_len
            .and_then(|len| usize::try_from(len).ok()),
        context_token_identity,
        display_context_tokens: state
            .display_context_tokens
            .and_then(|tokens| u32::try_from(tokens).ok())
            .or(context_tokens),
        display_context_token_identity,
        turn_metas,
        context_snapshots,
        session_cost_usd: state.session_cost_usd,
        session_usage,
    }
}

fn snapshots_from_values<T: for<'de> Deserialize<'de>>(
    rows: Vec<(u64, Value)>,
) -> Option<HistorySnapshots<T>> {
    rows.into_iter()
        .map(|(idx, value)| {
            serde_json::from_value(value)
                .ok()
                .map(|value| (idx as usize, value))
        })
        .collect::<Option<Vec<_>>>()
        .map(HistorySnapshots::from_vec)
}

pub fn backfill_transcript_descriptor_records_from_history_range(
    session_dir: &Path,
    history_range: std::ops::Range<usize>,
    descriptor_start_idx: usize,
    block_start_idx: u64,
) -> Result<usize, smelt_store::StoreError> {
    let db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let history = db.read_history_items_range(history_range.clone())?;
    let records = transcript_descriptor_records_from_history_items(
        history_range.start,
        block_start_idx,
        &history,
        &std::collections::HashMap::new(),
    )?;
    let written = records.len();
    db.replace_transcript_descriptor_suffix_for_repair(descriptor_start_idx, &records)?;
    Ok(written)
}

pub fn backfill_transcript_descriptors_in_history_chunks(
    session_dir: &Path,
    chunk_items: usize,
    max_chunks: Option<usize>,
) -> Result<usize, smelt_store::StoreError> {
    let chunk_items = chunk_items.max(1);
    let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db"))?;
    let history_len = db.history_item_count()?;
    let mut history_start = db
        .transcript_descriptor_max_history_idx()?
        .map_or(0, |idx| idx.saturating_add(1))
        .min(history_len);
    drop(db);

    let mut total_written = 0usize;
    let mut chunks = 0usize;
    while history_start < history_len && max_chunks.is_none_or(|max| chunks < max) {
        let history_end = history_start.saturating_add(chunk_items).min(history_len);
        let descriptor_start = {
            let db = smelt_store::SessionDb::open_read_only(session_dir.join("session.db"))?;
            db.transcript_descriptor_count()?
        };
        let written = backfill_transcript_descriptor_records_from_history_range(
            session_dir,
            history_start..history_end,
            descriptor_start,
            descriptor_start as u64,
        )?;
        if written == 0 {
            break;
        }
        total_written += written;
        history_start = history_end;
        chunks += 1;
        smelt_perf::perf::record_value("session:descriptor_backfill:chunks", chunks as u64);
        smelt_perf::perf::record_value("session:descriptor_backfill:records", total_written as u64);
    }
    Ok(total_written)
}

fn transcript_descriptor_records_from_history_items(
    first_history_idx: usize,
    block_start_idx: u64,
    history: &[HistoryItem],
    tool_elapsed: &std::collections::HashMap<String, u64>,
) -> Result<Vec<smelt_store::TranscriptDescriptorRecord>, smelt_store::StoreError> {
    let mut records = Vec::new();
    for (offset, item) in history.iter().enumerate() {
        push_history_item_descriptor_rows(
            &mut records,
            first_history_idx + offset,
            item,
            tool_elapsed,
        )?;
    }
    for (offset, record) in records.iter_mut().enumerate() {
        record.block_idx = block_start_idx.saturating_add(offset as u64);
    }
    Ok(records)
}

fn push_history_item_descriptor_rows(
    records: &mut Vec<smelt_store::TranscriptDescriptorRecord>,
    history_idx: usize,
    item: &HistoryItem,
    tool_elapsed: &std::collections::HashMap<String, u64>,
) -> Result<(), smelt_store::StoreError> {
    let origin = Some(crate::transcript_model::BlockOrigin::History(history_idx));
    match item {
        HistoryItem::User { content, display } => {
            let text = content.text_content();
            let descriptor =
                if let Some(rest) = text.strip_prefix(engine::SUMMARY_PREFIX.trim_end()) {
                    crate::transcript_model::TranscriptBlockDescriptor::Compacted {
                        summary: rest.trim_start_matches('\n').to_string(),
                    }
                } else if let Some(note) = text.strip_prefix(protocol::MODE_NOTE_PREFIX) {
                    crate::transcript_model::TranscriptBlockDescriptor::Mode {
                        text: note.trim().to_string(),
                        icon: String::new(),
                        hl_group: "SmeltAccent".to_string(),
                    }
                } else if let Some(note) = text.strip_prefix(protocol::PROCESS_STATUS_NOTE_PREFIX) {
                    crate::transcript_model::TranscriptBlockDescriptor::ProcessStatus {
                        text: note.trim().to_string(),
                        event: None,
                    }
                } else {
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
                    crate::transcript_model::TranscriptBlockDescriptor::User {
                        text: display_text,
                        image_labels,
                    }
                };
            records.push(transcript_descriptor_record(
                records.len(),
                descriptor,
                origin,
                None,
            )?);
        }
        HistoryItem::Assistant(turn) => {
            if let Some(reasoning) = turn.reasoning.as_ref().filter(|text| !text.is_empty()) {
                records.push(transcript_descriptor_record(
                    records.len(),
                    crate::transcript_model::TranscriptBlockDescriptor::Thinking {
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
                records.push(transcript_descriptor_record(
                    records.len(),
                    crate::transcript_model::TranscriptBlockDescriptor::Text {
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
                let elapsed_ms = inv
                    .elapsed_ms
                    .or_else(|| tool_elapsed.get(&inv.call_id).copied());
                let tool_state = crate::transcript_model::ToolState {
                    status,
                    elapsed: elapsed_ms.map(std::time::Duration::from_millis),
                    output: Some(Box::new(crate::transcript_model::ToolOutput {
                        content: inv.result.content.clone(),
                        is_error: inv.result.is_error,
                        metadata: inv.result.metadata.clone(),
                    })),
                    user_message: None,
                    preview_output: None,
                };
                records.push(transcript_descriptor_record(
                    records.len(),
                    crate::transcript_model::TranscriptBlockDescriptor::ToolCall {
                        call_id: inv.call_id.clone(),
                        name: inv.name.clone(),
                        summary: crate::mcp::args_summary(&args),
                        args,
                    },
                    origin,
                    Some((inv.call_id.clone(), tool_state)),
                )?);
            }
        }
        HistoryItem::Note(note) => match note.kind() {
            protocol::HistoryNoteKind::Context => {}
            protocol::HistoryNoteKind::ModeChange => records.push(transcript_descriptor_record(
                records.len(),
                crate::transcript_model::TranscriptBlockDescriptor::Mode {
                    text: note.text().to_string(),
                    icon: String::new(),
                    hl_group: "SmeltAccent".to_string(),
                },
                origin,
                None,
            )?),
            protocol::HistoryNoteKind::ProcessStatus => records.push(transcript_descriptor_record(
                records.len(),
                crate::transcript_model::TranscriptBlockDescriptor::ProcessStatus {
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

fn transcript_descriptor_record(
    descriptor_idx: usize,
    descriptor: crate::transcript_model::TranscriptBlockDescriptor,
    origin: Option<crate::transcript_model::BlockOrigin>,
    tool_state: Option<(String, crate::transcript_model::ToolState)>,
) -> Result<smelt_store::TranscriptDescriptorRecord, smelt_store::StoreError> {
    let content_hash = crate::utils::hash_serializable(&descriptor);
    let record = crate::transcript_model::TranscriptBlockRecord {
        descriptor,
        content_hash,
        origin,
        tool_state,
    };
    crate::transcript_model::transcript_descriptor_row(descriptor_idx, &record)
}

/// Returns `None` when no match or prefix is ambiguous.
pub(crate) fn resolve_prefix(prefix: &str) -> Option<String> {
    let dir = sessions_dir();

    if dir.join(prefix).join("session.db").is_file() {
        return Some(prefix.to_string());
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("session.db").is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(prefix) {
            matches.push(name_str.to_string());
        }
    }
    if matches.len() == 1 {
        Some(matches.into_iter().next().unwrap())
    } else {
        None
    }
}

pub fn delete(id: &str) {
    let session_dir = sessions_dir().join(id);
    if session_dir.is_dir() {
        let _ = fs::remove_dir_all(&session_dir);
    }
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let _perf = smelt_perf::perf::begin("session:list");
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let paths: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            p.is_dir().then_some(p)
        })
        .collect();
    let mut out = crate::utils::parallel_filter_map(paths, |path| {
        load_meta_for_dir(path, MetaLoadMode::List)
    });
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetaLoadMode {
    /// Listing path. Prefer sidecars for speed, but regenerate from SQLite when missing.
    List,
    /// Exact load path. Fill fields required to resume display-only sessions.
    Full,
}

fn load_meta_for_dir(path: PathBuf, mode: MetaLoadMode) -> Option<SessionMeta> {
    if !path.join("session.db").is_file() {
        return None;
    }
    let meta = if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
        load_meta_sidecar(&path, &contents, mode)
    } else {
        None
    };
    if let Some(meta) = meta {
        return Some(meta);
    }
    let meta = load_meta_from_db(&path)?;
    write_meta(&path, &meta);
    Some(meta)
}

fn load_meta_sidecar(path: &Path, contents: &str, mode: MetaLoadMode) -> Option<SessionMeta> {
    let mut meta = serde_json::from_str::<SessionMeta>(contents).ok()?;
    if mode == MetaLoadMode::List {
        return Some(meta);
    }

    let sidecar_fields = serde_json::from_str::<Value>(contents)
        .ok()
        .and_then(|value| value.as_object().cloned());
    let checkpoint_field_present = sidecar_fields
        .as_ref()
        .is_some_and(|object| object.contains_key("checkpoint"));
    let token_identity_fields_present = sidecar_fields.as_ref().is_some_and(|object| {
        object.contains_key("context_token_identity")
            && object.contains_key("display_context_token_identity")
    });
    if meta.text_bytes.is_none() {
        backfill_text_bytes(path, &mut meta);
    }
    if enrich_meta_from_db(
        path,
        &mut meta,
        checkpoint_field_present,
        token_identity_fields_present,
    ) {
        write_meta(path, &meta);
    }
    Some(meta)
}

fn enrich_meta_from_db(
    path: &Path,
    meta: &mut SessionMeta,
    checkpoint_field_present: bool,
    token_identity_fields_present: bool,
) -> bool {
    if meta.history_len.is_some() && checkpoint_field_present && token_identity_fields_present {
        return false;
    }
    let db_path = path.join("session.db");
    let Ok(db) = smelt_store::SessionDb::open_read_only(&db_path) else {
        return false;
    };
    let Ok(Some(state)) = db.session_state() else {
        return false;
    };
    let mut changed = false;
    if meta.history_len.is_none() {
        meta.history_len = Some(state.history_len as usize);
        changed = true;
    }
    if !checkpoint_field_present {
        meta.checkpoint = checkpoint_from_json(state.checkpoint_json, state.history_len as usize);
        changed = true;
    }
    let context_state = context_snapshot_state_from_json(state.accounting_json);
    let context_token_identity = context_state.context_token_identity;
    let display_context_token_identity = context_state
        .display_context_token_identity
        .or_else(|| context_token_identity.clone());
    if !token_identity_fields_present {
        changed = true;
    }
    if meta.context_token_identity.is_none() && context_token_identity.is_some() {
        meta.context_token_identity = context_token_identity;
        changed = true;
    }
    if meta.display_context_token_identity.is_none() && display_context_token_identity.is_some() {
        meta.display_context_token_identity = display_context_token_identity;
        changed = true;
    }
    changed
}

pub fn refresh_derived_files(dir_path: &Path) -> Result<bool, String> {
    let meta_written = write_db_meta_sidecar(dir_path)?;
    let db_path = dir_path.join("session.db");
    if db_path.is_file() {
        let db = smelt_store::SessionDb::open_read_only(&db_path).map_err(|err| err.to_string())?;
        let blob = db.search_blob().map_err(|err| err.to_string())?;
        atomic_write(&dir_path.join("content.txt"), blob.as_bytes(), now_ms());
    }
    Ok(meta_written)
}

pub fn write_db_meta_sidecar(dir_path: &Path) -> Result<bool, String> {
    let Some(meta) = load_meta_from_db(dir_path) else {
        return Ok(false);
    };
    let json = serde_json::to_vec_pretty(&meta).map_err(|err| err.to_string())?;
    fs::write(dir_path.join("meta.json"), json).map_err(|err| err.to_string())?;
    Ok(true)
}

fn load_meta_from_db(path: &Path) -> Option<SessionMeta> {
    let db_path = path.join("session.db");
    if !db_path.is_file() {
        return None;
    }
    let db = smelt_store::SessionDb::open_read_only(&db_path).ok()?;
    let state = db.session_state().ok()??;
    let text_bytes = Some(db.history_text_bytes().ok()?);
    let retained_history_len = db
        .history_item_count()
        .ok()?
        .min(state.history_len as usize);
    let checkpoint = checkpoint_from_json(state.checkpoint_json.clone(), retained_history_len);
    let context_state = context_snapshot_state_from_json(state.accounting_json.clone());
    let context_token_identity = context_state.context_token_identity;
    let display_context_token_identity = context_state
        .display_context_token_identity
        .or_else(|| context_token_identity.clone());
    Some(SessionMeta {
        id: state.id,
        title: state.title,
        slug: state.slug,
        first_user_message: state.first_user_message,
        created_at_ms: state.created_at as u64,
        updated_at_ms: state.updated_at as u64,
        mode: state.mode,
        reasoning_effort: state
            .reasoning_effort
            .as_deref()
            .and_then(ReasoningEffort::parse),
        model: state.model,
        fast_mode: state.fast_mode,
        cwd: state.cwd,
        parent_id: state.parent_id,
        context_tokens: state
            .display_context_tokens
            .or(state.context_tokens)
            .and_then(|tokens| u32::try_from(tokens).ok()),
        context_token_identity,
        display_context_token_identity,
        history_len: Some(state.history_len as usize),
        checkpoint,
        text_bytes,
    })
}

fn backfill_text_bytes(session_dir: &std::path::Path, meta: &mut SessionMeta) {
    let Ok(db) = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")) else {
        return;
    };
    let Ok(bytes) = db.history_text_bytes() else {
        return;
    };
    meta.text_bytes = Some(bytes);
    write_meta(session_dir, meta);
}

/// Read the searchable text blob for `id`.
///
/// COMPAT(session-search-sidecar-missing): reads canonical SQLite search text,
/// then falls back to `content.txt` for sessions that predate generated caches.
pub fn load_search_blob(id: &str) -> Option<String> {
    let _perf = smelt_perf::perf::begin("session:load_search_blob");
    let session_dir = sessions_dir().join(id);
    if let Ok(db) = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")) {
        if let Ok(blob) = db.search_blob() {
            atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), now_ms());
            return Some(blob);
        }
    }
    if let Ok(contents) = fs::read_to_string(session_dir.join("content.txt")) {
        return Some(contents);
    }
    if let Err(err) = crate::session_store::ensure_session_db_read_only(&session_dir) {
        log_session_open_error(&session_dir, &err);
        return None;
    }
    if let Ok(db) = smelt_store::SessionDb::open_read_only(session_dir.join("session.db")) {
        if let Ok(blob) = db.search_blob() {
            atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), now_ms());
            return Some(blob);
        }
    }
    None
}

/// Parallel batch read of search blobs. Returns `(id, blob)` pairs; missing
/// sessions are silently dropped. Output order is not stable.
pub fn load_search_blobs(ids: Vec<String>) -> Vec<(String, String)> {
    let _perf = smelt_perf::perf::begin("session:load_search_blobs");
    crate::utils::parallel_filter_map(ids, |id| load_search_blob(&id).map(|b| (id, b)))
}

fn write_meta(session_dir: &std::path::Path, meta: &SessionMeta) {
    if let Ok(json) = serde_json::to_string(meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), now_ms());
    }
}

/// Walk every image-url part in a history item's content, applying `swap`.
fn rewrite_image_urls<F: Fn(&mut String)>(content: &mut protocol::Content, swap: &F) {
    if let protocol::Content::Parts(parts) = content {
        for part in parts {
            if let protocol::ContentPart::ImageUrl { url, .. } = part {
                swap(url);
            }
        }
    }
}

fn rewrite_history_image_urls<F: Fn(&mut String)>(history: &mut [HistoryItem], swap: F) {
    for item in history {
        match item {
            HistoryItem::System { content } | HistoryItem::User { content, .. } => {
                rewrite_image_urls(content, &swap);
            }
            HistoryItem::Assistant(turn) => {
                if let Some(c) = turn.content.as_mut() {
                    rewrite_image_urls(c, &swap);
                }
            }
            HistoryItem::Note(_) => {}
        }
    }
}

pub fn externalize_blobs(
    history: &mut [HistoryItem],
    url_to_blob: &std::collections::HashMap<String, String>,
) {
    rewrite_history_image_urls(history, |url| {
        if let Some(blob_ref) = url_to_blob.get(url.as_str()) {
            *url = blob_ref.clone();
        }
    });
}

fn internalize_blobs(
    history: &mut [HistoryItem],
    blob_to_url: &std::collections::HashMap<String, String>,
) {
    rewrite_history_image_urls(history, |url| {
        if let Some(data_url) = blob_to_url.get(url.as_str()) {
            *url = data_url.clone();
        }
    });
}

fn session_updated_at(meta: &SessionMeta) -> u64 {
    if meta.updated_at_ms > 0 {
        meta.updated_at_ms
    } else {
        meta.created_at_ms
    }
}

pub(crate) fn sessions_dir() -> PathBuf {
    config::state_dir().join("sessions")
}

fn new_session_id(now_ms: u64, pid: u32) -> String {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(now_ms.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    format!("{:x}", hasher.finalize())
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

    #[test]
    fn delete_currently_accepts_absolute_and_parent_traversal_paths() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let sessions = sessions_dir();
        fs::create_dir_all(&sessions).expect("create sessions dir");

        let absolute_target = state.path().join("absolute-target");
        fs::create_dir_all(&absolute_target).expect("create absolute target");
        fs::write(absolute_target.join("sentinel"), "keep").expect("write absolute sentinel");
        delete(absolute_target.to_str().expect("utf-8 temp path"));
        assert!(
            !absolute_target.exists(),
            "an unchecked absolute id currently escapes the sessions root"
        );

        let parent_target = sessions.parent().unwrap().join("parent-target");
        fs::create_dir_all(&parent_target).expect("create parent target");
        fs::write(parent_target.join("sentinel"), "keep").expect("write parent sentinel");
        delete("../parent-target");
        assert!(
            !parent_target.exists(),
            "an unchecked parent id currently escapes the sessions root"
        );
    }

    #[test]
    fn delete_currently_removes_valid_exact_id_but_not_its_unique_prefix() {
        let state = tempfile::tempdir().expect("state dir");
        let _guard = crate::test_util::isolate_xdg_state(state.path());
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let dir = sessions_dir().join(id);
        fs::create_dir_all(&dir).expect("create valid session dir");
        fs::write(dir.join("session.db"), "fixture").expect("write fixture database");

        delete("01234567");
        assert!(
            dir.exists(),
            "delete currently does not resolve unique prefixes"
        );
        delete(id);
        assert!(!dir.exists(), "valid exact ids are deleted");
    }

    use protocol::{AssistantStep, Content, ContentPart, HistoryItem, ToolInvocation, ToolOutcome};

    fn user_item(text: &str) -> HistoryItem {
        HistoryItem::User {
            content: Content::Text(text.into()),
            display: None,
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
            model: "test-model".into(),
            api_base: "https://test.example".into(),
            provider_type: "test-provider".into(),
        }
    }

    #[test]
    fn finish_turn_state_records_turn_meta_and_rewindable_context() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![user_item("hello")];
        session.record_context_tokens(123, test_context_identity());
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: true,
            tool_elapsed: std::collections::HashMap::new(),
        };

        session.finish_turn_state(7, meta, true, true);

        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
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
    fn load_store_header_for_dir_repairs_mismatched_transcript_descriptor_history_links() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "repair-mismatched-descriptor-history-link".into();
        s.history
            .push(HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            )));
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        db.save_session_snapshot_for_import(&store_snapshot_from_session(&s, 0).unwrap())
            .unwrap();
        db.replace_transcript_descriptor_records_for_repair(&[
            smelt_store::TranscriptDescriptorRecord {
                block_idx: 0,
                history_idx: Some(0),
                kind: "user".to_string(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "bad-user-link".to_string(),
                estimated_text_bytes: "continue".len() as u64,
                preview_text: "continue".to_string(),
                indexed_text: "continue".to_string(),
                descriptor_json: serde_json::json!({
                    "kind": "user",
                    "text": "continue",
                    "image_labels": [],
                })
                .to_string(),
                origin_json: Some(serde_json::json!({ "History": 0 }).to_string()),
                tool_state_json: None,
            },
        ])
        .unwrap();
        drop(db);

        let (_header, _store_ref) = load_store_header_for_dir(dir.path().to_path_buf())
            .expect("store header loads after repair");

        let db = smelt_store::SessionDb::open_read_only(dir.path().join("session.db")).unwrap();
        let rows = db.read_all_transcript_descriptor_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].history_idx, None);
        assert_eq!(rows[0].origin_json, None);
    }

    #[test]
    fn store_state_drops_checkpoint_past_target_history_len() {
        let mut s = fixture_session();
        s.history = vec![user_item("kept")];
        s.checkpoint = Some(checkpoint("stale summary", 3));

        let state = store_state_from_session(&s, 1).unwrap();

        assert_eq!(state.history_len, 1);
        assert!(state.checkpoint_json.is_none());
    }

    #[test]
    fn load_store_header_for_dir_repairs_checkpoint_first_live_index_past_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "repair-checkpoint-live-index".into();
        s.history = vec![user_item("old prompt"), assistant_text_item("recent reply")];
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        db.save_session_snapshot_for_import(&store_snapshot_from_session(&s, 0).unwrap())
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

        let (header, store_ref) = load_store_header_for_dir(dir.path().to_path_buf())
            .expect("store header loads after repair");

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
        match live.model_history_source("SUMMARY:", checkpoint.as_ref()) {
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

        let db = smelt_store::SessionDb::open_read_only(dir.path().join("session.db")).unwrap();
        let repaired = db
            .session_state()
            .unwrap()
            .unwrap()
            .checkpoint_json
            .unwrap();
        assert_eq!(repaired["first_live_index"].as_u64(), Some(0));
    }

    #[test]
    fn list_sessions_uses_sidecar_metadata_and_regenerates_missing_cache() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut s = fixture_session();
            s.id = format!("stale-meta-{i}");
            s.title = Some(format!("stale meta {i}"));
            s.updated_at_ms = 1_700_000_000_000 + i as u64;
            s.history.push(user_item(&format!("prompt {i}")));
            save(&s, &crate::attachment::AttachmentStore::new());

            let meta_path = dir_for(&s).join("meta.json");
            let mut meta_json: Value =
                serde_json::from_str(&fs::read_to_string(&meta_path).expect("read meta"))
                    .expect("parse meta");
            let object = meta_json.as_object_mut().expect("meta object");
            object.remove("history_len");
            object.remove("checkpoint");
            object.remove("text_bytes");
            if i == 2 {
                fs::remove_file(&meta_path).expect("remove meta sidecar");
            } else {
                fs::write(
                    &meta_path,
                    serde_json::to_vec(&meta_json).expect("encode stale meta"),
                )
                .expect("write stale meta");
            }
            ids.push(s.id);
        }

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let listed = list_sessions();
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        for id in ids.iter().take(2) {
            let meta = listed
                .iter()
                .find(|meta| meta.id == *id)
                .expect("listed meta");
            assert_eq!(meta.history_len, None);
            assert!(meta.checkpoint.is_none());
            assert_eq!(meta.text_bytes, None);
        }
        let missing_sidecar_meta = listed
            .iter()
            .find(|meta| meta.id == ids[2])
            .expect("db-only session is listed from sqlite");
        assert_eq!(missing_sidecar_meta.history_len, Some(1));
        assert!(missing_sidecar_meta
            .text_bytes
            .is_some_and(|bytes| bytes > 0));
        assert!(dir_for_id(&ids[2]).join("meta.json").is_file());
        let read_only_count = snapshot
            .durations
            .iter()
            .find(|row| row.label == "store:db:open_read_only")
            .map(|row| row.count)
            .unwrap_or(0);
        assert!(
            read_only_count > 0,
            "list_sessions should open sqlite when regenerating missing metadata"
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
    fn resolve_session_dir_for_read_ignores_directories_without_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let stale_dir = sessions_dir().join("legacy-resolve");
        fs::create_dir_all(&stale_dir).expect("create stale session dir");
        fs::write(stale_dir.join("session.json"), "{}").expect("write stale marker");

        let mut session = fixture_session();
        session.id = "store-header-resolve".into();
        session.history.push(user_item("hello"));
        save(&session, &crate::attachment::AttachmentStore::new());

        assert!(resolve_session_dir_for_read("legacy-res").is_none());
        assert!(!stale_dir.join("session.db").exists());

        let store = resolve_session_dir_for_read("store-header").expect("resolve store prefix");
        assert_eq!(store.id, session.id);
        assert_eq!(store.kind, SessionDirKind::Store);

        let (header, store_ref) = load_store_header("store-header").expect("load store header");
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

        let id = "legacy-prepare-no-db";
        let dir = stale_session_dir_without_db(id);

        assert!(prepare_session_dir_for_read(id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn load_meta_does_not_create_missing_session_db() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = "legacy-meta-no-db";
        let dir = stale_session_dir_without_db(id);

        assert!(load_meta(id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn list_sessions_ignores_directory_without_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = "legacy-list-no-db";
        let dir = stale_session_dir_without_db(id);

        assert!(list_sessions().is_empty());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn load_search_blob_does_not_create_missing_session_db() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = "legacy-search-no-db";
        let dir = stale_session_dir_without_db(id);

        assert!(load_search_blob(id).is_none());
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn load_search_blob_uses_content_sidecar_without_database() {
        let state = tempfile::tempdir().expect("state dir");
        let _g = crate::test_util::isolate_xdg_state(state.path());

        let id = "legacy-search-sidecar";
        let dir = stale_session_dir_without_db(id);
        fs::write(dir.join("content.txt"), "cached search text").expect("write search sidecar");

        assert_eq!(load_search_blob(id).as_deref(), Some("cached search text"));
        assert!(!dir.join("session.db").exists());
    }

    #[test]
    fn db_session_loads_without_history_jsonl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "db-session".into();
        s.title = Some("DB".into());
        s.first_user_message = Some("hello sqlite".into());
        s.reasoning_effort = Some(ReasoningEffort::High);
        s.parent_id = Some("parent-session".into());
        s.session_cost_usd = 1.5;
        s.history.push(user_item("hello sqlite"));
        s.record_context_tokens(42, test_context_identity());
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        db.save_session_snapshot_for_import(&store_snapshot_from_session(&s, 0).unwrap())
            .unwrap();

        let loaded = load_session_files(dir.path()).expect("load db session");
        assert_eq!(loaded.id, "db-session");
        assert_eq!(loaded.title.as_deref(), Some("DB"));
        assert_eq!(loaded.first_user_message.as_deref(), Some("hello sqlite"));
        assert_eq!(loaded.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(loaded.parent_id.as_deref(), Some("parent-session"));
        assert_eq!(loaded.context_tokens, Some(42));
        assert_eq!(loaded.display_context_tokens, Some(42));
        assert_eq!(loaded.session_cost_usd, 1.5);
        assert_eq!(loaded.history.len(), 1);
        assert!(!dir.path().join("history.jsonl").exists());
    }

    #[test]
    fn session_serializes_history_native_with_user_display() {
        let mut s = fixture_session();
        s.history.push(HistoryItem::User {
            content: Content::Text("expanded command body".into()),
            display: Some("/reflect".into()),
        });

        let json = serde_json::to_value(&s).expect("serialize session");
        assert_eq!(json["schema_version"], CURRENT_SESSION_SCHEMA_VERSION);
        assert!(json.get("messages").is_none());
        assert_eq!(json["history"][0]["display"], "/reflect");

        let loaded: Session = serde_json::from_value(json).expect("deserialize session");
        assert!(matches!(
            &loaded.history[0],
            HistoryItem::User { content, display: Some(display) }
                if content.text_content() == "expanded command body" && display == "/reflect"
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
        s.record_context_tokens(500, test_context_identity());

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
        s.record_context_tokens(710, test_context_identity());
        s.clear_context_tokens_baseline();
        s.snapshot_context();

        s.history.extend([user_item("c"), assistant_text_item("d")]);
        s.record_context_tokens(700, test_context_identity());
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
    fn model_history_with_checkpoint_prepends_summary_and_tail() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.checkpoint = Some(checkpoint("summary text", 2));

        let model = s.model_history("SUMMARY:");

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
        s.record_context_tokens(100, test_context_identity());
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
        s.record_context_tokens(100, test_context_identity());

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
        s.record_context_tokens(500, test_context_identity());
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
            context_tokens: None,
            context_token_identity: None,
            display_context_token_identity: None,
            history_len: None,
            checkpoint: None,
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
    fn externalize_blobs_swaps_urls_to_blob_refs() {
        let mut items = vec![image_item("data:image/png;base64,AAA")];
        let mut map = std::collections::HashMap::new();
        map.insert("data:image/png;base64,AAA".into(), "blob://abc".into());
        externalize_blobs(&mut items, &map);
        assert_eq!(first_image_url(&items[0]), "blob://abc");
    }

    #[test]
    fn externalize_blobs_leaves_unmapped_urls_alone() {
        let mut items = vec![image_item("data:other")];
        let map = std::collections::HashMap::new();
        externalize_blobs(&mut items, &map);
        assert_eq!(first_image_url(&items[0]), "data:other");
    }

    #[test]
    fn internalize_blobs_replaces_blob_refs_with_data_urls() {
        let mut items = vec![image_item("blob://abc")];
        let mut map = std::collections::HashMap::new();
        map.insert("blob://abc".into(), "data:image/png;base64,AAA".into());
        internalize_blobs(&mut items, &map);
        assert_eq!(first_image_url(&items[0]), "data:image/png;base64,AAA");
    }

    #[test]
    fn internalize_blobs_leaves_text_history_unchanged() {
        let mut items = vec![user_item("hello")];
        let mut map = std::collections::HashMap::new();
        map.insert("foo".into(), "bar".into());
        internalize_blobs(&mut items, &map);
        match &items[0] {
            HistoryItem::User { content, .. } => match content {
                Content::Text(t) => assert_eq!(t, "hello"),
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user"),
        }
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
    fn round_trip_preserves_inv_elapsed_ms_and_turn_metas() {
        // Native history carries ToolInvocation telemetry directly, while
        // turn_metas.tool_elapsed remains available for legacy message-shaped
        // sessions and render fallbacks. Verify both channels survive save/load.
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
                }],
            )));
        let meta = protocol::TurnMeta {
            elapsed_ms: 100,
            avg_tps: None,
            display_tps: None,
            interrupted: false,
            tool_elapsed: [("c1".to_string(), 42u64)].into_iter().collect(),
        };
        original.turn_metas.push((1, meta));

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
        let restored_meta_elapsed = round.turn_metas[0].1.tool_elapsed.get("c1").copied();
        assert_eq!(
            restored_meta_elapsed,
            Some(42),
            "turn_metas.tool_elapsed is the canonical on-disk channel - must survive"
        );
    }

    // ── atomic_write ──────────────────────────────────────────────────

    #[test]
    fn atomic_write_writes_contents_and_renames_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        atomic_write(&path, b"hello", 42);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        // No leftover tmp file in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].to_str(), Some("data.json"));
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, b"new", 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }
}
