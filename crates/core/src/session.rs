use crate::config;
use protocol::{
    history_from_messages, history_item_message_count, message_to_history_positions, HistoryItem,
    Message, ReasoningEffort, TokenUsage, TurnMeta,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::BufRead;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);
static MIGRATION_IMPORT_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    #[serde(default = "default_checkpoint_kind")]
    pub kind: String,
    pub summary: String,
    pub first_live_index: usize,
    pub created_at_ms: u64,
    pub tokens_before: Option<u32>,
    pub tokens_after_estimate: Option<u32>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    /// Legacy spend reading from older `accounting_snapshots` entries. New
    /// snapshots keep cumulative spend only at the session level.
    #[serde(default, rename = "cost_usd", skip_serializing)]
    legacy_cost_usd: f64,
    pub context_tokens: Option<u32>,
    pub context_tokens_history_len: Option<usize>,
    /// Sticky display reading at this history point. This can differ from the
    /// authoritative baseline after checkpointing clears the next-request estimate.
    #[serde(default)]
    pub display_context_tokens: Option<u32>,
    pub checkpoint: Option<ContextSnapshotKey>,
}

impl ContextSnapshot {
    fn from_session(session: &Session) -> Self {
        Self {
            legacy_cost_usd: 0.0,
            context_tokens: session.context_tokens,
            context_tokens_history_len: session.context_tokens_history_len,
            display_context_tokens: session.display_context_tokens,
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
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }
    }
}

/// In-memory conversation state.
///
/// Storage shape is `Vec<HistoryItem>` (the sum-type history that makes
/// orphan tool_calls impossible). Current session files persist that history
/// directly. Older files with legacy `messages: Vec<Message>` still load via a
/// compatibility reader that converts them into history and repairs orphan
/// tool_use blocks by synthesizing an "interrupted" tool result (see
/// [`protocol::history_from_messages`]).
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
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
    pub history: Vec<HistoryItem>,
    pub checkpoint: Option<ContextCheckpoint>,
    pub context_tokens: Option<u32>,
    /// History length at the time `context_tokens` was recorded. Used to
    /// decide whether the provider baseline exactly covers the current
    /// history or needs a delta estimate for appended messages.
    pub context_tokens_history_len: Option<usize>,
    /// Last non-background provider context-token reading surfaced to the UI.
    /// It may lag the current history while a new request is in flight.
    pub display_context_tokens: Option<u32>,
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

pub use crate::session_migration::{
    ensure_session_db, export_history_jsonl, export_requests_jsonl, migrate_all_sessions_once,
    migrate_session_dir_to_db, pending_session_migration_count, spawn_background_migration,
    spawn_background_migration_with_event, spawn_background_migration_with_report,
    SessionMigrationBatchReport, SessionMigrationError, SessionMigrationEvent,
    SessionMigrationFailure, SessionMigrationOutcome, SessionMigrationState,
    SessionMigrationStatus,
};

// COMPAT(session-v1-messages): load old session.json files that stored
// provider-style messages instead of native HistoryItem rows.
/// Legacy on-disk JSON shape. Older sessions stored provider-style messages;
/// snapshot keys are in `Vec<Message>` position space and get remapped to
/// `Vec<HistoryItem>` positions on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionWire {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub metadata_snapshots: Vec<(usize, SessionMetadataSnapshot)>,
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
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub checkpoint: Option<ContextCheckpoint>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub context_tokens_history_len: Option<usize>,
    #[serde(default)]
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub cost_snapshots: Vec<(usize, f64)>,
    #[serde(default)]
    pub turn_metas: Vec<(usize, TurnMeta)>,
    #[serde(default, rename = "accounting_snapshots", alias = "context_snapshots")]
    pub context_snapshots: Vec<(usize, ContextSnapshot)>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub session_usage: TokenUsage,
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
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub turn_metas: HistorySnapshots<TurnMeta>,
    #[serde(default, rename = "accounting_snapshots", alias = "context_snapshots")]
    pub context_snapshots: HistorySnapshots<ContextSnapshot>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub session_usage: TokenUsage,
}

#[derive(Debug, Clone, Deserialize)]
struct SessionWireProbe {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    history: Option<serde_json::Value>,
}

// COMPAT(session-v1-messages): old snapshot keys were stored in provider-message
// positions, not semantic history positions.
/// `msg_to_hist[i]` = index into history that absorbed message i.
/// `msg_len` = total messages count (history_to_messages length).
fn remap_msg_to_hist<T: Clone>(
    snapshots: &[(usize, T)],
    msg_to_hist: &[usize],
    hist_len: usize,
) -> HistorySnapshots<T> {
    HistorySnapshots::from_vec(
        snapshots
            .iter()
            .map(|(msg_pos, v)| {
                let hist_pos = if *msg_pos == 0 {
                    0
                } else if *msg_pos <= msg_to_hist.len() {
                    // Snapshot was taken AT messages.len() == msg_pos, i.e.
                    // after the (msg_pos-1)th message landed. The equivalent
                    // history-space key is one past the history index of the
                    // last absorbed message.
                    msg_to_hist[*msg_pos - 1] + 1
                } else {
                    hist_len
                };
                (hist_pos, v.clone())
            })
            .collect(),
    )
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

// COMPAT(session-v1-messages): rebuild context snapshots from old cost-only snapshots.
fn legacy_context_snapshots(
    cost_snapshots: Vec<(usize, f64)>,
    context_tokens: Option<u32>,
    context_tokens_history_len: Option<usize>,
    display_context_tokens: Option<u32>,
) -> HistorySnapshots<ContextSnapshot> {
    HistorySnapshots::from_vec(
        cost_snapshots
            .into_iter()
            .map(|(len, cost_usd)| {
                let (context_tokens, context_tokens_history_len, display_context_tokens) =
                    if context_tokens_history_len == Some(len) {
                        (
                            context_tokens,
                            context_tokens_history_len,
                            display_context_tokens,
                        )
                    } else {
                        (None, None, None)
                    };
                (
                    len,
                    ContextSnapshot {
                        legacy_cost_usd: cost_usd,
                        context_tokens,
                        context_tokens_history_len,
                        display_context_tokens,
                        checkpoint: None,
                    },
                )
            })
            .collect(),
    )
}

impl From<SessionWire> for Session {
    fn from(w: SessionWire) -> Self {
        let table = message_to_history_positions(&w.messages);
        let history = history_from_messages(w.messages);
        let hist_len = history.len();
        let context_tokens = w.context_tokens;
        let context_tokens_history_len = w.context_tokens_history_len;
        let display_context_tokens = w.display_context_tokens.or(context_tokens);
        let cost_snapshots = remap_msg_to_hist(&w.cost_snapshots, &table, hist_len);
        let context_snapshots = remap_msg_to_hist(&w.context_snapshots, &table, hist_len);
        let context_snapshots = if context_snapshots.is_empty() {
            legacy_context_snapshots(
                cost_snapshots.into_vec(),
                context_tokens,
                context_tokens_history_len,
                display_context_tokens,
            )
        } else {
            context_snapshots
        };
        let session_cost_usd = if w.session_cost_usd == 0.0 {
            context_snapshots
                .last()
                .map(|(_, snapshot)| snapshot.legacy_cost_usd)
                .unwrap_or(w.session_cost_usd)
        } else {
            w.session_cost_usd
        };
        let metadata_snapshots = remap_msg_to_hist(&w.metadata_snapshots, &table, hist_len);
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
            cwd: w.cwd,
            parent_id: w.parent_id,
            turn_metas: remap_msg_to_hist(&w.turn_metas, &table, hist_len),
            context_snapshots,
            history,
            checkpoint: w.checkpoint,
            context_tokens,
            context_tokens_history_len,
            display_context_tokens,
            session_cost_usd,
            session_usage: w.session_usage,
        }
    }
}

