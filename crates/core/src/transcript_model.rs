//! Transcript domain model: content-addressed block store, layout cache,
//! and mutable sidecar state (tool output, exec output). Held inside
//! `app::transcript::Transcript`, which adds streaming and paint orchestration.

use crate::paused_timer::PausedTimer;
use crate::permissions::PermissionGrant;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
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
    pub tool_paths: Vec<crate::permissions::ToolPath>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_output: Option<ToolOutputRef>,
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
        struct DisplayOutput<'a> {
            content: &'a str,
            is_error: bool,
            metadata_hash: Option<u64>,
        }

        #[derive(serde::Serialize)]
        struct DisplayState<'a> {
            status: ToolStatus,
            output: Option<DisplayOutput<'a>>,
            preview_output: Option<DisplayOutput<'a>>,
            user_message: &'a Option<String>,
        }

        fn display_output(output: &ToolOutput) -> DisplayOutput<'_> {
            DisplayOutput {
                content: output.content.as_str(),
                is_error: output.is_error,
                metadata_hash: output
                    .metadata
                    .as_ref()
                    .map(crate::utils::hash_serializable),
            }
        }

        crate::utils::hash_serializable(&DisplayState {
            status: self.status,
            output: self.output.as_deref().map(display_output),
            preview_output: self.preview_output.as_deref().map(display_output),
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
        /// Whether the leading slash-command token receives accent styling.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        command: bool,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary_titles: Vec<String>,
        content: String,
        #[serde(default)]
        kind: protocol::ReasoningKind,
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
    CompactionPreview {
        summary: String,
    },
}

impl Block {
    pub(crate) fn normalize_content(self) -> Self {
        match self {
            Block::Thinking {
                title,
                summary_titles,
                content,
                kind,
            } => Block::Thinking {
                title,
                summary_titles,
                content: crate::content::markdown_stream::normalize_thinking_title_spacing(
                    &content,
                ),
                kind,
            },
            other => other,
        }
    }

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
            Block::CompactionPreview { .. } => "compaction_preview",
        }
    }

    /// Whether a viewport content anchor inside this block can be reused across
    /// later projection frames. Live-rewritten blocks can shift their rendered
    /// rows while preserving `BlockId`, so row-based viewport gestures should
    /// fall back to the resolved row instead.
    pub fn is_stable_scroll_anchor(&self) -> bool {
        !matches!(
            self,
            Block::ToolDraft {
                finished: false,
                ..
            }
        )
    }

    pub fn row_estimate_text(&self) -> Option<BlockText<'_>> {
        match self {
            Block::User { text, .. }
            | Block::ProcessStatus { text, .. }
            | Block::Text { content: text }
            | Block::Compacted { summary: text }
            | Block::CompactionPreview { summary: text }
            | Block::CodeLine { content: text, .. } => Some(BlockText::Plain(text)),
            Block::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => Some(BlockText::Thinking {
                title: title.as_deref(),
                summary_titles,
                content,
            }),
            Block::Mode { text, icon, .. } => Some(BlockText::Prefixed { prefix: icon, text }),
            Block::Exec { command, output } => Some(BlockText::Exec { command, output }),
            Block::ToolDraft { .. } | Block::ToolCall { .. } => None,
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
            Block::Text { content } => Some(content.clone()),
            Block::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => Some(thinking_markdown_source(
                title.as_deref(),
                summary_titles,
                content,
            )),
            Block::Compacted { summary } | Block::CompactionPreview { summary } => {
                Some(summary.clone())
            }
            Block::CodeLine { content, .. } => Some(content.clone()),
            Block::Exec { command, output } => Some(format!("$ {command}\n{output}")),
            Block::ToolDraft { .. } | Block::ToolCall { .. } => None,
        }
    }

    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::ToolDraft { name, .. } | Self::ToolCall { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolDraft { call_id, .. } => call_id.as_deref(),
            Self::ToolCall { call_id, .. } => Some(call_id),
            _ => None,
        }
    }

    pub fn process_field(&self, field: &str) -> Option<String> {
        match self {
            Self::ProcessStatus {
                event: Some(event), ..
            } => event.field_value(field),
            _ => None,
        }
    }

    pub fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        match self {
            Self::ToolDraft { args, .. } | Self::ToolCall { args, .. } => args.get(arg),
            _ => None,
        }
    }
}

pub(crate) fn thinking_markdown_source(
    title: Option<&str>,
    summary_titles: &[String],
    content: &str,
) -> String {
    let mut sections = Vec::with_capacity(summary_titles.len().saturating_add(1));
    if summary_titles.is_empty() {
        if let Some(title) = title {
            sections.push(format!("**{title}**"));
        }
    } else {
        sections.extend(summary_titles.iter().map(|title| format!("**{title}**")));
    }
    if !content.is_empty() {
        sections.push(content.to_string());
    }
    sections.join("\n")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockText<'a> {
    Plain(&'a str),
    Prefixed {
        prefix: &'a str,
        text: &'a str,
    },
    Thinking {
        title: Option<&'a str>,
        summary_titles: &'a [String],
        content: &'a str,
    },
    Exec {
        command: &'a str,
        output: &'a str,
    },
}

impl BlockText<'_> {
    pub fn first_source_line(self) -> String {
        fn first_nonempty_line(text: &str) -> String {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .to_string()
        }

        match self {
            Self::Plain(text) => first_nonempty_line(text),
            Self::Prefixed { prefix, text } => {
                let first_text_line = text.lines().next().unwrap_or_default();
                let first_line = format!("{prefix}{first_text_line}");
                if first_line.trim().is_empty() {
                    text.lines()
                        .skip(1)
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    first_line
                }
            }
            Self::Thinking {
                title,
                summary_titles,
                content,
            } => {
                let title = summary_titles.first().map(String::as_str).or(title);
                title
                    .map(|title| format!("**{title}**"))
                    .unwrap_or_else(|| first_nonempty_line(content))
            }
            Self::Exec { command, .. } => format!("$ {command}"),
        }
    }
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TranscriptBlockRecord {
    pub block: Block,
    #[serde(default)]
    pub content_hash: u64,
    pub origin: Option<BlockOrigin>,
    pub tool_state: Option<(String, ToolState)>,
}

#[derive(Clone)]
pub struct TranscriptBlockRecordWithId {
    pub block_id: BlockId,
    pub record: TranscriptBlockRecord,
}

#[derive(Clone)]
pub struct StoredBlockWithId {
    pub block_id: BlockId,
    pub stored: Arc<StoredBlockRef>,
}

impl TryFrom<smelt_store::StoredTranscriptBlock> for TranscriptBlockRecord {
    type Error = serde_json::Error;

    fn try_from(row: smelt_store::StoredTranscriptBlock) -> Result<Self, Self::Error> {
        let block: Block = serde_json::from_str(&row.block_json)?;
        let origin = row
            .origin_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .or_else(|| {
                row.history_idx
                    .map(|idx| BlockOrigin::History(idx as usize))
            });
        let tool_state = row
            .tool_state_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let content_hash = row.content_hash.parse::<u64>().unwrap_or_default();
        Ok(Self {
            block,
            content_hash,
            origin,
            tool_state,
        })
    }
}

impl TryFrom<smelt_store::StoredTranscriptBlock> for TranscriptBlockRecordWithId {
    type Error = serde_json::Error;

    fn try_from(row: smelt_store::StoredTranscriptBlock) -> Result<Self, Self::Error> {
        let block_id = BlockId::new(row.block_idx);
        let record = TranscriptBlockRecord::try_from(row)?;
        Ok(Self { block_id, record })
    }
}

pub fn compact_block_rows(
    start_record_index: usize,
    rows: Vec<smelt_store::StoredTranscriptBlock>,
) -> Result<Vec<StoredBlockWithId>, serde_json::Error> {
    rows.into_iter()
        .enumerate()
        .map(|(offset, row)| {
            let estimated_text_bytes = row.estimated_text_bytes;
            let preview = row.preview_text.clone();
            let record = TranscriptBlockRecordWithId::try_from(row)?;
            let (block_id, stored) = StoredBlockRef::from_record(
                start_record_index.saturating_add(offset),
                record.block_id,
                &record.record,
                estimated_text_bytes,
                preview,
            );
            Ok(StoredBlockWithId { block_id, stored })
        })
        .collect()
}

const TRANSCRIPT_INDEXED_TEXT_MAX_BYTES: usize = 128 * 1024;
const TOOL_ARG_INDEXED_TEXT_MAX_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptIndexedText {
    pub indexed_text: String,
    pub estimated_text_bytes: u64,
}

pub fn transcript_indexed_text(
    block: &Block,
    tool_state: Option<&ToolState>,
) -> TranscriptIndexedText {
    let full_text = transcript_block_full_indexed_text(block, tool_state);
    let estimated_text_bytes = full_text.len() as u64;
    let indexed_text = cap_indexed_text(&full_text, TRANSCRIPT_INDEXED_TEXT_MAX_BYTES);
    TranscriptIndexedText {
        indexed_text,
        estimated_text_bytes,
    }
}

fn cap_indexed_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let head_end = smelt_buffer::text::snap(text, max_bytes / 2);
    let tail_min = text.len().saturating_sub(max_bytes - head_end);
    let snapped_tail = smelt_buffer::text::snap(text, tail_min);
    let tail_start = if snapped_tail == tail_min {
        snapped_tail
    } else {
        smelt_buffer::text::next_char_boundary(text, tail_min)
    };
    let omitted_bytes = tail_start.saturating_sub(head_end);
    let marker = format!("\n… {omitted_bytes} bytes omitted from persistent search index …\n");
    format!(
        "{}{}{}",
        smelt_buffer::text::slice(text, 0..head_end),
        marker,
        smelt_buffer::text::slice(text, tail_start..text.len())
    )
}

fn transcript_block_full_indexed_text(block: &Block, tool_state: Option<&ToolState>) -> String {
    if block.tool_name().is_some() {
        return tool_indexed_text(block, tool_state);
    }

    let mut text = block.raw_text().unwrap_or_default();
    append_indexed_line(&mut text, thinking_summary(block).as_deref());
    append_indexed_line(&mut text, compacted_label(block));
    append_indexed_line(&mut text, compacted_separator(block));
    text
}

fn tool_indexed_text(block: &Block, tool_state: Option<&ToolState>) -> String {
    let mut text = String::new();
    append_indexed_line(&mut text, block.tool_name());
    append_indexed_line(&mut text, tool_state.map(|state| state.status.label()));
    append_indexed_line(&mut text, tool_summary_text(block).as_deref());
    append_indexed_line(&mut text, tool_arg_indexed_text(block).as_deref());
    append_indexed_line(
        &mut text,
        tool_state.and_then(|state| state.user_message.as_deref()),
    );
    append_indexed_line(
        &mut text,
        tool_state
            .and_then(|state| state.preview_output.as_ref())
            .map(|output| output.content.as_str()),
    );
    append_indexed_line(
        &mut text,
        tool_state
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.as_str()),
    );
    append_indexed_line(
        &mut text,
        edit_file_indexed_text(block, tool_state).as_deref(),
    );
    if let Some(display_count) = tool_state.and_then(display_count_indexed_text) {
        append_indexed_line(&mut text, Some(&display_count));
    }
    text
}

fn tool_summary_text(block: &Block) -> Option<String> {
    match block {
        Block::ToolDraft { summary, .. } | Block::ToolCall { summary, .. } => {
            Some(summary.as_plain_text())
        }
        _ => None,
    }
    .filter(|summary| !summary.is_empty())
}

