//! Transcript domain model: content-addressed block store, layout cache,
//! and mutable sidecar state (tool output, exec output). Held inside
//! `app::transcript::Transcript`, which adds streaming and paint orchestration.

use crate::paused_timer::PausedTimer;
use crate::permissions::PermissionGrant;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Handle to an in-flight tool call; full mutable state lives in `tool_states[call_id]`.
pub struct ActiveTool {
    pub call_id: String,
    pub(crate) block_id: BlockId,
    timer: PausedTimer,
}

impl ActiveTool {
    pub fn new(call_id: String, block_id: BlockId, start_time: Instant) -> Self {
        Self {
            call_id,
            block_id,
            timer: PausedTimer::new(start_time),
        }
    }

    pub fn elapsed_at(&self, now: Instant) -> Duration {
        self.timer.elapsed_at(now)
    }

    pub fn pause(&mut self, now: Instant) {
        self.timer.pause(now);
    }

    pub fn resume(&mut self, now: Instant) {
        self.timer.resume(now);
    }
}

#[derive(Clone)]
pub struct ConfirmRequest {
    pub call_id: String,
    pub tool_name: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub approval_candidates: Vec<String>,
    pub grant_options: Vec<ConfirmApprovalOption>,
    /// Styled summary of the pending call. Sole source for the dialog
    /// body header.
    pub summary: protocol::StyledLines,
    pub request_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Pending,
    Confirm,
    Ok,
    Err,
    Denied,
}

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::Confirm => "confirm",
            ToolStatus::Ok => "ok",
            ToolStatus::Err => "err",
            ToolStatus::Denied => "denied",
        }
    }

    pub fn hl_group(self) -> &'static str {
        match self {
            ToolStatus::Pending => "SmeltToolPending",
            ToolStatus::Ok => "SmeltSuccess",
            ToolStatus::Err | ToolStatus::Denied => "ErrorMsg",
            ToolStatus::Confirm => "SmeltAccent",
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<serde_json::Value>,
}

pub type ToolOutputRef = Box<ToolOutput>;

/// Mutable sidecar for a committed `Block::ToolCall`, keyed by `call_id`.
/// Splitting mutable fields out keeps `Block::ToolCall` immutable so its
/// layout can be cached permanently.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolState {
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    pub output: Option<ToolOutputRef>,
    pub user_message: Option<String>,
}

impl ToolState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ToolStatus::Ok | ToolStatus::Err | ToolStatus::Denied
        )
    }

    pub fn display_hash(&self) -> u64 {
        #[derive(serde::Serialize)]
        struct DisplayState<'a> {
            status: ToolStatus,
            output: &'a Option<ToolOutputRef>,
            user_message: &'a Option<String>,
        }

        crate::utils::hash_serializable(&DisplayState {
            status: self.status,
            output: &self.output,
            user_message: &self.user_message,
        })
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Block {
    User {
        text: String,
        /// Accent-highlighted in the rendered message.
        image_labels: Vec<String>,
    },
    Mode {
        text: String,
        icon: String,
        hl_group: String,
    },
    ProcessStatus {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        event: Option<protocol::ProcessStatusEvent>,
    },
    Thinking {
        content: String,
    },
    Text {
        content: String,
    },
    CodeLine {
        content: String,
        lang: String,
    },
    ToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: String,
        /// Styled best-effort summary produced from partial arguments.
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        raw_arguments: String,
        finished: bool,
    },
    ToolCall {
        call_id: String,
        name: String,
        /// Styled summary, produced by the tool's `summary(args)` Lua
        /// hook. The renderer consumes the styled spans; for plain-text
        /// callers (copy, search, snapshots) call `summary.as_plain_text()`.
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
    },
    Exec {
        command: String,
        output: String,
    },
    Compacted {
        summary: String,
    },
}

