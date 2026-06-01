//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::TuiApp;
use crate::content::selection::wrap_with_offsets;
use crate::smelt_term::{BufCreateOpts, Buffer, Theme};

use smelt_core::content::block_layout::{BlockLayout, HboxItem, RenderedLayout};
use smelt_core::transcript_model::{
    Block, BlockId, ToolOutput, ToolOutputRef, ToolState, ToolStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct TranscriptData {
    pub(crate) clamped_scroll: crate::smelt_term::RowIndex,
    pub(crate) row_base: crate::smelt_term::RowIndex,
    pub(crate) total_rows: crate::smelt_term::RowIndex,
}

struct ToolRenderJob {
    call_id: String,
    name: String,
    args: HashMap<String, serde_json::Value>,
    output: Option<ToolOutput>,
    status: ToolStatus,
    elapsed_secs: Option<u64>,
}

type TranscriptBlockSnapshot = (
    usize,
    &'static str,
    crate::smelt_term::RowIndex,
    crate::smelt_term::RowIndex,
    String,
);

fn transcript_block_role(block: &Block) -> &'static str {
    match block {
        Block::User { .. } => "user",
        Block::Mode { .. } => "mode",
        Block::ProcessStatus { .. } => "process_status",
        Block::Text { .. } => "assistant",
        Block::Thinking { .. } => "thinking",
        Block::ToolCall { .. } => "tool",
        Block::CodeLine { .. } => "code",
        Block::Exec { .. } => "exec",
        Block::Compacted { .. } => "compacted",
    }
}

