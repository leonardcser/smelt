//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::TuiApp;
use crate::content::selection::wrap_with_offsets;
use crate::smelt_term::{BufCreateOpts, Buffer};

use crate::content::transcript_parsers as blocks;
use smelt_core::content::block_layout::{BlockLayout, HboxItem, RenderedLayout};
use smelt_core::transcript_model::{
    Block, BlockId, ToolOutput, ToolOutputRef, ToolState, ToolStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct TranscriptData {
    pub(crate) clamped_scroll: u16,
}

impl TuiApp {
    pub(crate) fn begin_turn(&mut self) {
        self.parser.begin_turn();
    }

    pub(crate) fn push_tool_call(&mut self, block: Block, state: ToolState) {
        self.transcript.push_tool_call(block, state);
    }

    pub(crate) fn push_block(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: &str) {
        self.parser
            .append_streaming_thinking(&mut self.transcript.history, delta);
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.parser
            .flush_streaming_thinking(&mut self.transcript.history);
    }

    pub(crate) fn append_streaming_text(&mut self, delta: &str) {
        self.parser
            .append_streaming_text(&mut self.transcript.history, delta);
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.parser
            .flush_streaming_text(&mut self.transcript.history);
    }

    pub(crate) fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: ::protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
    ) {
        let now = self.core.clock.instant_now();
        self.parser.start_tool(
            &mut self.transcript.history,
            call_id,
            name,
            summary,
            args,
            now,
        );
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.parser
            .start_exec(&mut self.transcript.history, command);
    }

    pub(crate) fn append_exec_output(&mut self, chunk: &str) {
        self.parser
            .append_exec_output(&mut self.transcript.history, chunk);
    }

    pub(crate) fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.parser.finish_exec(exit_code);
    }

    pub(crate) fn finalize_exec(&mut self) {
        self.parser.finalize_exec(&mut self.transcript.history);
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub(crate) fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.parser
            .append_active_output(&mut self.transcript.history, call_id, chunk);
    }

    pub(crate) fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        let now = self.core.clock.instant_now();
        self.parser
            .set_active_status(&mut self.transcript.history, call_id, status, now);
    }

    pub(crate) fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.parser
            .set_active_user_message(&mut self.transcript.history, call_id, msg);
    }

    pub(crate) fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        self.parser.finish_tool(
            &mut self.transcript.history,
            call_id,
            status,
            output,
            engine_elapsed,
        );
    }

    pub(crate) fn has_transcript_content(&mut self, _show_thinking: bool) -> bool {
        !self.transcript.history.is_empty()
    }

    /// Full transcript as one string per display row. Result is cached as an
    /// `Arc<Vec<String>>` until the generation, width, or `show_thinking` changes.
    pub(crate) fn full_transcript_display_text(&mut self, show_thinking: bool) -> Arc<Vec<String>> {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript_projection.build_rows(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        )
    }

    /// `\n` byte positions in `full_transcript_display_text(..).join("\n")`,
    /// partitioned into soft-wrap and hard-break sets. Soft positions are
    /// transparent to word-select; hard positions bound line-select.
    pub(crate) fn transcript_line_breaks(
        &mut self,
        show_thinking: bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let (mut soft, mut hard) = self.transcript_projection.line_breaks(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        soft.sort_unstable();
        hard.sort_unstable();
        (soft, hard)
    }

    /// Snap a clicked cell column to the nearest selectable cell on `abs_row`.
    pub(crate) fn snap_col_to_selectable(
        &mut self,
        abs_row: usize,
        col: usize,
        _show_thinking: bool,
    ) -> usize {
        let buf_id = self.transcript_win().buf;
        let Some(buf) = self.ui.buf(buf_id) else {
            return col;
        };
        crate::content::transcript_buf::snap_col_to_selectable(buf, abs_row, col)
    }

    pub(crate) fn snap_cpos_to_selectable(
        &mut self,
        rows: &[String],
        cpos: usize,
        _show_thinking: bool,
    ) -> usize {
        let buf_id = self.transcript_win().buf;
        let Some(buf) = self.ui.buf(buf_id) else {
            return cpos;
        };
        let mut acc = 0usize;
        for (r, row) in rows.iter().enumerate() {
            let row_end = acc + row.len();
            if cpos <= row_end {
                let col_byte = cpos.saturating_sub(acc).min(row.len());
                let col = row[..col_byte].chars().count();
                let snapped = crate::content::transcript_buf::snap_col_to_selectable(buf, r, col);
                if snapped == col {
                    return cpos;
                }
                let byte_col: usize = row.chars().take(snapped).map(|c| c.len_utf8()).sum();
                return acc + byte_col;
            }
            acc = row_end + 1;
        }
        cpos
    }

    /// Snapshot of the laid-out transcript blocks as `(idx, role, first_row,
    /// rows, first_line)`. `idx` is 0-based into `transcript.history.order` to
    /// match `session.rewind_to(block_idx)`. `first_line` is the first
    /// non-empty line of the block's raw source text (truncated upstream by
    /// the caller as needed). Returns empty when no projection has run yet
    /// (i.e. before the first frame).
    pub(crate) fn transcript_block_snapshots(
        &self,
    ) -> Vec<(usize, &'static str, u16, u16, String)> {
        use smelt_core::transcript_model::Block;
        let mut out = Vec::new();
        let history = &self.transcript.history;
        for (block_id, first_row, rows) in self.transcript_projection.block_layout() {
            let Some(idx) = history.order.iter().position(|id| *id == block_id) else {
                continue;
            };
            let Some(block) = history.blocks.get(&block_id) else {
                continue;
            };
            let role = match block {
                Block::User { .. } => "user",
                Block::Text { .. } => "assistant",
                Block::Thinking { .. } => "thinking",
                Block::ToolCall { .. } => "tool",
                Block::CodeLine { .. } => "code",
                Block::Exec { .. } => "exec",
                Block::Compacted { .. } => "compacted",
            };
            let first_line = block
                .raw_text()
                .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
                .unwrap_or_default();
            out.push((idx, role, first_row, rows, first_line));
        }
        out
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    /// No-op: width changes invalidate the cache implicitly on next paint.
    pub(crate) fn invalidate_for_width(&mut self, _width: u16) {}

    pub(crate) fn clear_transcript(&mut self) {
        self.transcript.history.clear();
        self.parser.clear();
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
        self.parser.clear_tools();
    }

    /// Advance spinner animation. Returns `true` if the frame changed.
    pub(crate) fn update_spinner(&mut self) -> bool {
        let mut changed = false;
        if let (Some(elapsed), Some(prev_frame)) =
            (self.working.elapsed(), self.working.last_spinner_frame())
        {
            let frame = smelt_core::content::spinner_frame_index(elapsed);
            if frame != prev_frame {
                self.working.set_last_spinner_frame(frame);
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn project_transcript_buffer(
        &mut self,
        width: usize,
        viewport_rows: u16,
        scroll_top: u16,
        show_thinking: bool,
    ) -> TranscriptData {
        let gutters = self.transcript_gutters();
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        // Run plugin `render` hooks on the main thread (Lua is single-threaded) and stash
        // the resulting owned buffers on `ToolState.render_cache`. Worker layout below
        // reads those buffers without touching `app.ui` or the Lua VM.
        self.prerender_tool_blocks(tw as u16);
        let theme = self.ui.theme().clone();

        let buf = self
            .ui
            .win_buf_mut(self.well_known.transcript)
            .expect("transcript window must be registered at startup");
        let out = self.transcript_projection.project(
            buf,
            &mut self.transcript.history,
            tw as u16,
            show_thinking,
            &theme,
            scroll_top,
            viewport_rows,
        );

        TranscriptData {
            clamped_scroll: out.clamped_scroll,
        }
    }

    /// Main-thread pre-pass: walk every `Block::ToolCall` whose `ToolState.render_cache`
    /// is missing or width-stale, call the plugin's `render` hook (Lua VM is single-
    /// threaded), and stash the resulting owned-buffer tree on the state. Parallel layout
    /// workers downstream just read those buffers — they never touch `app.ui` or Lua.
    fn prerender_tool_blocks(&mut self, width: u16) {
        struct Job {
            call_id: String,
            name: String,
            args: HashMap<String, serde_json::Value>,
            output: Option<ToolOutput>,
            status: ToolStatus,
            elapsed_secs: Option<u64>,
        }

        let jobs: Vec<Job> = {
            let history = &self.transcript.history;
            let mut jobs = Vec::new();
            for id in &history.order {
                let block = &history.blocks[id];
                let Block::ToolCall {
                    call_id,
                    name,
                    args,
                    ..
                } = block
                else {
                    continue;
                };
                let Some(state) = history.tool_states.get(call_id) else {
                    continue;
                };
                if matches!(state.status, ToolStatus::Denied) {
                    continue;
                }
                if matches!(&state.render_cache, Some((w, _)) if *w == width) {
                    continue;
                }
                jobs.push(Job {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    output: state.output.as_deref().cloned(),
                    status: state.status,
                    elapsed_secs: state.elapsed.map(|d| d.as_secs()),
                });
            }
            jobs
        };
        if jobs.is_empty() {
            return;
        }

        for job in jobs {
            let status_label = match job.status {
                ToolStatus::Pending => "pending",
                ToolStatus::Ok => "ok",
                ToolStatus::Err => "err",
                ToolStatus::Denied => "denied",
                ToolStatus::Confirm => "confirm",
            };
            let ctx = smelt_core::lua::runtime::ToolRenderCtx {
                width: width as usize,
                summary: "",
                status: status_label,
                elapsed_secs: job.elapsed_secs,
                call_id: Some(&job.call_id),
            };
            let Some(layout) =
                self.lua
                    .render_tool_layout(&job.name, &job.args, job.output.as_ref(), ctx)
            else {
                continue;
            };
            let rendered = extract_rendered_layout(&layout, &mut self.ui);
            if let Some(state) = self.transcript.history.tool_states.get_mut(&job.call_id) {
                state.render_cache = Some((width, rendered));
            }
        }
    }

    /// Per-line selection ranges (line, col_start, col_end) in display-cell units.
    /// No-op when no vim visual, selection anchor, or yank-flash is active.
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: u16,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let win = self.transcript_win();
        let vim_visual = win.vim_enabled
            && matches!(
                win.vim_mode,
                crate::smelt_term::VimMode::Visual | crate::smelt_term::VimMode::VisualLine
            );
        let anchor_set = win.selection_anchor.is_some();
        let yank_flash = self
            .core
            .clipboard
            .kill_ring
            .yank_flash_range(self.core.clock.instant_now())
            .is_some();
        if !vim_visual && !anchor_set && !yank_flash {
            return Vec::new();
        }

        let buf_id = self.transcript_win().buf;
        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Vec::new(),
        };
        let rows = buf.lines();
        if rows.is_empty() {
            return Vec::new();
        }
        let text = buf.text();
        let win = self.transcript_win();
        let endpoint = win.effective_endpoint();
        let active_selection = if win.vim_enabled {
            match win.vim_mode {
                crate::smelt_term::VimMode::Visual | crate::smelt_term::VimMode::VisualLine => {
                    crate::smelt_term::vim::visual_range(
                        &win.vim_state,
                        &text,
                        endpoint,
                        win.vim_mode,
                    )
                }
                _ => win.selection_range_at(endpoint, &text),
            }
        } else {
            win.selection_range_at(endpoint, &text)
        };
        // Fall back to yank-flash range (mirrors nvim's `vim.highlight.on_yank`).
        let (s, e) = match active_selection.or_else(|| {
            self.core
                .clipboard
                .kill_ring
                .yank_flash_range(self.core.clock.instant_now())
        }) {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        // Route through the shared coord helper so the prompt's per-row
        // selection painting and the transcript's stay one implementation —
        // including the "1-cell virtual span on empty middle rows" rule.
        let first = scroll_top as usize;
        let last = first + viewport_rows as usize;
        smelt_buffer::coords::selection_to_row_ranges(buf, s, e)
            .into_iter()
            .filter(|r| r.line >= first && r.line < last)
            .map(|r| (r.line, r.col_start, r.col_end))
            .collect()
    }

    /// Row counts `(above, input)` for the prompt block. `above` = queued + stash + top bar.
    pub(crate) fn measure_prompt_rows(
        &self,
        state: &crate::input::PromptState,
        edit_buf: &crate::smelt_term::Buffer,
        width: usize,
        queued: &[String],
    ) -> (u16, u16) {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            let geom = blocks::UserBlockGeometry::new(msg, text_w);
            for line in &geom.lines {
                let w = crate::content::builder::display_width(line);
                queued_rows += if w == 0 { 1 } else { w.div_ceil(text_w) as u16 };
            }
        }

        let wrap = wrap_with_offsets(edit_buf.source(), &[], usable);
        let input_rows = wrap.visual_lines.len().max(1) as u16;

        let above = queued_rows + stash + 1; // +1 = top bar
        (above, input_rows)
    }
}

/// Move every leaf buffer out of `ui` into a `RenderedLayout`. Missing buf ids fall back
/// to an empty placeholder so a registration race doesn't take down the frame.
pub(crate) fn extract_rendered_layout(
    layout: &BlockLayout,
    ui: &mut crate::smelt_term::Ui,
) -> RenderedLayout {
    use smelt_core::content::block_layout::{LuaLeaf, RenderedLeaf};
    match layout {
        BlockLayout::Leaf(LuaLeaf::Buf(id)) => {
            let buf = ui
                .buf_destroy(*id)
                .unwrap_or_else(|| Buffer::new(*id, BufCreateOpts::default()));
            BlockLayout::Leaf(RenderedLeaf::Buf(Box::new(buf)))
        }
        BlockLayout::Leaf(LuaLeaf::Diff(spec)) => {
            BlockLayout::Leaf(RenderedLeaf::Diff(spec.clone()))
        }
        BlockLayout::Leaf(LuaLeaf::FileView(spec)) => {
            BlockLayout::Leaf(RenderedLeaf::FileView(spec.clone()))
        }
        BlockLayout::Vbox(items) => BlockLayout::Vbox(
            items
                .iter()
                .map(|c| extract_rendered_layout(c, ui))
                .collect(),
        ),
        BlockLayout::Hbox(items) => BlockLayout::Hbox(
            items
                .iter()
                .map(|item| HboxItem {
                    constraint: item.constraint,
                    layout: extract_rendered_layout(&item.layout, ui),
                })
                .collect(),
        ),
    }
}