fn tool_arg_indexed_text(block: &Block) -> Option<String> {
    let (tool_name, args) = match block {
        Block::ToolDraft { name, args, .. } | Block::ToolCall { name, args, .. } => {
            (name.as_str(), args)
        }
        _ => return None,
    };
    if tool_name == "edit_file" {
        return None;
    }

    let mut fields = args.iter().collect::<Vec<_>>();
    fields.sort_by_key(|(key, _)| *key);
    let mut text = String::new();
    for (key, value) in fields {
        if !searchable_tool_arg_key(key) || bulky_tool_arg_key(key) {
            continue;
        }
        let Some(value) = tool_arg_value_text(value) else {
            continue;
        };
        if value.len() > TOOL_ARG_INDEXED_TEXT_MAX_BYTES {
            continue;
        }
        append_indexed_line(&mut text, Some(&format!("{key}: {value}")));
    }
    (!text.is_empty()).then_some(text)
}

fn searchable_tool_arg_key(key: &str) -> bool {
    matches!(
        key,
        "base"
            | "cell_number"
            | "cell_type"
            | "command"
            | "description"
            | "edit_mode"
            | "file_path"
            | "format"
            | "glob"
            | "name"
            | "notebook_path"
            | "output_mode"
            | "path"
            | "pattern"
            | "prompt"
            | "query"
            | "type"
            | "url"
    )
}

fn bulky_tool_arg_key(key: &str) -> bool {
    matches!(
        key,
        "content"
            | "new_content"
            | "new_source"
            | "new_string"
            | "old_content"
            | "old_string"
            | "source"
    )
}

fn tool_arg_value_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).ok()
        }
        serde_json::Value::Null => None,
    }
}

fn append_indexed_line(out: &mut String, text: Option<&str>) {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return;
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
}

fn thinking_summary(block: &Block) -> Option<String> {
    let Block::Thinking { title, content, .. } = block else {
        return None;
    };
    let (inferred_label, line_count) = thinking_summary_label(content);
    let label = title.as_deref().unwrap_or(&inferred_label);
    let collapsed_lines = if title.is_some() || inferred_label == "thinking" {
        line_count
    } else {
        line_count.saturating_sub(1)
    };
    Some(format!(
        "{label}\n… {} …",
        pluralize(collapsed_lines, "line collapsed", "lines collapsed")
    ))
}

fn thinking_summary_label(content: &str) -> (String, usize) {
    let mut label = None;
    let mut lines = 0usize;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = trimmed
                .strip_prefix("**")
                .and_then(|inner| inner.strip_suffix("**"))
                .map(str::trim)
                .filter(|inner| !inner.is_empty())
                .map(str::to_string);
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn compacted_label(block: &Block) -> Option<&'static str> {
    matches!(block, Block::Compacted { .. }).then_some("compacted")
}

fn compacted_separator(block: &Block) -> Option<&'static str> {
    matches!(block, Block::Compacted { .. }).then_some("─")
}

fn edit_file_indexed_text(block: &Block, tool_state: Option<&ToolState>) -> Option<String> {
    let args = edit_file_args(block)?;
    let mut text = String::new();
    let old_string = string_field(args, "old_string").unwrap_or_default();
    let new_string = string_field(args, "new_string").unwrap_or_default();
    append_indexed_line(
        &mut text,
        Some(&replacement_line_detail(old_string, new_string)),
    );
    append_indexed_line(&mut text, string_field(args, "file_path"));

    let metadata = tool_state
        .and_then(|state| state.output.as_ref())
        .and_then(|output| output.metadata.as_ref())
        .and_then(serde_json::Value::as_object);
    let has_snapshot = metadata.is_some_and(|metadata| {
        let old_content = metadata
            .get("old_content")
            .and_then(serde_json::Value::as_str);
        let new_content = metadata
            .get("new_content")
            .and_then(serde_json::Value::as_str);
        append_indexed_line(&mut text, old_content);
        append_indexed_line(&mut text, new_content);
        old_content.is_some() || new_content.is_some()
    });
    if !has_snapshot {
        append_indexed_line(&mut text, Some(old_string));
        append_indexed_line(&mut text, Some(new_string));
    }
    (!text.is_empty()).then_some(text)
}

fn edit_file_args(block: &Block) -> Option<&std::collections::HashMap<String, serde_json::Value>> {
    match block {
        Block::ToolDraft { name, args, .. } | Block::ToolCall { name, args, .. }
            if name == "edit_file" =>
        {
            Some(args)
        }
        _ => None,
    }
}

fn string_field<'a>(
    fields: &'a std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    fields.get(key).and_then(serde_json::Value::as_str)
}

fn replacement_line_detail(old_text: &str, new_text: &str) -> String {
    format!(
        "{}, {}",
        line_label(line_count(old_text), "old line"),
        line_label(line_count(new_text), "new line")
    )
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn line_label(count: usize, label: &str) -> String {
    format!("{} {}{}", count, label, if count == 1 { "" } else { "s" })
}

fn display_count_indexed_text(state: &ToolState) -> Option<String> {
    let metadata = state.output.as_ref()?.metadata.as_ref()?;
    let display_count = metadata.get("display_count")?.as_object()?;
    let (count, count_text) = display_count_count_text(display_count.get("value"));
    let unit = display_count
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("item");
    let plural = display_count
        .get("plural")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if unit == "match" {
                "matches".to_string()
            } else {
                format!("{unit}s")
            }
        });
    let label = if count == 1.0 { unit } else { &plural };
    Some(format!("{count_text} {label}"))
}

fn display_count_count_text(value: Option<&serde_json::Value>) -> (f64, String) {
    let count = value.and_then(display_count_count).unwrap_or(0.0);
    (count, format_lua_number(count))
}

fn display_count_count(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn format_lua_number(value: f64) -> String {
    if value.fract() == 0.0 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

pub fn transcript_block_row(
    record_idx: usize,
    record: &TranscriptBlockRecord,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    transcript_block_row_with_block_idx(record_idx, record_idx as u64, record)
}

pub fn transcript_block_row_with_block_idx(
    _record_idx: usize,
    block_idx: u64,
    record: &TranscriptBlockRecord,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    let indexed_text = transcript_indexed_text(
        &record.block,
        record.tool_state.as_ref().map(|(_, state)| state),
    );
    let block_json = serde_json::to_string(&record.block)?;
    let origin_json = record
        .origin
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let tool_state_json = record
        .tool_state
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let history_idx = match record.origin {
        Some(BlockOrigin::History(idx)) => Some(idx as u64),
        _ => None,
    };
    Ok(smelt_store::StoredTranscriptBlock {
        block_idx,
        history_idx,
        kind: record.block.kind().to_string(),
        tool_call_id: record.block.tool_call_id().map(str::to_string),
        tool_name: record.block.tool_name().map(str::to_string),
        content_hash: record.content_hash.to_string(),
        estimated_text_bytes: indexed_text.estimated_text_bytes,
        preview_text: preview(&indexed_text.indexed_text, 512),
        indexed_text: indexed_text.indexed_text,
        block_json,
        origin_json,
        tool_state_json,
    })
}

fn preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    smelt_buffer::text::slice(text, 0..max_bytes).to_string()
}

const STORED_SELECTOR_VALUE_MAX_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoredBlockKind {
    User,
    Mode,
    ProcessStatus,
    Thinking,
    Assistant,
    Code,
    Tool,
    Exec,
    Compacted,
    CompactionPreview,
}

impl StoredBlockKind {
    fn from_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "user" => Self::User,
            "mode" => Self::Mode,
            "process_status" => Self::ProcessStatus,
            "thinking" => Self::Thinking,
            "assistant" => Self::Assistant,
            "code" => Self::Code,
            "tool" => Self::Tool,
            "exec" => Self::Exec,
            "compacted" => Self::Compacted,
            "compaction_preview" => Self::CompactionPreview,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Mode => "mode",
            Self::ProcessStatus => "process_status",
            Self::Thinking => "thinking",
            Self::Assistant => "assistant",
            Self::Code => "code",
            Self::Tool => "tool",
            Self::Exec => "exec",
            Self::Compacted => "compacted",
            Self::CompactionPreview => "compaction_preview",
        }
    }
}

/// Compact canonical locator and render-plan metadata for a durable block.
/// Full block JSON, tool output, and block text remain in SQLite.
#[derive(Clone, Debug)]
pub struct StoredBlockRef {
    pub record_index: usize,
    pub kind: StoredBlockKind,
    pub preview: String,
    pub estimated_text_bytes: u64,
    pub content_hash: u64,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<ToolStatus>,
    pub tool_display_hash: u64,
    pub origin: Option<BlockOrigin>,
    pub stable_scroll_anchor: bool,
    starts_with_thinking_title: bool,
    ends_with_heading: bool,
    selector_fields: HashMap<String, serde_json::Value>,
    process_fields: HashMap<String, String>,
    retained_bytes: usize,
}

impl StoredBlockRef {
    pub fn from_record(
        record_index: usize,
        block_id: BlockId,
        record: &TranscriptBlockRecord,
        estimated_text_bytes: u64,
        preview: String,
    ) -> (BlockId, Arc<Self>) {
        let block = &record.block;
        let tool_state = record.tool_state.as_ref().map(|(_, state)| state);
        let kind = StoredBlockKind::from_kind(block.kind())
            .expect("transcript blocks use a known block kind");
        let tool_call_id = block.tool_call_id().map(str::to_string);
        let tool_name = block.tool_name().map(str::to_string);
        let selector_fields = match block {
            Block::ToolDraft { args, .. } | Block::ToolCall { args, .. } => args
                .iter()
                .filter_map(|(key, value)| {
                    let encoded = serde_json::to_string(value).ok()?;
                    (encoded.len() <= STORED_SELECTOR_VALUE_MAX_BYTES)
                        .then(|| (key.clone(), value.clone()))
                })
                .collect(),
            _ => HashMap::new(),
        };
        let process_fields: HashMap<String, String> =
            ["event", "event_type", "process_id", "exit_code"]
                .into_iter()
                .filter_map(|field| {
                    block
                        .process_field(field)
                        .map(|value| (field.to_string(), value))
                })
                .collect();
        let starts_with_thinking_title = match block {
            Block::Thinking { title, content, .. } => has_thinking_title(title.as_deref(), content),
            _ => false,
        };
        let ends_with_heading = match block {
            Block::Text { content } => crate::content::markdown_ir::ends_with_heading(content),
            _ => false,
        };
        let retained_bytes = std::mem::size_of::<Self>()
            .saturating_add(preview.capacity())
            .saturating_add(tool_call_id.as_ref().map_or(0, String::capacity))
            .saturating_add(tool_name.as_ref().map_or(0, String::capacity))
            .saturating_add(
                selector_fields
                    .iter()
                    .map(|(key, value)| {
                        key.capacity()
                            .saturating_add(serde_json::to_string(value).map_or(0, |v| v.len()))
                    })
                    .sum::<usize>(),
            )
            .saturating_add(
                process_fields
                    .iter()
                    .map(|(key, value)| key.capacity().saturating_add(value.capacity()))
                    .sum::<usize>(),
            );
        (
            block_id,
            Arc::new(Self {
                record_index,
                kind,
                preview,
                estimated_text_bytes,
                content_hash: if record.content_hash == 0 {
                    block.content_hash()
                } else {
                    record.content_hash
                },
                tool_call_id,
                tool_name,
                tool_status: tool_state.map(|state| state.status),
                tool_display_hash: tool_state.map_or(0, ToolState::display_hash),
                origin: record.origin,
                stable_scroll_anchor: !matches!(
                    block,
                    Block::ToolDraft {
                        finished: false,
                        ..
                    }
                ),
                starts_with_thinking_title,
                ends_with_heading,
                selector_fields,
                process_fields,
                retained_bytes,
            }),
        )
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub fn first_line(&self) -> &str {
        self.preview
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
    }

    fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        self.selector_fields.get(arg)
    }