impl From<SessionWireV2> for Session {
    fn from(w: SessionWireV2) -> Self {
        let context_tokens = w.context_tokens;
        let display_context_tokens = w.display_context_tokens.or(context_tokens);
        let metadata_snapshots = w.metadata_snapshots;
        let context_snapshots = w.context_snapshots;
        let session_cost_usd = if w.session_cost_usd == 0.0 {
            context_snapshots
                .last()
                .map(|(_, snapshot)| snapshot.legacy_cost_usd)
                .unwrap_or(w.session_cost_usd)
        } else {
            w.session_cost_usd
        };
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
            cwd: w.cwd,
            parent_id: w.parent_id,
            history: w.history,
            checkpoint: w.checkpoint,
            context_tokens,
            context_tokens_history_len: w.context_tokens_history_len,
            display_context_tokens,
            turn_metas: w.turn_metas,
            context_snapshots,
            session_cost_usd,
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
            cwd: s.cwd.clone(),
            parent_id: s.parent_id.clone(),
            history: s.history.clone(),
            checkpoint: s.checkpoint.clone(),
            context_tokens: s.context_tokens,
            context_tokens_history_len: s.context_tokens_history_len,
            display_context_tokens: s.display_context_tokens,
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
        let value = serde_json::Value::deserialize(de)?;
        let probe: SessionWireProbe =
            serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
        match (probe.schema_version, probe.history.is_some()) {
            (Some(CURRENT_SESSION_SCHEMA_VERSION), _) | (None, true) => {
                serde_json::from_value::<SessionWireV2>(value)
                    .map(Session::from)
                    .map_err(serde::de::Error::custom)
            }
            (Some(version), _) => Err(serde::de::Error::custom(format!(
                "unsupported session schema version {version}"
            ))),
            (None, false) => serde_json::from_value::<SessionWire>(value)
                .map(Session::from)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    /// Approximate text byte size (message bodies, reasoning, tool-call args).
    /// Populated in `meta.json` so the resume dialog avoids loading session history.
    #[serde(default)]
    pub text_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<SessionMigrationStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionJsonlMeta {
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
    pub cwd: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub checkpoint: Option<ContextCheckpoint>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub context_tokens_history_len: Option<usize>,
    #[serde(default)]
    pub display_context_tokens: Option<u32>,
    #[serde(default)]
    pub turn_metas: HistorySnapshots<TurnMeta>,
    #[serde(default, rename = "accounting_snapshots", alias = "context_snapshots")]
    pub context_snapshots: HistorySnapshots<ContextSnapshot>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub session_usage: TokenUsage,
    #[serde(default)]
    pub text_bytes: Option<u64>,
}

impl From<&Session> for SessionJsonlMeta {
    fn from(s: &Session) -> Self {
        Self {
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
            cwd: s.cwd.clone(),
            parent_id: s.parent_id.clone(),
            checkpoint: s.checkpoint.clone(),
            context_tokens: s.context_tokens,
            context_tokens_history_len: s.context_tokens_history_len,
            display_context_tokens: s.display_context_tokens,
            turn_metas: s.turn_metas.clone(),
            context_snapshots: s.context_snapshots.clone(),
            session_cost_usd: s.session_cost_usd,
            session_usage: s.session_usage.clone(),
            text_bytes: Some(compute_text_bytes(&s.history)),
        }
    }
}

impl SessionJsonlMeta {
    fn into_session(self, history: Vec<HistoryItem>) -> Session {
        Session {
            id: self.id,
            title: self.title,
            slug: self.slug,
            first_user_message: self.first_user_message,
            metadata_snapshots: self.metadata_snapshots,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            mode: self.mode,
            reasoning_effort: self.reasoning_effort,
            model: self.model,
            cwd: self.cwd,
            parent_id: self.parent_id,
            history,
            checkpoint: self.checkpoint,
            context_tokens: self.context_tokens,
            context_tokens_history_len: self.context_tokens_history_len,
            display_context_tokens: self.display_context_tokens.or(self.context_tokens),
            turn_metas: self.turn_metas,
            context_snapshots: self.context_snapshots,
            session_cost_usd: self.session_cost_usd,
            session_usage: self.session_usage,
        }
    }
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
            cwd,
            parent_id: None,
            history: Vec::new(),
            checkpoint: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            turn_metas: HistorySnapshots::default(),
            context_snapshots: HistorySnapshots::default(),
            session_cost_usd: 0.0,
            session_usage: TokenUsage::default(),
        }
    }

    fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            parent_id: self.parent_id.clone(),
            context_tokens: self.display_context_tokens(),
            text_bytes: Some(compute_text_bytes(&self.history)),
            migration: None,
        }
    }

    pub fn record_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.context_tokens_history_len = Some(self.history.len());
        self.display_context_tokens = Some(tokens);
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
        self.display_context_tokens = None;
    }

    pub fn clear_context_tokens_baseline(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
    }

    pub fn current_context_tokens(&self) -> Option<u32> {
        (self.context_tokens_history_len == Some(self.history.len()))
            .then_some(self.context_tokens)
            .flatten()
    }

    pub fn display_context_tokens(&self) -> Option<u32> {
        self.display_context_tokens.or(self.context_tokens)
    }

    pub fn checkpoint_snapshot_key(&self) -> Option<ContextSnapshotKey> {
        self.checkpoint.as_ref().map(ContextSnapshotKey::from)
    }

    pub fn snapshot_context(&mut self) {
        let snapshot = ContextSnapshot::from_session(self);
        self.context_snapshots.push((self.history.len(), snapshot));
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
        self.context_snapshots.truncate_after(hist_idx);
        if !self.context_snapshots.is_empty() {
            self.restore_context_tokens_after_rewind(hist_idx, None);
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
            self.display_context_tokens =
                snapshot.display_context_tokens.or(snapshot.context_tokens);
        } else if let Some((tokens, Some(history_len))) = checkpoint_fallback {
            if history_len <= hist_idx {
                self.context_tokens = tokens;
                self.context_tokens_history_len = Some(history_len);
                self.display_context_tokens = tokens;
            } else {
                self.clear_context_tokens();
            }
        } else if self.context_snapshots.is_empty()
            && self
                .context_tokens_history_len
                .is_some_and(|len| len <= hist_idx)
        {
            // COMPAT(session-v1-messages): old sessions may not have context
            // snapshots; keep a baseline that still fits the rewound history.
        } else {
            self.clear_context_tokens();
        }
    }

    fn clear_checkpoint_for_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) -> Option<(Option<u32>, Option<usize>)> {
        if keep_checkpoint_at_boundary {
            return None;
        }
        if self
            .checkpoint
            .as_ref()
            .is_none_or(|cp| cp.first_live_index < hist_idx)
        {
            return None;
        }
        self.checkpoint.take().map(|cp| {
            (
                cp.pre_checkpoint_context_tokens,
                cp.pre_checkpoint_context_history_len,
            )
        })
    }

    pub fn model_history(&self, summary_prefix: &str) -> Vec<HistoryItem> {
        let Some(cp) = &self.checkpoint else {
            return self.history.clone();
        };
        let mut out =
            Vec::with_capacity(self.history.len().saturating_sub(cp.first_live_index) + 1);
        out.push(HistoryItem::user(protocol::Content::text(format!(
            "{}\n{}",
            summary_prefix.trim_end(),
            cp.summary
        ))));
        out.extend(self.history.iter().skip(cp.first_live_index).cloned());
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
        self.checkpoint = Some(ContextCheckpoint {
            kind,
            summary,
            first_live_index,
            created_at_ms: now_ms(),
            tokens_before,
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: self.context_tokens,
            pre_checkpoint_context_history_len: self.context_tokens_history_len,
        });
        // The next provider response is the first authoritative baseline
        // token reading for checkpointed model history.
        self.clear_context_tokens_baseline();
        self.snapshot_context();
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

    pub fn merge_model_history_snapshot(
        &mut self,
        summary_prefix: &str,
        history: Vec<HistoryItem>,
    ) {
        let Some(cp) = self.checkpoint.clone() else {
            self.history = history;
            return;
        };
        let mut incoming = history;
        if incoming
            .first()
            .is_some_and(|item| is_context_checkpoint_summary(item, summary_prefix))
        {
            incoming.remove(0);
        }
        self.history.truncate(cp.first_live_index);
        self.history.extend(incoming);
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
                        self.display_context_tokens = cp.pre_checkpoint_context_tokens;
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
            cwd: self.cwd.clone(),
            parent_id: Some(self.id.clone()),
            history: self.history.clone(),
            checkpoint: self.checkpoint.clone(),
            context_tokens: self.context_tokens,
            context_tokens_history_len: self.context_tokens_history_len,
            display_context_tokens: self.display_context_tokens,
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
    sessions_dir().join(&session.id)
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
) -> Result<smelt_store::SessionSaveReport, smelt_store::StoreError> {
    save_with_blobs_result_with_history_start(session, url_to_blob, 0)
}

pub fn save_with_blobs_result_with_history_start(
    session: &Session,
    url_to_blob: &std::collections::HashMap<String, String>,
    history_start_idx: usize,
) -> Result<smelt_store::SessionSaveReport, smelt_store::StoreError> {
    let _perf = smelt_perf::perf::begin("session:write");
    let session_dir = dir_for(session);
    fs::create_dir_all(&session_dir)?;
    let ts = now_ms();

    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.history, url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    let db = smelt_store::SessionDb::open(session_dir.join("session.db"))?;
    let snapshot = session_store_snapshot(&session_out, history_start_idx)?;
    let report = db.save_session_snapshot_as_writer(&snapshot)?;
    write_meta(&session_dir, &session_out.meta());
    let blob = db
        .search_blob()
        .unwrap_or_else(|_| build_search_blob(&session_out.history));
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), ts);
    Ok(report)
}