fn transcript_block_first_line(block: &Block) -> String {
    block
        .raw_text()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

impl TuiApp {
    pub(crate) fn begin_turn(&mut self) {
        self.context_tokens_updated_this_turn = false;
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
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_full");
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript_projection.build_rows(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        )
    }

    pub(crate) fn transcript_display_rows_range(
        &mut self,
        show_thinking: bool,
        start: crate::smelt_term::RowIndex,
        count: crate::smelt_term::RowIndex,
    ) -> Vec<String> {
        let rows = self.full_transcript_display_text(show_thinking);
        let start_idx = crate::smelt_term::document::row_to_usize(start).min(rows.len());
        let end =
            crate::smelt_term::document::row_to_usize(start.saturating_add(count)).min(rows.len());
        rows[start_idx..end].to_vec()
    }

    /// `\n` byte positions in `full_transcript_display_text(..).join("\n")`,
    /// partitioned into soft-wrap and hard-break sets. Soft positions are
    /// transparent to word-select; hard positions bound line-select.
    pub(crate) fn transcript_line_breaks(
        &mut self,
        show_thinking: bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let _perf = smelt_perf::perf::begin("transcript:materialize_breaks_full");
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

    pub(crate) fn transcript_line_breaks_range(
        &mut self,
        show_thinking: bool,
        start: crate::smelt_term::RowIndex,
        count: crate::smelt_term::RowIndex,
    ) -> (Vec<usize>, Vec<usize>) {
        let rows = self.transcript_display_rows_range(show_thinking, start, count);
        (Vec::new(), crate::smelt_term::hard_breaks_for_lines(&rows))
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
    pub(crate) fn visible_transcript_block_snapshots(&self) -> Vec<TranscriptBlockSnapshot> {
        self.transcript_block_snapshots_from_layout(
            self.transcript_projection.visible_block_layout(),
        )
    }

    pub(crate) fn transcript_block_snapshots(&mut self) -> Vec<TranscriptBlockSnapshot> {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let layout = self.transcript_projection.materialize_block_layout(
            &mut self.transcript.history,
            tw,
            self.core.config.settings.show_thinking,
            &theme,
        );
        self.transcript_block_snapshots_from_layout(layout.into_iter())
    }

    fn transcript_block_snapshots_from_layout(
        &self,
        layout: impl Iterator<
            Item = (
                BlockId,
                crate::smelt_term::RowIndex,
                crate::smelt_term::RowIndex,
            ),
        >,
    ) -> Vec<TranscriptBlockSnapshot> {
        let mut out = Vec::new();
        let history = &self.transcript.history;
        for (block_id, first_row, rows) in layout {
            let Some(idx) = history.order.iter().position(|id| *id == block_id) else {
                continue;
            };
            let Some(block) = history.blocks.get(&block_id) else {
                continue;
            };
            let role = transcript_block_role(block);
            let first_line = transcript_block_first_line(block);
            out.push((idx, role, first_row, rows, first_line));
        }
        out
    }

    pub(crate) fn transcript_block_at_row(
        &mut self,
        row: crate::smelt_term::RowIndex,
    ) -> Option<TranscriptBlockSnapshot> {
        self.transcript_block_snapshots()
            .into_iter()
            .find(|(_, _, first_row, rows, _)| {
                let end = first_row.saturating_add(*rows);
                row >= *first_row && row < end
            })
    }

    pub(crate) fn transcript_visible_rows(
        &mut self,
        start: crate::smelt_term::RowIndex,
        count: crate::smelt_term::RowIndex,
    ) -> Vec<String> {
        self.transcript_display_rows_range(self.core.config.settings.show_thinking, start, count)
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
    }

    pub(crate) fn apply_pending_history_appends_for_request(&mut self) {
        let appends = std::mem::take(&mut self.pending_history_appends);
        for append in appends {
            self.apply_history_append_to_history(
                append.history_note(),
                append.replacement_prefix(),
            );
            self.commit_history_append_block(
                append.transcript_block(),
                append.replacement_prefix(),
            );
        }
    }

    pub(crate) fn commit_pending_history_append(&mut self, note: &str) {
        let Some(idx) = self
            .pending_history_appends
            .iter()
            .position(|append| append.history_note() == note)
        else {
            return;
        };
        let append = self.pending_history_appends.remove(idx);
        self.commit_history_append_block(append.transcript_block(), append.replacement_prefix());
    }

    pub(crate) fn commit_history_append_block(
        &mut self,
        block: Block,
        replace_user_prefix: Option<&str>,
    ) {
        if let Some(prefix) = replace_user_prefix {
            if let Some(id) = self.transcript.history.order.last().copied() {
                let replaces_mode_block = prefix == protocol::MODE_NOTE_PREFIX
                    && matches!(
                        self.transcript.history.blocks.get(&id),
                        Some(Block::Mode { .. })
                    );
                let replaces_prefixed_text = matches!(
                    self.transcript.history.blocks.get(&id),
                    Some(Block::User { text, .. }) if text.starts_with(prefix)
                );
                if replaces_mode_block || replaces_prefixed_text {
                    self.transcript.history.rewrite(id, block);
                    return;
                }
            }
        }
        self.push_block(block);
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    /// No-op: width changes invalidate the cache implicitly on next paint.
    pub(crate) fn invalidate_for_width(&mut self, _width: u16) {}

    pub(crate) fn invalidate_for_theme(&mut self) {
        self.transcript_projection.invalidate_theme();
    }

    /// Install a complete theme and publish it to the process-wide active slot.
    pub(crate) fn install_theme(&mut self, theme: Theme) {
        *self.ui.theme_mut() = theme;
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.invalidate_for_theme();
    }

    /// Mutate the current theme and republish.
    pub(crate) fn mutate_theme(&mut self, f: impl FnOnce(&mut Theme)) {
        f(self.ui.theme_mut());
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.invalidate_for_theme();
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.pending_history_appends.clear();
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
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        show_thinking: bool,
    ) -> TranscriptData {
        let gutters = self.transcript_gutters();
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        self.parser
            .sync_active_tool_elapsed(&mut self.transcript.history);
        // Run plugin `render` hooks on the main thread (Lua is single-threaded) and stash
        // the resulting owned buffers on `ToolState.render_cache`. Worker layout below
        // reads those buffers without touching `app.ui` or the Lua VM. Tail-follow asks
        // the transcript height index for the bounded suffix; arbitrary scroll positions
        // still use the compatibility full pre-pass until random-access projection lands.
        if scroll_target == crate::content::transcript_buf::ScrollTarget::Tail {
            let ids = self.tail_prerender_block_ids(tw as u16, show_thinking, viewport_rows);
            self.prerender_tool_blocks_for_ids(tw as u16, &ids);
        } else {
            self.prerender_tool_blocks(tw as u16);
        }
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
            scroll_target,
            viewport_rows,
        );

        TranscriptData {
            clamped_scroll: out.clamped_scroll,
            row_base: out.row_base,
            total_rows: out.total_rows,
        }
    }

    fn tail_prerender_block_ids(
        &mut self,
        width: u16,
        show_thinking: bool,
        viewport_rows: u16,
    ) -> Vec<BlockId> {
        self.transcript_projection.tail_block_ids(
            &self.transcript.history,
            width,
            show_thinking,
            viewport_rows,
        )
    }

    fn prerender_tool_blocks_for_ids(&mut self, width: u16, ids: &[BlockId]) {
        let jobs: Vec<ToolRenderJob> = {
            let history = &self.transcript.history;
            let mut jobs = Vec::new();
            for id in ids {
                let Some(block) = history.blocks.get(id) else {
                    continue;
                };
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
                jobs.push(ToolRenderJob {
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
        self.render_tool_jobs(width, jobs);
    }

    /// Main-thread pre-pass: walk every `Block::ToolCall` whose `ToolState.render_cache`
    /// is missing or width-stale, call the plugin's `render` hook (Lua VM is single-
    /// threaded), and stash the resulting owned-buffer tree on the state. Parallel layout
    /// workers downstream just read those buffers — they never touch `app.ui` or Lua.
    fn prerender_tool_blocks(&mut self, width: u16) {
        let jobs: Vec<ToolRenderJob> = {
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
                jobs.push(ToolRenderJob {
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
        self.render_tool_jobs(width, jobs);
    }

    fn render_tool_jobs(&mut self, width: u16, jobs: Vec<ToolRenderJob>) {
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
        scroll_top: crate::smelt_term::RowIndex,
        row_base: crate::smelt_term::RowIndex,
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
        let first = scroll_top
            .saturating_sub(row_base)
            .min(usize::MAX as crate::smelt_term::RowIndex) as usize;
        let last = first + viewport_rows as usize;
        smelt_buffer::coords::selection_to_row_ranges(buf, s, e)
            .into_iter()
            .filter(|r| r.line >= first && r.line < last)
            .map(|r| (r.line, r.col_start, r.col_end))
            .collect()
    }

    /// Wrap the prompt input against `width` and return the resulting row count.
    /// The Lua layout composer reads this as `state.prompt_input_rows` and
    /// gives the prompt window that many rows in the splits tree.
    pub(crate) fn measure_prompt_input_rows(
        &self,
        edit_buf: &crate::smelt_term::Buffer,
        width: usize,
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let wrap = wrap_with_offsets(edit_buf.source(), &[], usable);
        wrap.visual_lines.len().max(1) as u16
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
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext);
            let cache = smelt_core::content::highlight::build_inline_diff_cache_ext(
                &spec.old,
                &spec.new,
                &spec.path,
                &spec.anchor,
                ext,
            );
            BlockLayout::Leaf(RenderedLeaf::DiffCache(cache))
        }
        BlockLayout::Leaf(LuaLeaf::FileView(spec)) => {
            BlockLayout::Leaf(RenderedLeaf::FileView(spec.clone()))
        }
        BlockLayout::Leaf(LuaLeaf::DiffCache(_)) => {
            panic!("DiffCache should not be produced by Lua render hooks")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_harness::TestApp;
    use crate::content::transcript_buf::ScrollTarget;

    #[test]
    fn selection_highlights_subtract_tail_row_base() {
        let mut harness = TestApp::builder().build();
        for i in 0..40 {
            harness.app.transcript.push(Block::Text {
                content: format!("line {i}"),
            });
        }

        let data = harness
            .app
            .project_transcript_buffer(80, 5, ScrollTarget::Tail, false);
        assert!(data.row_base > 0);

        let buf_id = harness.app.transcript_win().buf;
        let (line_idx, start, end, line_count) = {
            let buf = harness.app.ui.buf(buf_id).expect("transcript buffer");
            let line_idx = buf
                .lines()
                .iter()
                .position(|line| line == "line 39")
                .expect("tail line is materialized");
            let offsets = smelt_buffer::text::line_start_offsets(buf.lines());
            let start = offsets[line_idx];
            (line_idx, start, start + "line 39".len(), buf.lines().len())
        };
        {
            let win = harness.app.transcript_win_mut();
            win.selection_anchor = Some(start);
            win.cpos = end;
        }

        let ranges =
            harness
                .app
                .transcript_selection_highlights(data.clamped_scroll, data.row_base, 5);
        assert!(
            ranges.iter().any(|(line, _, _)| *line == line_idx),
            "selection range should be expressed in materialized buffer rows"
        );
        assert!(ranges.iter().all(|(line, _, _)| *line < line_count));
    }
}