    fn process_field(&self, field: &str) -> Option<String> {
        self.process_fields.get(field).cloned()
    }
}

fn serialized_retained_bytes<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len())
        .unwrap_or_default()
}

pub fn block_retained_bytes(block: &Block) -> usize {
    std::mem::size_of::<Block>().saturating_add(serialized_retained_bytes(block))
}

pub fn tool_state_retained_bytes(state: &ToolState) -> usize {
    std::mem::size_of::<ToolState>().saturating_add(serialized_retained_bytes(state))
}

#[derive(Clone)]
enum ToolStateEntry {
    Live(ToolState),
    Hydrated(ToolState),
    Stored {
        status: ToolStatus,
        display_hash: u64,
    },
}

impl ToolStateEntry {
    fn state(&self) -> Option<&ToolState> {
        match self {
            Self::Live(state) | Self::Hydrated(state) => Some(state),
            Self::Stored { .. } => None,
        }
    }

    fn display_hash(&self) -> u64 {
        match self {
            Self::Live(state) | Self::Hydrated(state) => state.display_hash(),
            Self::Stored { display_hash, .. } => *display_hash,
        }
    }

    fn status(&self) -> ToolStatus {
        match self {
            Self::Live(state) | Self::Hydrated(state) => state.status,
            Self::Stored { status, .. } => *status,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct BlockMetadata {
    content_hash: u64,
    status: Status,
    origin: Option<BlockOrigin>,
}

struct LiveBlock {
    block: Block,
    metadata: BlockMetadata,
}

enum BlockEntry {
    Live(Box<LiveBlock>),
    Stored(Arc<StoredBlockRef>),
    Hydrated {
        stored: Arc<StoredBlockRef>,
        block: Box<Block>,
        block_weight: usize,
        tool_state_weight: usize,
    },
}

impl BlockEntry {
    fn block(&self) -> Option<&Block> {
        match self {
            Self::Live(live) => Some(&live.block),
            Self::Hydrated { block, .. } => Some(block),
            Self::Stored(_) => None,
        }
    }

    fn into_materialized(self) -> Option<Block> {
        match self {
            Self::Live(live) => Some(live.block),
            Self::Hydrated { block, .. } => Some(*block),
            Self::Stored(_) => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Live(live) => live.block.kind(),
            Self::Hydrated { block, .. } => block.kind(),
            Self::Stored(stored) => stored.kind.as_str(),
        }
    }

    fn tool_name(&self) -> Option<&str> {
        match self {
            Self::Stored(stored) => stored.tool_name.as_deref(),
            _ => match self.block()? {
                Block::ToolDraft { name, .. } | Block::ToolCall { name, .. } => Some(name),
                _ => None,
            },
        }
    }

    fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::Stored(stored) => stored.tool_call_id.as_deref(),
            _ => match self.block()? {
                Block::ToolDraft { call_id, .. } => call_id.as_deref(),
                Block::ToolCall { call_id, .. } => Some(call_id),
                _ => None,
            },
        }
    }

    fn process_field(&self, field: &str) -> Option<String> {
        match self {
            Self::Stored(stored) => stored.process_field(field),
            _ => match self.block()? {
                Block::ProcessStatus {
                    event: Some(event), ..
                } => event.field_value(field),
                _ => None,
            },
        }
    }

    fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        match self {
            Self::Stored(stored) => stored.arg_field(arg),
            _ => match self.block()? {
                Block::ToolDraft { args, .. } | Block::ToolCall { args, .. } => args.get(arg),
                _ => None,
            },
        }
    }

    fn is_tool_draft(&self) -> bool {
        self.block()
            .is_some_and(|block| matches!(block, Block::ToolDraft { .. }))
    }

    fn is_persisted_block(&self) -> bool {
        self.kind() != "compaction_preview" && !self.is_tool_draft()
    }

    fn row_estimate_text(&self) -> Option<BlockText<'_>> {
        self.block().and_then(Block::row_estimate_text).or_else(|| {
            let Self::Stored(stored) = self else {
                return None;
            };
            (!stored.preview.is_empty()).then_some(BlockText::Plain(stored.preview.as_str()))
        })
    }

    fn estimated_text_bytes(&self) -> u64 {
        match self {
            Self::Stored(stored) | Self::Hydrated { stored, .. } => stored.estimated_text_bytes,
            Self::Live(live) => live.block.raw_text().map_or(0, |text| text.len() as u64),
        }
    }

    fn navigation_signature(&self) -> (&'static str, String) {
        (
            self.kind(),
            self.row_estimate_text()
                .map(BlockText::first_source_line)
                .unwrap_or_default(),
        )
    }

    fn raw_text(&self) -> Option<String> {
        self.block().and_then(Block::raw_text)
    }

    fn cloned_block(&self) -> Option<Block> {
        self.block().cloned()
    }

    fn stored(&self) -> Option<&Arc<StoredBlockRef>> {
        match self {
            Self::Stored(stored) | Self::Hydrated { stored, .. } => Some(stored),
            Self::Live(_) => None,
        }
    }

    fn hydrated_weight(&self) -> usize {
        match self {
            Self::Hydrated {
                block_weight,
                tool_state_weight,
                ..
            } => (*block_weight).saturating_add(*tool_state_weight),
            _ => 0,
        }
    }

    fn hydrated_block_weight(&self) -> usize {
        match self {
            Self::Hydrated { block_weight, .. } => *block_weight,
            _ => 0,
        }
    }

    fn hydrated_tool_state_weight(&self) -> usize {
        match self {
            Self::Hydrated {
                tool_state_weight, ..
            } => *tool_state_weight,
            _ => 0,
        }
    }
}

const RECORD_CHANGE_LOG_CAPACITY: usize = 64;

pub struct BlockHistory {
    pub order: Vec<BlockId>,
    entries: HashMap<BlockId, BlockEntry>,
    pub(crate) next_id: u64,
    tool_states: HashMap<String, ToolStateEntry>,
    /// Blocks that transitioned `Streaming` → `Done` since last drain;
    /// drained by the app loop to emit `block_done` autocmds.
    pub finished_blocks: Vec<BlockId>,
    /// Bumped on every mutation; used by `TranscriptSnapshot` to detect staleness.
    generation: u64,
    /// Bumped only when transcript order changes, so projections can reuse
    /// block-to-node structure across content and sidecar updates.
    order_generation: u64,
    /// Bumped when semantic navigation coordinates, roles, or labels may change.
    navigation_generation: u64,
    /// Number of ordered entries represented by canonical transcript blocks.
    persisted_block_count: usize,
    /// Hydrated entries and their retained memory, maintained across entry transitions.
    hydrated_ids: HashSet<BlockId>,
    hydrated_block_bytes: usize,
    hydrated_tool_state_bytes: usize,
    /// Earliest transcript order index whose persisted block may be stale.
    record_dirty_from: Option<usize>,
    /// Bumped when a persisted block may have changed. Unlike `generation`,
    /// this ignores display-only status changes.
    record_dirty_generation: u64,
    /// Recent block mutation boundaries remain available after persistence
    /// clears `block_dirty_from`, allowing projections that have not rendered
    /// yet to update an appended suffix without scanning the unchanged prefix.
    record_changes: VecDeque<(u64, usize)>,
}

impl BlockHistory {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            entries: HashMap::new(),
            next_id: 0,
            tool_states: HashMap::new(),
            finished_blocks: Vec::new(),
            generation: 0,
            order_generation: 0,
            navigation_generation: 0,
            persisted_block_count: 0,
            hydrated_ids: HashSet::new(),
            hydrated_block_bytes: 0,
            hydrated_tool_state_bytes: 0,
            record_dirty_from: None,
            record_dirty_generation: 0,
            record_changes: VecDeque::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn order_generation(&self) -> u64 {
        self.order_generation
    }

    pub fn navigation_generation(&self) -> u64 {
        self.navigation_generation
    }

    pub(crate) fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn bump_order_generation(&mut self) {
        self.bump_generation();
        self.order_generation = self.order_generation.wrapping_add(1);
        self.bump_navigation_generation();
    }

    fn bump_navigation_generation(&mut self) {
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
    }

    fn recount_persisted_blocks(&mut self) {
        self.persisted_block_count = self
            .order
            .iter()
            .filter(|id| {
                self.entries
                    .get(id)
                    .is_some_and(BlockEntry::is_persisted_block)
            })
            .count();
    }

    fn recount_hydrated_entries(&mut self) {
        self.hydrated_ids.clear();
        self.hydrated_block_bytes = 0;
        self.hydrated_tool_state_bytes = 0;
        for id in &self.order {
            let Some(entry @ BlockEntry::Hydrated { .. }) = self.entries.get(id) else {
                continue;
            };
            self.hydrated_ids.insert(*id);
            self.hydrated_block_bytes = self
                .hydrated_block_bytes
                .saturating_add(entry.hydrated_block_weight());
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_add(entry.hydrated_tool_state_weight());
        }
    }

    fn insert_entry(&mut self, id: BlockId, entry: BlockEntry) -> Option<BlockEntry> {
        let block_bytes = entry.hydrated_block_weight();
        let tool_state_bytes = entry.hydrated_tool_state_weight();
        let hydrated = matches!(&entry, BlockEntry::Hydrated { .. });
        let persisted = entry.is_persisted_block();
        let previous = self.entries.insert(id, entry);
        let was_persisted = previous
            .as_ref()
            .is_some_and(BlockEntry::is_persisted_block);
        match (was_persisted, persisted) {
            (false, true) => {
                self.persisted_block_count = self.persisted_block_count.saturating_add(1);
            }
            (true, false) => {
                self.persisted_block_count = self.persisted_block_count.saturating_sub(1);
            }
            _ => {}
        }
        if let Some(previous) = previous.as_ref() {
            self.hydrated_block_bytes = self
                .hydrated_block_bytes
                .saturating_sub(previous.hydrated_block_weight());
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_sub(previous.hydrated_tool_state_weight());
        }
        if hydrated {
            self.hydrated_ids.insert(id);
            self.hydrated_block_bytes = self.hydrated_block_bytes.saturating_add(block_bytes);
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_add(tool_state_bytes);
        } else {
            self.hydrated_ids.remove(&id);
        }
        previous
    }

    fn remove_entry(&mut self, id: BlockId) -> Option<BlockEntry> {
        let entry = self.entries.remove(&id)?;
        if entry.is_persisted_block() {
            self.persisted_block_count = self.persisted_block_count.saturating_sub(1);
        }
        self.hydrated_ids.remove(&id);
        self.hydrated_block_bytes = self
            .hydrated_block_bytes
            .saturating_sub(entry.hydrated_block_weight());
        self.hydrated_tool_state_bytes = self
            .hydrated_tool_state_bytes
            .saturating_sub(entry.hydrated_tool_state_weight());
        Some(entry)
    }

