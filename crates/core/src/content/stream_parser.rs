//! Streaming input adapter: accumulates character deltas, detects structural boundaries
//! (paragraphs, code blocks, tables), and writes finished blocks into `BlockHistory`.

use super::markdown_stream::MarkdownStream;
use super::tool_draft::{ToolArguments, ToolDraft};
use crate::transcript_model::{
    ActiveTool, Block, BlockHistory, BlockId, ContentChannel, Status, ToolOutputRef, ToolState,
    ToolStateMutation, ToolStatus,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct StreamParser {
    active_thinking: MarkdownStream,
    active_text: MarkdownStream,
    stream_exec_id: Option<BlockId>,
    active_tools: Vec<ActiveTool>,
    tool_drafts: HashMap<String, BlockId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolDraftChange {
    pub block_id: BlockId,
    pub presentation_changed: bool,
}

pub struct ToolStart {
    pub invocation_id: protocol::InvocationId,
    pub call_id: String,
    pub name: String,
    pub summary: protocol::StyledLines,
    pub args: HashMap<String, serde_json::Value>,
    pub preview_output: Option<ToolOutputRef>,
    pub called_at_ms: u64,
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamParser {
    pub fn new() -> Self {
        Self {
            active_thinking: MarkdownStream::thinking(),
            active_text: MarkdownStream::new(),
            stream_exec_id: None,
            active_tools: Vec::new(),
            tool_drafts: HashMap::new(),
        }
    }

    pub fn sync_active_tool_elapsed(&self, history: &mut BlockHistory) {
        self.sync_active_tool_elapsed_at(history, Instant::now());
    }

    pub fn sync_active_tool_elapsed_at(&self, history: &mut BlockHistory, now: Instant) {
        for tool in &self.active_tools {
            let Some(state) = history.tool_state(tool.block_id) else {
                continue;
            };
            if state.status != ToolStatus::Pending {
                continue;
            }
            let elapsed = tool.elapsed_at(now);
            if elapsed_bucket(state.elapsed) == elapsed_bucket(Some(elapsed)) {
                continue;
            }
            history
                .apply_tool_state_mutation(tool.block_id, ToolStateMutation::SyncElapsed(elapsed));
        }
    }

    pub fn set_active_tools_paused(
        &mut self,
        history: &mut BlockHistory,
        paused: bool,
        now: Instant,
    ) {
        let mut updates = Vec::new();
        for tool in &mut self.active_tools {
            let Some(state) = history.tool_state(tool.block_id) else {
                continue;
            };
            if state.status != ToolStatus::Pending || state.elapsed_active != paused {
                continue;
            }
            if paused {
                tool.pause(now);
            } else {
                tool.resume(now);
            }
            updates.push((tool.block_id, tool.elapsed_at(now)));
        }
        for (block_id, elapsed) in updates {
            history.apply_tool_state_mutation(
                block_id,
                ToolStateMutation::SetElapsedActive {
                    elapsed,
                    active: !paused,
                },
            );
        }
    }

    pub fn clear(&mut self) {
        self.active_thinking.clear();
        self.active_text.clear();
        self.active_tools.clear();
        self.tool_drafts.clear();
        self.stream_exec_id = None;
    }

    pub fn begin_turn(&mut self) {
        self.active_tools.clear();
        self.tool_drafts.clear();
    }

    pub fn has_live_transcript_blocks(&self) -> bool {
        self.active_thinking.is_active()
            || self.active_text.is_active()
            || self.stream_exec_id.is_some()
            || !self.active_tools.is_empty()
            || !self.tool_drafts.is_empty()
    }

    pub fn has_active_exec(&self) -> bool {
        self.stream_exec_id.is_some()
    }

    pub fn has_active_thinking(&self) -> bool {
        self.active_thinking.is_active()
    }

    pub fn has_active_text(&self) -> bool {
        self.active_text.is_active()
    }

    pub fn clear_tools(&mut self) {
        self.active_tools.clear();
    }

    // ── Streaming thinking ──────────────────────────────────────────

    pub fn append_streaming_thinking(&mut self, history: &mut BlockHistory, delta: &str) {
        if self.active_text.is_active() {
            self.active_text.flush(history);
        }
        self.active_thinking.append(history, delta);
    }

    pub fn flush_streaming_thinking(&mut self, history: &mut BlockHistory) {
        self.active_thinking.flush(history);
    }

    // ── Streaming text ──────────────────────────────────────────────

    pub fn append_streaming_text(&mut self, history: &mut BlockHistory, delta: &str) {
        self.flush_streaming_thinking(history);
        self.active_text.append(history, delta);
    }

    pub fn flush_streaming_text(&mut self, history: &mut BlockHistory) {
        self.flush_streaming_thinking(history);
        self.active_text.flush(history);
    }

    // ── Tool lifecycle ──────────────────────────────────────────────

    pub fn start_tool_draft(
        &mut self,
        history: &mut BlockHistory,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
    ) -> ToolDraftChange {
        self.flush_streaming_thinking(history);
        self.flush_streaming_text(history);
        if let Some(block_id) = self.tool_drafts.get(&stream_id).copied() {
            return ToolDraftChange {
                block_id,
                presentation_changed: false,
            };
        }
        let name = name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "tool".into());
        let block_id = history.push(Block::ToolDraft(ToolDraft::new(
            stream_id.clone(),
            call_id,
            name,
        )));
        history.set_status(block_id, Status::Streaming);
        self.tool_drafts.insert(stream_id, block_id);
        ToolDraftChange {
            block_id,
            presentation_changed: true,
        }
    }

    pub fn append_tool_draft(
        &mut self,
        history: &mut BlockHistory,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
    ) -> ToolDraftChange {
        let change =
            self.start_tool_draft(history, stream_id.clone(), call_id.clone(), name.clone());
        let presentation_changed = history
            .append_tool_draft(change.block_id, call_id, name, delta)
            .is_some_and(|append| append.presentation_changed);
        ToolDraftChange {
            block_id: change.block_id,
            presentation_changed: change.presentation_changed || presentation_changed,
        }
    }

    pub fn finish_tool_draft(
        &mut self,
        history: &mut BlockHistory,
        stream_id: String,
        call_id: String,
        name: String,
        arguments: String,
    ) -> ToolDraftChange {
        let change = self.start_tool_draft(
            history,
            stream_id,
            Some(call_id.clone()),
            Some(name.clone()),
        );
        let presentation_changed = history
            .finish_tool_draft(change.block_id, call_id, name, arguments)
            .is_some_and(|append| append.presentation_changed);
        ToolDraftChange {
            block_id: change.block_id,
            presentation_changed: change.presentation_changed || presentation_changed,
        }
    }

    pub fn set_tool_draft_summary(
        &mut self,
        history: &mut BlockHistory,
        block_id: BlockId,
        summary: protocol::StyledLines,
    ) -> bool {
        history.set_tool_draft_summary(block_id, summary)
    }

    pub fn tool_draft_block_for_call(
        &self,
        history: &BlockHistory,
        call_id: &str,
    ) -> Option<BlockId> {
        self.tool_drafts.values().copied().find(|id| {
            history
                .block(*id)
                .is_some_and(|block| block.tool_call_id() == Some(call_id))
        })
    }

    pub fn promote_tool_draft(
        &mut self,
        history: &mut BlockHistory,
        stream_id: Option<&str>,
        tool: ToolStart,
        now: Instant,
    ) -> bool {
        let ToolStart {
            invocation_id,
            call_id,
            name,
            summary,
            args,
            preview_output,
            called_at_ms,
        } = tool;
        let block_id = stream_id.and_then(|stream_id| self.tool_drafts.remove(stream_id));
        let Some(block_id) = block_id else {
            return false;
        };
        let arguments = ToolArguments::from_values_reusing(
            args,
            history.block(block_id).and_then(|block| match block {
                Block::ToolDraft(draft) => Some(draft),
                _ => None,
            }),
        );
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name,
            summary,
            args: arguments,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            called_at_ms: Some(called_at_ms),
            elapsed_active: true,
            output: None,
            user_message: None,
            preview_output,
        };
        history.rewrite_with_tool_state(block_id, block, state);
        history.set_status(block_id, Status::Streaming);
        self.active_tools
            .push(ActiveTool::new(invocation_id, block_id, now));
        true
    }

    pub fn clear_tool_drafts(&mut self, history: &mut BlockHistory) {
        let drafts: Vec<BlockId> = self.tool_drafts.drain().map(|(_, id)| id).collect();
        for id in drafts {
            history.remove_block(id);
        }
    }

    pub fn start_tool(&mut self, history: &mut BlockHistory, tool: ToolStart, now: Instant) {
        let ToolStart {
            invocation_id,
            call_id,
            name,
            summary,
            args,
            preview_output,
            called_at_ms,
        } = tool;
        let start_time = now;
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name,
            summary,
            args: args.into(),
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            called_at_ms: Some(called_at_ms),
            elapsed_active: true,
            output: None,
            user_message: None,
            preview_output,
        };
        let block_id = history.push_with_state(block, state);
        history.set_status(block_id, Status::Streaming);
        self.active_tools
            .push(ActiveTool::new(invocation_id, block_id, start_time));
    }

    fn active_tool_index(&self, invocation_id: protocol::InvocationId) -> Option<usize> {
        self.active_tools
            .iter()
            .position(|tool| tool.invocation_id == invocation_id)
    }

    pub fn active_tool_block_id(&self, invocation_id: protocol::InvocationId) -> Option<BlockId> {
        self.active_tools
            .iter()
            .find(|tool| tool.invocation_id == invocation_id)
            .map(|tool| tool.block_id)
    }

    pub fn append_active_output_line(
        &mut self,
        history: &mut BlockHistory,
        invocation_id: protocol::InvocationId,
        line: String,
    ) {
        let Some(active_idx) = self.active_tool_index(invocation_id) else {
            return;
        };
        let block_id = self.active_tools[active_idx].block_id;
        self.append_tool_output_slice(
            history,
            block_id,
            crate::transcript_content::SharedContentSlice::from_owned(line),
            true,
        );
    }

    pub fn append_tool_output_slice(
        &mut self,
        history: &mut BlockHistory,
        block_id: BlockId,
        chunk: crate::transcript_content::SharedContentSlice,
        line_start: bool,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:stream:append_tool_output");
        history.append_live_output_slice(block_id, ContentChannel::ToolOutput, chunk, line_start);
        if smelt_perf::perf::enabled() {
            if let Some(output_bytes) = history
                .tool_state(block_id)
                .and_then(|state| state.output.as_ref())
                .map(|output| output.content.len())
            {
                smelt_perf::perf::record_value(
                    "transcript:stream:tool_output_accumulated_bytes",
                    output_bytes as u64,
                );
            }
        }
    }

    pub fn set_active_status(
        &mut self,
        history: &mut BlockHistory,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        now: Instant,
    ) {
        let Some(active_idx) = self.active_tool_index(invocation_id) else {
            return;
        };
        let block_id = self.active_tools[active_idx].block_id;
        let previous = history.tool_state(block_id).map(|state| state.status);
        let active = &mut self.active_tools[active_idx];
        match (previous, status) {
            (_, ToolStatus::Confirm) => active.pause(now),
            (Some(ToolStatus::Confirm), ToolStatus::Pending) => active.resume(now),
            _ => {}
        }
        let elapsed = (status == ToolStatus::Confirm).then(|| active.elapsed_at(now));
        history
            .apply_tool_state_mutation(block_id, ToolStateMutation::SetStatus { status, elapsed });
    }

    pub fn set_active_user_message(
        &mut self,
        history: &mut BlockHistory,
        invocation_id: protocol::InvocationId,
        msg: String,
    ) {
        let Some(active_idx) = self.active_tool_index(invocation_id) else {
            return;
        };
        let block_id = self.active_tools[active_idx].block_id;
        history.apply_tool_state_mutation(block_id, ToolStateMutation::SetUserMessage(msg));
    }

    pub fn finish_tool(
        &mut self,
        history: &mut BlockHistory,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        _engine_elapsed: Option<Duration>,
        now: Instant,
    ) {
        let Some(active_idx) = self.active_tool_index(invocation_id) else {
            return;
        };
        let active = &self.active_tools[active_idx];
        let block_id = active.block_id;
        // The UI timer excludes time spent in blocking dialogs.
        let elapsed = (status != ToolStatus::Denied).then(|| active.elapsed_at(now));
        history.apply_tool_state_mutation(
            block_id,
            ToolStateMutation::Finish {
                status,
                output,
                elapsed,
            },
        );
        self.active_tools.remove(active_idx);
        history.set_status(block_id, Status::Done);
    }

    pub fn finalize_active_tools(&mut self, history: &mut BlockHistory) {
        self.finalize_active_tools_at(history, ToolStatus::Err, Instant::now());
    }

    fn finalize_active_tools_at(
        &mut self,
        history: &mut BlockHistory,
        status: ToolStatus,
        now: Instant,
    ) {
        for tool in std::mem::take(&mut self.active_tools) {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                Some(tool.elapsed_at(now))
            };
            history.set_status(tool.block_id, Status::Done);
            history.apply_tool_state_mutation(
                tool.block_id,
                ToolStateMutation::Finish {
                    status,
                    output: None,
                    elapsed,
                },
            );
        }
    }

    // ── Exec lifecycle ──────────────────────────────────────────────

    pub fn start_exec(&mut self, history: &mut BlockHistory, command: String) {
        let id = history.push(Block::Exec {
            command,
            output: String::new().into(),
        });
        history.set_status(id, Status::Streaming);
        self.stream_exec_id = Some(id);
    }

    pub fn append_exec_output(&mut self, history: &mut BlockHistory, line: String) {
        let _perf = smelt_perf::perf::begin("transcript:stream:append_exec_output");
        let Some(id) = self.stream_exec_id else {
            return;
        };
        if history
            .append_live_output_line(id, ContentChannel::ExecOutput, line)
            .is_some()
        {
            let output_bytes = history
                .block(id)
                .and_then(|block| match block {
                    Block::Exec { output, .. } => Some(output.len()),
                    _ => None,
                })
                .unwrap_or_default();
            smelt_perf::perf::record_value(
                "transcript:stream:exec_output_accumulated_bytes",
                output_bytes as u64,
            );
        }
    }

    pub fn finish_exec(&mut self, _exit_code: Option<i32>) {}

    pub fn finalize_exec(&mut self, history: &mut BlockHistory) {
        let Some(id) = self.stream_exec_id.take() else {
            return;
        };
        history.trim_live_exec_output(id);
        history.set_status(id, Status::Done);
    }
}

