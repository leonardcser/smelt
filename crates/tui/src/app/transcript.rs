//! Transcript ownership on `TuiApp` — block history, streaming state
//! (thinking / text / tools / exec), projection to a
//! crate::ui::Buffer, and the transcript-cursor glyph cache.

use crate::app::TuiApp;
use crate::content::layout_out::SpanCollector;
use crate::content::selection::wrap_and_locate_cursor;
use crate::ui::{BufCreateOpts, BufId, Buffer, Theme};

use crate::content::transcript_parsers as blocks;
use crate::content::transcript_parsers::{render_thinking_summary, thinking_summary};
use smelt_core::transcript_model::{
    Block, BlockId, ToolOutput, ToolOutputRef, ToolState, ToolStatus, ViewState,
};
use smelt_core::transcript_present::{gap_between, Element, ToolBodyRenderer};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Renders tool output bodies by calling the tool's Lua `render` hook
/// with a `Buffer` userdata (full `smelt.buf.*` API + the
/// `smelt.{diff,syntax,bash,notebook,markdown}.render` convenience
/// helpers). The hook writes into a fresh scratch buffer; this
/// projector then walks the buffer back into the still-`SpanCollector`
/// transcript pipeline. Falls back to plain wrapped text when Lua is
/// unavailable or the tool has no `render` hook registered.
pub(crate) struct LuaRenderRenderer;

impl ToolBodyRenderer for LuaRenderRenderer {
    fn render(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
        output: Option<&ToolOutput>,
        width: usize,
        out: &mut SpanCollector,
    ) -> u16 {
        let Some(tool_out) = output else { return 0 };
        let before = out.line_count();
        let ran = crate::lua::app_ref::try_with_app(|app| {
            let buf_id = app.ui.buf_create(crate::ui::BufCreateOpts::default());
            let ok = app
                .lua
                .render_tool_body(name, args, tool_out, width, buf_id.0);
            if !ok {
                let _ = app.ui.buf_destroy(buf_id);
                return false;
            }
            if let Some(buf) = app.ui.buf_destroy(buf_id) {
                crate::content::to_buffer::replay_buffer_into(&buf, out);
            }
            true
        })
        .unwrap_or(false);
        if !ran {
            return crate::content::transcript_parsers::render_default_output(
                out,
                &tool_out.content,
                tool_out.is_error,
                width,
            );
        }
        let after = out.line_count();
        (after - before) as u16
    }

    fn elapsed_visible(&self, name: &str) -> bool {
        crate::lua::app_ref::try_with_app(|app| app.lua.tool_elapsed_visible(name)).unwrap_or(false)
    }

    fn render_summary_line(
        &self,
        name: &str,
        line: &str,
        args: &HashMap<String, serde_json::Value>,
        out: &mut SpanCollector,
    ) -> bool {
        crate::lua::app_ref::try_with_app(|app| {
            if !app.lua.tool_has_render_summary(name) {
                return false;
            }
            let buf_id = app.ui.buf_create(crate::ui::BufCreateOpts::default());
            let ok = app.lua.render_tool_summary_line(name, line, args, buf_id.0);
            if !ok {
                let _ = app.ui.buf_destroy(buf_id);
                return false;
            }
            if let Some(buf) = app.ui.buf_destroy(buf_id) {
                crate::content::to_buffer::replay_buffer_row_into(&buf, 0, out);
            }
            true
        })
        .unwrap_or(false)
    }

    fn render_subhead(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
        _width: usize,
        out: &mut SpanCollector,
    ) -> u16 {
        crate::lua::app_ref::try_with_app(|app| {
            if !app.lua.tool_has_render_subhead(name) {
                return 0u16;
            }
            let buf_id = app.ui.buf_create(crate::ui::BufCreateOpts::default());
            let ok = app.lua.render_tool_subhead(name, args, buf_id.0);
            if !ok {
                let _ = app.ui.buf_destroy(buf_id);
                return 0;
            }
            let Some(buf) = app.ui.buf_destroy(buf_id) else {
                return 0;
            };
            let n = buf.line_count();
            crate::content::to_buffer::replay_buffer_into(&buf, out);
            n as u16
        })
        .unwrap_or(0)
    }