    /// Marks externally-mutated history as changed so snapshots and projections rebuild.
    pub fn mark_changed(&mut self) {
        self.recount_persisted_blocks();
        self.recount_hydrated_entries();
        self.bump_order_generation();
        self.mark_record_dirty_from(0);
    }

    pub fn persisted_block_count(&self) -> usize {
        self.persisted_block_count
    }

    pub fn record_dirty_from(&self) -> Option<usize> {
        self.record_dirty_from
    }

    pub fn record_dirty_generation(&self) -> u64 {
        self.record_dirty_generation
    }

    pub fn record_changed_from_since(&self, generation: u64) -> Option<usize> {
        if generation == self.record_dirty_generation {
            return Some(self.order.len());
        }
        let first_generation = generation.checked_add(1)?;
        let first = self
            .record_changes
            .iter()
            .position(|(change_generation, _)| *change_generation == first_generation)?;
        let mut expected_generation = first_generation;
        let mut changed_from = self.order.len();
        for (change_generation, index) in self.record_changes.iter().skip(first) {
            if *change_generation != expected_generation {
                return None;
            }
            changed_from = changed_from.min(*index);
            if *change_generation == self.record_dirty_generation {
                return Some(changed_from);
            }
            expected_generation = expected_generation.checked_add(1)?;
        }
        None
    }

    pub fn clear_record_dirty(&mut self) {
        self.record_dirty_from = None;
    }

    pub fn require_record_resave_from(&mut self, idx: usize) {
        self.record_dirty_generation = self
            .record_dirty_generation
            .checked_add(1)
            .expect("transcript block generation overflow");
        self.record_changes
            .push_back((self.record_dirty_generation, idx));
        if self.record_changes.len() > RECORD_CHANGE_LOG_CAPACITY {
            self.record_changes.pop_front();
        }
        self.record_dirty_from = Some(
            self.record_dirty_from
                .map_or(idx, |current| current.min(idx)),
        );
    }

    fn mark_record_dirty_from(&mut self, idx: usize) {
        self.require_record_resave_from(idx);
    }

    fn mark_record_dirty_for_id(&mut self, id: BlockId) {
        if let Some(idx) = self.order.iter().position(|candidate| *candidate == id) {
            self.mark_record_dirty_from(idx);
        }
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        std::mem::take(&mut self.finished_blocks)
    }