fn elapsed_bucket(elapsed: Option<Duration>) -> Option<u64> {
    Some(elapsed?.as_millis().min(u128::from(u64::MAX)) as u64 / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVOCATION_ID: protocol::InvocationId = protocol::InvocationId::new(1);

    fn setup() -> (StreamParser, BlockHistory) {
        (StreamParser::new(), BlockHistory::new())
    }

    fn block_at(history: &BlockHistory, index: usize) -> &Block {
        history
            .materialized_block_at(index)
            .expect("materialized test block")
    }

    #[test]
    fn tool_output_line_chunks_preserve_graphemes_split_at_boundaries() {
        let (mut parser, mut history) = setup();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "unicode".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::empty(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            Instant::now(),
        );

        for chunk in [
            "besta",
            "\u{308}tigt 👩",
            "\u{200d}💻 9",
            "\u{fe0f} 🇨",
            "🇦",
            "next line",
        ] {
            parser.append_active_output_line(&mut history, INVOCATION_ID, chunk.to_string());
        }

        let state = history
            .tool_state(history.order[0])
            .expect("tool state should exist");
        assert_eq!(
            state
                .output
                .as_ref()
                .map(|output| output.content.snapshot()),
            Some("besta\u{308}tigt 👩\u{200d}💻 9\u{fe0f} 🇨🇦\nnext line".to_string())
        );
    }

    // -- Text streaming -----------------------------------------------

    #[test]
    fn text_single_chunk() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "hello world");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "hello world".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    #[test]
    fn text_multi_chunk_same_result() {
        let (mut parser, mut history) = setup();
        for chunk in ["hel", "lo wo", "rld"] {
            parser.append_streaming_text(&mut history, chunk);
        }
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "hello world".into(),
            }
        );
    }

    #[test]
    fn text_empty_deltas_no_blocks() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "");
        parser.append_streaming_text(&mut history, "");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 0);
    }

    // -- Thinking streaming -------------------------------------------

    #[test]
    fn thinking_then_flush() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "thinking...");
        parser.flush_streaming_thinking(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "thinking...".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    #[test]
    fn thinking_auto_flushes_when_text_arrives() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "thinking");
        parser.append_streaming_text(&mut history, "text");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(
            block_at(&history, 0),
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "thinking".into(),
            }
        );
        assert_eq!(
            block_at(&history, 1),
            &Block::Text {
                content: "text".into()
            }
        );
    }

    #[test]
    fn clear_preserves_thinking_stream_kind() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "discard me");
        parser.clear();
        parser.append_streaming_thinking(&mut history, "thinking after clear");
        parser.flush_streaming_thinking(&mut history);

        assert_eq!(history.len(), 2);
        assert_eq!(
            block_at(&history, 1),
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "thinking after clear".into(),
            }
        );
    }

    #[test]
    fn streaming_thinking_opening_fence_is_not_shown_as_text() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "``");
        assert_eq!(history.len(), 0);
        parser.append_streaming_thinking(&mut history, "`rust\nfn main() {}");
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "```rust\nfn main() {}".into(),
            }
        );
    }

    #[test]
    fn streaming_thinking_table_rows_are_not_shown_until_line_commit() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "| a | b |");
        assert_eq!(history.len(), 0);

        for chunk in ["\n|", "---", "|", "---", "|", "\n|"] {
            parser.append_streaming_thinking(&mut history, chunk);
            assert_eq!(history.len(), 0);
        }

        parser.append_streaming_thinking(&mut history, " 1 | 2 |");
        assert_eq!(history.len(), 0);
        parser.append_streaming_thinking(&mut history, "\n");
        assert_eq!(
            block_at(&history, 0),
            &Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "| a | b |\n|---|---|\n| 1 | 2 |".into(),
            }
        );
    }

    // -- Code blocks --------------------------------------------------

    #[test]
    fn code_block_detected() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "```rust\nfn main() {}\n```");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\nfn main() {}\n```".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    #[test]
    fn code_block_chunked_at_line_boundaries() {
        let (mut parser, mut history) = setup();
        for chunk in ["```rust\n", "fn main() {}\n", "```"] {
            parser.append_streaming_text(&mut history, chunk);
        }
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\nfn main() {}\n```".into(),
            }
        );
    }

    #[test]
    fn code_block_with_multiple_lines() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "```py\nprint(1)\nprint(2)\n```");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```py\nprint(1)\nprint(2)\n```".into(),
            }
        );
    }

    #[test]
    fn streaming_opening_fence_is_not_shown_as_text() {
        let (mut parser, mut history) = setup();
        for chunk in ["`", "`", "`", "rust"] {
            parser.append_streaming_text(&mut history, chunk);
            assert_eq!(history.len(), 0);
        }

        parser.append_streaming_text(&mut history, "\nfn main() {}");
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\nfn main() {}".into(),
            }
        );
    }

    #[test]
    fn streaming_closing_fence_is_not_shown_as_text() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "```rust\nfn main() {}\n");
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\nfn main() {}".into(),
            }
        );

        for chunk in ["`", "`", "`", "   "] {
            parser.append_streaming_text(&mut history, chunk);
            assert_eq!(
                block_at(&history, 0),
                &Block::Text {
                    content: "```rust\nfn main() {}".into(),
                }
            );
        }

        parser.append_streaming_text(&mut history, "\nafter");
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\nfn main() {}\n```   \nafter".into(),
            }
        );
    }

    #[test]
    fn code_block_can_contain_shorter_fenced_block() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(
            &mut history,
            "````markdown\n```rust\nfn main() {}\n```\n````",
        );
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "````markdown\n```rust\nfn main() {}\n```\n````".into(),
            }
        );
    }

    #[test]
    fn code_block_does_not_close_on_longer_opening_fence_line() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "````markdown\n`````text\ninside\n`````\n````");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "````markdown\n`````text\ninside\n`````\n````".into(),
            }
        );
    }

    #[test]
    fn code_block_keeps_fence_with_trailing_text_as_content() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "````\n```` text\ninside\n````");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "````\n```` text\ninside\n````".into(),
            }
        );
    }

    #[test]
    fn code_block_closes_on_longer_plain_fence() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "````\ninside\n`````\nafter");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "````\ninside\n`````\nafter".into(),
            }
        );
    }

    #[test]
    fn adjacent_nested_code_blocks_preserve_inner_fences() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(
            &mut history,
            "````\n```\n```\n````\n````\n```\nnested code block\n```\n````",
        );
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "````\n```\n```\n````\n````\n```\nnested code block\n```\n````".into(),
            }
        );
    }

    // -- Tables -------------------------------------------------------

    #[test]
    fn table_detected() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "| a | b |\n|---|---|\n| 1 | 2 |");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "| a | b |\n|---|---|\n| 1 | 2 |".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    #[test]
    fn streaming_table_rows_are_not_shown_until_line_commit() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "| a | b |");
        assert_eq!(history.len(), 0);

        for chunk in ["\n|", "---", "|", "---", "|", "\n|"] {
            parser.append_streaming_text(&mut history, chunk);
            assert_eq!(history.len(), 0);
        }

        parser.append_streaming_text(&mut history, " 1 | 2 |");
        assert_eq!(history.len(), 0);
        parser.append_streaming_text(&mut history, "\n");
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "| a | b |\n|---|---|\n| 1 | 2 |".into(),
            }
        );
    }

    #[test]
    fn tool_start_flushes_active_text() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "before tool");
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            Instant::now(),
        );
        assert_eq!(history.len(), 2);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "before tool".into(),
            }
        );
        assert!(matches!(block_at(&history, 1), Block::ToolCall { .. }));
        assert_eq!(history.status(history.order[0]), Some(Status::Streaming));
    }

    #[test]
    fn tool_finish_sets_done_and_preserves_start_timestamp() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 1_700_000_000_123,
            },
            start,
        );
        let tool_block_id = history.order[0];
        assert_eq!(history.status(tool_block_id), Some(Status::Streaming));
        let pending = history.tool_state(history.order[0]).unwrap();
        assert_eq!(pending.called_at_ms, Some(1_700_000_000_123));
        assert!(pending.elapsed_active);

        parser.finish_tool(
            &mut history,
            INVOCATION_ID,
            ToolStatus::Ok,
            None,
            None,
            start + Duration::from_secs(1),
        );
        assert_eq!(history.status(tool_block_id), Some(Status::Done));
        let finished = history.tool_state(history.order[0]).unwrap();
        assert_eq!(finished.called_at_ms, Some(1_700_000_000_123));
        assert_eq!(finished.elapsed, Some(Duration::from_secs(1)));
        assert!(!finished.elapsed_active);
    }

    #[test]
    fn tool_finish_retains_streamed_content_identity() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            start,
        );
        parser.append_active_output_line(&mut history, INVOCATION_ID, "first".into());
        parser.append_active_output_line(&mut history, INVOCATION_ID, "second".into());
        let block_id = history.order[0];
        let content_id = history
            .tool_state(block_id)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.id())
            .expect("streamed output");

        parser.finish_tool(
            &mut history,
            INVOCATION_ID,
            ToolStatus::Err,
            Some(Box::new(crate::transcript_model::ToolOutput {
                content: "first\nsecond".into(),
                is_error: true,
                metadata: Some(serde_json::json!({ "exit_code": 1 })),
                content_fields: Vec::new(),
            })),
            None,
            start + Duration::from_secs(1),
        );

        let finished = history.tool_state(block_id).expect("finished state");
        let output = finished.output.as_ref().expect("finished output");
        assert_eq!(output.content.id(), content_id);
        assert_eq!(output.content.snapshot(), "first\nsecond");
        assert!(output.is_error);
        assert_eq!(output.metadata, Some(serde_json::json!({ "exit_code": 1 })));
    }

    #[test]
    fn tool_elapsed_pauses_while_waiting_for_confirm() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            start,
        );

        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(2));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_secs(2))
        );
        assert!(history.tool_state(history.order[0]).unwrap().elapsed_active);

        parser.set_active_status(
            &mut history,
            INVOCATION_ID,
            ToolStatus::Confirm,
            start + Duration::from_secs(2),
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(12));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_secs(2))
        );
        assert!(!history.tool_state(history.order[0]).unwrap().elapsed_active);

        parser.set_active_status(
            &mut history,
            INVOCATION_ID,
            ToolStatus::Pending,
            start + Duration::from_secs(12),
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(15));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_secs(5))
        );
        assert!(history.tool_state(history.order[0]).unwrap().elapsed_active);
    }

    #[test]
    fn active_elapsed_syncs_on_hundred_millisecond_buckets() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            start,
        );

        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_millis(149));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_millis(149))
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_millis(199));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_millis(149)),
            "updates within one display bucket should not invalidate the node"
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_millis(200));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn tool_elapsed_pauses_while_blocking_overlay_is_open() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "c1".into(),
                name: "bash".into(),
                summary: "bash".into(),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            start,
        );

        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(1));
        parser.set_active_tools_paused(&mut history, true, start + Duration::from_secs(1));
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(8));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_secs(1))
        );
        assert!(!history.tool_state(history.order[0]).unwrap().elapsed_active);

        parser.set_active_tools_paused(&mut history, false, start + Duration::from_secs(8));
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(10));
        assert_eq!(
            history.tool_state(history.order[0]).unwrap().elapsed,
            Some(Duration::from_secs(3))
        );
        assert!(history.tool_state(history.order[0]).unwrap().elapsed_active);
    }

    // -- Exec lifecycle -----------------------------------------------

    #[test]
    fn exec_lifecycle() {
        let (mut parser, mut history) = setup();
        parser.start_exec(&mut history, "ls".into());
        assert_eq!(history.len(), 1);
        let exec_id = history.order[0];
        assert_eq!(history.status(exec_id), Some(Status::Streaming));
        parser.append_exec_output(&mut history, "file.txt".into());
        assert_eq!(
            block_at(&history, 0),
            &Block::Exec {
                command: "ls".into(),
                output: "file.txt".into(),
            }
        );
        parser.finalize_exec(&mut history);
        assert_eq!(history.status(exec_id), Some(Status::Done));
    }

    #[test]
    fn exec_finalization_never_splits_trailing_graphemes() {
        let (mut parser, mut history) = setup();
        parser.start_exec(&mut history, "printf".into());
        parser.append_exec_output(&mut history, "value\u{600} \n".to_string());
        parser.finalize_exec(&mut history);

        assert_eq!(
            block_at(&history, 0),
            &Block::Exec {
                command: "printf".into(),
                output: "value\u{600} ".into(),
            }
        );
    }

    // -- Turn flush ---------------------------------------------------

    #[test]
    fn turn_flush_captures_partial_text() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "partial");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "partial".into()
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    #[test]
    fn turn_flush_captures_partial_code_line() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "```rust\npartial");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "```rust\npartial".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Some(Status::Done));
    }

    // -- Chunk boundary stress ----------------------------------------

    #[test]
    fn newline_split_across_chunks() {
        let (mut parser, mut history) = setup();
        for chunk in ["line1\n", "line2"] {
            parser.append_streaming_text(&mut history, chunk);
        }
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "line1\nline2".into(),
            }
        );
    }

    #[test]
    fn table_row_split_across_chunks() {
        let (mut parser, mut history) = setup();
        for chunk in ["| a ", "| b |\n|---", "--|\n| 1 | 2 |"] {
            parser.append_streaming_text(&mut history, chunk);
        }
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "| a | b |\n|-----|\n| 1 | 2 |".into(),
            }
        );
    }

    // -- Paragraph break ----------------------------------------------

    #[test]
    fn text_paragraph_break() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "first paragraph\n\nsecond paragraph");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: "first paragraph".into(),
            }
        );
        assert_eq!(
            block_at(&history, 1),
            &Block::Text {
                content: "second paragraph".into(),
            }
        );
    }

    // -- No-drop invariant --------------------------------------------

    #[test]
    fn no_text_lost_across_many_small_chunks() {
        let (mut parser, mut history) = setup();
        let full = "The quick brown fox jumps over the lazy dog.";
        for ch in full.chars() {
            parser.append_streaming_text(&mut history, &ch.to_string());
        }
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            block_at(&history, 0),
            &Block::Text {
                content: full.into()
            }
        );
    }

    // -- Tool drafts -----------------------------------------------------

    #[test]
    fn tool_draft_appends_and_finishes_in_place_without_replay() {
        let (mut parser, mut history) = setup();
        let first = parser.append_tool_draft(
            &mut history,
            "s1".into(),
            None,
            Some("bash".into()),
            "{\"command\":\"ec".into(),
        );
        let id = history.order[0];
        assert_eq!(first.block_id, id);

        parser.append_tool_draft(
            &mut history,
            "s1".into(),
            Some("call-1".into()),
            Some("bash".into()),
            "ho hi\"}".into(),
        );
        let raw_revision = match block_at(&history, 0) {
            Block::ToolDraft(draft) => draft.raw_arguments.revision(),
            block => panic!("expected draft block, got {block:?}"),
        };
        let layout_key = history.resolve_key(
            id,
            crate::transcript_model::LayoutKey {
                width: 80,
                view_state: crate::transcript_model::ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        );
        parser.finish_tool_draft(
            &mut history,
            "s1".into(),
            "call-1".into(),
            "bash".into(),
            "{\"command\":\"echo hi\"}".into(),
        );

        assert_eq!(history.len(), 1);
        assert_eq!(history.order[0], id);
        match block_at(&history, 0) {
            Block::ToolDraft(draft) => {
                assert_eq!(draft.stream_id, "s1");
                assert_eq!(draft.call_id.as_deref(), Some("call-1"));
                assert_eq!(draft.raw_arguments.snapshot(), "{\"command\":\"echo hi\"}");
                assert_eq!(draft.raw_arguments.revision(), raw_revision);
                assert!(draft.finished);
            }
            block => panic!("expected draft block, got {block:?}"),
        }
        assert_eq!(history.status(id), Some(Status::Streaming));
        assert_ne!(
            history.resolve_key(
                id,
                crate::transcript_model::LayoutKey {
                    width: 80,
                    view_state: crate::transcript_model::ViewState::Expanded,
                    content_hash: 0,
                    sidecar_hash: 0,
                },
            ),
            layout_key,
            "draft completion changes structural renderer inputs even without new bytes"
        );
    }

    #[test]
    fn tool_draft_promotes_in_place_and_reuses_field_content() {
        let (mut parser, mut history) = setup();
        parser.append_tool_draft(
            &mut history,
            "s1".into(),
            Some("call-1".into()),
            Some("bash".into()),
            "{\"command\":\"echo hi\"}".into(),
        );
        let id = history.order[0];
        let field_id = match block_at(&history, 0) {
            Block::ToolDraft(draft) => draft.string_field("command").unwrap().content.id(),
            block => panic!("expected draft block, got {block:?}"),
        };

        let promoted = parser.promote_tool_draft(
            &mut history,
            Some("s1"),
            ToolStart {
                invocation_id: INVOCATION_ID,
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::from_plain("echo hi"),
                args: HashMap::from([("command".into(), serde_json::json!("echo hi"))]),
                preview_output: None,
                called_at_ms: 0,
            },
            Instant::now(),
        );

        assert!(promoted);
        assert_eq!(history.len(), 1);
        assert_eq!(history.order[0], id);
        match block_at(&history, 0) {
            Block::ToolCall {
                call_id,
                name,
                args,
                ..
            } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(name, "bash");
                assert_eq!(args.string_field("command").unwrap().content.id(), field_id);
            }
            block => panic!("expected tool call, got {block:?}"),
        }
        assert_eq!(history.status(id), Some(Status::Streaming));
        assert_eq!(
            history.tool_state(id).map(|state| state.status),
            Some(ToolStatus::Pending)
        );
    }

    #[test]
    fn clear_tool_drafts_removes_draft_blocks() {
        let (mut parser, mut history) = setup();
        parser.start_tool_draft(&mut history, "s1".into(), None, Some("bash".into()));
        parser.start_tool_draft(&mut history, "s2".into(), None, Some("bash".into()));

        parser.clear_tool_drafts(&mut history);

        assert_eq!(history.len(), 0);
        assert!(history.order.is_empty());
    }
}