fn session_store_snapshot(
    session: &Session,
    history_start_idx: usize,
) -> Result<smelt_store::SessionSnapshot, smelt_store::StoreError> {
    let meta_json = serde_json::to_value(SessionJsonlMeta::from(session))?;
    let history_start_idx = history_start_idx.min(session.history.len());
    Ok(smelt_store::SessionSnapshot {
        state: smelt_store::SessionState {
            id: session.id.clone(),
            title: session.title.clone(),
            slug: session.slug.clone(),
            cwd: session.cwd.clone(),
            mode: session.mode.clone(),
            model: session.model.clone(),
            accounting_json: Some(serde_json::to_value(&session.session_usage)?),
            checkpoint_json: session
                .checkpoint
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
            revision: 0,
            history_len: session.history.len() as u64,
            created_at: session.created_at_ms as i64,
            updated_at: session.updated_at_ms as i64,
        },
        meta_json: Some(meta_json),
        history_start_idx,
        history_len: session.history.len(),
        history: session.history[history_start_idx..].to_vec(),
        turn_metas: snapshot_values(&session.turn_metas)?,
        metadata_snapshots: snapshot_values(&session.metadata_snapshots)?,
        accounting_snapshots: snapshot_values(&session.context_snapshots)?,
    })
}

fn snapshot_values<T: Serialize>(
    snapshots: &HistorySnapshots<T>,
) -> Result<Vec<(u64, Value)>, smelt_store::StoreError> {
    snapshots
        .iter()
        .map(|(idx, value)| Ok((*idx as u64, serde_json::to_value(value)?)))
        .collect()
}

#[cfg(test)]
fn encode_session_jsonl_meta(session: &Session) -> Option<String> {
    let _perf = smelt_perf::perf::begin("session:write:encode_meta_json");
    serde_json::to_string(&SessionJsonlMeta::from(session)).ok()
}