    pub fn content_hash(&self, id: BlockId) -> u64 {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => live.metadata.content_hash,
            Some(BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. }) => {
                stored.content_hash
            }
            None => 0,
        }
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.tool_states
            .get(call_id)
            .and_then(ToolStateEntry::state)
    }

    pub fn tool_status(&self, call_id: &str) -> Option<ToolStatus> {
        self.tool_states.get(call_id).map(ToolStateEntry::status)
    }

    pub fn tool_states(&self) -> impl Iterator<Item = (&str, &ToolState)> {
        self.tool_states
            .iter()
            .filter_map(|(call_id, state)| state.state().map(|state| (call_id.as_str(), state)))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Returns a full block only when it is live or explicitly hydrated.
    /// Stored entries never perform I/O or hydrate through this accessor.
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.entries.get(&id).and_then(BlockEntry::block)
    }

    pub fn is_materialized(&self, id: BlockId) -> bool {
        self.block(id).is_some()
    }

    pub fn is_live(&self, id: BlockId) -> bool {
        matches!(self.entries.get(&id), Some(BlockEntry::Live(_)))
    }

    pub fn is_hydrated(&self, id: BlockId) -> bool {
        matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. }))
    }

    pub fn stored_ref(&self, id: BlockId) -> Option<&Arc<StoredBlockRef>> {
        self.entries.get(&id).and_then(BlockEntry::stored)
    }

    pub fn reindex_stored_records_from(&mut self, order_start: usize, record_start: usize) {
        let mut record_index = record_start;
        for id in self.order.iter().skip(order_start.min(self.order.len())) {
            let Some(entry) = self.entries.get_mut(id) else {
                continue;
            };
            if !entry.is_persisted_block() {
                continue;
            }
            match entry {
                BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. } => {
                    Arc::make_mut(stored).record_index = record_index;
                }
                BlockEntry::Live(_) => {}
            }
            record_index = record_index.saturating_add(1);
        }
    }

    pub fn row_estimate_text(&self, id: BlockId) -> Option<BlockText<'_>> {
        self.entries
            .get(&id)
            .and_then(BlockEntry::row_estimate_text)
    }

    pub fn estimated_text_bytes(&self, id: BlockId) -> u64 {
        self.entries
            .get(&id)
            .map_or(0, BlockEntry::estimated_text_bytes)
    }

    pub fn raw_text(&self, id: BlockId) -> Option<String> {
        self.entries.get(&id).and_then(BlockEntry::raw_text)
    }

    pub fn cloned_block(&self, id: BlockId) -> Option<Block> {
        self.entries.get(&id).and_then(BlockEntry::cloned_block)
    }

    pub fn first_line(&self, id: BlockId) -> Option<String> {
        if let Some(stored) = self.stored_ref(id) {
            return Some(stored.first_line().to_string());
        }
        self.raw_text(id).map(|text| {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .to_string()
        })
    }

    pub fn is_stable_scroll_anchor(&self, id: BlockId) -> bool {
        self.stored_ref(id).map_or_else(
            || self.block(id).is_some_and(Block::is_stable_scroll_anchor),
            |stored| stored.stable_scroll_anchor,
        )
    }

    pub fn block_kind(&self, id: BlockId) -> Option<&'static str> {
        self.entries.get(&id).map(BlockEntry::kind)
    }

    pub fn tool_name(&self, id: BlockId) -> Option<&str> {
        self.entries.get(&id).and_then(BlockEntry::tool_name)
    }

    pub fn tool_call_id(&self, id: BlockId) -> Option<&str> {
        self.entries.get(&id).and_then(BlockEntry::tool_call_id)
    }

    pub fn is_tool_draft(&self, id: BlockId) -> bool {
        self.entries.get(&id).is_some_and(BlockEntry::is_tool_draft)
    }

    pub fn process_field(&self, id: BlockId, field: &str) -> Option<String> {
        self.entries
            .get(&id)
            .and_then(|entry| entry.process_field(field))
    }

    pub fn arg_field(&self, id: BlockId, arg: &str) -> Option<&serde_json::Value> {
        self.entries.get(&id).and_then(|entry| entry.arg_field(arg))
    }

    pub fn block_records(&self) -> Vec<TranscriptBlockRecord> {
        self.block_records_from(0)
    }

    pub fn block_records_with_ids(&self) -> Vec<TranscriptBlockRecordWithId> {
        self.block_records_with_ids_from(0)
    }

    pub fn record_index_for_order_index(&self, start: usize) -> usize {
        let start = start.min(self.order.len());
        let suffix_len = self.order.len().saturating_sub(start);
        let record_index = if start <= suffix_len {
            self.order
                .iter()
                .take(start)
                .filter(|id| {
                    self.entries
                        .get(id)
                        .is_some_and(BlockEntry::is_persisted_block)
                })
                .count()
        } else {
            let suffix_blocks = self
                .order
                .iter()
                .skip(start)
                .filter(|id| {
                    self.entries
                        .get(id)
                        .is_some_and(BlockEntry::is_persisted_block)
                })
                .count();
            self.persisted_block_count.saturating_sub(suffix_blocks)
        };
        smelt_perf::perf::record_value(
            "transcript:block_record_index:entries_scanned",
            start.min(suffix_len) as u64,
        );
        record_index
    }

    pub fn block_records_from(&self, start: usize) -> Vec<TranscriptBlockRecord> {
        self.block_records_with_ids_from(start)
            .into_iter()
            .map(|record| record.record)
            .collect()
    }

    pub fn block_records_with_ids_from(&self, start: usize) -> Vec<TranscriptBlockRecordWithId> {
        self.order
            .iter()
            .skip(start.min(self.order.len()))
            .filter_map(|id| self.block_record_with_id(*id))
            .collect()
    }

    fn block_record_with_id(&self, id: BlockId) -> Option<TranscriptBlockRecordWithId> {
        let entry = self.entries.get(&id)?;
        if !entry.is_persisted_block() {
            return None;
        }
        let block = entry.cloned_block()?;
        let tool_state = block.tool_call_id().and_then(|call_id| {
            self.tool_state(call_id)
                .cloned()
                .map(|state| (call_id.to_string(), state))
        });
        Some(TranscriptBlockRecordWithId {
            block_id: id,
            record: TranscriptBlockRecord {
                block,
                content_hash: self.content_hash(id),
                origin: self.block_origin(id),
                tool_state,
            },
        })
    }

    pub fn stored_ref_for_materialized(
        &self,
        id: BlockId,
        record_index: usize,
    ) -> Option<Arc<StoredBlockRef>> {
        if let Some(stored) = self.stored_ref(id) {
            if stored.record_index == record_index {
                return Some(Arc::clone(stored));
            }
        }
        let record = self.block_record_with_id(id)?;
        let indexed = transcript_indexed_text(
            &record.record.block,
            record.record.tool_state.as_ref().map(|(_, state)| state),
        );
        let (_, stored) = StoredBlockRef::from_record(
            record_index,
            id,
            &record.record,
            indexed.estimated_text_bytes,
            preview(&indexed.indexed_text, 512),
        );
        Some(stored)
    }

    pub fn from_block_records(records: Vec<TranscriptBlockRecord>) -> Self {
        let records = records
            .into_iter()
            .enumerate()
            .map(|(index, record)| TranscriptBlockRecordWithId {
                block_id: BlockId::new(index as u64),
                record,
            })
            .collect();
        Self::from_block_records_with_ids(records)
    }

    pub fn from_block_records_with_ids(records: Vec<TranscriptBlockRecordWithId>) -> Self {
        let mut history = Self::new();
        let stored = records
            .into_iter()
            .enumerate()
            .map(|(record_index, record)| {
                let indexed = transcript_indexed_text(
                    &record.record.block,
                    record.record.tool_state.as_ref().map(|(_, state)| state),
                );
                StoredBlockRef::from_record(
                    record_index,
                    record.block_id,
                    &record.record,
                    indexed.estimated_text_bytes,
                    preview(&indexed.indexed_text, 512),
                )
            })
            .collect::<Vec<_>>();
        history.install_stored_projection(stored);
        history.clear_record_dirty();
        history
    }

    pub fn install_stored_projection(
        &mut self,
        records: impl IntoIterator<Item = (BlockId, Arc<StoredBlockRef>)>,
    ) {
        let records = records.into_iter().collect::<Vec<_>>();
        let projected_ids = records
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<BlockId>>();
        let preserved_live = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                !projected_ids.contains(id)
                    && matches!(self.entries.get(id), Some(BlockEntry::Live(_)))
            })
            .collect::<Vec<_>>();
        let mut next_order = Vec::with_capacity(records.len().saturating_add(preserved_live.len()));
        for (id, stored) in records {
            self.next_id = self.next_id.max(id.0.saturating_add(1));
            next_order.push(id);
            if let Some(call_id) = stored.tool_call_id.as_ref() {
                let entry = self.tool_states.entry(call_id.clone());
                if let std::collections::hash_map::Entry::Vacant(entry) = entry {
                    if let Some(status) = stored.tool_status {
                        entry.insert(ToolStateEntry::Stored {
                            status,
                            display_hash: stored.tool_display_hash,
                        });
                    }
                }
            }
            match self.entries.remove(&id) {
                Some(entry @ BlockEntry::Live(_)) => {
                    self.entries.insert(id, entry);
                }
                Some(BlockEntry::Hydrated {
                    stored: previous,
                    block,
                    block_weight,
                    tool_state_weight,
                }) if previous.record_index == stored.record_index => {
                    self.entries.insert(
                        id,
                        BlockEntry::Hydrated {
                            stored,
                            block,
                            block_weight,
                            tool_state_weight,
                        },
                    );
                }
                _ => {
                    self.entries.insert(id, BlockEntry::Stored(stored));
                }
            }
        }
        next_order.extend(preserved_live);
        let retained_ids = next_order.iter().copied().collect::<HashSet<_>>();
        self.entries.retain(|id, _| retained_ids.contains(id));
        if self.order != next_order {
            self.order = next_order;
            self.bump_order_generation();
            self.bump_navigation_generation();
        }
        self.recount_persisted_blocks();
        self.recount_hydrated_entries();
        self.gc_tool_states();
    }

    pub fn install_hydrated_record(
        &mut self,
        id: BlockId,
        stored: Arc<StoredBlockRef>,
        record: TranscriptBlockRecord,
    ) -> bool {
        if stored.content_hash != 0
            && record.content_hash != 0
            && stored.content_hash != record.content_hash
        {
            return false;
        }
        let block = record.block.clone().normalize_content();
        if block.kind() != stored.kind.as_str() {
            return false;
        }
        let block_hash = block.content_hash();
        if stored.content_hash != 0 && stored.content_hash != block_hash {
            return false;
        }
        let tool_state_weight = record
            .tool_state
            .as_ref()
            .map_or(0, |(_, state)| tool_state_retained_bytes(state));
        let block_weight = block_retained_bytes(&block);
        let weight = block_weight.saturating_add(tool_state_weight);
        let had_entry = self.entries.contains_key(&id);
        debug_assert_eq!(had_entry, self.order.contains(&id));
        if let Some((call_id, state)) = record.tool_state {
            if !matches!(
                self.tool_states.get(&call_id),
                Some(ToolStateEntry::Live(_))
            ) {
                self.tool_states
                    .insert(call_id, ToolStateEntry::Hydrated(state));
            }
        }
        if !had_entry {
            self.order.push(id);
            self.bump_order_generation();
            if stored.kind == StoredBlockKind::User {
                self.bump_navigation_generation();
            }
        }
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        self.insert_entry(
            id,
            BlockEntry::Hydrated {
                stored,
                block: Box::new(block),
                block_weight,
                tool_state_weight,
            },
        );
        smelt_perf::perf::record_value("transcript:block_cache:hydrated_bytes", weight as u64);
        true
    }

    pub fn promote_hydrated(&mut self, id: BlockId) -> bool {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. })) {
            return self.is_live(id);
        }
        let Some(BlockEntry::Hydrated { block, stored, .. }) = self.remove_entry(id) else {
            unreachable!("entry was checked as hydrated");
        };
        let metadata = BlockMetadata {
            content_hash: stored.content_hash,
            origin: stored.origin,
            ..BlockMetadata::default()
        };
        if let Some(call_id) = stored.tool_call_id.as_ref() {
            if let Some(ToolStateEntry::Hydrated(state)) = self.tool_states.remove(call_id) {
                self.tool_states
                    .insert(call_id.clone(), ToolStateEntry::Live(state));
            }
        }
        self.insert_entry(
            id,
            BlockEntry::Live(Box::new(LiveBlock {
                block: *block,
                metadata,
            })),
        );
        true
    }

    pub fn evict_hydrated(&mut self, id: BlockId) -> usize {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. })) {
            return 0;
        }
        let Some(BlockEntry::Hydrated {
            stored,
            block_weight,
            tool_state_weight,
            ..
        }) = self.remove_entry(id)
        else {
            unreachable!("entry was checked as hydrated");
        };
        let weight = block_weight.saturating_add(tool_state_weight);
        if let Some(call_id) = stored.tool_call_id.as_ref() {
            if matches!(
                self.tool_states.get(call_id),
                Some(ToolStateEntry::Hydrated(_))
            ) {
                if let Some(status) = stored.tool_status {
                    self.tool_states.insert(
                        call_id.clone(),
                        ToolStateEntry::Stored {
                            status,
                            display_hash: stored.tool_display_hash,
                        },
                    );
                } else {
                    self.tool_states.remove(call_id);
                }
            }
        }
        self.insert_entry(id, BlockEntry::Stored(stored));
        smelt_perf::perf::record_value("transcript:block_cache:evicted_bytes", weight as u64);
        weight
    }

    pub fn dematerialize_live(&mut self, id: BlockId, stored: Arc<StoredBlockRef>) -> usize {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Live(_))) {
            return 0;
        }
        let Some(BlockEntry::Live(live)) = self.remove_entry(id) else {
            unreachable!("entry was checked as live");
        };
        let mut weight = block_retained_bytes(&live.block);
        if let Some(call_id) = stored.tool_call_id.as_ref() {
            if let Some(ToolStateEntry::Live(state)) = self.tool_states.remove(call_id) {
                weight = weight.saturating_add(tool_state_retained_bytes(&state));
            }
            if let Some(status) = stored.tool_status {
                self.tool_states.insert(
                    call_id.clone(),
                    ToolStateEntry::Stored {
                        status,
                        display_hash: stored.tool_display_hash,
                    },
                );
            }
        }
        self.insert_entry(id, BlockEntry::Stored(stored));
        weight
    }

    pub fn hydrated_blocks(&self) -> impl Iterator<Item = (BlockId, usize)> + '_ {
        self.hydrated_ids.iter().filter_map(|id| {
            self.entries
                .get(id)
                .map(|entry| (*id, entry.hydrated_weight()))
        })
    }

    pub fn materialized_retained_bytes(&self, id: BlockId) -> usize {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => block_retained_bytes(&live.block).saturating_add(
                self.tool_call_id(id)
                    .and_then(|call_id| self.tool_states.get(call_id))
                    .and_then(|entry| match entry {
                        ToolStateEntry::Live(state) => Some(tool_state_retained_bytes(state)),
                        _ => None,
                    })
                    .unwrap_or_default(),
            ),
            Some(entry @ BlockEntry::Hydrated { .. }) => entry.hydrated_weight(),
            _ => 0,
        }
    }

    pub fn live_block_retained_bytes(&self) -> usize {
        self.entries
            .values()
            .filter_map(|entry| match entry {
                BlockEntry::Live(live) => Some(block_retained_bytes(&live.block)),
                _ => None,
            })
            .sum()
    }

    pub fn live_tool_state_retained_bytes(&self) -> usize {
        self.tool_states
            .values()
            .filter_map(|entry| match entry {
                ToolStateEntry::Live(state) => Some(tool_state_retained_bytes(state)),
                _ => None,
            })
            .sum()
    }

    pub fn hydrated_block_retained_bytes(&self) -> usize {
        self.hydrated_block_bytes
    }

    pub fn hydrated_tool_state_retained_bytes(&self) -> usize {
        self.hydrated_tool_state_bytes
    }

    pub fn tool_state_metadata_retained_bytes(&self) -> usize {
        self.tool_states
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, ToolStateEntry)>())
            .saturating_add(self.tool_states.keys().map(String::capacity).sum::<usize>())
    }

    pub fn block_metadata_retained_bytes(&self) -> usize {
        self.live_block_count()
            .saturating_mul(std::mem::size_of::<BlockMetadata>())
    }

    pub fn live_retained_bytes(&self) -> usize {
        self.live_block_retained_bytes()
            .saturating_add(self.live_tool_state_retained_bytes())
    }

    pub fn hydrated_retained_bytes(&self) -> usize {
        self.hydrated_block_bytes
            .saturating_add(self.hydrated_tool_state_bytes)
    }

    pub fn live_block_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, BlockEntry::Live(_)))
            .count()
    }

    pub fn stored_block_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, BlockEntry::Stored(_)))
            .count()
    }

    pub fn hydrated_block_count(&self) -> usize {
        self.hydrated_ids.len()
    }

    pub fn block_id_at(&self, i: usize) -> Option<BlockId> {
        let id = self.order.get(i).copied()?;
        assert!(
            self.entries.contains_key(&id),
            "block id in transcript order"
        );
        Some(id)
    }

    pub fn materialized_block_at(&self, i: usize) -> Option<&Block> {
        self.block(self.block_id_at(i)?)
    }

    pub fn last_block_id(&self) -> Option<BlockId> {
        let id = self.order.last().copied()?;
        assert!(
            self.entries.contains_key(&id),
            "block id in transcript order"
        );
        Some(id)
    }

    pub fn has_history_origin_at_or_after(&self, before_history_index: usize) -> bool {
        self.first_block_index_for_history_origin_at_or_after(before_history_index)
            .is_some()
    }

    pub fn block_origin(&self, id: BlockId) -> Option<BlockOrigin> {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => live.metadata.origin,
            Some(BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. }) => stored.origin,
            None => None,
        }
    }

    pub fn block_origin_at(&self, i: usize) -> Option<BlockOrigin> {
        self.order.get(i).and_then(|id| self.block_origin(*id))
    }

    pub fn first_block_index_for_history_origin_at_or_after(
        &self,
        before_history_index: usize,
    ) -> Option<usize> {
        self.order.iter().position(|id| {
            matches!(
                self.block_origin(*id),
                Some(BlockOrigin::History(history_index)) if history_index >= before_history_index
            )
        })
    }

    pub fn status(&self, id: BlockId) -> Option<Status> {
        self.entries.get(&id).map(|entry| match entry {
            BlockEntry::Live(live) => live.metadata.status,
            BlockEntry::Stored(_) | BlockEntry::Hydrated { .. } => Status::Done,
        })
    }

    /// Status changes do not invalidate the layout cache (style concern only).
    pub(crate) fn set_status(&mut self, id: BlockId, status: Status) {
        let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
            panic!("status can only change on a live transcript block: {id:?}");
        };
        let was_streaming = matches!(live.metadata.status, Status::Streaming);
        live.metadata.status = status;
        if matches!(status, Status::Done) && was_streaming {
            self.finished_blocks.push(id);
        }
        self.bump_generation();
    }

    fn add_block(
        &mut self,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
    ) -> BlockId {
        let block = block.normalize_content();
        let hash = block.content_hash();
        let id = BlockId(self.next_id);
        self.next_id += 1;
        let order_index = idx.map_or(self.order.len(), |idx| idx.min(self.order.len()));
        let entry = BlockEntry::Live(Box::new(LiveBlock {
            block,
            metadata: BlockMetadata {
                content_hash: hash,
                origin,
                ..BlockMetadata::default()
            },
        }));
        self.order.insert(order_index, id);
        self.insert_entry(id, entry);
        self.bump_order_generation();
        self.mark_record_dirty_from(order_index);
        id
    }

    fn add_hydrated_block(
        &mut self,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
        content_hash: Option<u64>,
    ) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.add_hydrated_block_with_id(id, idx, block, origin, content_hash)
    }

    fn add_hydrated_block_with_id(
        &mut self,
        id: BlockId,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
        content_hash: Option<u64>,
    ) -> BlockId {
        let normalized = block.normalize_content();
        let normalized_hash = normalized.content_hash();
        let hash = content_hash
            .filter(|hash| *hash == normalized_hash)
            .unwrap_or(normalized_hash);
        let block = normalized;
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        let order_index = idx.map_or(self.order.len(), |idx| idx.min(self.order.len()));
        let tool_state = block.tool_call_id().and_then(|call_id| {
            self.tool_state(call_id)
                .cloned()
                .map(|state| (call_id.to_string(), state))
        });
        let record = TranscriptBlockRecord {
            block,
            content_hash: hash,
            origin,
            tool_state,
        };
        let indexed = transcript_indexed_text(
            &record.block,
            record.tool_state.as_ref().map(|(_, state)| state),
        );
        let (_, stored) = StoredBlockRef::from_record(
            order_index,
            id,
            &record,
            indexed.estimated_text_bytes,
            preview(&indexed.indexed_text, 512),
        );
        let block = record.block.clone();
        let block_weight = block_retained_bytes(&block);
        let tool_state_weight = record
            .tool_state
            .as_ref()
            .map_or(0, |(_, state)| tool_state_retained_bytes(state));
        let entry = BlockEntry::Hydrated {
            stored,
            block: Box::new(block),
            block_weight,
            tool_state_weight,
        };
        self.order.insert(order_index, id);
        self.insert_entry(id, entry);
        self.bump_order_generation();
        self.mark_record_dirty_from(order_index);
        id
    }

    pub(crate) fn push(&mut self, block: Block) -> BlockId {
        self.add_block(None, block, None)
    }

    pub(crate) fn push_with_origin(&mut self, block: Block, origin: BlockOrigin) -> BlockId {
        self.add_block(None, block, Some(origin))
    }

    pub fn push_hydrated_block_with_origin(
        &mut self,
        block: Block,
        origin: BlockOrigin,
    ) -> BlockId {
        self.add_hydrated_block(None, block, Some(origin), None)
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
                    self.block_origin(*id),
                    Some(BlockOrigin::History(history_index)) if history_index >= before_history_index
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
            .filter(|id| matches!(self.block_origin(**id), Some(BlockOrigin::CheckpointMarker)))
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
        if self.block_origin(id).is_some() || !self.is_materialized(id) {
            return None;
        }
        self.order.remove(idx);
        if let Some(call_id) = self.tool_call_id(id).map(str::to_string) {
            self.tool_states.remove(&call_id);
        }
        let block = self
            .remove_entry(id)
            .and_then(BlockEntry::into_materialized);
        self.bump_order_generation();
        self.mark_record_dirty_from(idx);
        block
    }

    fn remove_checkpoint_marker(&mut self) {
        let removed: Vec<BlockId> = self
            .order
            .iter()
            .copied()
            .filter(|id| matches!(self.block_origin(*id), Some(BlockOrigin::CheckpointMarker)))
            .collect();
        if removed.is_empty() {
            return;
        }
        let first_removed = self
            .order
            .iter()
            .position(|id| removed.contains(id))
            .unwrap_or(self.order.len());
        self.order.retain(|id| !removed.contains(id));
        for id in removed {
            self.remove_entry(id);
        }
        self.bump_order_generation();
        self.mark_record_dirty_from(first_removed);
    }

    pub(crate) fn push_with_state(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
    ) -> BlockId {
        self.tool_states
            .insert(call_id, ToolStateEntry::Live(state));
        self.push(block)
    }

    pub(crate) fn push_with_state_and_origin(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
        origin: BlockOrigin,
    ) -> BlockId {
        self.tool_states
            .insert(call_id, ToolStateEntry::Live(state));
        self.push_with_origin(block, origin)
    }

    pub fn push_hydrated_block_with_state_and_origin(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
        origin: BlockOrigin,
    ) -> BlockId {
        self.tool_states
            .insert(call_id, ToolStateEntry::Hydrated(state));
        self.add_hydrated_block(None, block, Some(origin), None)
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let dirty_idx = self
            .order
            .iter()
            .position(|id| self.tool_call_id(*id) == Some(call_id));
        if let Some(id) = dirty_idx.and_then(|idx| self.order.get(idx).copied()) {
            self.promote_hydrated(id);
        }
        let Some(ToolStateEntry::Live(state)) = self.tool_states.get_mut(call_id) else {
            return false;
        };
        mutator(state);
        self.bump_generation();
        if let Some(idx) = dirty_idx {
            self.mark_record_dirty_from(idx);
        }
        true
    }

    /// Replace block content in place. Preserves `BlockId`, `Status`, and
    /// `ViewState`. No-ops when the block doesn't exist (e.g. truncated during
    /// a stream). Same content hash skips the generation bump.
    pub fn rewrite(&mut self, id: BlockId, block: Block) {
        let Some(previous_navigation) = self.entries.get(&id).map(BlockEntry::navigation_signature)
        else {
            return;
        };
        let origin = self.block_origin(id);
        let status = self
            .status(id)
            .expect("rewrite target disappeared after existence check");
        let previous_hash = self.content_hash(id);
        self.promote_hydrated(id);
        let block = block.normalize_content();
        let navigation = (
            block.kind(),
            block
                .row_estimate_text()
                .map(BlockText::first_source_line)
                .unwrap_or_default(),
        );
        let hash = block.content_hash();
        let entry = BlockEntry::Live(Box::new(LiveBlock {
            block,
            metadata: BlockMetadata {
                content_hash: hash,
                status,
                origin,
            },
        }));
        self.insert_entry(id, entry);
        if previous_hash == hash {
            return;
        }
        self.bump_generation();
        if navigation != previous_navigation {
            self.bump_navigation_generation();
        }
        self.mark_record_dirty_for_id(id);
    }

    pub(crate) fn rewrite_with_tool_state(
        &mut self,
        id: BlockId,
        block: Block,
        call_id: String,
        state: ToolState,
    ) {
        self.rewrite(id, block);
        self.tool_states
            .insert(call_id, ToolStateEntry::Live(state));
        self.bump_generation();
        self.mark_record_dirty_for_id(id);
    }

    pub(crate) fn remove_block(&mut self, id: BlockId) {
        if !self.entries.contains_key(&id) {
            return;
        }
        let dirty_idx = self.order.iter().position(|candidate| *candidate == id);
        self.order.retain(|candidate| *candidate != id);
        self.remove_entry(id);
        self.bump_order_generation();
        if let Some(idx) = dirty_idx {
            self.mark_record_dirty_from(idx);
        }
        self.gc_tool_states();
    }

    pub fn clear(&mut self) {
        if self.order.is_empty() {
            self.entries.clear();
            self.persisted_block_count = 0;
            self.hydrated_ids.clear();
            self.hydrated_block_bytes = 0;
            self.hydrated_tool_state_bytes = 0;
            self.next_id = 0;
            self.tool_states.clear();
            return;
        }
        self.order.clear();
        self.entries.clear();
        self.persisted_block_count = 0;
        self.hydrated_ids.clear();
        self.hydrated_block_bytes = 0;
        self.hydrated_tool_state_bytes = 0;
        self.next_id = 0;
        self.tool_states.clear();
        self.bump_order_generation();
        self.mark_record_dirty_from(0);
    }

    pub fn block_gap(&self, i: usize) -> u16 {
        if i == 0 {
            return 0;
        }
        let Some(above) = self.order.get(i - 1).and_then(|id| self.entries.get(id)) else {
            return 0;
        };
        let Some(below) = self.order.get(i).and_then(|id| self.entries.get(id)) else {
            return 0;
        };
        gap_between_entries(above, below)
    }

    pub fn rendered_block_gap(&self, i: usize, rendered_rows: usize) -> u16 {
        if rendered_rows == 0 {
            0
        } else {
            self.block_gap(i)
        }
    }

    pub fn sidecar_hash(&self, id: BlockId) -> u64 {
        self.tool_call_id(id)
            .and_then(|call_id| self.tool_states.get(call_id))
            .map_or(0, ToolStateEntry::display_hash)
    }

    /// Substitute the actual per-block content and sidecar hash into a base
    /// `LayoutKey` so cache lookups and layout passes agree.
    pub fn resolve_key(&self, id: BlockId, base: LayoutKey) -> LayoutKey {
        LayoutKey {
            content_hash: self.content_hash(id),
            sidecar_hash: self.sidecar_hash(id),
            ..base
        }
    }

    pub(crate) fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        for id in removed {
            self.remove_entry(id);
        }
        self.bump_order_generation();
        self.mark_record_dirty_from(idx);
        self.gc_tool_states();
    }

    pub(crate) fn gc_tool_states(&mut self) {
        let live: HashSet<String> = self
            .order
            .iter()
            .filter_map(|id| self.tool_call_id(*id).map(str::to_string))
            .collect();
        self.tool_states.retain(|cid, _| live.contains(cid));
    }
}

