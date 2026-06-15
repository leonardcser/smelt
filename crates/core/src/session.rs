use crate::config;
use protocol::{
    history_from_messages, history_item_message_count, message_to_history_positions, HistoryItem,
    Message, ReasoningEffort, TokenUsage, TurnMeta,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

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
pub struct AccountingSnapshot {
    #[serde(default)]
    pub cost_usd: f64,
    pub session_usage: TokenUsage,
    pub context_tokens: Option<u32>,
    pub context_tokens_history_len: Option<usize>,
    pub checkpoint: Option<ContextSnapshotKey>,
}

impl AccountingSnapshot {
    fn from_session(session: &Session) -> Self {
        Self {
            cost_usd: session.session_cost_usd,
            session_usage: session.session_usage.clone(),
            context_tokens: session.context_tokens,
            context_tokens_history_len: session.context_tokens_history_len,
            checkpoint: session.checkpoint_snapshot_key(),
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
    /// Per-turn metadata, keyed by `history.len()` at turn-complete time.
    pub turn_metas: Vec<(usize, TurnMeta)>,
    /// Cost and token accounting snapshots, keyed by `history.len()` at
    /// turn-complete time. Used to restore usage and context baselines after rewind.
    pub accounting_snapshots: Vec<(usize, AccountingSnapshot)>,
    /// Running session cost in USD; updated incrementally as token usage events arrive.
    pub session_cost_usd: f64,
    /// Cumulative token usage across every turn this session has made;
    /// distinct from the per-turn `context_tokens` snapshot.
    pub session_usage: TokenUsage,
}

const CURRENT_SESSION_SCHEMA_VERSION: u32 = 2;

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
    pub cost_snapshots: Vec<(usize, f64)>,
    #[serde(default)]
    pub turn_metas: Vec<(usize, TurnMeta)>,
    #[serde(default)]
    pub accounting_snapshots: Vec<(usize, AccountingSnapshot)>,
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
    pub turn_metas: Vec<(usize, TurnMeta)>,
    #[serde(default)]
    pub accounting_snapshots: Vec<(usize, AccountingSnapshot)>,
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
) -> Vec<(usize, T)> {
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
        .collect()
}

fn truncate_snapshots_after<T>(snapshots: &mut Vec<(usize, T)>, hist_idx: usize) {
    while snapshots.last().is_some_and(|(len, _)| *len > hist_idx) {
        snapshots.pop();
    }
}

// COMPAT(session-v1-messages): rebuild accounting from old cost-only snapshots.
fn legacy_accounting_snapshots(
    cost_snapshots: Vec<(usize, f64)>,
    context_tokens: Option<u32>,
    context_tokens_history_len: Option<usize>,
) -> Vec<(usize, AccountingSnapshot)> {
    cost_snapshots
        .into_iter()
        .map(|(len, cost_usd)| {
            let (context_tokens, context_tokens_history_len) =
                if context_tokens_history_len == Some(len) {
                    (context_tokens, context_tokens_history_len)
                } else {
                    (None, None)
                };
            (
                len,
                AccountingSnapshot {
                    cost_usd,
                    session_usage: TokenUsage::default(),
                    context_tokens,
                    context_tokens_history_len,
                    checkpoint: None,
                },
            )
        })
        .collect()
}

impl From<SessionWire> for Session {
    fn from(w: SessionWire) -> Self {
        let table = message_to_history_positions(&w.messages);
        let history = history_from_messages(w.messages);
        let hist_len = history.len();
        let context_tokens = w.context_tokens;
        let context_tokens_history_len = w.context_tokens_history_len;
        let cost_snapshots = remap_msg_to_hist(&w.cost_snapshots, &table, hist_len);
        let accounting_snapshots = remap_msg_to_hist(&w.accounting_snapshots, &table, hist_len);
        let accounting_snapshots = if accounting_snapshots.is_empty() {
            legacy_accounting_snapshots(cost_snapshots, context_tokens, context_tokens_history_len)
        } else {
            accounting_snapshots
        };
        let session_cost_usd = if w.session_cost_usd == 0.0 {
            accounting_snapshots
                .last()
                .map(|(_, snapshot)| snapshot.cost_usd)
                .unwrap_or(w.session_cost_usd)
        } else {
            w.session_cost_usd
        };
        Self {
            id: w.id,
            title: w.title,
            slug: w.slug,
            first_user_message: w.first_user_message,
            created_at_ms: w.created_at_ms,
            updated_at_ms: w.updated_at_ms,
            mode: w.mode,
            reasoning_effort: w.reasoning_effort,
            model: w.model,
            cwd: w.cwd,
            parent_id: w.parent_id,
            turn_metas: remap_msg_to_hist(&w.turn_metas, &table, hist_len),
            accounting_snapshots,
            history,
            checkpoint: w.checkpoint,
            context_tokens,
            context_tokens_history_len,
            session_cost_usd,
            session_usage: w.session_usage,
        }
    }
}

impl From<SessionWireV2> for Session {
    fn from(w: SessionWireV2) -> Self {
        let context_tokens = w.context_tokens;
        Self {
            id: w.id,
            title: w.title,
            slug: w.slug,
            first_user_message: w.first_user_message,
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
            turn_metas: w.turn_metas,
            accounting_snapshots: w.accounting_snapshots,
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
            turn_metas: s.turn_metas.clone(),
            accounting_snapshots: s.accounting_snapshots.clone(),
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
    /// Populated in `meta.json` so the resume dialog avoids loading `session.json`.
    #[serde(default)]
    pub text_bytes: Option<u64>,
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
            turn_metas: Vec::new(),
            accounting_snapshots: Vec::new(),
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
            context_tokens: self.current_context_tokens(),
            text_bytes: Some(compute_text_bytes(&self.history)),
        }
    }

    pub fn record_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.context_tokens_history_len = Some(self.history.len());
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
    }

    pub fn clear_context_tokens_baseline(&mut self) {
        self.clear_context_tokens();
    }

    pub fn current_context_tokens(&self) -> Option<u32> {
        (self.context_tokens_history_len == Some(self.history.len()))
            .then_some(self.context_tokens)
            .flatten()
    }

    pub fn checkpoint_snapshot_key(&self) -> Option<ContextSnapshotKey> {
        self.checkpoint.as_ref().map(ContextSnapshotKey::from)
    }

    pub fn snapshot_accounting(&mut self) {
        let snapshot = AccountingSnapshot::from_session(self);
        self.accounting_snapshots
            .push((self.history.len(), snapshot));
    }

    pub fn clear_accounting_snapshots(&mut self) {
        self.accounting_snapshots.clear();
        self.session_cost_usd = 0.0;
        self.session_usage = TokenUsage::default();
    }

    pub fn restore_accounting_after_rewind(
        &mut self,
        hist_idx: usize,
        keep_checkpoint_at_boundary: bool,
    ) {
        let checkpoint_fallback =
            self.clear_checkpoint_for_rewind(hist_idx, keep_checkpoint_at_boundary);
        truncate_snapshots_after(&mut self.accounting_snapshots, hist_idx);
        if let Some((_, snapshot)) = self.accounting_snapshots.last().cloned() {
            self.apply_accounting_snapshot(&snapshot);
        } else {
            self.session_cost_usd = 0.0;
            self.session_usage = TokenUsage::default();
        }
        self.restore_context_tokens_after_rewind(hist_idx, checkpoint_fallback);
    }

    pub fn prune_accounting_snapshots(&mut self, hist_idx: usize) {
        truncate_snapshots_after(&mut self.accounting_snapshots, hist_idx);
        if let Some((_, snapshot)) = self.accounting_snapshots.last().cloned() {
            self.apply_accounting_snapshot(&snapshot);
            self.restore_context_tokens_after_rewind(hist_idx, None);
        } else {
            self.session_cost_usd = 0.0;
            self.session_usage = TokenUsage::default();
            if self
                .context_tokens_history_len
                .is_some_and(|len| len > hist_idx)
            {
                self.clear_context_tokens();
            }
        }
    }

    fn apply_accounting_snapshot(&mut self, snapshot: &AccountingSnapshot) {
        self.session_cost_usd = snapshot.cost_usd;
        self.session_usage = snapshot.session_usage.clone();
    }

    fn restore_context_tokens_after_rewind(
        &mut self,
        hist_idx: usize,
        checkpoint_fallback: Option<(Option<u32>, Option<usize>)>,
    ) {
        let checkpoint = self.checkpoint_snapshot_key();
        let snapshot = self
            .accounting_snapshots
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
        } else if let Some((tokens, Some(history_len))) = checkpoint_fallback {
            if history_len <= hist_idx {
                self.context_tokens = tokens;
                self.context_tokens_history_len = Some(history_len);
            } else {
                self.clear_context_tokens();
            }
        } else if self.accounting_snapshots.is_empty()
            && self
                .context_tokens_history_len
                .is_some_and(|len| len <= hist_idx)
        {
            // COMPAT(session-v1-messages): old sessions may not have accounting
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
        self.snapshot_accounting();
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
            turn_metas: self.turn_metas.clone(),
            accounting_snapshots: self.accounting_snapshots.clone(),
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

/// Write `session.json` + `meta.json`. Assumes blobs are already flushed.
/// Safe to call from a background thread.
pub fn save_with_blobs(session: &Session, url_to_blob: &std::collections::HashMap<String, String>) {
    let _perf = smelt_perf::perf::begin("session:write");
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let ts = now_ms();

    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.history, url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    if let Ok(json) = serde_json::to_string(&*session_out) {
        atomic_write(&session_dir.join("session.json"), json.as_bytes(), ts);
    }
    let meta = session_out.meta();
    if let Ok(json) = serde_json::to_string(&meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), ts);
    }
    let blob = build_search_blob(&session_out.history);
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), ts);
}