#[cfg(test)]
fn encode_history_jsonl(history: &[HistoryItem]) -> Option<Vec<u8>> {
    let _perf = smelt_perf::perf::begin("session:write:encode_history_jsonl");
    let mut out = Vec::new();
    for item in history {
        serde_json::to_writer(&mut out, item).ok()?;
        out.write_all(b"\n").ok()?;
    }
    Some(out)
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

/// Load by exact ID or unique prefix (git-style short ID).
pub fn load(id_or_prefix: &str) -> Option<Session> {
    let _perf = smelt_perf::perf::begin("session:load");
    let id = {
        let _perf = smelt_perf::perf::begin("session:load:resolve");
        resolve_prefix(id_or_prefix)?
    };
    load_exact(&id)
}

fn load_exact(id: &str) -> Option<Session> {
    let _perf = smelt_perf::perf::begin("session:load:exact");
    load_session_files(&sessions_dir().join(id))
}

fn load_session_files(dir_path: &std::path::Path) -> Option<Session> {
    if let Err(err) = crate::session_migration::ensure_session_db(dir_path) {
        log_session_migration_error(dir_path, &err);
        return None;
    }
    load_db_session(dir_path).and_then(|session| internalize_session_blobs(dir_path, session))
}

fn log_session_migration_error(dir_path: &std::path::Path, err: &SessionMigrationError) {
    engine::log::entry(
        engine::log::Level::Warn,
        "session_migration_failed",
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
            let _perf = smelt_perf::perf::begin("session:load:read_blobs");
            crate::attachment::AttachmentStore::load_blobs(&blob_dir)
        };
        smelt_perf::perf::record_value("session:load:blobs", blob_to_url.len() as u64);
        if !blob_to_url.is_empty() {
            let _perf = smelt_perf::perf::begin("session:load:internalize_blobs");
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
    let db = smelt_store::SessionDb::open(&db_path).ok()?;
    let mut snapshot = db.load_session_snapshot().ok()??;
    let history = std::mem::take(&mut snapshot.history);
    if let Some(meta_json) = snapshot.meta_json.take() {
        if let Ok(meta) = serde_json::from_value::<SessionJsonlMeta>(meta_json) {
            return Some(meta.into_session(history));
        }
    }
    Some(session_from_store_snapshot(snapshot, history))
}

fn session_from_store_snapshot(
    snapshot: smelt_store::SessionSnapshot,
    history: Vec<HistoryItem>,
) -> Session {
    let state = snapshot.state;
    let turn_metas = snapshots_from_values(snapshot.turn_metas).unwrap_or_default();
    let metadata_snapshots = snapshots_from_values(snapshot.metadata_snapshots).unwrap_or_default();
    let context_snapshots =
        snapshots_from_values(snapshot.accounting_snapshots).unwrap_or_default();
    let session_usage = state
        .accounting_json
        .clone()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let checkpoint = state
        .checkpoint_json
        .clone()
        .and_then(|value| serde_json::from_value(value).ok());
    Session {
        id: state.id,
        title: state.title,
        slug: state.slug,
        first_user_message: None,
        metadata_snapshots,
        created_at_ms: state.created_at as u64,
        updated_at_ms: state.updated_at as u64,
        mode: state.mode,
        reasoning_effort: None,
        model: state.model,
        cwd: state.cwd,
        parent_id: None,
        history,
        checkpoint,
        context_tokens: None,
        context_tokens_history_len: None,
        display_context_tokens: None,
        turn_metas,
        context_snapshots,
        session_cost_usd: 0.0,
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

pub(crate) fn write_generated_sidecars(dir_path: &Path, session: &Session) {
    write_meta(dir_path, &session.meta());
    atomic_write(
        &dir_path.join("content.txt"),
        build_search_blob(&session.history).as_bytes(),
        now_ms(),
    );
}

pub(crate) fn import_legacy_session_to_db(
    dir_path: &Path,
    session: &Session,
) -> Result<(), smelt_store::StoreError> {
    let db_path = dir_path.join("session.db");
    if db_path.is_file() {
        return Ok(());
    }

    let temp_path = dir_path.join(format!(
        "session.db.import-{}-{}-{}.tmp",
        std::process::id(),
        now_ms(),
        MIGRATION_IMPORT_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    cleanup_sqlite_files(&temp_path);

    let db = smelt_store::SessionDb::open(&temp_path)?;
    let result = (|| {
        let snapshot = session_store_snapshot(session, 0)?;
        db.save_session_snapshot_as_writer(&snapshot)?;
        db.import_legacy_requests_jsonl(dir_path)?;
        db.connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    })();
    drop(db);

    if let Err(err) = result {
        cleanup_sqlite_files(&temp_path);
        return Err(err);
    }

    if db_path.is_file() {
        cleanup_sqlite_files(&temp_path);
        return Ok(());
    }

    match fs::hard_link(&temp_path, &db_path) {
        Ok(()) => {
            cleanup_sqlite_files(&temp_path);
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && db_path.is_file() => {
            cleanup_sqlite_files(&temp_path);
            Ok(())
        }
        Err(err) => {
            cleanup_sqlite_files(&temp_path);
            Err(err.into())
        }
    }
}

fn cleanup_sqlite_files(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(sqlite_sidecar_path(path, "wal"));
    let _ = fs::remove_file(sqlite_sidecar_path(path, "shm"));
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}-{suffix}", path.display()))
}

// COMPAT(session-split-jsonl): import pre-SQLite meta.json + history.jsonl
// sessions into canonical SQLite storage during the alpha migration window.
pub(crate) fn read_jsonl_session(
    dir_path: &Path,
) -> crate::session_migration::SessionMigrationResult<Session> {
    let meta_path = dir_path.join("meta.json");
    let meta_contents = {
        let _perf = smelt_perf::perf::begin("session:load:read_meta_json");
        fs::read_to_string(&meta_path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                crate::session_migration::SessionMigrationError::MissingFile {
                    path: meta_path.clone(),
                }
            }
            _ => crate::session_migration::SessionMigrationError::ReadFile {
                path: meta_path.clone(),
                message: err.to_string(),
            },
        })?
    };
    smelt_perf::perf::record_value("session:load:meta_json_bytes", meta_contents.len() as u64);
    let meta: SessionJsonlMeta = {
        let _perf = smelt_perf::perf::begin("session:load:parse_meta_json");
        serde_json::from_str(&meta_contents).map_err(|err| {
            crate::session_migration::SessionMigrationError::ParseJson {
                path: meta_path.clone(),
                message: err.to_string(),
            }
        })?
    };
    if meta.schema_version != CURRENT_SESSION_SCHEMA_VERSION {
        return Err(
            crate::session_migration::SessionMigrationError::UnsupportedSchema {
                path: meta_path,
                version: meta.schema_version,
            },
        );
    }

    let history_path = dir_path.join("history.jsonl");
    let history_file = {
        let _perf = smelt_perf::perf::begin("session:load:open_history_jsonl");
        fs::File::open(&history_path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                crate::session_migration::SessionMigrationError::MissingFile {
                    path: history_path.clone(),
                }
            }
            _ => crate::session_migration::SessionMigrationError::ReadFile {
                path: history_path.clone(),
                message: err.to_string(),
            },
        })?
    };
    let history_len = history_file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    smelt_perf::perf::record_value("session:load:history_jsonl_bytes", history_len);
    let history = {
        let _perf = smelt_perf::perf::begin("session:load:parse_history_jsonl");
        let reader = std::io::BufReader::new(history_file);
        let mut history = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line =
                line.map_err(
                    |err| crate::session_migration::SessionMigrationError::ReadFile {
                        path: history_path.clone(),
                        message: format!("line {}: {err}", idx + 1),
                    },
                )?;
            if line.trim().is_empty() {
                continue;
            }
            history.push(serde_json::from_str::<HistoryItem>(&line).map_err(|err| {
                crate::session_migration::SessionMigrationError::ParseJson {
                    path: history_path.clone(),
                    message: format!("line {}: {err}", idx + 1),
                }
            })?);
        }
        history
    };
    smelt_perf::perf::record_value("session:load:history_items", history.len() as u64);
    Ok(meta.into_session(history))
}

// COMPAT(session-json-monolith): read old monolithic session.json files only as
// migration input for canonical SQLite storage.
pub(crate) fn read_legacy_json_session(
    dir_path: &Path,
) -> crate::session_migration::SessionMigrationResult<Session> {
    let session_path = dir_path.join("session.json");
    let contents = {
        let _perf = smelt_perf::perf::begin("session:load:read_json");
        fs::read_to_string(&session_path).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                crate::session_migration::SessionMigrationError::MissingFile {
                    path: session_path.clone(),
                }
            }
            _ => crate::session_migration::SessionMigrationError::ReadFile {
                path: session_path.clone(),
                message: err.to_string(),
            },
        })?
    };
    smelt_perf::perf::record_value("session:load:json_bytes", contents.len() as u64);
    let session: Session = {
        let _perf = smelt_perf::perf::begin("session:load:parse_json");
        serde_json::from_str(&contents).map_err(|err| {
            crate::session_migration::SessionMigrationError::ParseJson {
                path: session_path,
                message: err.to_string(),
            }
        })?
    };
    smelt_perf::perf::record_value("session:load:history_items", session.history.len() as u64);
    Ok(session)
}

