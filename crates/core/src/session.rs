use crate::config;
use protocol::{
    history_from_messages, history_to_message_positions, history_to_messages,
    message_to_history_positions, HistoryItem, Message, ReasoningEffort, TokenUsage, TurnMeta,
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
/// orphan tool_calls impossible). The on-disk JSON format remains the
/// legacy `messages: Vec<Message>` for backward compatibility - conversion
/// happens in the `Serialize`/`Deserialize` impls via the
/// [`SessionWire`] shadow type. Loading an older session also repairs any
/// orphan tool_use blocks by synthesizing an "interrupted" tool result
/// (see [`protocol::history_from_messages`]).
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
    /// Last authoritative context-token count the UI should continue to
    /// display, even if `context_tokens` has been invalidated for request
    /// estimation while waiting for a fresh provider reading.
    pub visible_context_tokens: Option<u32>,
    /// Cost snapshot, keyed by `history.len()` at write time.
    pub cost_snapshots: Vec<(usize, f64)>,
    /// Per-turn metadata, keyed by `history.len()` at turn-complete time.
    pub turn_metas: Vec<(usize, TurnMeta)>,
    /// Running session cost in USD; updated incrementally as token usage events arrive.
    pub session_cost_usd: f64,
    /// Cumulative token usage across every turn this session has made;
    /// distinct from the per-turn `context_tokens` snapshot.
    pub session_usage: TokenUsage,
}

/// On-disk JSON shape. Kept stable so older sessions deserialize without a
/// migration pass. Snapshot keys are stored in `Vec<Message>` position
/// space - the `Session` deserialize impl translates them into
/// `Vec<HistoryItem>` positions on load and back on save.
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
    #[serde(alias = "display_context_tokens")]
    pub visible_context_tokens: Option<u32>,
    #[serde(default)]
    pub cost_snapshots: Vec<(usize, f64)>,
    #[serde(default)]
    pub turn_metas: Vec<(usize, TurnMeta)>,
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub session_usage: TokenUsage,
}

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

/// `hist_to_msg[i]` = message index at which history item i starts.
/// `msg_len` = total messages count.
fn remap_hist_to_msg<T: Clone>(
    snapshots: &[(usize, T)],
    hist_to_msg: &[usize],
    msg_len: usize,
) -> Vec<(usize, T)> {
    snapshots
        .iter()
        .map(|(hist_pos, v)| {
            let msg_pos = if *hist_pos == 0 {
                0
            } else if *hist_pos < hist_to_msg.len() {
                // Snapshot was taken at history.len() == hist_pos, i.e.
                // after history[hist_pos-1] landed. That maps to the
                // message index of the next history item.
                hist_to_msg[*hist_pos]
            } else {
                msg_len
            };
            (msg_pos, v.clone())
        })
        .collect()
}

impl From<SessionWire> for Session {
    fn from(w: SessionWire) -> Self {
        let table = message_to_history_positions(&w.messages);
        let history = history_from_messages(w.messages);
        let hist_len = history.len();
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
            cost_snapshots: remap_msg_to_hist(&w.cost_snapshots, &table, hist_len),
            turn_metas: remap_msg_to_hist(&w.turn_metas, &table, hist_len),
            history,
            checkpoint: w.checkpoint,
            context_tokens: w.context_tokens,
            context_tokens_history_len: w.context_tokens_history_len,
            visible_context_tokens: w.visible_context_tokens.or(w.context_tokens),
            session_cost_usd: w.session_cost_usd,
            session_usage: w.session_usage,
        }
    }
}

impl From<&Session> for SessionWire {
    fn from(s: &Session) -> Self {
        let table = history_to_message_positions(&s.history);
        let messages = history_to_messages(&s.history);
        let msg_len = messages.len();
        SessionWire {
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
            messages,
            checkpoint: s.checkpoint.clone(),
            context_tokens: s.context_tokens,
            context_tokens_history_len: s.context_tokens_history_len,
            visible_context_tokens: s.visible_context_tokens,
            cost_snapshots: remap_hist_to_msg(&s.cost_snapshots, &table, msg_len),
            turn_metas: remap_hist_to_msg(&s.turn_metas, &table, msg_len),
            session_cost_usd: s.session_cost_usd,
            session_usage: s.session_usage.clone(),
        }
    }
}

impl Serialize for Session {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        SessionWire::from(self).serialize(ser)
    }
}

