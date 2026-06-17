//! Streaming input adapter: accumulates character deltas, detects structural boundaries
//! (paragraphs, code blocks, tables), and writes finished blocks into `BlockHistory`.

use super::markdown_stream::MarkdownStream;
use crate::transcript_model::{
    ActiveTool, Block, BlockHistory, BlockId, Status, ToolOutput, ToolOutputRef, ToolState,
    ToolStatus,
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

pub struct ToolDraftUpdate {
    pub stream_id: String,
    pub call_id: Option<String>,
    pub name: String,
    pub summary: protocol::StyledLines,
    pub args: HashMap<String, serde_json::Value>,
    pub raw_arguments: String,
    pub finished: bool,
}

pub struct ToolStart {
    pub call_id: String,
    pub name: String,
    pub summary: protocol::StyledLines,
    pub args: HashMap<String, serde_json::Value>,
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
            let Some(state) = history.tool_state(&tool.call_id) else {
                continue;
            };
            if state.status != ToolStatus::Pending {
                continue;
            }
            let elapsed = tool.elapsed_at(now);
            if elapsed_bucket(state.elapsed) == elapsed_bucket(Some(elapsed)) {
                continue;
            }
            let call_id = tool.call_id.clone();
            history.update_tool_state(&call_id, |state| {
                state.elapsed = Some(elapsed);
            });
        }
    }

    pub fn set_active_tools_paused(&mut self, history: &BlockHistory, paused: bool, now: Instant) {
        for tool in &mut self.active_tools {
            if !matches!(
                history.tool_state(&tool.call_id).map(|s| s.status),
                Some(ToolStatus::Pending)
            ) {
                continue;
            }
            if paused {
                tool.pause(now);
            } else {
                tool.resume(now);
            }
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
        if self.active_thinking.is_active() {
            self.flush_streaming_thinking(history);
        }
        self.active_text.append(history, delta);
    }

    pub fn flush_streaming_text(&mut self, history: &mut BlockHistory) {
        self.flush_streaming_thinking(history);
        self.active_text.flush(history);
    }

    // ── Tool lifecycle ──────────────────────────────────────────────

    pub fn upsert_tool_draft(&mut self, history: &mut BlockHistory, update: ToolDraftUpdate) {
        self.flush_streaming_thinking(history);
        self.flush_streaming_text(history);
        let ToolDraftUpdate {
            stream_id,
            call_id,
            name,
            summary,
            args,
            raw_arguments,
            finished,
        } = update;
        let block = Block::ToolDraft {
            stream_id: stream_id.clone(),
            call_id,
            name,
            summary,
            args,
            raw_arguments,
            finished,
        };
        if let Some(id) = self.tool_drafts.get(&stream_id).copied() {
            history.rewrite(id, block);
            history.set_status(id, Status::Streaming);
        } else {
            let id = history.push(block);
            history.set_status(id, Status::Streaming);
            self.tool_drafts.insert(stream_id, id);
        }
    }

    pub fn promote_tool_draft(
        &mut self,
        history: &mut BlockHistory,
        stream_id: Option<&str>,
        tool: ToolStart,
        now: Instant,
    ) -> bool {
        let ToolStart {
            call_id,
            name,
            summary,
            args,
        } = tool;
        let block_id = stream_id.and_then(|stream_id| self.tool_drafts.remove(stream_id));
        let Some(block_id) = block_id else {
            return false;
        };
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name,
            summary,
            args,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        };
        history.rewrite_with_tool_state(block_id, block, call_id.clone(), state);
        history.set_status(block_id, Status::Streaming);
        self.active_tools
            .push(ActiveTool::new(call_id, block_id, now));
        true
    }

    pub fn clear_tool_drafts(&mut self, history: &mut BlockHistory) {
        let drafts: Vec<BlockId> = self.tool_drafts.drain().map(|(_, id)| id).collect();
        for id in drafts {
            history.remove_block(id);
        }
    }

    pub fn start_tool(
        &mut self,
        history: &mut BlockHistory,
        call_id: String,
        name: String,
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        now: Instant,
    ) {
        let start_time = now;
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name,
            summary,
            args,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        };
        let block_id = history.push_with_state(block, call_id.clone(), state);
        history.set_status(block_id, Status::Streaming);
        self.active_tools
            .push(ActiveTool::new(call_id, block_id, start_time));
    }

    fn resolve_active_call_id(&self, history: &BlockHistory, call_id: &str) -> Option<String> {
        if !call_id.is_empty() {
            return Some(call_id.to_string());
        }
        self.active_tools
            .last()
            .map(|t| t.call_id.clone())
            .or_else(|| Self::last_tool_call_id(history))
    }

    fn last_tool_call_id(history: &BlockHistory) -> Option<String> {
        history
            .order
            .iter()
            .rev()
            .find_map(|id| match history.blocks.get(id) {
                Some(Block::ToolCall { call_id, .. }) => Some(call_id.clone()),
                _ => None,
            })
    }

    pub fn append_active_output(&mut self, history: &mut BlockHistory, call_id: &str, chunk: &str) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        let chunk = chunk.to_string();
        Self::update_tool_state(history, &cid, move |state| match state.output {
            Some(ref mut out) => {
                if !out.content.is_empty() {
                    out.content.push('\n');
                }
                out.content.push_str(&chunk);
            }
            None => {
                state.output = Some(Box::new(ToolOutput {
                    content: chunk,
                    is_error: false,
                    metadata: None,
                }));
            }
        });
    }

    pub fn set_active_status(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        status: ToolStatus,
        now: Instant,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        if let Some(active) = self.active_tools.iter_mut().find(|t| t.call_id == cid) {
            let previous = history.tool_state(&cid).map(|s| s.status);
            match (previous, status) {
                (_, ToolStatus::Confirm) => active.pause(now),
                (Some(ToolStatus::Confirm), ToolStatus::Pending) => active.resume(now),
                _ => {}
            }
        }
        Self::update_tool_state(history, &cid, |state| {
            state.status = status;
            if status == ToolStatus::Confirm {
                state.elapsed = self
                    .active_tools
                    .iter()
                    .find(|t| t.call_id == cid)
                    .map(|tool| tool.elapsed_at(now));
            }
        });
    }

    pub fn set_active_user_message(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        msg: String,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        Self::update_tool_state(history, &cid, |state| state.user_message = Some(msg));
    }

    pub fn finish_tool(
        &mut self,
        history: &mut BlockHistory,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
        now: Instant,
    ) {
        let Some(cid) = self.resolve_active_call_id(history, call_id) else {
            return;
        };
        let active_idx = self.active_tools.iter().position(|t| t.call_id == cid);
        // Active tools use the UI timer so elapsed excludes time spent in
        // blocking dialogs; replayed/completed tools keep engine-provided elapsed.
        let elapsed = if status == ToolStatus::Denied {
            None
        } else if let Some(idx) = active_idx {
            let tool = &self.active_tools[idx];
            Some(tool.elapsed_at(now))
        } else {
            engine_elapsed
        };
        Self::update_tool_state(history, &cid, |state| {
            state.status = status;
            if let Some(out) = output {
                state.output = Some(out);
            }
            state.elapsed = elapsed;
        });
        if let Some(idx) = active_idx {
            let block_id = self.active_tools[idx].block_id;
            self.active_tools.remove(idx);
            history.set_status(block_id, Status::Done);
        }
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
        let tools: Vec<ActiveTool> = self.active_tools.drain(..).collect();
        for tool in tools {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                Some(tool.elapsed_at(now))
            };
            history.set_status(tool.block_id, Status::Done);
            let cid = tool.call_id.clone();
            Self::update_tool_state(history, &cid, |state| {
                state.status = status;
                state.elapsed = elapsed;
            });
        }
    }

    fn update_tool_state(
        history: &mut BlockHistory,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) {
        history.update_tool_state(call_id, mutator);
    }

    // ── Exec lifecycle ──────────────────────────────────────────────

    pub fn start_exec(&mut self, history: &mut BlockHistory, command: String) {
        let id = history.push(Block::Exec {
            command,
            output: String::new(),
        });
        history.set_status(id, Status::Streaming);
        self.stream_exec_id = Some(id);
    }

    pub fn append_exec_output(&mut self, history: &mut BlockHistory, chunk: &str) {
        let Some(id) = self.stream_exec_id else {
            return;
        };
        let Some(Block::Exec { command, output }) = history.blocks.get(&id).cloned() else {
            return;
        };
        let mut new_output = output;
        if !new_output.is_empty() && !new_output.ends_with('\n') {
            new_output.push('\n');
        }
        new_output.push_str(chunk);
        history.rewrite(
            id,
            Block::Exec {
                command,
                output: new_output,
            },
        );
    }

    pub fn finish_exec(&mut self, _exit_code: Option<i32>) {}

    pub fn finalize_exec(&mut self, history: &mut BlockHistory) {
        let Some(id) = self.stream_exec_id.take() else {
            return;
        };
        if let Some(Block::Exec { command, output }) = history.blocks.get(&id).cloned() {
            let mut trimmed = output;
            trimmed.truncate(trimmed.trim_end().len());
            history.rewrite(
                id,
                Block::Exec {
                    command,
                    output: trimmed,
                },
            );
        }
        history.set_status(id, Status::Done);
    }
}