// COMPAT(session-json-monolith): import old monolithic sessions to SQLite as
// soon as they are opened, then remove the monolith once SQLite and sidecars exist.
pub(crate) fn migrate_legacy_json_session(dir_path: &Path, session: &Session) {
    let _perf = smelt_perf::perf::begin("session:load:migrate_sqlite");
    write_generated_sidecars(dir_path, session);
    if dir_path.join("session.db").is_file() && dir_path.join("meta.json").is_file() {
        let _ = fs::remove_file(dir_path.join("session.json"));
    }
}

/// Returns `None` when no match or prefix is ambiguous.
pub(crate) fn resolve_prefix(prefix: &str) -> Option<String> {
    let dir = sessions_dir();

    if dir.join(prefix).join("session.db").is_file()
        || dir.join(prefix).join("history.jsonl").is_file()
        || dir.join(prefix).join("session.json").is_file()
    {
        return Some(prefix.to_string());
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
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
    let mut out = crate::utils::parallel_filter_map(paths, load_meta_for_dir);
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

/// COMPAT(session-search-sidecar-missing): uses `meta.json` when present,
/// reads canonical SQLite metadata when available, and surfaces pending/failed
/// migration status without reading legacy session payloads.
fn load_meta_for_dir(path: PathBuf) -> Option<SessionMeta> {
    let meta = if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
        if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) {
            if meta.text_bytes.is_none() {
                backfill_text_bytes(&path, &mut meta);
            }
            Some(meta)
        } else {
            None
        }
    } else {
        None
    };
    let meta = if let Some(meta) = meta {
        meta
    } else if let Some(meta) = load_meta_from_db(&path) {
        write_meta(&path, &meta);
        meta
    } else {
        return migration_meta_for_dir(&path);
    };
    Some(with_migration_status(&path, meta))
}

fn migration_meta_for_dir(path: &Path) -> Option<SessionMeta> {
    let migration = migration_status_for_dir(path)?;
    let id = path.file_name().and_then(|name| name.to_str())?.to_string();
    Some(SessionMeta {
        id,
        title: None,
        slug: None,
        first_user_message: None,
        created_at_ms: 0,
        updated_at_ms: migration.updated_at_ms,
        mode: None,
        reasoning_effort: None,
        model: None,
        cwd: None,
        parent_id: None,
        context_tokens: None,
        text_bytes: None,
        migration: Some(migration),
    })
}

fn with_migration_status(path: &Path, mut meta: SessionMeta) -> SessionMeta {
    meta.migration = migration_status_for_dir(path);
    meta
}

fn migration_status_for_dir(path: &Path) -> Option<SessionMigrationStatus> {
    crate::session_migration::migration_status_for_dir(path).or_else(|| {
        crate::session_migration::session_dir_needs_migration(path).then_some(
            SessionMigrationStatus {
                state: SessionMigrationState::Pending,
                message: None,
                updated_at_ms: 0,
            },
        )
    })
}

fn load_meta_from_db(path: &Path) -> Option<SessionMeta> {
    let db_path = path.join("session.db");
    if !db_path.is_file() {
        return None;
    }
    let db = smelt_store::SessionDb::open(&db_path).ok()?;
    let snapshot = db.load_session_snapshot().ok()??;
    if let Some(meta_json) = snapshot.meta_json {
        if let Ok(meta) = serde_json::from_value::<SessionJsonlMeta>(meta_json) {
            return Some(SessionMeta {
                id: meta.id,
                title: meta.title,
                slug: meta.slug,
                first_user_message: meta.first_user_message,
                created_at_ms: meta.created_at_ms,
                updated_at_ms: meta.updated_at_ms,
                mode: meta.mode,
                reasoning_effort: meta.reasoning_effort,
                model: meta.model,
                cwd: meta.cwd,
                parent_id: meta.parent_id,
                context_tokens: meta.display_context_tokens.or(meta.context_tokens),
                text_bytes: Some(db.history_text_bytes().ok()?),
                migration: None,
            });
        }
    }
    let state = snapshot.state;
    Some(SessionMeta {
        id: state.id,
        title: state.title,
        slug: state.slug,
        first_user_message: None,
        created_at_ms: state.created_at as u64,
        updated_at_ms: state.updated_at as u64,
        mode: state.mode,
        reasoning_effort: None,
        model: state.model,
        cwd: state.cwd,
        parent_id: None,
        context_tokens: None,
        text_bytes: Some(db.history_text_bytes().ok()?),
        migration: None,
    })
}