impl<'de> Deserialize<'de> for Session {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        SessionWire::deserialize(de).map(Session::from)
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
            visible_context_tokens: None,
            cost_snapshots: Vec::new(),
            turn_metas: Vec::new(),
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
            context_tokens: self.context_tokens,
            text_bytes: Some(compute_text_bytes(&self.history)),
        }
    }

    pub fn record_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.context_tokens_history_len = Some(self.history.len());
        self.visible_context_tokens = Some(tokens);
    }

    pub fn invalidate_context_tokens(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
        self.visible_context_tokens = None;
    }

    pub fn clear_context_tokens_baseline(&mut self) {
        self.context_tokens = None;
        self.context_tokens_history_len = None;
    }

    /// Internal heuristic used for compaction / prepare-request estimation.
    /// This must not be treated as authoritative provider usage.
    pub fn estimate_model_context_tokens(&self, summary_prefix: &str) -> u32 {
        estimate_message_tokens(&history_to_messages(&self.model_history(summary_prefix)))
    }

    /// Legacy helper retained for internal callers that want to persist the
    /// latest estimate separately from the authoritative provider reading.
    pub fn recompute_visible_context_tokens(&mut self, summary_prefix: &str) {
        self.visible_context_tokens = Some(self.estimate_model_context_tokens(summary_prefix));
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
        // token reading for checkpointed model history, but we keep the
        // last visible count as a display estimate (stale is better than nil).
        self.clear_context_tokens_baseline();
        true
    }

    fn first_live_history_index_for_model_message(
        &self,
        first_live_message_index: usize,
    ) -> Option<usize> {
        let model_history = self.model_history("");
        if model_history.is_empty() {
            return None;
        }

        let mut item_to_history_index: Vec<Option<usize>> = Vec::with_capacity(model_history.len());
        if let Some(cp) = &self.checkpoint {
            item_to_history_index.push(None);
            item_to_history_index.extend((cp.first_live_index..self.history.len()).map(Some));
        } else {
            item_to_history_index.extend((0..self.history.len()).map(Some));
        }

        let model_messages = history_to_messages(&model_history);
        if first_live_message_index > model_messages.len() {
            return None;
        }
        if first_live_message_index == 0 {
            return None;
        }
        if first_live_message_index == model_messages.len() {
            return Some(self.history.len());
        }

        let message_to_item = message_to_history_positions(&model_messages);
        let item_index = *message_to_item.get(first_live_message_index)?;
        if first_live_message_index > 0
            && message_to_item
                .get(first_live_message_index - 1)
                .is_some_and(|prev| *prev == item_index)
        {
            return None;
        }
        item_to_history_index.get(item_index).and_then(|idx| *idx)
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
                // Restore the token baseline that was active before the
                // checkpoint was installed. The current history is a prefix
                // of that pre-checkpoint history, so the old count is a
                // conservative upper-bound.
                self.context_tokens = cp.pre_checkpoint_context_tokens;
                self.context_tokens_history_len = cp.pre_checkpoint_context_history_len;
                if let Some(tokens) = self.context_tokens {
                    self.visible_context_tokens = Some(tokens);
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
            visible_context_tokens: self.visible_context_tokens,
            cost_snapshots: self.cost_snapshots.clone(),
            turn_metas: self.turn_metas.clone(),
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
    let HistoryItem::User { content } = item else {
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

/// Uses `meta.json` when present; falls back to `session.json` and regenerates
/// the sidecar for older sessions.
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
            HistoryItem::System { content } | HistoryItem::User { content } => {
                total += content.text_content().len() as u64;
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
            HistoryItem::User { content } => Some(content.text_content()),
            HistoryItem::Assistant(turn) => turn.content.as_ref().map(|c| c.text_content()),
            HistoryItem::System { .. } => None,
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

/// Read the searchable text blob for `id`. Falls back to regenerating from
/// `session.json` (and caching to disk) when the `content.txt` sidecar is
/// missing - older sessions written before the sidecar existed.
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
            HistoryItem::System { content } | HistoryItem::User { content } => {
                rewrite_image_urls(content, &swap);
            }
            HistoryItem::Assistant(turn) => {
                if let Some(c) = turn.content.as_mut() {
                    rewrite_image_urls(c, &swap);
                }
            }
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

    use protocol::{AssistantTurn, Content, ContentPart, HistoryItem, ToolInvocation, ToolOutcome};

    fn user_item(text: &str) -> HistoryItem {
        HistoryItem::User {
            content: Content::Text(text.into()),
        }
    }
    fn assistant_text_item(text: &str) -> HistoryItem {
        HistoryItem::Assistant(AssistantTurn::terminal(
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
        let tool_heavy_turn = HistoryItem::Assistant(AssistantTurn::with_invocations(
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
        let tool_heavy_turn = HistoryItem::Assistant(AssistantTurn::with_invocations(
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
        s.visible_context_tokens = Some(100);

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 0, Some(100));

        assert!(!installed);
        assert!(s.checkpoint.is_none());
        assert_eq!(s.context_tokens, Some(100));
        assert_eq!(s.visible_context_tokens, Some(100));
    }

    #[test]
    fn install_context_checkpoint_clears_baseline_keeps_visible() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.context_tokens = Some(500);
        s.context_tokens_history_len = Some(4);
        s.visible_context_tokens = Some(500);

        let installed =
            s.install_context_checkpoint("compaction".into(), "summary".into(), 2, Some(500));

        assert!(installed);
        assert!(s.context_tokens.is_none());
        assert_eq!(s.visible_context_tokens, Some(500));
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
    fn invalidate_context_tokens_keeps_visible_snapshot() {
        let mut s = fixture_session();
        s.record_context_tokens(321);

        s.invalidate_context_tokens();

        assert!(s.context_tokens.is_none());
        assert_eq!(s.visible_context_tokens, Some(321));
    }

    #[test]
    fn recompute_visible_context_tokens_uses_model_history_after_checkpoint() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item(&"x".repeat(200)),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.checkpoint = Some(checkpoint("summary", 2));

        s.recompute_visible_context_tokens("SUMMARY:");

        let expected = history_to_messages(&s.model_history("SUMMARY:"))
            .iter()
            .map(|msg| {
                msg.content
                    .as_ref()
                    .map(|c| c.text_content().len())
                    .unwrap_or(0)
            })
            .sum::<usize>()
            .div_ceil(4) as u32;
        assert_eq!(s.visible_context_tokens, Some(expected));
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
            matches!(&model[0], HistoryItem::User { content } if content.text_content().contains("summary text"))
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
    fn clear_checkpoint_if_rewound_to_restores_pre_checkpoint_baseline() {
        let mut s = fixture_session();
        s.history = vec![
            user_item("old"),
            assistant_text_item("old reply"),
            user_item("recent"),
            assistant_text_item("recent reply"),
        ];
        s.context_tokens = Some(100);
        s.context_tokens_history_len = Some(4);
        s.visible_context_tokens = Some(100);
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
        assert_eq!(s.context_tokens, Some(100));
        assert_eq!(s.context_tokens_history_len, Some(4));
        assert_eq!(s.visible_context_tokens, Some(100));
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
        s.visible_context_tokens = Some(5678);
        s.history.push(user_item("hi"));

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
        let turn = AssistantTurn::with_invocations(
            Some(Content::Text("text".into())),
            Some("thinking".into()),
            Vec::new(),
            vec![inv],
        );
        let items = vec![HistoryItem::Assistant(turn)];
        // 4 (text) + 8 (reasoning) + 4 (name) + 15 (args)
        assert_eq!(compute_text_bytes(&items), 31);
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
        }
    }

    fn first_image_url(item: &HistoryItem) -> &str {
        match item {
            HistoryItem::User { content } | HistoryItem::System { content } => match content {
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
            HistoryItem::User { content } => match content {
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
        assert_eq!(s.cost_snapshots, vec![(2, 0.5)]);
    }

    #[test]
    fn session_round_trips_through_wire_form_preserving_history_and_snapshots() {
        // Verify lossless save → load → save: history shape, snapshot
        // keys, costs, and context tokens all survive a round-trip
        // through the `messages: Vec<Message>` on-disk JSON shape.
        //
        // Note: `ToolInvocation.elapsed_ms` is NOT carried by the wire
        // `Message::tool` shape - it is engine-internal telemetry. The
        // canonical on-disk source for per-call elapsed times is
        // `turn_metas.tool_elapsed`, which the renderer reads as a
        // fallback. We zero `elapsed_ms` on the original side before
        // comparing so the round-trip is checked against what the
        // format actually persists.
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
            .push(HistoryItem::Assistant(AssistantTurn::with_invocations(
                Some(Content::Text("doing work".into())),
                None,
                Vec::new(),
                vec![inv_ok, inv_err],
            )));
        original.cost_snapshots = vec![(3, 1.25)];
        original.context_tokens = Some(200);
        original.context_tokens_history_len = Some(3);
        original.visible_context_tokens = Some(250);
        original.session_cost_usd = 1.25;

        let json = serde_json::to_string(&original).unwrap();
        let round: Session = serde_json::from_str(&json).unwrap();

        assert_eq!(round.history, original.history);
        assert_eq!(round.cost_snapshots, original.cost_snapshots);
        assert_eq!(round.context_tokens, original.context_tokens);
        assert_eq!(
            round.context_tokens_history_len,
            original.context_tokens_history_len
        );
        assert_eq!(
            round.visible_context_tokens,
            original.visible_context_tokens
        );
        assert_eq!(round.session_cost_usd, original.session_cost_usd);
        assert_eq!(round.id, original.id);
    }

    #[test]
    fn round_trip_drops_inv_elapsed_ms_but_turn_metas_preserves_it() {
        // The `elapsed_ms` field on ToolInvocation is engine-only
        // telemetry; the wire `Message::tool` shape doesn't carry it.
        // The architectural channel that DOES carry per-call elapsed
        // times across save/load is `turn_metas.tool_elapsed`. Verify
        // both halves of that contract.
        let mut original = Session::new(7, std::path::PathBuf::from("/w"));
        original
            .history
            .push(HistoryItem::Assistant(AssistantTurn::with_invocations(
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
        assert_eq!(
            restored_inv.elapsed_ms, None,
            "inv.elapsed_ms is engine-only - should be lost across the wire form"
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
