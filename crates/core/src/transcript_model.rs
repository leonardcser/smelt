//! Transcript domain model: content-addressed block store, layout cache,
//! and mutable sidecar state (tool output, exec output). Held inside
//! `app::transcript::Transcript`, which adds streaming and paint orchestration.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Handle to an in-flight tool call; full mutable state lives in `tool_states[call_id]`.
pub struct ActiveTool {
    pub call_id: String,
    pub(crate) block_id: BlockId,
    pub(crate) start_time: Instant,
}

impl ActiveTool {
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

#[derive(Clone)]
pub struct ConfirmRequest {
    pub call_id: String,
    pub tool_name: String,
    pub desc: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub approval_patterns: Vec<String>,
    pub outside_dir: Option<std::path::PathBuf>,
    pub summary: Option<String>,
    pub request_id: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Pending,
    Confirm,
    Ok,
    Err,
    Denied,
}

#[derive(Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<serde_json::Value>,
}

pub type ToolOutputRef = Box<ToolOutput>;

/// Mutable sidecar for a committed `Block::ToolCall`, keyed by `call_id`.
/// Splitting mutable fields out keeps `Block::ToolCall` immutable so its
/// layout can be cached permanently.
#[derive(Clone)]
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
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Block {
    User {
        text: String,
        /// Accent-highlighted in the rendered message.
        image_labels: Vec<String>,
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
    ToolCall {
        call_id: String,
        name: String,
        summary: String,
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
    /// Stable content hash of this block. Two blocks with the same
    /// content hash produce identical `DisplayBlock`s for the same
    /// `LayoutKey` and `ToolState`. For `ToolCall`, `ToolState` (status
    /// / output / elapsed) is deliberately *not* hashed — mutable tool
    /// state lives separately and is invalidated via
    /// `BlockHistory::invalidate_block_layout`.
    ///
    /// Implementation: serialize through `serde_json::Value` first
    /// (whose `Map` is a `BTreeMap` without the `preserve_order`
    /// feature) so the `HashMap<String, Value>` arg fields are emitted
    /// in sorted-key order, then hash the resulting bytes. Without the
    /// intermediate `to_value` step, two blocks with identical content
    /// but different HashMap insertion orders would produce different
    /// hashes.
    pub fn content_hash(&self) -> u64 {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        seahash::hash(&bytes)
    }

    /// Raw source text for the block, before markdown rendering. Used
    /// by whole-block yank so copying a rendered markdown block returns
    /// the original `**bold**`, `` `code` ``, fenced ```` ``` ```` blocks,
    /// `|` tables, `---` rules, etc. — instead of walking display cells
    /// (which strips inline markup).
    ///
    /// Returns `None` for structured blocks (tool calls,
    /// confirm dialogs) that don't have a single "markdown source"; the
    /// caller falls back to cell-walking for those.
    pub fn raw_text(&self) -> Option<String> {
        match self {
            Block::User { text, .. } => Some(text.clone()),
            Block::Text { content } | Block::Thinking { content } => Some(content.clone()),
            Block::Compacted { summary } => Some(summary.clone()),
            Block::CodeLine { content, .. } => Some(content.clone()),
            Block::Exec { command, output } => Some(format!("$ {command}\n{output}")),
            Block::ToolCall { .. } => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, serde::Serialize)]
pub enum ApprovalScope {
    Session,
    Workspace,
}

#[derive(Clone)]
pub struct PermissionEntry {
    pub tool: String,
    pub pattern: String,
}

#[derive(Clone, PartialEq, serde::Serialize)]
pub enum ConfirmChoice {
    Yes,
    No,
    Always(ApprovalScope),
    AlwaysPatterns(Vec<String>, ApprovalScope),
    AlwaysDir(String, ApprovalScope),
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
}

/// How the block is presented in the transcript. Independent of [`Status`] —
/// a streaming block can be `Collapsed`. The layout cache keys on this, so
/// flipping view state invalidates only that block.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ViewState {
    /// Full content — default.
    #[default]
    Expanded,
    /// One summary line only.
    Collapsed,
    /// Show the first `keep` rows of the block's content, elide the rest.
    TrimmedHead { keep: u16 },
    /// Show the last `keep` rows of the block's content, elide the rest.
    TrimmedTail { keep: u16 },
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
/// `content_hash` misses the old entry — invalidation by keying, not eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
    pub content_hash: u64,
}

pub struct BlockHistory {
    pub order: Vec<BlockId>,
    pub blocks: HashMap<BlockId, Block>,
    /// Cached per-block content hashes; avoids re-hashing on layout-key construction.
    pub(crate) content_hashes: HashMap<BlockId, u64>,
    pub(crate) next_id: u64,
    pub tool_states: HashMap<String, ToolState>,
    /// Absent entries default to `ViewState::Expanded`.
    pub(crate) view_states: HashMap<BlockId, ViewState>,
    /// Absent entries default to `Status::Done`.
    pub(crate) statuses: HashMap<BlockId, Status>,
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
            view_states: HashMap::new(),
            statuses: HashMap::new(),
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

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        std::mem::take(&mut self.finished_blocks)
    }

    pub fn content_hash(&self, id: BlockId) -> u64 {
        if let Some(h) = self.content_hashes.get(&id) {
            return *h;
        }
        self.blocks.get(&id).map(|b| b.content_hash()).unwrap_or(0)
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

    pub(crate) fn view_state(&self, id: BlockId) -> ViewState {
        self.view_states.get(&id).copied().unwrap_or_default()
    }

    pub(crate) fn set_view_state(&mut self, id: BlockId, state: ViewState) {
        let prev = self.view_states.get(&id).copied().unwrap_or_default();
        if prev == state {
            return;
        }
        if matches!(state, ViewState::Expanded) {
            self.view_states.remove(&id);
        } else {
            self.view_states.insert(id, state);
        }
        self.bump_generation();
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

    pub(crate) fn push(&mut self, block: Block) -> BlockId {
        let hash = block.content_hash();
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.order.push(id);
        self.blocks.insert(id, block);
        self.content_hashes.insert(id, hash);
        self.bump_generation();
        id
    }

    pub(crate) fn push_with_state(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
    ) -> BlockId {
        self.tool_states.insert(call_id, state);
        self.push(block)
    }

    /// Replace block content in place. Preserves `BlockId`, `Status`, and
    /// `ViewState`. No-ops when the block doesn't exist (e.g. truncated during
    /// a stream). Same content hash skips the generation bump.
    pub(crate) fn rewrite(&mut self, id: BlockId, block: Block) {
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

    pub fn clear(&mut self) {
        self.order.clear();
        self.blocks.clear();
        self.content_hashes.clear();
        self.next_id = 0;
        self.tool_states.clear();
        self.view_states.clear();
        self.statuses.clear();
        self.bump_generation();
    }

    pub fn block_gap(&self, i: usize) -> u16 {
        if i > 0 {
            gap_between(self.block_at(i - 1), self.block_at(i))
        } else {
            0
        }
    }

    /// Substitute the actual per-block view state and content hash into a base
    /// `LayoutKey` so cache lookups and layout passes agree.
    pub fn resolve_key(&self, id: BlockId, base: LayoutKey) -> LayoutKey {
        LayoutKey {
            view_state: self.view_state(id),
            content_hash: self.content_hash(id),
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
            self.view_states.remove(&id);
            self.statuses.remove(&id);
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
    }
}

/// Completed lines are committed immediately; only the current incomplete line lives here.
pub struct ActiveThinking {
    pub current_line: String,
    pub paragraph: String,
    pub streaming_id: Option<BlockId>,
}

pub struct ActiveText {
    pub(crate) current_line: String,
    pub(crate) paragraph: String,
    pub(crate) in_code_block: Option<String>,
    pub(crate) table_rows: Vec<String>,
    /// Cached non-separator row count; avoids recomputing per frame.
    pub(crate) table_data_rows: usize,
    pub(crate) streaming_id: Option<BlockId>,
    pub(crate) table_streaming_id: Option<BlockId>,
    pub(crate) code_line_streaming_id: Option<BlockId>,
}

/// Blank row gap before `below` given the preceding block. Headings suppress
/// the trailing gap; CodeLine→CodeLine is zero; most other transitions are 1.
pub fn gap_between(above: &Block, below: &Block) -> u16 {
    match (above, below) {
        (Block::CodeLine { .. }, Block::CodeLine { .. }) => return 0,
        (Block::CodeLine { .. }, _) => return 1,
        (Block::Text { content }, Block::CodeLine { .. }) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                return 0;
            }
            return 1;
        }
        (_, Block::CodeLine { .. }) => return 1,
        _ => {}
    }
    match (above, below) {
        (Block::User { .. }, _) => 1,
        (_, Block::User { .. }) => 1,
        (Block::Exec { .. }, _) => 1,
        (_, Block::Exec { .. }) => 1,
        (Block::ToolCall { .. }, Block::ToolCall { .. }) => 1,
        (Block::Text { .. }, Block::ToolCall { .. }) => 1,
        (Block::Thinking { .. }, Block::Thinking { .. }) => 0,
        (_, Block::Thinking { .. }) => 1,
        (Block::Thinking { .. }, _) => 1,
        (Block::ToolCall { .. }, Block::Text { .. }) => 1,
        (_, Block::Compacted { .. }) => 1,
        (Block::Compacted { .. }, _) => 1,

        (Block::Text { content }, Block::Text { .. }) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                0
            } else {
                1
            }
        }
        _ => 0,
    }
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
        // markdown construct — bold, italic, inline code, fenced code,
        // tables, horizontal rules — because the cell-walked fallback
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
        // Tool blocks don't have a single markdown source — yank falls back
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
}