/// Blank row gap before `below` given the preceding block. Most block
/// transitions are separated by one blank row. Adjacent code lines collapse,
/// and markdown headings sit directly on top of their following content.
pub fn gap_between(above: &Block, below: &Block) -> u16 {
    gap_between_parts(
        above.kind(),
        below.kind(),
        starts_with_thinking_title(below),
        ends_with_heading(above),
    )
}

fn gap_between_entries(above: &BlockEntry, below: &BlockEntry) -> u16 {
    gap_between_parts(
        above.kind(),
        below.kind(),
        entry_starts_with_thinking_title(below),
        entry_ends_with_heading(above),
    )
}

fn gap_between_parts(
    above_kind: &str,
    below_kind: &str,
    below_starts_thinking_title: bool,
    above_ends_heading: bool,
) -> u16 {
    if above_kind == "code" && below_kind == "code" {
        return 0;
    }
    if above_kind == "thinking" && below_kind == "thinking" {
        return if below_starts_thinking_title { 1 } else { 0 };
    }
    if matches!(below_kind, "assistant" | "code") && above_ends_heading {
        return 0;
    }
    1
}

fn entry_starts_with_thinking_title(entry: &BlockEntry) -> bool {
    if let Some(stored) = entry.stored() {
        return stored.starts_with_thinking_title;
    }
    match entry.block() {
        Some(Block::Thinking { title, content, .. }) => {
            has_thinking_title(title.as_deref(), content)
        }
        _ => false,
    }
}

fn entry_ends_with_heading(entry: &BlockEntry) -> bool {
    if let Some(stored) = entry.stored() {
        return stored.ends_with_heading;
    }
    match entry.block() {
        Some(Block::Text { content }) => crate::content::markdown_ir::ends_with_heading(content),
        _ => false,
    }
}

fn starts_with_thinking_title(block: &Block) -> bool {
    match block {
        Block::Thinking { title, content, .. } => has_thinking_title(title.as_deref(), content),
        _ => false,
    }
}

fn has_thinking_title(title: Option<&str>, content: &str) -> bool {
    title.is_some()
        || content
            .lines()
            .find(|line| !line.trim().is_empty())
            .and_then(crate::content::markdown_stream::thinking_title)
            .is_some()
}