impl Block {
    pub fn kind(&self) -> &'static str {
        match self {
            Block::User { .. } => "user",
            Block::Mode { .. } => "mode",
            Block::ProcessStatus { .. } => "process_status",
            Block::Thinking { .. } => "thinking",
            Block::Text { .. } => "assistant",
            Block::CodeLine { .. } => "code",
            Block::ToolDraft { .. } | Block::ToolCall { .. } => "tool",
            Block::Exec { .. } => "exec",
            Block::Compacted { .. } => "compacted",
        }
    }

    /// Stable content hash of this block. Two blocks with the same
    /// content hash produce identical `LayoutIr` for the same
    /// `LayoutKey` and `ToolState`. For `ToolCall`, `ToolState` (status
    /// / output / elapsed) is deliberately *not* hashed - mutable tool
    /// state lives separately and is invalidated via
    /// `BlockHistory::invalidate_block_layout`.
    pub fn content_hash(&self) -> u64 {
        crate::utils::hash_serializable(self)
    }

    /// Raw source text for the block, before markdown rendering. Used
    /// by whole-block yank so copying a rendered markdown block returns
    /// the original `**bold**`, `` `code` ``, fenced ```` ``` ```` blocks,
    /// `|` tables, `---` rules, etc. - instead of walking display cells
    /// (which strips inline markup).
    ///
    /// Returns `None` for structured blocks (tool calls,
    /// confirm dialogs) that don't have a single "markdown source"; the
    /// caller falls back to cell-walking for those.
    pub fn raw_text(&self) -> Option<String> {
        match self {
            Block::User { text, .. } => Some(text.clone()),
            Block::Mode { text, icon, .. } => Some(format!("{icon}{text}")),
            Block::ProcessStatus { text, .. } => Some(text.clone()),
            Block::Text { content } | Block::Thinking { content } => Some(content.clone()),
            Block::Compacted { summary } => Some(summary.clone()),
            Block::CodeLine { content, .. } => Some(content.clone()),
            Block::Exec { command, output } => Some(format!("$ {command}\n{output}")),
            Block::ToolDraft { .. } | Block::ToolCall { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ApprovalScope {
    Session,
    Workspace,
}

#[derive(Clone)]
pub struct PermissionEntry {
    pub tool: String,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ConfirmApprovalOption {
    pub id: String,
    pub label: String,
    pub scope: ApprovalScope,
    #[serde(skip)]
    pub grants: Vec<PermissionGrant>,
}

#[derive(Clone, PartialEq, serde::Serialize)]
pub enum ConfirmChoice {
    Yes,
    No,
    Grant(ConfirmApprovalOption),
}

/// Stable monotonic per-session handle. Mutating a block in place preserves its
/// `BlockId`; content changes are detected via `LayoutKey::content_hash`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockId(pub(crate) u64);

impl BlockId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BlockOrigin {
    History(usize),
    CheckpointMarker,
}

/// How the block is presented in the transcript. Independent of [`Status`] -
/// a streaming block can be `Collapsed`. The layout cache keys on this, so
/// flipping view state invalidates only that block.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ViewState {
    /// Full content - default.
    #[default]
    Expanded,
    /// A compact live preview; renderers decide exact rows.
    Peek,
    /// One summary line only.
    Collapsed,
    /// Show the first `keep` rows of the block's content, elide the rest.
    TrimmedHead { keep: u16 },
    /// Show the last `keep` rows of the block's content, elide the rest.
    TrimmedTail { keep: u16 },
}

impl ViewState {
    pub fn measured_height(self, total_rows: u64) -> u64 {
        match self {
            Self::Expanded | Self::Peek => total_rows,
            Self::Collapsed => {
                if total_rows > 1 {
                    2
                } else {
                    total_rows
                }
            }
            Self::TrimmedHead { keep } | Self::TrimmedTail { keep } => {
                let keep = keep as u64;
                if total_rows > keep {
                    keep.saturating_add(1)
                } else {
                    total_rows
                }
            }
        }
    }

    pub fn elides_rows(self, total_rows: u64) -> bool {
        match self {
            Self::Expanded | Self::Peek => false,
            Self::Collapsed => total_rows > 1,
            Self::TrimmedHead { keep } | Self::TrimmedTail { keep } => total_rows > keep as u64,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Status {
    Streaming,
    #[default]
    Done,
}

/// Cache key for a block's per-frame layout. When content changes, the new
/// `content_hash` misses the old entry - invalidation by keying, not eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub view_state: ViewState,
    pub content_hash: u64,
    pub sidecar_hash: u64,
}

pub struct BlockHistory {
    pub order: Vec<BlockId>,
    pub blocks: HashMap<BlockId, Block>,
    /// Cached per-block content hashes; avoids re-hashing on layout-key construction.
    pub(crate) content_hashes: HashMap<BlockId, u64>,
    pub(crate) next_id: u64,
    tool_states: HashMap<String, ToolState>,
    tool_display_hashes: HashMap<String, u64>,
    /// Absent entries default to `Status::Done`.
    pub(crate) statuses: HashMap<BlockId, Status>,
    /// Optional provenance for blocks projected from durable history or session checkpoints.
    origins: HashMap<BlockId, BlockOrigin>,
    /// Blocks that transitioned `Streaming` → `Done` since last drain;
    /// drained by the app loop to emit `block_done` autocmds.
    pub finished_blocks: Vec<BlockId>,
    /// Bumped on every mutation; used by `TranscriptSnapshot` to detect staleness.
    generation: u64,
}

impl BlockHistory {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            blocks: HashMap::new(),
            content_hashes: HashMap::new(),
            next_id: 0,
            tool_states: HashMap::new(),
            tool_display_hashes: HashMap::new(),
            statuses: HashMap::new(),
            origins: HashMap::new(),
            finished_blocks: Vec::new(),
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn invalidate_display_cache(&mut self) {
        self.bump_generation();
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        std::mem::take(&mut self.finished_blocks)
    }

    pub fn content_hash(&self, id: BlockId) -> u64 {
        if let Some(h) = self.content_hashes.get(&id) {
            return *h;
        }
        self.blocks.get(&id).map(|b| b.content_hash()).unwrap_or(0)
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.tool_states.get(call_id)
    }

    pub fn tool_states(&self) -> impl Iterator<Item = (&str, &ToolState)> {
        self.tool_states
            .iter()
            .map(|(call_id, state)| (call_id.as_str(), state))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn block_at(&self, i: usize) -> &Block {
        &self.blocks[&self.order[i]]
    }

    pub fn has_history_origin_at_or_after(&self, before_history_index: usize) -> bool {
        self.first_block_index_for_history_origin_at_or_after(before_history_index)
            .is_some()
    }

    pub fn first_block_index_for_history_origin_at_or_after(
        &self,
        before_history_index: usize,
    ) -> Option<usize> {
        self.order.iter().position(|id| {
            matches!(self.origins.get(id), Some(BlockOrigin::History(history_index)) if *history_index >= before_history_index)
        })
    }

    #[cfg(test)]
    pub(crate) fn status(&self, id: BlockId) -> Status {
        self.statuses.get(&id).copied().unwrap_or_default()
    }

    /// Status changes do not invalidate the layout cache (style concern only).
    pub(crate) fn set_status(&mut self, id: BlockId, status: Status) {
        let was_streaming = matches!(
            self.statuses.get(&id).copied().unwrap_or_default(),
            Status::Streaming
        );
        if matches!(status, Status::Done) {
            self.statuses.remove(&id);
            if was_streaming {
                self.finished_blocks.push(id);
            }
        } else {
            self.statuses.insert(id, status);
        }
        self.bump_generation();
    }

    fn add_block(
        &mut self,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
    ) -> BlockId {
        let hash = block.content_hash();
        let id = BlockId(self.next_id);
        self.next_id += 1;
        match idx {
            Some(idx) => self.order.insert(idx.min(self.order.len()), id),
            None => self.order.push(id),
        }
        self.blocks.insert(id, block);
        self.content_hashes.insert(id, hash);
        if let Some(origin) = origin {
            self.origins.insert(id, origin);
        }
        self.bump_generation();
        id
    }

    pub(crate) fn push(&mut self, block: Block) -> BlockId {
        self.add_block(None, block, None)
    }

    pub(crate) fn push_with_origin(&mut self, block: Block, origin: BlockOrigin) -> BlockId {
        self.add_block(None, block, Some(origin))
    }

    pub(crate) fn insert_checkpoint_marker(
        &mut self,
        before_history_index: usize,
        block: Block,
    ) -> BlockId {
        self.remove_checkpoint_marker();
        let idx = self
            .order
            .iter()
            .position(|id| {
                matches!(
                    self.origins.get(id),
                    Some(BlockOrigin::History(history_index)) if *history_index >= before_history_index
                )
            })
            .unwrap_or(self.order.len());
        self.add_block(Some(idx), block, Some(BlockOrigin::CheckpointMarker))
    }

    pub(crate) fn insert_checkpoint_marker_at(
        &mut self,
        block_index: usize,
        block: Block,
    ) -> BlockId {
        let removed_before = self
            .order
            .iter()
            .take(block_index)
            .filter(|id| matches!(self.origins.get(id), Some(BlockOrigin::CheckpointMarker)))
            .count();
        self.remove_checkpoint_marker();
        self.add_block(
            Some(block_index.saturating_sub(removed_before)),
            block,
            Some(BlockOrigin::CheckpointMarker),
        )
    }

    pub(crate) fn remove_unoriginated_at(&mut self, idx: usize) -> Option<Block> {
        let id = *self.order.get(idx)?;
        if self.origins.contains_key(&id) {
            return None;
        }
        self.order.remove(idx);
        self.content_hashes.remove(&id);
        self.statuses.remove(&id);
        if let Some(Block::ToolCall { call_id, .. }) = self.blocks.get(&id) {
            self.tool_states.remove(call_id);
            self.tool_display_hashes.remove(call_id);
        }
        let block = self.blocks.remove(&id);
        self.bump_generation();
        block
    }

    fn remove_checkpoint_marker(&mut self) {
        let removed: Vec<BlockId> = self
            .order
            .iter()
            .copied()
            .filter(|id| matches!(self.origins.get(id), Some(BlockOrigin::CheckpointMarker)))
            .collect();
        if removed.is_empty() {
            return;
        }
        self.order.retain(|id| !removed.contains(id));
        for id in removed {
            self.blocks.remove(&id);
            self.content_hashes.remove(&id);
            self.statuses.remove(&id);
            self.origins.remove(&id);
        }
        self.bump_generation();
    }

    pub(crate) fn push_with_state(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
    ) -> BlockId {
        let hash = state.display_hash();
        self.tool_states.insert(call_id.clone(), state);
        self.tool_display_hashes.insert(call_id, hash);
        self.push(block)
    }

    pub(crate) fn push_with_state_and_origin(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
        origin: BlockOrigin,
    ) -> BlockId {
        let hash = state.display_hash();
        self.tool_states.insert(call_id.clone(), state);
        self.tool_display_hashes.insert(call_id, hash);
        self.push_with_origin(block, origin)
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let Some(state) = self.tool_states.get_mut(call_id) else {
            return false;
        };
        mutator(state);
        self.tool_display_hashes
            .insert(call_id.to_string(), state.display_hash());
        self.bump_generation();
        true
    }

    /// Replace block content in place. Preserves `BlockId`, `Status`, and
    /// `ViewState`. No-ops when the block doesn't exist (e.g. truncated during
    /// a stream). Same content hash skips the generation bump.
    pub fn rewrite(&mut self, id: BlockId, block: Block) {
        if !self.blocks.contains_key(&id) {
            return;
        }
        let hash = block.content_hash();
        if self.content_hashes.get(&id) == Some(&hash) {
            self.blocks.insert(id, block);
            return;
        }
        self.blocks.insert(id, block);
        self.content_hashes.insert(id, hash);
        self.bump_generation();
    }

    pub(crate) fn rewrite_with_tool_state(
        &mut self,
        id: BlockId,
        block: Block,
        call_id: String,
        state: ToolState,
    ) {
        self.rewrite(id, block);
        let hash = state.display_hash();
        self.tool_states.insert(call_id.clone(), state);
        self.tool_display_hashes.insert(call_id, hash);
        self.bump_generation();
    }

    pub(crate) fn remove_block(&mut self, id: BlockId) {
        if !self.blocks.contains_key(&id) {
            return;
        }
        self.order.retain(|candidate| *candidate != id);
        self.blocks.remove(&id);
        self.content_hashes.remove(&id);
        self.statuses.remove(&id);
        self.origins.remove(&id);
        self.bump_generation();
        self.gc_tool_states();
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.blocks.clear();
        self.content_hashes.clear();
        self.next_id = 0;
        self.tool_states.clear();
        self.tool_display_hashes.clear();
        self.statuses.clear();
        self.origins.clear();
        self.bump_generation();
    }

    pub fn block_gap(&self, i: usize) -> u16 {
        if i > 0 {
            gap_between(self.block_at(i - 1), self.block_at(i))
        } else {
            0
        }
    }

    pub fn rendered_block_gap(&self, i: usize, rendered_rows: usize) -> u16 {
        if rendered_rows == 0 {
            0
        } else {
            self.block_gap(i)
        }
    }

    /// Substitute the actual per-block content and sidecar hash into a base
    /// `LayoutKey` so cache lookups and layout passes agree.
    pub fn resolve_key(&self, id: BlockId, base: LayoutKey) -> LayoutKey {
        let sidecar_hash = match self.blocks.get(&id) {
            Some(Block::ToolCall { call_id, .. }) => {
                self.tool_display_hashes.get(call_id).copied().unwrap_or(0)
            }
            _ => 0,
        };
        LayoutKey {
            content_hash: self.content_hash(id),
            sidecar_hash,
            ..base
        }
    }

    pub(crate) fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        for id in removed {
            self.blocks.remove(&id);
            self.content_hashes.remove(&id);
            self.statuses.remove(&id);
            self.origins.remove(&id);
        }
        self.bump_generation();
        self.gc_tool_states();
    }

    pub(crate) fn gc_tool_states(&mut self) {
        let live: HashSet<String> = self
            .order
            .iter()
            .filter_map(|id| self.blocks.get(id))
            .filter_map(|b| {
                if let Block::ToolCall { call_id, .. } = b {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        self.tool_states.retain(|cid, _| live.contains(cid));
        self.tool_display_hashes.retain(|cid, _| live.contains(cid));
    }
}

/// Blank row gap before `below` given the preceding block. Most block
/// transitions are separated by one blank row. Adjacent code lines collapse,
/// and markdown headings sit directly on top of their following content.
pub fn gap_between(above: &Block, below: &Block) -> u16 {
    if matches!(
        (above, below),
        (Block::CodeLine { .. }, Block::CodeLine { .. })
    ) {
        return 0;
    }
    if matches!(below, Block::Text { .. } | Block::CodeLine { .. }) && ends_with_heading(above) {
        return 0;
    }
    1
}

fn ends_with_heading(block: &Block) -> bool {
    let Block::Text { content } = block else {
        return false;
    };
    crate::content::markdown_ir::ends_with_heading(content)
}

/// Heuristic: does this look like a `/command` line?
pub fn is_command_like(text: &str) -> bool {
    let name = text
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    !name.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_preserves_id_and_bumps_generation() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });

        let h0 = history.content_hash(id);
        let g0 = history.generation();

        history.rewrite(
            id,
            Block::Text {
                content: "hello world".into(),
            },
        );
        let h1 = history.content_hash(id);
        assert_ne!(h0, h1, "content hash must update on rewrite");
        assert_eq!(
            history.order.to_vec(),
            vec![id],
            "rewrite must not change order"
        );
        assert_ne!(history.generation(), g0, "rewrite must bump generation");
    }

    #[test]
    fn identical_blocks_get_distinct_ids() {
        // Each push mints a fresh monotonic `BlockId`. Identical content
        // at two positions no longer shares a slot in `blocks`.
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "same".into(),
        });
        let b = history.push(Block::Text {
            content: "same".into(),
        });
        assert_ne!(a, b);
        assert_eq!(history.order.len(), 2);
        assert_eq!(history.blocks.len(), 2);
        assert_eq!(history.content_hash(a), history.content_hash(b));
    }

    #[test]
    fn raw_text_preserves_markdown_markers() {
        // Whole-block yank must round-trip every inline / block
        // markdown construct - bold, italic, inline code, fenced code,
        // tables, horizontal rules - because the cell-walked fallback
        // strips the markers.
        let md = concat!(
            "**bold** and *italic* and `inline code`\n",
            "\n",
            "```rust\n",
            "let x = 1;\n",
            "```\n",
            "\n",
            "| col | val |\n",
            "| --- | --- |\n",
            "| a   | 1   |\n",
            "\n",
            "---\n",
        );
        let block = Block::Text { content: md.into() };
        assert_eq!(block.raw_text().as_deref(), Some(md));
    }

    #[test]
    fn raw_text_returns_user_text_verbatim() {
        let block = Block::User {
            text: "Explain **this** in detail.".into(),
            image_labels: vec!["[screenshot.png]".into()],
        };
        // Image labels are a render-time annotation, not part of the
        // user's typed message.
        assert_eq!(
            block.raw_text().as_deref(),
            Some("Explain **this** in detail.")
        );
    }

    #[test]
    fn raw_text_is_none_for_structured_blocks() {
        // Tool blocks don't have a single markdown source - yank falls back
        // to cell-walking for them.
        assert!(Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new(),
        }
        .raw_text()
        .is_none());
    }

    fn pending_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        }
    }

    #[test]
    fn tool_state_is_terminal_for_ok_err_denied_only() {
        for status in [ToolStatus::Ok, ToolStatus::Err, ToolStatus::Denied] {
            let s = ToolState {
                status,
                ..pending_state()
            };
            assert!(s.is_terminal());
        }
        assert!(!pending_state().is_terminal());
    }

    #[test]
    fn tool_display_hash_ignores_elapsed_ticks() {
        let mut a = pending_state();
        a.elapsed = Some(std::time::Duration::from_secs(1));
        let mut b = pending_state();
        b.elapsed = Some(std::time::Duration::from_secs(2));
        assert_eq!(a.display_hash(), b.display_hash());

        b.status = ToolStatus::Ok;
        assert_ne!(a.display_hash(), b.display_hash());
    }

    #[test]
    fn raw_text_for_thinking_returns_content() {
        let block = Block::Thinking {
            content: "ponder".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("ponder"));
    }

    #[test]
    fn raw_text_for_compacted_returns_summary() {
        let block = Block::Compacted {
            summary: "earlier session compacted".into(),
        };
        assert_eq!(
            block.raw_text().as_deref(),
            Some("earlier session compacted")
        );
    }

    #[test]
    fn raw_text_for_code_line_returns_content() {
        let block = Block::CodeLine {
            content: "let x = 1;".into(),
            lang: "rust".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("let x = 1;"));
    }

    #[test]
    fn raw_text_for_exec_combines_command_and_output() {
        let block = Block::Exec {
            command: "ls".into(),
            output: "foo\nbar".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("$ ls\nfoo\nbar"));
    }

    #[test]
    fn content_hash_returns_zero_for_unknown_id() {
        let history = BlockHistory::new();
        assert_eq!(history.content_hash(BlockId(9999)), 0);
    }

    #[test]
    fn set_status_streaming_then_done_pushes_finished_block() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        history.set_status(id, Status::Streaming);
        assert_eq!(history.status(id), Status::Streaming);
        history.set_status(id, Status::Done);
        assert_eq!(history.status(id), Status::Done);
        assert!(history.finished_blocks.contains(&id));
    }

    #[test]
    fn set_status_done_without_prior_streaming_does_not_push_finished() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        history.set_status(id, Status::Done);
        assert!(!history.finished_blocks.contains(&id));
    }

    #[test]
    fn rewrite_with_same_hash_skips_generation_bump() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "same".into(),
        });
        let g = history.generation();
        history.rewrite(
            id,
            Block::Text {
                content: "same".into(),
            },
        );
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn rewrite_unknown_id_is_noop() {
        let mut history = BlockHistory::new();
        let g = history.generation();
        history.rewrite(
            BlockId(9999),
            Block::Text {
                content: "x".into(),
            },
        );
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn clear_resets_everything() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "a".into(),
        });
        history.push(Block::Text {
            content: "b".into(),
        });
        history.set_status(id, Status::Streaming);
        let g = history.generation();
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.next_id, 0);
        assert!(history.statuses.is_empty());
        assert!(history.tool_states.is_empty());
        assert_ne!(history.generation(), g);
    }

    #[test]
    fn truncate_drops_tail_and_gcs_tool_states() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        history.push_with_state(
            Block::ToolCall {
                call_id: "tc1".into(),
                name: "x".into(),
                summary: "s".into(),
                args: HashMap::new(),
            },
            "tc1".into(),
            pending_state(),
        );
        history.push(Block::Text {
            content: "c".into(),
        });
        assert_eq!(history.len(), 3);
        assert!(history.tool_states.contains_key("tc1"));
        // Truncate to before the ToolCall - the tool_state entry should be GC'd.
        history.truncate(1);
        assert_eq!(history.len(), 1);
        assert!(!history.tool_states.contains_key("tc1"));
    }

    #[test]
    fn remove_unoriginated_at_refuses_originated_blocks() {
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "a".into(),
        });
        let b = history.push_with_origin(
            Block::Text {
                content: "b".into(),
            },
            BlockOrigin::History(1),
        );
        let g = history.generation();

        assert!(history.remove_unoriginated_at(1).is_none());
        assert_eq!(history.order, vec![a, b]);
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn remove_unoriginated_at_gcs_tool_state() {
        let mut history = BlockHistory::new();
        history.push_with_state(
            Block::ToolCall {
                call_id: "tc1".into(),
                name: "x".into(),
                summary: "s".into(),
                args: HashMap::new(),
            },
            "tc1".into(),
            pending_state(),
        );
        assert!(history.tool_states.contains_key("tc1"));
        assert!(history.tool_display_hashes.contains_key("tc1"));

        assert!(matches!(
            history.remove_unoriginated_at(0),
            Some(Block::ToolCall { call_id, .. }) if call_id == "tc1"
        ));
        assert!(history.is_empty());
        assert!(!history.tool_states.contains_key("tc1"));
        assert!(!history.tool_display_hashes.contains_key("tc1"));
    }

    #[test]
    fn truncate_past_end_is_noop() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        let g = history.generation();
        history.truncate(99);
        assert_eq!(history.len(), 1);
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn block_gap_is_zero_for_first_block() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        assert_eq!(history.block_gap(0), 0);
    }

    #[test]
    fn block_gap_consults_gap_between_for_subsequent_blocks() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        history.push(Block::User {
            text: "q".into(),
            image_labels: vec![],
        });
        // Text -> User: 1
        assert_eq!(history.block_gap(1), 1);
    }

    #[test]
    fn resolve_key_substitutes_content_and_sidecar_hashes() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        let base = LayoutKey {
            width: 80,
            view_state: ViewState::Collapsed,
            content_hash: 0,
            sidecar_hash: 0,
        };
        let resolved = history.resolve_key(id, base);
        assert_eq!(resolved.view_state, ViewState::Collapsed);
        assert_eq!(resolved.content_hash, history.content_hash(id));
        assert_eq!(resolved.sidecar_hash, 0);
        assert_eq!(resolved.width, 80);
    }

    // ── gap_between coverage ──────────────────────────────────────────

    fn text(s: &str) -> Block {
        Block::Text { content: s.into() }
    }
    fn code(s: &str) -> Block {
        Block::CodeLine {
            content: s.into(),
            lang: "rust".into(),
        }
    }
    fn user(s: &str) -> Block {
        Block::User {
            text: s.into(),
            image_labels: vec![],
        }
    }
    fn thinking(s: &str) -> Block {
        Block::Thinking { content: s.into() }
    }
    fn tool(call_id: &str) -> Block {
        Block::ToolCall {
            call_id: call_id.into(),
            name: "x".into(),
            summary: "s".into(),
            args: HashMap::new(),
        }
    }
    fn exec() -> Block {
        Block::Exec {
            command: "ls".into(),
            output: "out".into(),
        }
    }
    fn compacted() -> Block {
        Block::Compacted {
            summary: "sum".into(),
        }
    }
    fn mode() -> Block {
        Block::Mode {
            text: "now in apply mode".into(),
            icon: "● ".into(),
            hl_group: "SmeltModeApply".into(),
        }
    }

    #[test]
    fn gap_between_codeline_to_codeline_is_zero() {
        assert_eq!(gap_between(&code("a"), &code("b")), 0);
    }

    #[test]
    fn gap_between_codeline_to_other_is_one() {
        assert_eq!(gap_between(&code("a"), &text("b")), 1);
    }

    #[test]
    fn gap_between_text_to_codeline_zero_after_heading() {
        assert_eq!(gap_between(&text("# heading"), &code("a")), 0);
        // Leading whitespace before # still counts as heading.
        assert_eq!(gap_between(&text("   ## sub"), &code("a")), 0);
    }

    #[test]
    fn gap_between_text_to_codeline_one_otherwise() {
        assert_eq!(gap_between(&text("plain"), &code("a")), 1);
    }

    #[test]
    fn gap_between_other_to_codeline_is_one() {
        assert_eq!(gap_between(&user("q"), &code("a")), 1);
    }

    #[test]
    fn gap_between_user_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&user("a"), &text("b")), 1);
        assert_eq!(gap_between(&text("a"), &user("b")), 1);
    }

    #[test]
    fn gap_between_exec_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&exec(), &text("a")), 1);
        assert_eq!(gap_between(&text("a"), &exec()), 1);
    }

    #[test]
    fn gap_between_thinking_blocks_collapse() {
        assert_eq!(gap_between(&thinking("a"), &thinking("b")), 0);
    }

    #[test]
    fn gap_between_thinking_and_text_is_one() {
        assert_eq!(gap_between(&thinking("a"), &text("b")), 1);
        assert_eq!(gap_between(&text("a"), &thinking("b")), 1);
    }

    #[test]
    fn gap_between_tool_calls_separated_by_one() {
        assert_eq!(gap_between(&tool("a"), &tool("b")), 1);
        assert_eq!(gap_between(&text("a"), &tool("b")), 1);
        assert_eq!(gap_between(&tool("a"), &text("b")), 1);
    }

    #[test]
    fn gap_between_compacted_separates_both_sides() {
        assert_eq!(gap_between(&text("a"), &compacted()), 1);
        assert_eq!(gap_between(&compacted(), &text("a")), 1);
    }

    #[test]
    fn gap_between_text_after_heading_collapses() {
        assert_eq!(gap_between(&text("# heading"), &text("body")), 0);
    }

    #[test]
    fn gap_between_mode_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&text("a"), &mode()), 1);
        assert_eq!(gap_between(&mode(), &text("b")), 1);
        assert_eq!(gap_between(&mode(), &tool("b")), 1);
    }

    // ── is_command_like ──────────────────────────────────────────────

    #[test]
    fn is_command_like_detects_slash_command() {
        assert!(is_command_like("/help"));
        assert!(is_command_like("/quit"));
        assert!(is_command_like("/foo bar baz"));
    }

    #[test]
    fn is_command_like_rejects_bare_slash_or_non_slash() {
        assert!(!is_command_like("/"));
        assert!(!is_command_like("/   "));
        assert!(!is_command_like("help"));
        assert!(!is_command_like(""));
    }
}