    fn header_suffix(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
        status: &str,
    ) -> Option<String> {
        crate::lua::app_ref::try_with_app(|app| app.lua.tool_header_suffix(name, args, status))
            .flatten()
    }
}

pub(crate) struct TranscriptData {
    pub(crate) clamped_scroll: u16,
    pub(crate) total_rows: u16,
    pub(crate) scrollbar_col: u16,
    pub(crate) viewport: crate::ui::WindowViewport,
}

/// Soft cursor placement carried back from `compute_transcript_cursor`
/// to the painted-split sync. `(col, row)` is viewport-relative;
/// `glyph` is the buffer character under the cursor cell so the block
/// cursor renders the same glyph.
pub(crate) struct SoftCursor {
    pub(crate) col: u16,
    pub(crate) row: u16,
    pub(crate) glyph: char,
}

pub(crate) struct TranscriptCursor {
    pub(crate) clamped_line: u16,
    pub(crate) clamped_col: u16,
    pub(crate) soft_cursor: Option<SoftCursor>,
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

    fn thinking_summary_gap(&self) -> u16 {
        if let Some(last) = self
            .transcript
            .history
            .order
            .iter()
            .rev()
            .filter_map(|id| self.transcript.history.blocks.get(id))
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.transcript.history.is_empty() {
            0
        } else {
            1
        }
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
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.parser
            .start_tool(&mut self.transcript.history, call_id, name, summary, args);
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
        self.parser
            .set_active_status(&mut self.transcript.history, call_id, status);
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

    pub(crate) fn has_transcript_content(&mut self, show_thinking: bool) -> bool {
        !self.transcript.history.is_empty() || self.has_ephemeral(show_thinking)
    }

    /// Full transcript as one string per display row. Cheap when there
    /// are no ephemeral rows (returns an `Arc::clone` of the cached
    /// snapshot); otherwise clones the vec once to append ephemeral
    /// rows. Callers treat it as a `&[String]` via deref coercion.
    pub(crate) fn full_transcript_display_text(&mut self, show_thinking: bool) -> Arc<Vec<String>> {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        if !self.has_ephemeral(show_thinking) {
            let snap = crate::content::transcript_snapshot::build_snapshot(
                &mut self.transcript.history,
                tw,
                show_thinking,
                &theme,
            );
            return Arc::clone(&snap.rows);
        }
        let ephemeral_buf = self.render_ephemeral_to_buffer(tw, show_thinking, &theme);
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        let mut rows: Vec<String> = (*snap.rows).clone();
        for r in 0..ephemeral_buf.line_count() {
            rows.push(ephemeral_buf.get_line(r).unwrap_or("").to_string());
        }
        Arc::new(rows)
    }

    /// Byte positions in `rows.join("\n")` of each `\n` separator,
    /// partitioned into soft-wrap continuations and real line breaks.
    /// Soft-wrap positions are "transparent" to word-select; hard
    /// positions are the boundaries used by line-select. Ephemeral
    /// rows (appended after the snapshot) are treated as hard breaks.
    pub(crate) fn transcript_line_breaks(
        &mut self,
        show_thinking: bool,
    ) -> (Vec<usize>, Vec<usize>) {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        let rows = snap.rows.clone();
        let mut soft = Vec::new();
        let mut hard = Vec::new();
        let mut pos = 0usize;
        for (i, row) in rows.iter().enumerate() {
            pos += row.len();
            if i + 1 < rows.len() {
                let next_is_soft = snap.soft_wrapped.get(i + 1).copied().unwrap_or(false);
                if next_is_soft {
                    soft.push(pos);
                } else {
                    hard.push(pos);
                }
                pos += 1;
            }
        }
        // Every boundary between the snapshot's last row and subsequent
        // ephemeral rows (and between ephemeral rows themselves) is a
        // hard break.
        let snap_row_count = rows.len();
        if self.has_ephemeral(show_thinking) {
            let ephemeral_buf = self.render_ephemeral_to_buffer(tw, show_thinking, &theme);
            let mut first_ephemeral = true;
            for r in 0..ephemeral_buf.line_count() {
                if !first_ephemeral || snap_row_count > 0 {
                    hard.push(pos);
                    pos += 1;
                }
                first_ephemeral = false;
                pos += ephemeral_buf.get_line(r).unwrap_or("").len();
            }
        }
        (soft, hard)
    }

    pub(crate) fn block_text_at_row(
        &mut self,
        abs_row: usize,
        show_thinking: bool,
    ) -> Option<String> {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        // Prefer the block's raw markdown source (text-bearing variants
        // expose `Block::raw_text`) so yanking a rendered markdown block
        // returns `**bold**`, `` `code` ``, fenced blocks, tables etc.
        // verbatim. Fall back to cell-walking for structured blocks
        // (tool / confirm) whose "raw" form isn't a single
        // string.
        let block_id = {
            let snap = crate::content::transcript_snapshot::build_snapshot(
                &mut self.transcript.history,
                tw,
                show_thinking,
                &theme,
            );
            snap.block_of_row.get(abs_row).copied().flatten()
        };
        if let Some(id) = block_id {
            if let Some(raw) = self.transcript.block(id).and_then(|b| b.raw_text()) {
                return Some(raw);
            }
        }
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        snap.block_text_at(abs_row)
    }

    pub(crate) fn snap_col_to_selectable(
        &mut self,
        abs_row: usize,
        col: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        snap.snap_to_selectable(abs_row, col)
            .map(|(_, c)| c)
            .unwrap_or(col)
    }

    pub(crate) fn snap_cpos_to_selectable(
        &mut self,
        rows: &[String],
        cpos: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        let (row, col) = snap.byte_to_row_col(cpos);
        if let Some((_, snapped_col)) = snap.snap_to_selectable(row, col) {
            if snapped_col == col {
                return cpos;
            }
            let mut offset = 0;
            for (r, line) in rows.iter().enumerate() {
                if r == row {
                    let byte_col: usize =
                        line.chars().take(snapped_col).map(|c| c.len_utf8()).sum();
                    return offset + byte_col;
                }
                offset += line.len() + 1;
            }
        }
        cpos
    }

    pub(crate) fn copy_display_range(
        &mut self,
        start: usize,
        end: usize,
        show_thinking: bool,
    ) -> String {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let snap = crate::content::transcript_snapshot::build_snapshot(
            &mut self.transcript.history,
            tw,
            show_thinking,
            &theme,
        );
        snap.copy_byte_range(start, end)
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_core::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
    }

    pub(crate) fn block_view_state(&self, id: BlockId) -> ViewState {
        self.transcript.block_view_state(id)
    }

    pub(crate) fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.transcript.set_block_view_state(id, state);
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    /// Invalidate the width-dependent block layout cache when the
    /// terminal width changes. The TranscriptProjection's BlockBufferCache
    /// is keyed by width, so a width change naturally invalidates on
    /// the next paint pass — this hook is preserved as a no-op for
    /// callers that explicitly want to signal the resize.
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

    /// Update spinner animation state. Call before rendering. Returns
    /// `true` if the spinner frame changed and the caller should
    /// redraw.
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

    /// Project transcript blocks into the transcript display buffer
    /// (`Ui::bufs[transcript_display_buf]`). Gated by generation —
    /// skips work when nothing changed since the last projection.
    pub(crate) fn project_transcript_buffer(
        &mut self,
        width: usize,
        viewport_rows: u16,
        scroll_top: u16,
        show_thinking: bool,
    ) -> TranscriptData {
        let gutters = crate::window::TRANSCRIPT_GUTTERS;
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        let theme = self.ui.theme().clone();

        let ephemeral_buf = self.render_ephemeral_to_buffer(tw as u16, show_thinking, &theme);

        let renderer_arc = self.transcript.history.body_renderer.clone();
        let renderer = renderer_arc.as_deref();
        let buf = self
            .ui
            .win_buf_mut(self.well_known.transcript)
            .expect("transcript window must be registered at startup");
        self.transcript_projection.project(
            buf,
            &mut self.transcript.history,
            tw as u16,
            show_thinking,
            &theme,
            &ephemeral_buf,
            renderer,
        );

        let total_rows = buf.line_count() as u16;

        let clamped_scroll = scroll_top.min(total_rows.saturating_sub(viewport_rows));

        let layer_w = gutters.layer_width(width as u16);
        let scrollbar_col = layer_w.saturating_sub(1);

        // Snapshot visible rows for the soft-cursor glyph lookup in
        // `compute_transcript_cursor`.
        let start = clamped_scroll as usize;
        let end = (start + viewport_rows as usize).min(buf.line_count());
        self.last_viewport_text = buf.get_lines(start, end).to_vec();

        let viewport = crate::ui::WindowViewport::new(
            crate::ui::Rect::new(0, 0, tw as u16, viewport_rows),
            tw as u16,
            total_rows,
            clamped_scroll,
            crate::ui::ScrollbarState::new(scrollbar_col, total_rows, viewport_rows),
        );

        TranscriptData {
            clamped_scroll,
            total_rows,
            scrollbar_col,
            viewport,
        }
    }

    /// Compute transcript cursor position for the compositor pipeline.
    pub(crate) fn compute_transcript_cursor(
        &self,
        width: usize,
        viewport_rows: u16,
        history_cursor_line: u16,
        history_cursor_col: u16,
        transcript_owns_cursor: bool,
        viewport: Option<&crate::ui::WindowViewport>,
    ) -> TranscriptCursor {
        let gutters = crate::window::TRANSCRIPT_GUTTERS;
        let tw = (gutters.content_width(width as u16) as usize).max(1);

        if !transcript_owns_cursor || viewport_rows == 0 {
            return TranscriptCursor {
                clamped_line: history_cursor_line,
                clamped_col: history_cursor_col,
                soft_cursor: None,
            };
        }

        let visible = viewport
            .map(|v| v.total_rows.min(viewport_rows))
            .unwrap_or(viewport_rows);
        let max_line = visible.saturating_sub(1);
        let line = history_cursor_line.min(max_line);
        let max_col = (tw as u16).saturating_sub(1);
        let col = history_cursor_col.min(max_col);
        let under: char = self
            .last_viewport_text
            .get(line as usize)
            .map(|row| {
                let byte = crate::ui::text::cell_to_byte(row, col as usize);
                row[byte..].chars().next()
            })
            .and_then(|c| c)
            .unwrap_or(' ');

        TranscriptCursor {
            clamped_line: line,
            clamped_col: history_cursor_col,
            soft_cursor: Some(SoftCursor {
                col,
                row: line,
                glyph: under,
            }),
        }
    }

    /// Build the transcript's per-line selection ranges (absolute
    /// buffer line index, col_start, col_end) in display-cell units.
    /// Used by the per-frame sync to overlay selection-bg on top of
    /// the projected buffer's text highlights. Cheap no-op when no
    /// vim visual, cursor anchor, or yank-flash is active.
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: u16,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let vim_visual = self.transcript_window.vim_enabled
            && matches!(
                self.vim_mode,
                crate::ui::VimMode::Visual | crate::ui::VimMode::VisualLine
            );
        let anchor_set = self.transcript_window.selection_anchor.is_some();
        let yank_flash = self
            .core
            .clipboard
            .kill_ring
            .yank_flash_range(std::time::Instant::now())
            .is_some();
        if !vim_visual && !anchor_set && !yank_flash {
            return Vec::new();
        }

        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        if rows.is_empty() {
            return Vec::new();
        }
        let buf = rows.join("\n");
        let cpos = self.transcript_window.compute_cpos(&rows);
        let active_selection = if self.transcript_window.vim_enabled {
            match self.vim_mode {
                crate::ui::VimMode::Visual | crate::ui::VimMode::VisualLine => {
                    crate::ui::vim::visual_range(
                        &self.transcript_window.vim_state,
                        &buf,
                        cpos,
                        self.vim_mode,
                    )
                }
                _ => self.transcript_window.selection_range_at(cpos),
            }
        } else {
            self.transcript_window.selection_range_at(cpos)
        };
        // Fall back to the yank-flash range so the selection bg
        // briefly paints over the yanked text after `y`-family vim ops
        // (mirrors nvim's `vim.highlight.on_yank`).
        let (s, e) = match active_selection.or_else(|| {
            self.core
                .clipboard
                .kill_ring
                .yank_flash_range(std::time::Instant::now())
        }) {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        let first = scroll_top as usize;
        let last = (first + viewport_rows as usize).min(rows.len());
        let mut line_start = rows[..first].iter().map(|r| r.len() + 1).sum::<usize>();
        let mut out = Vec::new();
        for (idx, row) in rows.iter().enumerate().take(last).skip(first) {
            let line_end = line_start + row.len();
            if e > line_start && s <= line_end {
                let clip_s = s.saturating_sub(line_start).min(row.len());
                let clip_e = e.saturating_sub(line_start).min(row.len());
                let start_cell = crate::ui::text::byte_to_cell(row, clip_s) as u16;
                let end_cell = crate::ui::text::byte_to_cell(row, clip_e) as u16;
                if end_cell > start_cell {
                    out.push((idx, start_cell, end_cell));
                } else if row.is_empty() && s <= line_start && e > line_start {
                    // Empty line inside the selection: paint a single
                    // virtual cell so the user can see the line is part
                    // of the range. Mirrors vim's "$" virtual-space
                    // behavior on empty lines in v / V mode.
                    out.push((idx, 0, 1));
                }
            }
            line_start = line_end + 1;
        }
        out
    }

    fn has_ephemeral(&self, show_thinking: bool) -> bool {
        self.parser.has_active_thinking() && !show_thinking
    }

    fn render_ephemeral_into(&self, out: &mut SpanCollector, width: usize, show_thinking: bool) {
        let Some(at) = self.parser.active_thinking() else {
            return;
        };
        if show_thinking {
            return;
        }
        let mut combined = at.paragraph.clone();
        if !at.current_line.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&at.current_line);
        }
        if !combined.is_empty() {
            let (label, line_count) = thinking_summary(&combined);
            crate::content::emit_newlines(out, self.thinking_summary_gap());
            render_thinking_summary(out, width, &label, line_count, true);
        }
    }

    /// Render the ephemeral (active thinking) summary into a fresh
    /// scratch Buffer at the given width. Returns an empty buffer when
    /// there's no ephemeral content. Used by the transcript snapshot
    /// helpers and projection path so the same rendering writes
    /// directly into a Buffer (no SpanCollector→DisplayBlock detour).
    fn render_ephemeral_to_buffer(&self, tw: u16, show_thinking: bool, theme: &Theme) -> Buffer {
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        if !self.has_ephemeral(show_thinking) {
            return buf;
        }
        let mut col = SpanCollector::new(&mut buf, theme, tw);
        self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
        let _ = col.finish();
        buf
    }

    pub(crate) fn measure_prompt_height(
        &self,
        state: &crate::input::PromptState,
        width: usize,
        queued: &[String],
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            let geom = blocks::UserBlockGeometry::new(msg, text_w);
            for line in &geom.lines {
                let w = crate::content::layout_out::display_width(line);
                queued_rows += if w == 0 { 1 } else { w.div_ceil(text_w) as u16 };
            }
        }

        let (visual_lines, _, _, _) = wrap_and_locate_cursor(&state.win.text, &[], 0, usable);
        let input_rows = visual_lines.len() as u16;

        queued_rows + stash + 1 + input_rows + 1 + 1
    }
}