/// Write `contents` to `path` atomically via a tmp file + rename.
fn atomic_write(path: &std::path::Path, contents: &[u8], ts: u64) {
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
    let id = resolve_prefix(id_or_prefix)?;
    load_exact(&id)
}

fn load_exact(id: &str) -> Option<Session> {
    let dir_path = sessions_dir().join(id);
    let contents = fs::read_to_string(dir_path.join("session.json")).ok()?;
    let mut session: Session = serde_json::from_str(&contents).ok()?;

    let blob_dir = dir_path.join("blobs");
    if blob_dir.is_dir() {
        let blob_to_url = crate::attachment::AttachmentStore::load_blobs(&blob_dir);
        if !blob_to_url.is_empty() {
            internalize_blobs(&mut session.history, &blob_to_url);
        }
    }
    Some(session)
}

/// Returns `None` when no match or prefix is ambiguous.
fn resolve_prefix(prefix: &str) -> Option<String> {
    let dir = sessions_dir();

    if dir.join(prefix).join("session.json").is_file() {
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

/// COMPAT(session-search-sidecar-missing): uses `meta.json` when present;
/// falls back to `session.json` and regenerates the sidecar for older sessions.
fn load_meta_for_dir(path: PathBuf) -> Option<SessionMeta> {
    if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
        if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) {
            if meta.text_bytes.is_none() {
                backfill_text_bytes(&path, &mut meta);
            }
            return Some(meta);
        }
    }
    let contents = fs::read_to_string(path.join("session.json")).ok()?;
    let session: Session = serde_json::from_str(&contents).ok()?;
    let mut meta = session.meta();
    if meta.id.is_empty() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            meta.id = name.to_string();
        }
    }
    write_meta(&path, &meta);
    Some(meta)
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
    let Ok(contents) = fs::read_to_string(session_dir.join("session.json")) else {
        return;
    };
    let Ok(session) = serde_json::from_str::<Session>(&contents) else {
        return;
    };
    let bytes = compute_text_bytes(&session.history);
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
/// COMPAT(session-search-sidecar-missing): falls back to regenerating from
/// `session.json` and caching to disk when the `content.txt` sidecar is missing.
pub fn load_search_blob(id: &str) -> Option<String> {
    let _perf = smelt_perf::perf::begin("session:load_search_blob");
    let session_dir = sessions_dir().join(id);
    if let Ok(contents) = fs::read_to_string(session_dir.join("content.txt")) {
        return Some(contents);
    }
    let full = fs::read_to_string(session_dir.join("session.json")).ok()?;
    let session: Session = serde_json::from_str(&full).ok()?;
    let blob = build_search_blob(&session.history);
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), now_ms());
    Some(blob)
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