fn compute_text_bytes(history: &[HistoryItem]) -> u64 {
    let mut total: u64 = 0;
    for item in history {
        match item {
            HistoryItem::System { content } | HistoryItem::User { content, .. } => {
                total += content.text_content().len() as u64;
            }
            HistoryItem::Note(note) => {
                total += note.text().len() as u64;
            }
            HistoryItem::Assistant(turn) => {
                if let Some(ref c) = turn.content {
                    total += c.text_content().len() as u64;
                }
                if let Some(ref r) = turn.reasoning {
                    total += r.len() as u64;
                }
                for inv in &turn.invocations {
                    total += inv.name.len() as u64;
                    total += inv.arguments.len() as u64;
                    total += inv.result.content.len() as u64;
                }
            }
        }
    }
    total
}

fn backfill_text_bytes(session_dir: &std::path::Path, meta: &mut SessionMeta) {
    let Ok(db) = smelt_store::SessionDb::open(session_dir.join("session.db")) else {
        return;
    };
    let Ok(bytes) = db.history_text_bytes() else {
        return;
    };
    meta.text_bytes = Some(bytes);
    write_meta(session_dir, meta);
}

/// User + assistant text only; reasoning, tool output, and system messages excluded.
fn build_search_blob(history: &[HistoryItem]) -> String {
    let mut out = String::new();
    for item in history {
        let text_opt = match item {
            HistoryItem::User { content, .. } => Some(content.text_content()),
            HistoryItem::Assistant(turn) => turn.content.as_ref().map(|c| c.text_content()),
            HistoryItem::System { .. } | HistoryItem::Note(_) => None,
        };
        if let Some(text) = text_opt {
            if !text.is_empty() {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }
    out
}

/// Read the searchable text blob for `id`.
///
/// COMPAT(session-search-sidecar-missing): reads canonical SQLite search text,
/// migrating legacy inputs first when necessary, then writes `content.txt`.
pub fn load_search_blob(id: &str) -> Option<String> {
    let _perf = smelt_perf::perf::begin("session:load_search_blob");
    let session_dir = sessions_dir().join(id);
    if let Ok(contents) = fs::read_to_string(session_dir.join("content.txt")) {
        return Some(contents);
    }
    if let Ok(db) = smelt_store::SessionDb::open(session_dir.join("session.db")) {
        if let Ok(blob) = db.search_blob() {
            atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), now_ms());
            return Some(blob);
        }
    }
    if let Err(err) = crate::session_migration::ensure_session_db(&session_dir) {
        log_session_migration_error(&session_dir, &err);
        return None;
    }
    if let Ok(db) = smelt_store::SessionDb::open(session_dir.join("session.db")) {
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

fn externalize_blobs(
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
    fn system_item(text: &str) -> HistoryItem {
        HistoryItem::system(text)
    }

    fn checkpoint(summary: &str, first_live_index: usize) -> ContextCheckpoint {
        ContextCheckpoint {
            kind: "compaction".to_string(),
            summary: summary.to_string(),
            first_live_index,
            created_at_ms: 0,
            tokens_before: None,
            tokens_after_estimate: None,
            pre_checkpoint_context_tokens: None,
            pre_checkpoint_context_history_len: None,
        }
    }

    #[test]
    fn jsonl_session_round_trips_native_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.title = Some("JSONL".into());
        s.history.push(user_item("hello jsonl"));
        s.history.push(assistant_text_item("assistant jsonl"));
        s.record_context_tokens(42);

        let meta = encode_session_jsonl_meta(&s).expect("encode meta");
        let history = encode_history_jsonl(&s.history).expect("encode history");
        fs::write(dir.path().join("meta.json"), meta).expect("write meta");
        fs::write(dir.path().join("history.jsonl"), history).expect("write history");

        let loaded = read_jsonl_session(dir.path()).expect("load jsonl session");
        assert_eq!(loaded.title.as_deref(), Some("JSONL"));
        assert_eq!(loaded.context_tokens, Some(42));
        assert_eq!(loaded.current_context_tokens(), Some(42));
        assert_eq!(loaded.history.len(), 2);
        assert!(
            matches!(&loaded.history[0], HistoryItem::User { content, .. } if content.text_content() == "hello jsonl")
        );
    }

    #[test]
    fn legacy_json_session_migrates_to_sqlite_storage() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.history.push(user_item("legacy prompt"));
        fs::write(
            dir.path().join("session.json"),
            serde_json::to_string(&s).expect("encode legacy session"),
        )
        .expect("write legacy session");

        let loaded = read_legacy_json_session(dir.path()).expect("load legacy session");
        import_legacy_session_to_db(dir.path(), &loaded).expect("import sqlite");
        migrate_legacy_json_session(dir.path(), &loaded);

        assert!(dir.path().join("session.db").is_file());
        assert!(dir.path().join("meta.json").is_file());
        assert!(!dir.path().join("history.jsonl").exists());
        assert!(!dir.path().join("session.json").exists());
        let migrated = load_db_session(dir.path()).expect("load migrated session");
        assert_eq!(migrated.history.len(), 1);
    }

    #[test]
    fn stale_split_session_falls_back_to_legacy_json_and_migrates_to_sqlite() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "stale-split".into();
        s.title = Some("legacy title".into());
        s.history.push(user_item("legacy prompt"));
        fs::write(
            dir.path().join("session.json"),
            serde_json::to_string(&s).expect("encode legacy session"),
        )
        .expect("write legacy session");
        fs::write(
            dir.path().join("meta.json"),
            serde_json::json!({
                "id": s.id,
                "title": "stale split missing schema_version",
            })
            .to_string(),
        )
        .expect("write stale meta");
        fs::write(dir.path().join("history.jsonl"), b"\n").expect("write stale history");

        let loaded = load_session_files(dir.path()).expect("load via legacy fallback");

        assert_eq!(loaded.title.as_deref(), Some("legacy title"));
        assert_eq!(loaded.history.len(), 1);
        assert!(dir.path().join("session.db").is_file());
        assert!(dir.path().join("meta.json").is_file());
        assert!(dir.path().join("history.jsonl").is_file());
        assert!(!dir.path().join("session.json").exists());
        let migrated = load_db_session(dir.path()).expect("load migrated db session");
        assert_eq!(migrated.title.as_deref(), Some("legacy title"));
        assert_eq!(migrated.history.len(), 1);
    }

    #[test]
    fn migration_helper_imports_split_session_to_sqlite() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "split-migrate".into();
        s.title = Some("split title".into());
        s.history.push(user_item("split prompt"));
        fs::write(
            dir.path().join("meta.json"),
            encode_session_jsonl_meta(&s).expect("encode meta"),
        )
        .expect("write meta");
        fs::write(
            dir.path().join("history.jsonl"),
            encode_history_jsonl(&s.history).expect("encode history"),
        )
        .expect("write history");

        let outcome = migrate_session_dir_to_db(dir.path()).expect("migrate split");

        assert_eq!(outcome, SessionMigrationOutcome::Migrated);
        assert!(dir.path().join("session.db").is_file());
        assert!(!dir
            .path()
            .join(crate::session_migration::MIGRATION_STATUS_FILE)
            .exists());
        let migrated = load_db_session(dir.path()).expect("load migrated db session");
        assert_eq!(migrated.title.as_deref(), Some("split title"));
        assert_eq!(migrated.history.len(), 1);
    }

    #[test]
    fn legacy_session_without_db_lists_as_pending_without_parsing_payload() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("session.json"), b"not json").expect("write invalid legacy");

        let listed = load_meta_for_dir(dir.path().to_path_buf()).expect("pending meta");

        assert_eq!(listed.id, dir.path().file_name().unwrap().to_str().unwrap());
        assert_eq!(
            listed.migration.as_ref().unwrap().state,
            SessionMigrationState::Pending
        );
        assert!(!dir.path().join("session.db").exists());
        assert!(!dir
            .path()
            .join(crate::session_migration::MIGRATION_STATUS_FILE)
            .exists());
    }

    #[test]
    fn failed_migration_keeps_legacy_files_and_is_retryable() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("session.json"), b"not json").expect("write invalid legacy");

        let first = migrate_session_dir_to_db(dir.path()).expect_err("invalid legacy fails");
        assert!(first.to_string().contains("failed to parse"));
        assert!(dir.path().join("session.json").is_file());
        assert!(!dir.path().join("session.db").exists());
        let failed =
            crate::session_migration::read_migration_status(dir.path()).expect("failure status");
        assert_eq!(failed.state, SessionMigrationState::Failed);
        let listed = load_meta_for_dir(dir.path().to_path_buf()).expect("failure meta");
        assert_eq!(
            listed.migration.as_ref().unwrap().state,
            SessionMigrationState::Failed
        );

        let mut s = fixture_session();
        s.id = "retry-migrate".into();
        s.history.push(user_item("fixed prompt"));
        fs::write(
            dir.path().join("session.json"),
            serde_json::to_string(&s).expect("encode legacy session"),
        )
        .expect("write fixed legacy");

        let retry = migrate_session_dir_to_db(dir.path()).expect("retry succeeds");
        assert_eq!(retry, SessionMigrationOutcome::Migrated);
        assert!(dir.path().join("session.db").is_file());
        assert!(!dir
            .path()
            .join(crate::session_migration::MIGRATION_STATUS_FILE)
            .exists());
    }

    #[test]
    fn migration_batch_counts_progress_and_bounds_failures() {
        let root = tempfile::tempdir().expect("temp dir");
        let existing = root.path().join("existing");
        fs::create_dir(&existing).expect("existing dir");
        let mut s = fixture_session();
        s.id = "existing".into();
        let db = smelt_store::SessionDb::open(existing.join("session.db")).unwrap();
        db.save_session_snapshot(&session_store_snapshot(&s, 0).unwrap(), None)
            .unwrap();

        let good = root.path().join("good");
        fs::create_dir(&good).expect("good dir");
        s.id = "good".into();
        s.history.push(user_item("good prompt"));
        fs::write(
            good.join("session.json"),
            serde_json::to_string(&s).expect("encode legacy session"),
        )
        .expect("write good legacy");

        for idx in 0..7 {
            let bad = root.path().join(format!("bad-{idx}"));
            fs::create_dir(&bad).expect("bad dir");
            fs::write(bad.join("session.json"), b"not json").expect("write bad legacy");
        }

        assert_eq!(
            crate::session_migration::pending_session_migration_count_in_dir(root.path()),
            8
        );

        let report = crate::session_migration::migrate_all_sessions_in_dir(root.path());

        assert_eq!(report.scanned, 9);
        assert_eq!(report.migrated, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.failed, 7);
        assert_eq!(
            report.failures.len(),
            crate::session_migration::max_migration_failure_logs()
        );
        assert!(good.join("session.db").is_file());
        assert!(!good.join("session.json").exists());
        assert!(root.path().join("bad-0/session.json").is_file());
    }

    #[test]
    fn db_session_loads_without_history_jsonl() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = fixture_session();
        s.id = "db-session".into();
        s.title = Some("DB".into());
        s.history.push(user_item("hello sqlite"));
        s.record_context_tokens(42);
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        db.save_session_snapshot(&session_store_snapshot(&s, 0).unwrap(), None)
            .unwrap();

        let loaded = load_session_files(dir.path()).expect("load db session");
        assert_eq!(loaded.id, "db-session");
        assert_eq!(loaded.title.as_deref(), Some("DB"));
        assert_eq!(loaded.context_tokens, Some(42));
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
    fn legacy_messages_session_loads_with_no_user_display() {
        let json = serde_json::json!({
            "id": "legacy",
            "created_at_ms": 1,
            "updated_at_ms": 1,
            "messages": [{
                "role": "user",
                "content": "expanded command body"
            }]
        });

        let loaded: Session = serde_json::from_value(json).expect("deserialize legacy session");
        assert!(matches!(
            &loaded.history[0],
            HistoryItem::User { content, display: None }
                if content.text_content() == "expanded command body"
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
    fn install_context_checkpoint_clears_authoritative_context_tokens() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.record_context_tokens(500);

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(500));

        assert!(installed);
        assert!(s.context_tokens.is_none());
        assert!(s.context_tokens_history_len.is_none());
        assert_eq!(s.display_context_tokens, Some(500));
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
        s.record_context_tokens(710);
        s.clear_context_tokens_baseline();
        s.snapshot_context();

        s.history.extend([user_item("c"), assistant_text_item("d")]);
        s.record_context_tokens(700);
        s.snapshot_context();

        s.history.truncate(2);
        s.restore_context_after_rewind(2, false);

        assert_eq!(s.context_tokens, None);
        assert_eq!(s.context_tokens_history_len, None);
        assert_eq!(s.display_context_tokens(), Some(710));
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
    fn merge_model_history_snapshot_strips_injected_summary() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.checkpoint = Some(checkpoint("the summary", 2));

        s.merge_model_history_snapshot(
            "SUMMARY:",
            vec![
                user_item("SUMMARY:\nthe summary"),
                user_item("recent"),
                assistant_text_item("recent reply"),
                assistant_text_item("new reply"),
            ],
        );

        assert_eq!(
            s.history,
            vec![
                user_item("old"),
                assistant_text_item("old reply"),
                user_item("recent"),
                assistant_text_item("recent reply"),
                assistant_text_item("new reply"),
            ]
        );
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
    fn meta_projects_visible_session_fields() {
        let mut s = fixture_session();
        s.title = Some("My Title".into());
        s.slug = Some("my-slug".into());
        s.first_user_message = Some("hello".into());
        s.mode = Some("ask".into());
        s.model = Some("claude-opus".into());
        s.cwd = Some("/work".into());
        s.parent_id = Some("p1".into());
        s.context_tokens = Some(1234);
        s.history.push(user_item("hi"));
        s.context_tokens_history_len = Some(s.history.len());

        let m = s.meta();
        assert_eq!(m.id, s.id);
        assert_eq!(m.title.as_deref(), Some("My Title"));
        assert_eq!(m.slug.as_deref(), Some("my-slug"));
        assert_eq!(m.first_user_message.as_deref(), Some("hello"));
        assert_eq!(m.mode.as_deref(), Some("ask"));
        assert_eq!(m.model.as_deref(), Some("claude-opus"));
        assert_eq!(m.cwd.as_deref(), Some("/work"));
        assert_eq!(m.parent_id.as_deref(), Some("p1"));
        assert_eq!(m.context_tokens, Some(1234));
        assert_eq!(m.text_bytes, Some(2)); // "hi"
        assert!(m.migration.is_none());
    }

    #[test]
    fn current_context_tokens_requires_exact_history_length() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.record_context_tokens(100);
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
        s.record_context_tokens(100);

        s.clear_context_tokens_baseline();

        assert_eq!(s.context_tokens, None);
        assert_eq!(s.context_tokens_history_len, None);
        assert_eq!(s.display_context_tokens, Some(100));
    }

    #[test]
    fn fork_clones_history_and_links_parent_with_fresh_id() {
        let mut s = fixture_session();
        s.history.push(user_item("q1"));
        s.history.push(assistant_text_item("a1"));
        s.title = Some("kept".into());
        s.record_context_tokens(500);
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

    // ── compute_text_bytes / build_search_blob ───────────────────────

    #[test]
    fn compute_text_bytes_sums_user_assistant_content_lengths() {
        let items = vec![user_item("hello"), assistant_text_item("hi there")];
        assert_eq!(compute_text_bytes(&items), 13);
    }

    #[test]
    fn compute_text_bytes_includes_reasoning_and_tool_calls() {
        let inv = ToolInvocation {
            call_id: "id-1".into(),
            name: "edit".into(),
            arguments: "{\"path\":\"f.rs\"}".into(),
            result: ToolOutcome {
                content: "ok".into(),
                is_error: false,
                metadata: None,
            },
            elapsed_ms: None,
        };
        let turn = AssistantStep::with_invocations(
            Some(Content::Text("text".into())),
            Some("thinking".into()),
            Vec::new(),
            vec![inv],
        );
        let items = vec![HistoryItem::Assistant(turn)];
        // 4 (text) + 8 (reasoning) + 4 (name) + 15 (args) + 2 (result)
        assert_eq!(compute_text_bytes(&items), 33);
    }

    #[test]
    fn compute_text_bytes_handles_empty_input() {
        assert_eq!(compute_text_bytes(&[]), 0);
    }

    #[test]
    fn build_search_blob_includes_only_user_and_assistant_text() {
        let items = vec![
            user_item("question"),
            assistant_text_item("answer"),
            system_item("system prompt"),
        ];
        let blob = build_search_blob(&items);
        assert!(blob.contains("question"));
        assert!(blob.contains("answer"));
        assert!(!blob.contains("system prompt"));
    }

    #[test]
    fn build_search_blob_skips_empty_history_items() {
        let items = vec![user_item(""), assistant_text_item("real")];
        let blob = build_search_blob(&items);
        assert_eq!(blob, "real\n");
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
            cwd: None,
            parent_id: None,
            context_tokens: None,
            text_bytes: None,
            migration: None,
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
    fn legacy_session_with_orphan_tool_use_is_repaired_on_deserialize() {
        // Real reproducer for issue #8: a session file persisted mid-tool
        // with an unpaired tool_use should load with a synthesized
        // "interrupted" tool result.
        let json = serde_json::json!({
            "id": "abc",
            "messages": [
                { "role": "user", "content": "go" },
                {
                    "role": "assistant",
                    "tool_calls": [
                        { "id": "web_fetch:36", "type": "function",
                          "function": { "name": "web_fetch", "arguments": "{}" } }
                    ]
                }
            ]
        });
        let s: Session = serde_json::from_value(json).unwrap();
        let assistant = s
            .history
            .iter()
            .find_map(|i| i.as_assistant())
            .expect("assistant item");
        assert_eq!(assistant.invocations.len(), 1);
        assert!(assistant.invocations[0].result.is_error);
        assert!(assistant.invocations[0]
            .result
            .content
            .contains("interrupted"));
    }

    #[test]
    fn legacy_session_with_token_snapshots_loads_without_error() {
        // Old session files may contain `token_snapshots`. The field is no
        // longer used, but deserialization should not fail.
        let json = serde_json::json!({
            "id": "abc",
            "messages": [
                { "role": "user", "content": "go" },
                {
                    "role": "assistant",
                    "tool_calls": [
                        { "id": "c1", "type": "function",
                          "function": { "name": "f", "arguments": "{}" } }
                    ]
                },
                { "role": "tool", "tool_call_id": "c1", "content": "ok" }
            ],
            "token_snapshots": [[3, 100]],
            "cost_snapshots": [[3, 0.5]]
        });
        let s: Session = serde_json::from_value(json).unwrap();
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.context_snapshots.len(), 1);
        assert_eq!(s.context_snapshots[0].0, 2);
        assert_eq!(s.session_cost_usd, 0.5);
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
        let snapshot = &value["accounting_snapshots"][0][1];
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