fn elapsed_bucket(elapsed: Option<Duration>) -> Option<u64> {
    let secs = elapsed?.as_secs();
    if secs < 1 {
        None
    } else if secs < 60 * 60 {
        Some(secs)
    } else {
        Some(60 * 60 + secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (StreamParser, BlockHistory) {
        (StreamParser::new(), BlockHistory::new())
    }

    // -- Text streaming -----------------------------------------------

    #[test]
    fn text_single_chunk() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "hello world");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history.block_at(0),
            &Block::Text {
                content: "hello world".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
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
            history.block_at(0),
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
            history.block_at(0),
            &Block::Thinking {
                content: "thinking...".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
    }

    #[test]
    fn thinking_auto_flushes_when_text_arrives() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_thinking(&mut history, "thinking");
        parser.append_streaming_text(&mut history, "text");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(
            history.block_at(0),
            &Block::Thinking {
                content: "thinking".into(),
            }
        );
        assert_eq!(
            history.block_at(1),
            &Block::Text {
                content: "text".into(),
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
            history.block_at(1),
            &Block::Thinking {
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
            history.block_at(0),
            &Block::Thinking {
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
            history.block_at(0),
            &Block::Thinking {
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
            history.block_at(0),
            &Block::Text {
                content: "```rust\nfn main() {}\n```".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
            &Block::Text {
                content: "```rust\nfn main() {}".into(),
            }
        );

        for chunk in ["`", "`", "`", "   "] {
            parser.append_streaming_text(&mut history, chunk);
            assert_eq!(
                history.block_at(0),
                &Block::Text {
                    content: "```rust\nfn main() {}".into(),
                }
            );
        }

        parser.append_streaming_text(&mut history, "\nafter");
        assert_eq!(
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
            &Block::Text {
                content: "| a | b |\n|---|---|\n| 1 | 2 |".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
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
            history.block_at(0),
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
            "c1".into(),
            "bash".into(),
            "bash".into(),
            HashMap::new(),
            Instant::now(),
        );
        assert_eq!(history.len(), 2);
        assert_eq!(
            history.block_at(0),
            &Block::Text {
                content: "before tool".into(),
            }
        );
        assert!(matches!(history.block_at(1), Block::ToolCall { .. }));
        assert_eq!(history.status(history.order[0]), Status::Streaming);
    }

    #[test]
    fn tool_finish_sets_done() {
        let (mut parser, mut history) = setup();
        parser.start_tool(
            &mut history,
            "c1".into(),
            "bash".into(),
            "bash".into(),
            HashMap::new(),
            Instant::now(),
        );
        let tool_block_id = history.order[0];
        assert_eq!(history.status(tool_block_id), Status::Streaming);
        parser.finish_tool(
            &mut history,
            "c1",
            ToolStatus::Ok,
            None,
            None,
            Instant::now(),
        );
        assert_eq!(history.status(tool_block_id), Status::Done);
    }

    #[test]
    fn tool_elapsed_pauses_while_waiting_for_confirm() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            "c1".into(),
            "bash".into(),
            "bash".into(),
            HashMap::new(),
            start,
        );

        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(2));
        assert_eq!(
            history.tool_state("c1").unwrap().elapsed,
            Some(Duration::from_secs(2))
        );

        parser.set_active_status(
            &mut history,
            "c1",
            ToolStatus::Confirm,
            start + Duration::from_secs(2),
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(12));
        assert_eq!(
            history.tool_state("c1").unwrap().elapsed,
            Some(Duration::from_secs(2))
        );

        parser.set_active_status(
            &mut history,
            "c1",
            ToolStatus::Pending,
            start + Duration::from_secs(12),
        );
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(15));
        assert_eq!(
            history.tool_state("c1").unwrap().elapsed,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn tool_elapsed_pauses_while_blocking_overlay_is_open() {
        let (mut parser, mut history) = setup();
        let start = Instant::now();
        parser.start_tool(
            &mut history,
            "c1".into(),
            "bash".into(),
            "bash".into(),
            HashMap::new(),
            start,
        );

        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(1));
        parser.set_active_tools_paused(&history, true, start + Duration::from_secs(1));
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(8));
        assert_eq!(
            history.tool_state("c1").unwrap().elapsed,
            Some(Duration::from_secs(1))
        );

        parser.set_active_tools_paused(&history, false, start + Duration::from_secs(8));
        parser.sync_active_tool_elapsed_at(&mut history, start + Duration::from_secs(10));
        assert_eq!(
            history.tool_state("c1").unwrap().elapsed,
            Some(Duration::from_secs(3))
        );
    }

    // -- Exec lifecycle -----------------------------------------------

    #[test]
    fn exec_lifecycle() {
        let (mut parser, mut history) = setup();
        parser.start_exec(&mut history, "ls".into());
        assert_eq!(history.len(), 1);
        let exec_id = history.order[0];
        assert_eq!(history.status(exec_id), Status::Streaming);
        parser.append_exec_output(&mut history, "file.txt");
        assert_eq!(
            history.block_at(0),
            &Block::Exec {
                command: "ls".into(),
                output: "file.txt".into(),
            }
        );
        parser.finalize_exec(&mut history);
        assert_eq!(history.status(exec_id), Status::Done);
    }

    // -- Turn flush ---------------------------------------------------

    #[test]
    fn turn_flush_captures_partial_text() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "partial");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history.block_at(0),
            &Block::Text {
                content: "partial".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
    }

    #[test]
    fn turn_flush_captures_partial_code_line() {
        let (mut parser, mut history) = setup();
        parser.append_streaming_text(&mut history, "```rust\npartial");
        parser.flush_streaming_text(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(
            history.block_at(0),
            &Block::Text {
                content: "```rust\npartial".into(),
            }
        );
        assert_eq!(history.status(history.order[0]), Status::Done);
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
            history.block_at(0),
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
            history.block_at(0),
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
            history.block_at(0),
            &Block::Text {
                content: "first paragraph".into(),
            }
        );
        assert_eq!(
            history.block_at(1),
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
            history.block_at(0),
            &Block::Text {
                content: full.into(),
            }
        );
    }

    // -- Tool drafts -----------------------------------------------------

    fn draft_update(
        stream_id: &str,
        call_id: Option<&str>,
        raw_arguments: &str,
    ) -> ToolDraftUpdate {
        ToolDraftUpdate {
            stream_id: stream_id.to_string(),
            call_id: call_id.map(str::to_string),
            name: "bash".to_string(),
            summary: protocol::StyledLines::from_plain("echo hi"),
            args: HashMap::new(),
            raw_arguments: raw_arguments.to_string(),
            finished: false,
        }
    }

    #[test]
    fn tool_draft_upserts_in_place() {
        let (mut parser, mut history) = setup();
        parser.upsert_tool_draft(&mut history, draft_update("s1", None, "{\"command\":\"ec"));
        let id = history.order[0];

        let mut update = draft_update("s1", Some("call-1"), "{\"command\":\"echo hi\"}");
        update.finished = true;
        parser.upsert_tool_draft(&mut history, update);

        assert_eq!(history.len(), 1);
        assert_eq!(history.order[0], id);
        match history.block_at(0) {
            Block::ToolDraft {
                stream_id,
                call_id,
                raw_arguments,
                finished,
                ..
            } => {
                assert_eq!(stream_id, "s1");
                assert_eq!(call_id.as_deref(), Some("call-1"));
                assert_eq!(raw_arguments, "{\"command\":\"echo hi\"}");
                assert!(*finished);
            }
            block => panic!("expected draft block, got {block:?}"),
        }
        assert_eq!(history.status(id), Status::Streaming);
    }

    #[test]
    fn tool_draft_promotes_in_place() {
        let (mut parser, mut history) = setup();
        parser.upsert_tool_draft(&mut history, draft_update("s1", Some("call-1"), "{}"));
        let id = history.order[0];

        let promoted = parser.promote_tool_draft(
            &mut history,
            Some("s1"),
            ToolStart {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::from_plain("echo hi"),
                args: HashMap::new(),
            },
            Instant::now(),
        );

        assert!(promoted);
        assert_eq!(history.len(), 1);
        assert_eq!(history.order[0], id);
        assert!(matches!(
            history.block_at(0),
            Block::ToolCall { call_id, name, .. } if call_id == "call-1" && name == "bash"
        ));
        assert_eq!(history.status(id), Status::Streaming);
        assert_eq!(
            history.tool_state("call-1").map(|state| state.status),
            Some(ToolStatus::Pending)
        );
    }

    #[test]
    fn clear_tool_drafts_removes_draft_blocks() {
        let (mut parser, mut history) = setup();
        parser.upsert_tool_draft(&mut history, draft_update("s1", None, "{}"));
        parser.upsert_tool_draft(&mut history, draft_update("s2", None, "{}"));

        parser.clear_tool_drafts(&mut history);

        assert_eq!(history.len(), 0);
        assert!(history.order.is_empty());
    }
}