fn ends_with_heading(block: &Block) -> bool {
    let Block::Text { content } = block else {
        return false;
    };
    crate::content::markdown_ir::ends_with_heading(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_tool_state_with_content(content: &str, metadata: serde_json::Value) -> ToolState {
        ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: content.to_string(),
                is_error: false,
                metadata: Some(metadata),
            })),
            user_message: None,
            preview_output: None,
        }
    }

    fn indexed_tool_state(metadata: serde_json::Value) -> ToolState {
        indexed_tool_state_with_content("output", metadata)
    }

    fn edit_file_block(old_string: &str, new_string: &str) -> Block {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "file_path".to_string(),
            serde_json::Value::String("/tmp/example.rs".to_string()),
        );
        args.insert(
            "old_string".to_string(),
            serde_json::Value::String(old_string.to_string()),
        );
        args.insert(
            "new_string".to_string(),
            serde_json::Value::String(new_string.to_string()),
        );
        Block::ToolCall {
            call_id: "call-1".to_string(),
            name: "edit_file".to_string(),
            summary: protocol::StyledLines::from_plain("example.rs"),
            args,
        }
    }

    #[test]
    fn indexed_text_preserves_full_size_for_extent_estimation() {
        let block = Block::Text {
            content: "alpha λ".to_string(),
        };

        let indexed = transcript_indexed_text(&block, None);

        assert_eq!(indexed.indexed_text, "alpha λ");
        assert_eq!(indexed.estimated_text_bytes, "alpha λ".len() as u64);
    }

    #[test]
    fn indexed_text_cap_is_utf8_safe() {
        let text = format!("α{}ω", "日".repeat(100));

        let capped = cap_indexed_text(&text, 17);

        assert!(capped.starts_with("α"));
        assert!(capped.ends_with("ω"));
        assert!(capped.contains("bytes omitted"));
    }

    #[test]
    fn block_row_computes_indexed_text_from_record() {
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::from_plain("bash"),
                args: std::collections::HashMap::new(),
            },
            content_hash: 42,
            origin: None,
            tool_state: Some((
                "call-1".to_string(),
                indexed_tool_state_with_content("alpha λ", serde_json::json!({})),
            )),
        };

        let row = transcript_block_row(7, &record).expect("block row");

        assert_eq!(row.block_idx, 7);
        let expected = "bash\nok\nbash\nalpha λ";
        assert_eq!(row.indexed_text, expected);
        assert_eq!(row.estimated_text_bytes, expected.len() as u64);
    }

    #[test]
    fn indexed_text_includes_tool_identity_args_and_bounded_output() {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("printf needle".to_string()),
        );
        args.insert(
            "api_key".to_string(),
            serde_json::Value::String("secret needle".to_string()),
        );
        let block = Block::ToolCall {
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            summary: protocol::StyledLines::from_plain("printf needle"),
            args,
        };
        let output = format!(
            "visible head\n{}\nhidden needle\n{}\nvisible tail",
            "a".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES * 2),
            "b".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES * 2)
        );
        let state = indexed_tool_state_with_content(&output, serde_json::json!({}));

        let indexed = transcript_indexed_text(&block, Some(&state));

        assert!(indexed.indexed_text.contains("bash"));
        assert!(indexed.indexed_text.contains("command: printf needle"));
        assert!(!indexed.indexed_text.contains("secret needle"));
        assert!(indexed.indexed_text.contains("visible head"));
        assert!(indexed.indexed_text.contains("visible tail"));
        assert!(indexed.indexed_text.contains("bytes omitted"));
        assert!(!indexed.indexed_text.contains("hidden needle"));
        assert!((indexed.indexed_text.len() as u64) < indexed.estimated_text_bytes);
    }

    #[test]
    fn display_count_indexed_text_matches_default_lua_plural_rules() {
        let state = indexed_tool_state(serde_json::json!({
            "display_count": { "value": 2, "unit": "match" }
        }));
        assert_eq!(
            display_count_indexed_text(&state).as_deref(),
            Some("2 matches")
        );

        let state = indexed_tool_state(serde_json::json!({
            "display_count": { "value": 1, "unit": "file", "plural": "files" }
        }));
        assert_eq!(
            display_count_indexed_text(&state).as_deref(),
            Some("1 file")
        );
    }

    #[test]
    fn indexed_text_includes_thinking_summary_chrome() {
        let block = Block::Thinking {
            title: Some("Analyzing the bug".to_string()),
            summary_titles: vec![
                "Inspecting the report".to_string(),
                "Analyzing the bug".to_string(),
            ],
            content: "Checking files\nReviewing output".to_string(),
            kind: protocol::ReasoningKind::Summary,
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "**Inspecting the report**\n**Analyzing the bug**\nChecking files\nReviewing output\nAnalyzing the bug\n… 2 lines collapsed …"
        );

        let block = Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            content: "Checking files\nReviewing output".to_string(),
            kind: protocol::ReasoningKind::Raw,
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "Checking files\nReviewing output\nthinking\n… 2 lines collapsed …"
        );
    }

    #[test]
    fn indexed_text_includes_default_compacted_chrome() {
        let block = Block::Compacted {
            summary: "archived".to_string(),
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "archived\ncompacted\n─"
        );
    }

    #[test]
    fn indexed_text_includes_edit_file_snapshot_metadata() {
        let block = edit_file_block("old needle", "new needle");
        let state = indexed_tool_state_with_content(
            "edited example.rs",
            serde_json::json!({
                "path": "/tmp/example.rs",
                "old_content": "fn old_snapshot() {}\n",
                "new_content": "fn new_snapshot() {}\n",
            }),
        );

        assert_eq!(
            transcript_indexed_text(&block, Some(&state)).indexed_text,
            "edit_file\nok\nexample.rs\nedited example.rs\n1 old line, 1 new line\n/tmp/example.rs\nfn old_snapshot() {}\nfn new_snapshot() {}\n"
        );
    }

    #[test]
    fn indexed_text_includes_edit_file_planned_strings_without_snapshot() {
        let block = edit_file_block("alpha\nbeta", "gamma");

        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "edit_file\nexample.rs\n2 old lines, 1 new line\n/tmp/example.rs\nalpha\nbeta\ngamma"
        );
    }

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
    fn status_changes_do_not_bump_block_dirty_generation() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });
        history.clear_record_dirty();
        let generation = history.generation();
        let block_generation = history.record_dirty_generation();

        history.set_status(id, Status::Streaming);

        assert_ne!(history.generation(), generation);
        assert_eq!(history.record_dirty_generation(), block_generation);
        assert_eq!(history.record_dirty_from(), None);
    }

    #[test]
    fn block_change_log_tracks_unrendered_mutations_after_persistence() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "first".into(),
        });
        history.clear_record_dirty();
        let generation = history.record_dirty_generation();

        assert_eq!(
            history.record_changed_from_since(generation),
            Some(history.order.len())
        );

        history.push(Block::Text {
            content: "second".into(),
        });
        history.clear_record_dirty();
        assert_eq!(history.record_changed_from_since(generation), Some(1));

        history.require_record_resave_from(0);
        assert_eq!(history.record_changed_from_since(generation), Some(0));

        for _ in 0..RECORD_CHANGE_LOG_CAPACITY {
            history.require_record_resave_from(0);
        }
        assert_eq!(history.record_changed_from_since(generation), None);
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
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.content_hash(a), history.content_hash(b));
    }

    #[test]
    fn stored_block_metadata_does_not_hydrate_block() {
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "read_file".into(),
                summary: "read".into(),
                args: HashMap::from([("path".into(), serde_json::json!("/tmp/a"))]),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
        };
        let history = BlockHistory::from_block_records(vec![record]);
        let id = history.order[0];

        assert!(!history.is_materialized(id));
        assert_eq!(history.block_kind(id), Some("tool"));
        assert_eq!(history.tool_name(id), Some("read_file"));
        assert_eq!(history.tool_call_id(id), Some("call-1"));
        assert_eq!(
            history.arg_field(id, "path"),
            Some(&serde_json::json!("/tmp/a"))
        );
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
        assert_ne!(history.content_hash(id), 0);
        assert!(history.block(id).is_none());
        assert_eq!(history.last_block_id(), Some(id));
        assert!(history.materialized_block_at(0).is_none());
    }

    #[test]
    #[should_panic(expected = "block id in transcript order")]
    fn materialized_block_at_rejects_dangling_order_id() {
        let mut history = BlockHistory::new();
        history.order.push(BlockId::new(42));

        let _ = history.materialized_block_at(0);
    }

    #[test]
    fn explicit_hydration_and_eviction_preserve_exact_record() {
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "hi".into(),
                is_error: false,
                metadata: Some(serde_json::json!({"small": true})),
            })),
            user_message: None,
            preview_output: None,
        };
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: "run".into(),
                args: HashMap::from([("command".into(), serde_json::json!("echo hi"))]),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(1)),
            tool_state: Some(("call-1".into(), state.clone())),
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        let expected_weight =
            block_retained_bytes(&record.block) + tool_state_retained_bytes(&state);

        assert!(history.install_hydrated_record(id, stored, record));
        assert!(history.is_hydrated(id));
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        assert_eq!(history.hydrated_block_count(), 1);
        assert_eq!(history.hydrated_blocks().count(), 1);
        assert_eq!(history.hydrated_retained_bytes(), expected_weight);
        assert_eq!(
            history.hydrated_retained_bytes(),
            history
                .hydrated_block_retained_bytes()
                .saturating_add(history.hydrated_tool_state_retained_bytes())
        );
        assert!(matches!(history.block(id), Some(Block::ToolCall { name, .. }) if name == "bash"));
        let output = history
            .tool_state("call-1")
            .and_then(|tool_state| tool_state.output.as_ref())
            .expect("hydrated tool output");
        assert_eq!(output.content, "hi");
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(1)));

        assert_eq!(history.evict_hydrated(id), expected_weight);
        assert!(!history.is_materialized(id));
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_blocks().count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.tool_status("call-1"), Some(ToolStatus::Ok));
        assert!(history.tool_state("call-1").is_none());
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(1)));
    }

    #[test]
    fn state_specific_eviction_never_removes_other_entry_states() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "stored".into(),
            },
            content_hash: 0,
            origin: None,
            tool_state: None,
        };
        let mut stored_history = BlockHistory::from_block_records(vec![record.clone()]);
        let stored_id = stored_history.order[0];
        let stored = stored_history
            .stored_ref(stored_id)
            .cloned()
            .expect("stored ref");

        assert_eq!(stored_history.evict_hydrated(stored_id), 0);
        assert!(stored_history.stored_ref(stored_id).is_some());
        assert!(!stored_history.promote_hydrated(stored_id));
        assert!(stored_history.stored_ref(stored_id).is_some());
        assert_eq!(
            stored_history.dematerialize_live(stored_id, Arc::clone(&stored)),
            0
        );
        assert!(stored_history.stored_ref(stored_id).is_some());

        assert!(stored_history.install_hydrated_record(stored_id, Arc::clone(&stored), record));
        assert_eq!(stored_history.dematerialize_live(stored_id, stored), 0);
        assert!(stored_history.is_hydrated(stored_id));

        let mut live_history = BlockHistory::new();
        let live_id = live_history.push(Block::Text {
            content: "live".into(),
        });
        assert_eq!(live_history.evict_hydrated(live_id), 0);
        assert!(live_history.is_live(live_id));
        assert!(live_history.promote_hydrated(live_id));
        assert!(live_history.is_live(live_id));
    }

    #[test]
    fn hydrated_mutation_promotes_to_live_and_marks_block_dirty() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "before".into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        assert!(history.install_hydrated_record(id, stored, record));

        history.rewrite(
            id,
            Block::Text {
                content: "after".into(),
            },
        );

        assert!(history.is_live(id));
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.record_dirty_from(), Some(0));
        assert!(matches!(history.block(id), Some(Block::Text { content }) if content == "after"));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
    }

    #[test]
    fn hydrated_tool_mutation_promotes_block_and_tool_state_to_live() {
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "before".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: None,
            preview_output: None,
        };
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: "run".into(),
                args: HashMap::new(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: Some(("call-1".into(), state)),
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        assert!(history.install_hydrated_record(id, stored, record));

        assert!(history.update_tool_state("call-1", |tool_state| {
            tool_state.output.as_mut().unwrap().content = "after".into();
        }));

        assert!(history.is_live(id));
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.record_dirty_from(), Some(0));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
        assert_eq!(
            history
                .tool_state("call-1")
                .and_then(|tool_state| tool_state.output.as_ref())
                .map(|output| output.content.as_str()),
            Some("after")
        );
        assert!(history.live_tool_state_retained_bytes() > 0);
        assert_eq!(history.hydrated_tool_state_retained_bytes(), 0);
    }

    #[test]
    fn block_records_skip_transient_tool_drafts() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        history.push(Block::ToolDraft {
            stream_id: "stream-1".into(),
            call_id: Some("call-1".into()),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("echo hi"),
            args: HashMap::from([("command".into(), serde_json::json!("echo hi"))]),
            raw_arguments: "{\"command\":\"echo hi\"}".into(),
            finished: false,
        });
        history.push(Block::Text {
            content: "after".into(),
        });

        let records = history.block_records();

        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| !matches!(record.block, Block::ToolDraft { .. })));
        assert_eq!(records[0].block.raw_text().as_deref(), Some("before"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("after"));
    }

    #[test]
    fn block_records_skip_compaction_previews() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        history.push(Block::CompactionPreview {
            summary: "streaming summary".into(),
        });
        history.push(Block::Text {
            content: "after".into(),
        });

        let records = history.block_records();

        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| !matches!(record.block, Block::CompactionPreview { .. })));
        assert_eq!(records[0].block.raw_text().as_deref(), Some("before"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("after"));
    }

    #[test]
    fn block_record_index_counts_only_persisted_prefix() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        let draft = history.push_hydrated_block_with_origin(
            Block::ToolDraft {
                stream_id: "stream-1".into(),
                call_id: Some("call-1".into()),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("echo hi"),
                args: HashMap::from([("command".into(), serde_json::json!("echo hi"))]),
                raw_arguments: "{\"command\":\"echo hi\"}".into(),
                finished: false,
            },
            BlockOrigin::History(1),
        );
        history.push(Block::CompactionPreview {
            summary: "streaming summary".into(),
        });
        history.push(Block::Text {
            content: "after".into(),
        });
        history.push(Block::Text {
            content: "tail".into(),
        });

        assert_eq!(history.record_index_for_order_index(0), 0);
        assert_eq!(history.record_index_for_order_index(1), 1);
        assert_eq!(history.record_index_for_order_index(2), 1);
        assert_eq!(history.record_index_for_order_index(3), 1);
        assert_eq!(history.record_index_for_order_index(4), 2);
        assert_eq!(history.record_index_for_order_index(5), 3);

        let records = history.block_records_from(3);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].block.raw_text().as_deref(), Some("after"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("tail"));

        history.rewrite(
            draft,
            Block::Text {
                content: "finished tool".into(),
            },
        );
        assert_eq!(history.record_index_for_order_index(5), 4);
        history.remove_block(draft);
        assert_eq!(history.record_index_for_order_index(4), 3);
        history.truncate(3);
        assert_eq!(history.record_index_for_order_index(3), 2);
    }

    #[test]
    fn block_dirty_range_is_owned_by_block_history_mutations() {
        let mut history = BlockHistory::new();
        assert_eq!(history.record_dirty_from(), None);

        let first = history.push(Block::Text {
            content: "first".into(),
        });
        history.push(Block::Text {
            content: "second".into(),
        });
        assert_eq!(history.record_dirty_from(), Some(0));
        history.clear_record_dirty();

        history.rewrite(
            first,
            Block::Text {
                content: "changed".into(),
            },
        );
        assert_eq!(history.record_dirty_from(), Some(0));
        history.clear_record_dirty();

        history.truncate(1);
        assert_eq!(history.record_dirty_from(), Some(1));

        let restored = BlockHistory::from_block_records(history.block_records());
        assert_eq!(restored.record_dirty_from(), None);
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
            command: false,
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
            preview_output: None,
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
    fn tool_display_hash_includes_preview_output() {
        let a = pending_state();
        let mut b = pending_state();
        b.preview_output = Some(Box::new(ToolOutput {
            content: String::new(),
            is_error: false,
            metadata: Some(serde_json::json!({ "old_content": "before", "new_content": "after" })),
        }));

        assert_ne!(a.display_hash(), b.display_hash());
    }

    #[test]
    fn tool_display_hash_includes_display_metadata() {
        let mut a = pending_state();
        a.status = ToolStatus::Ok;
        a.output = Some(Box::new(ToolOutput {
            content: "visible output".into(),
            is_error: false,
            metadata: Some(serde_json::json!({ "old_content": "a".repeat(32 * 1024) })),
        }));
        let mut b = a.clone();
        b.output.as_mut().unwrap().metadata = Some(serde_json::json!({
            "old_content": "b".repeat(32 * 1024),
            "new_content": "c".repeat(32 * 1024),
        }));

        assert_ne!(a.display_hash(), b.display_hash());

        b.output.as_mut().unwrap().metadata = a.output.as_ref().unwrap().metadata.clone();
        assert_eq!(a.display_hash(), b.display_hash());

        b.output.as_mut().unwrap().content.push_str(" changed");
        assert_ne!(a.display_hash(), b.display_hash());
    }

    #[test]
    fn raw_text_for_thinking_returns_content() {
        let block = Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "ponder".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("ponder"));
    }

    #[test]
    fn reasoning_summary_raw_text_includes_title_history_and_real_body() {
        let block = Block::Thinking {
            title: Some("Latest title".into()),
            summary_titles: vec!["First title".into(), "Latest title".into()],
            content: "real body".into(),
            kind: protocol::ReasoningKind::Summary,
        };

        assert_eq!(
            block.raw_text().as_deref(),
            Some("**First title**\n**Latest title**\nreal body")
        );
    }

    #[test]
    fn block_history_normalizes_thinking_title_spacing() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "**Plan**\n\nbody".into(),
        });

        assert_eq!(
            history.block(id),
            Some(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nbody".into(),
            })
        );
    }

    #[test]
    fn hydrated_block_history_normalizes_thinking_title_spacing() {
        let mut history = BlockHistory::new();
        let id = history.push_hydrated_block_with_origin(
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\n\nbody".into(),
            },
            BlockOrigin::History(0),
        );

        assert_eq!(
            history.block(id),
            Some(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nbody".into(),
            })
        );
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
        assert_eq!(history.status(BlockId(9999)), None);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_stored_blocks() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "persisted".into(),
            },
            content_hash: 0,
            origin: None,
            tool_state: None,
        };
        let mut history = BlockHistory::from_block_records(vec![record]);
        let id = history.order[0];
        assert_eq!(history.status(id), Some(Status::Done));
        history.set_status(id, Status::Streaming);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_hydrated_blocks() {
        let mut history = BlockHistory::new();
        let id = history.push_hydrated_block_with_origin(
            Block::Text {
                content: "hydrated".into(),
            },
            BlockOrigin::History(0),
        );
        assert_eq!(history.status(id), Some(Status::Done));
        history.set_status(id, Status::Streaming);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_missing_blocks() {
        let mut history = BlockHistory::new();
        history.set_status(BlockId(9999), Status::Streaming);
    }

    #[test]
    fn set_status_streaming_then_done_pushes_finished_block() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        history.set_status(id, Status::Streaming);
        assert_eq!(history.status(id), Some(Status::Streaming));
        history.set_status(id, Status::Done);
        assert_eq!(history.status(id), Some(Status::Done));
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
    fn metadata_updates_preserve_other_block_metadata() {
        let mut history = BlockHistory::new();
        let id = history.push_with_origin(
            Block::Text {
                content: "before".into(),
            },
            BlockOrigin::History(7),
        );
        let original_hash = history.content_hash(id);

        history.set_status(id, Status::Streaming);

        assert_eq!(history.content_hash(id), original_hash);
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(7)));

        history.rewrite(
            id,
            Block::Text {
                content: "after".into(),
            },
        );

        assert_ne!(history.content_hash(id), original_hash);
        assert_eq!(history.status(id), Some(Status::Streaming));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(7)));
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
        assert_eq!(history.block_metadata_retained_bytes(), 0);
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
    fn truncate_user_tail_invalidates_navigation() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "assistant".into(),
        });
        history.push(Block::User {
            text: "user".into(),
            image_labels: Vec::new(),
            command: false,
        });
        let navigation_generation = history.navigation_generation();

        history.truncate(1);

        assert_ne!(history.navigation_generation(), navigation_generation);
    }

    #[test]
    fn non_user_navigation_changes_invalidate_snapshots() {
        let mut history = BlockHistory::new();
        let first = history.push(Block::Text {
            content: "first assistant".into(),
        });
        let navigation_generation = history.navigation_generation();

        history.push(Block::Text {
            content: "second assistant".into(),
        });
        assert_ne!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.rewrite(
            first,
            Block::Text {
                content: "rewritten assistant".into(),
            },
        );
        assert_ne!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.rewrite(
            first,
            Block::Text {
                content: "rewritten assistant\ncontinued output".into(),
            },
        );
        assert_eq!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.truncate(1);
        assert_ne!(history.navigation_generation(), navigation_generation);
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

        assert!(matches!(
            history.remove_unoriginated_at(0),
            Some(Block::ToolCall { call_id, .. }) if call_id == "tc1"
        ));
        assert!(history.is_empty());
        assert!(!history.tool_states.contains_key("tc1"));
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
            command: false,
        });
        // Text -> User: 1
        assert_eq!(history.block_gap(1), 1);
    }

    #[test]
    fn block_gap_uses_blocks_without_materializing_blocks() {
        let blocks = vec![
            Block::Text {
                content: "# heading".into(),
            },
            Block::CodeLine {
                content: "let x = 1;".into(),
                lang: "rust".into(),
            },
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "plain thought".into(),
            },
            Block::Thinking {
                title: Some("New section".into()),
                summary_titles: vec!["New section".into()],
                kind: protocol::ReasoningKind::Summary,
                content: "body".into(),
            },
        ];
        let history = BlockHistory::from_block_records(
            blocks
                .into_iter()
                .enumerate()
                .map(|(index, block)| TranscriptBlockRecord {
                    block,
                    content_hash: 0,
                    origin: Some(BlockOrigin::History(index)),
                    tool_state: None,
                })
                .collect(),
        );

        assert_eq!(history.block_gap(1), 0);
        assert_eq!(history.block_gap(3), 1);
        assert_eq!(history.hydrated_block_count(), 0);
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
            command: false,
        }
    }
    fn thinking(s: &str) -> Block {
        Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            content: s.into(),
            kind: protocol::ReasoningKind::Raw,
        }
    }
    fn reasoning_summary(title: &str, content: &str) -> Block {
        Block::Thinking {
            title: Some(title.into()),
            summary_titles: vec![title.into()],
            content: content.into(),
            kind: protocol::ReasoningKind::Summary,
        }
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
    fn gap_between_thinking_blocks_only_separates_new_titles() {
        assert_eq!(gap_between(&thinking("a"), &thinking("b")), 0);
        assert_eq!(
            gap_between(
                &thinking("a"),
                &reasoning_summary("Assessing directory exclusions", "body")
            ),
            1
        );
        assert_eq!(
            gap_between(&reasoning_summary("First title", "body"), &thinking("body")),
            0
        );
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
}