fn sessions_dir() -> PathBuf {
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
        s.context_tokens = Some(500);
        s.context_tokens_history_len = Some(4);

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(500));

        assert!(installed);
        assert!(s.context_tokens.is_none());
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
    fn restore_accounting_after_rewind_restores_authoritative_context_snapshot() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.context_tokens = Some(710);
        s.context_tokens_history_len = Some(2);
        s.snapshot_accounting();

        s.history.extend([user_item("c"), assistant_text_item("d")]);
        s.context_tokens = Some(700);
        s.context_tokens_history_len = Some(4);
        s.snapshot_accounting();

        s.history.truncate(2);
        s.restore_accounting_after_rewind(2, false);

        assert_eq!(s.context_tokens, Some(710));
        assert_eq!(s.context_tokens_history_len, Some(2));
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
    }

    #[test]
    fn current_context_tokens_requires_exact_history_length() {
        let mut s = fixture_session();
        s.history = vec![user_item("a"), assistant_text_item("b")];
        s.context_tokens = Some(100);
        s.context_tokens_history_len = Some(2);
        assert_eq!(s.current_context_tokens(), Some(100));

        s.history.push(user_item("c"));
        assert_eq!(s.current_context_tokens(), None);
    }

    #[test]
    fn fork_clones_history_and_links_parent_with_fresh_id() {
        let mut s = fixture_session();
        s.history.push(user_item("q1"));
        s.history.push(assistant_text_item("a1"));
        s.title = Some("kept".into());
        s.context_tokens = Some(500);
        s.context_tokens_history_len = Some(2);
        s.session_cost_usd = 1.25;

        let forked = s.fork(4242);
        assert_ne!(forked.id, s.id);
        assert_eq!(forked.parent_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(forked.title.as_deref(), Some("kept"));
        assert_eq!(forked.history.len(), s.history.len());
        assert_eq!(forked.context_tokens, Some(500));
        assert_eq!(forked.context_tokens_history_len, Some(2));
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
        assert_eq!(s.accounting_snapshots.len(), 1);
        assert_eq!(s.accounting_snapshots[0].0, 2);
        assert_eq!(s.accounting_snapshots[0].1.cost_usd, 0.5);
        assert_eq!(s.session_cost_usd, 0.5);
    }

    #[test]
    fn session_round_trips_through_wire_form_preserving_history_and_snapshots() {
        // Verify lossless save → load → save: native history rows,
        // snapshot keys, costs, and context tokens all survive a
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
        original.snapshot_accounting();

        let json = serde_json::to_string(&original).unwrap();
        let round: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(round.history, original.history);
        assert_eq!(
            round.accounting_snapshots.len(),
            original.accounting_snapshots.len()
        );
        assert_eq!(round.context_tokens, original.context_tokens);
        assert_eq!(
            round.context_tokens_history_len,
            original.context_tokens_history_len
        );
        assert_eq!(round.accounting_snapshots.len(), 1);
        assert_eq!(round.accounting_snapshots[0].0, 3);
        assert_eq!(round.accounting_snapshots[0].1.context_tokens, Some(200));
        assert_eq!(round.accounting_snapshots[0].1.cost_usd, 1.25);
        assert_eq!(
            round.accounting_snapshots[0].1.session_usage.prompt_tokens,
            Some(10)
        );
        assert_eq!(round.session_cost_usd, original.session_cost_usd);
        assert_eq!(round.id, original.id);
    }

    #[test]
    fn round_trip_preserves_tool_elapsed_metadata() {
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
        assert_eq!(restored_inv.elapsed_ms, Some(42));
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
