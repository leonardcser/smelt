//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::TuiApp;
use crate::content::prompt_parser::build_prompt_display_lines;
use crate::smelt_edit::{BufCreateOpts, Buffer, Theme};
use smelt_buffer::wrap_layout::WrappedLayout;

use smelt_core::content::block_layout::{BlockLayout, HboxItem, RenderedLayout};
use smelt_core::content::transcript::Transcript;
use smelt_core::transcript_model::{
    Block, BlockHistory, BlockId, ToolOutput, ToolOutputRef, ToolStatus,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct TranscriptView {
    transcript: Transcript,
    projection: crate::content::transcript_buf::TranscriptProjection,
}

impl TranscriptView {
    pub(crate) fn new() -> Self {
        Self::from_transcript(Transcript::new())
    }

    pub(crate) fn from_transcript(transcript: Transcript) -> Self {
        Self {
            transcript,
            projection: crate::content::transcript_buf::TranscriptProjection::new(),
        }
    }

    pub(crate) fn replace_transcript(&mut self, transcript: Transcript) {
        *self = Self::from_transcript(transcript);
    }

    pub(crate) fn invalidate_theme(&mut self) {
        self.projection.invalidate_theme();
    }

    pub(crate) fn build_rows(
        &mut self,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        self.projection
            .build_rows(&mut self.transcript.history, width, show_thinking, theme)
    }

    pub(crate) fn line_breaks(
        &mut self,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> (Vec<usize>, Vec<usize>) {
        self.projection
            .line_breaks(&mut self.transcript.history, width, show_thinking, theme)
    }

    pub(crate) fn materialize_block_layout(
        &mut self,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
    ) -> Vec<(
        BlockId,
        crate::smelt_edit::RowIndex,
        crate::smelt_edit::RowIndex,
    )> {
        self.projection.materialize_block_layout(
            &mut self.transcript.history,
            width,
            show_thinking,
            theme,
        )
    }

    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<
        Item = (
            BlockId,
            crate::smelt_edit::RowIndex,
            crate::smelt_edit::RowIndex,
        ),
    > + '_ {
        self.projection.visible_block_layout()
    }

    pub(crate) fn plan_projection(
        &mut self,
        width: u16,
        show_thinking: bool,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
    ) -> crate::content::transcript_buf::ProjectionPlan {
        self.projection.plan_projection(
            &self.transcript.history,
            width,
            show_thinking,
            scroll_target,
            viewport_rows,
        )
    }

    pub(crate) fn project_planned(
        &mut self,
        buf: &mut Buffer,
        theme: &Theme,
        plan: crate::content::transcript_buf::ProjectionPlan,
    ) -> crate::smelt_edit::MaterializedRows {
        self.projection
            .project_planned(buf, &mut self.transcript.history, theme, plan)
    }

    pub(crate) fn rows_for_range(
        &mut self,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::content::transcript_buf::TranscriptRangeRows {
        self.projection.rows_for_range(
            &mut self.transcript.history,
            width,
            show_thinking,
            theme,
            start,
            count,
        )
    }

    pub(crate) fn history(&self) -> &BlockHistory {
        &self.transcript.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut BlockHistory {
        &mut self.transcript.history
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transcript.history.is_empty()
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    pub(crate) fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub(crate) fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
    }
}

impl Default for TranscriptView {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct ResumePreviewCache {
    views: HashMap<String, TranscriptView>,
    order: VecDeque<String>,
    limit: usize,
}

impl ResumePreviewCache {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            views: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    pub(crate) fn take(&mut self, key: &str) -> Option<TranscriptView> {
        self.views.remove(key)
    }

    pub(crate) fn store(&mut self, key: String, view: TranscriptView) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key.clone());
        self.views.insert(key.clone(), view);

        while self.order.len() > self.limit {
            let Some(old_key) = self.order.pop_front() else {
                break;
            };
            if old_key != key {
                self.views.remove(&old_key);
            }
        }
    }

    pub(crate) fn invalidate_theme(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate_theme();
        }
    }
}

struct ToolRenderJob {
    call_id: String,
    name: String,
    args: HashMap<String, serde_json::Value>,
    output: Option<ToolOutput>,
    status: ToolStatus,
    elapsed_secs: Option<u64>,
}

fn collect_tool_render_jobs(
    history: &BlockHistory,
    width: u16,
    ids: impl Iterator<Item = BlockId>,
) -> Vec<ToolRenderJob> {
    let mut jobs = Vec::new();
    for id in ids {
        let Some(block) = history.blocks.get(&id) else {
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
}

type TranscriptBlockSnapshot = (
    usize,
    &'static str,
    crate::smelt_edit::RowIndex,
    crate::smelt_edit::RowIndex,
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

    pub(crate) fn push_block(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: &str) {
        self.parser
            .append_streaming_thinking(self.transcript.history_mut(), delta);
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.parser
            .flush_streaming_thinking(self.transcript.history_mut());
    }

    pub(crate) fn append_streaming_text(&mut self, delta: &str) {
        self.parser
            .append_streaming_text(self.transcript.history_mut(), delta);
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.parser
            .flush_streaming_text(self.transcript.history_mut());
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
            self.transcript.history_mut(),
            call_id,
            name,
            summary,
            args,
            now,
        );
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.parser
            .start_exec(self.transcript.history_mut(), command);
    }

    pub(crate) fn append_exec_output(&mut self, chunk: &str) {
        self.parser
            .append_exec_output(self.transcript.history_mut(), chunk);
    }

    pub(crate) fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.parser.finish_exec(exit_code);
    }

    pub(crate) fn finalize_exec(&mut self) {
        self.parser.finalize_exec(self.transcript.history_mut());
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub(crate) fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.parser
            .append_active_output(self.transcript.history_mut(), call_id, chunk);
    }

    pub(crate) fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        let now = self.core.clock.instant_now();
        self.parser
            .set_active_status(self.transcript.history_mut(), call_id, status, now);
    }

    pub(crate) fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.parser
            .set_active_user_message(self.transcript.history_mut(), call_id, msg);
    }

    pub(crate) fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let now = self.core.clock.instant_now();
        self.parser.finish_tool(
            self.transcript.history_mut(),
            call_id,
            status,
            output,
            engine_elapsed,
            now,
        );
    }

    pub(crate) fn has_transcript_content(&mut self, _show_thinking: bool) -> bool {
        !self.transcript.is_empty()
    }

    /// Full transcript as one string per display row. Result is cached as an
    /// `Arc<Vec<String>>` until the generation, width, or `show_thinking` changes.
    pub(crate) fn full_transcript_display_text(&mut self, show_thinking: bool) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_full");
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript.build_rows(tw, show_thinking, &theme)
    }

    fn transcript_rows_and_breaks_range(
        &mut self,
        show_thinking: bool,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::content::transcript_buf::TranscriptRangeRows {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_range");
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript
            .rows_for_range(tw, show_thinking, &theme, start, count)
    }

    pub(crate) fn transcript_display_rows_range(
        &mut self,
        show_thinking: bool,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.transcript_rows_and_breaks_range(show_thinking, start, count)
            .rows
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
        let (mut soft, mut hard) = self.transcript.line_breaks(tw, show_thinking, &theme);
        soft.sort_unstable();
        hard.sort_unstable();
        (soft, hard)
    }

    pub(crate) fn transcript_line_breaks_range(
        &mut self,
        show_thinking: bool,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> (Vec<usize>, Vec<usize>) {
        let range = self.transcript_rows_and_breaks_range(show_thinking, start, count);
        (range.soft_breaks, range.hard_breaks)
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
        self.transcript_block_snapshots_from_layout(self.transcript.visible_block_layout())
    }

    pub(crate) fn transcript_block_snapshots(&mut self) -> Vec<TranscriptBlockSnapshot> {
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        let layout = self.transcript.materialize_block_layout(
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
                crate::smelt_edit::RowIndex,
                crate::smelt_edit::RowIndex,
            ),
        >,
    ) -> Vec<TranscriptBlockSnapshot> {
        let mut out = Vec::new();
        let history = self.transcript.history();
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
        row: crate::smelt_edit::RowIndex,
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
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.transcript_display_rows_range(self.core.config.settings.show_thinking, start, count)
    }

    pub(crate) fn finish_transcript_turn(&mut self) {
        let _perf = smelt_perf::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(self.transcript.history_mut());
    }

    pub(crate) fn set_agent_blocked_paused(&mut self, paused: bool) {
        let now = self.core.clock.instant_now();
        self.working.set_paused(paused);
        self.parser
            .set_active_tools_paused(self.transcript.history(), paused, now);
    }

    pub(crate) fn apply_pending_history_appends_for_request(&mut self) {
        let appends = std::mem::take(&mut self.pending_history_appends);
        for append in appends {
            let item = append.history_item();
            let replace_note_kind = append.replacement_note_kind();
            self.apply_history_append_to_history(item, replace_note_kind);
            self.commit_history_append_block(append.transcript_block(&self.lua), replace_note_kind);
        }
    }

    pub(crate) fn commit_pending_history_append(&mut self, item: &protocol::HistoryItem) {
        let Some(idx) = self
            .pending_history_appends
            .iter()
            .position(|append| append.matches_history_item(item))
        else {
            return;
        };
        let append = self.pending_history_appends.remove(idx);
        self.commit_history_append_block(
            append.transcript_block(&self.lua),
            append.replacement_note_kind(),
        );
    }

    pub(crate) fn commit_history_append_block(
        &mut self,
        block: Block,
        replace_note_kind: Option<protocol::HistoryNoteKind>,
    ) {
        let history = self.transcript.history();
        if let Some(kind) = replace_note_kind {
            if let Some(id) = history.order.last().copied() {
                let replaces_mode_block = kind == protocol::HistoryNoteKind::ModeChange
                    && matches!(history.blocks.get(&id), Some(Block::Mode { .. }));
                let replaces_legacy_prefixed_text = kind == protocol::HistoryNoteKind::ModeChange
                    && matches!(
                        history.blocks.get(&id),
                        Some(Block::User { text, .. }) if text.starts_with(protocol::MODE_NOTE_PREFIX)
                    );
                if replaces_mode_block || replaces_legacy_prefixed_text {
                    self.transcript.history_mut().rewrite(id, block);
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
        self.transcript.invalidate_theme();
        self.resume_preview_cache.invalidate_theme();
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
        self.transcript.history_mut().clear();
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
    ) -> crate::smelt_edit::MaterializedRows {
        let gutters = self.transcript_gutters();
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        let now = self.core.clock.instant_now();
        self.parser
            .sync_active_tool_elapsed_at(self.transcript.history_mut(), now);
        let plan =
            self.transcript
                .plan_projection(tw as u16, show_thinking, scroll_target, viewport_rows);
        self.prerender_transcript_tool_blocks_for_ids(tw as u16, plan.block_ids());
        let theme = self.ui.theme().clone();

        let buf = self
            .ui
            .win_buf_mut(self.well_known.transcript)
            .expect("transcript window must be registered at startup");
        self.transcript.project_planned(buf, &theme, plan)
    }

    pub(crate) fn prerender_tool_blocks_in_history_for_ids(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        ids: &[BlockId],
    ) {
        let jobs = collect_tool_render_jobs(history, width, ids.iter().copied());
        let rendered = self.render_tool_jobs(width, jobs);
        Self::store_tool_render_results(history, width, rendered);
    }

    /// Main-thread pre-pass: run plugin `render` hooks for the tool blocks the
    /// next projection can actually materialize. The Lua VM is single-threaded;
    /// worker layout downstream only reads the cached owned buffers.
    fn prerender_transcript_tool_blocks_for_ids(&mut self, width: u16, ids: &[BlockId]) {
        let jobs = collect_tool_render_jobs(self.transcript.history(), width, ids.iter().copied());
        let rendered = self.render_tool_jobs(width, jobs);
        Self::store_tool_render_results(self.transcript.history_mut(), width, rendered);
    }

    fn render_tool_jobs(
        &mut self,
        width: u16,
        jobs: Vec<ToolRenderJob>,
    ) -> Vec<(String, RenderedLayout)> {
        let mut rendered_jobs = Vec::new();
        if jobs.is_empty() {
            return rendered_jobs;
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
            rendered_jobs.push((job.call_id, extract_rendered_layout(&layout, &mut self.ui)));
        }
        rendered_jobs
    }

    fn store_tool_render_results(
        history: &mut BlockHistory,
        width: u16,
        rendered_jobs: Vec<(String, RenderedLayout)>,
    ) {
        for (call_id, rendered) in rendered_jobs {
            if let Some(state) = history.tool_states.get_mut(&call_id) {
                state.render_cache = Some((width, rendered));
            }
        }
    }

    /// Per-line selection ranges (line, col_start, col_end) in display-cell units.
    /// No-op when no vim visual, selection anchor, or yank-flash is active.
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: crate::smelt_edit::RowIndex,
        row_base: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let win = self.transcript_win();
        let vim_visual = win.vim_enabled
            && matches!(
                win.vim_mode,
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine
            );
        let anchor_set = win.selection_anchor.is_some();
        let yank_flash = self.ui.focused_overlay().is_none()
            && self
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
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => {
                    crate::smelt_edit::vim::visual_range(
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
        let flash_range = if self.ui.focused_overlay().is_none() {
            self.core
                .clipboard
                .kill_ring
                .yank_flash_range(self.core.clock.instant_now())
        } else {
            None
        };
        let (s, e) = match active_selection.or(flash_range) {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        // Route through the shared coord helper so the prompt's per-row
        // selection painting and the transcript's stay one implementation -
        // including the "1-cell virtual span on empty middle rows" rule.
        let first = scroll_top
            .saturating_sub(row_base)
            .min(usize::MAX as crate::smelt_edit::RowIndex) as usize;
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
        edit_buf: &crate::smelt_edit::Buffer,
        width: usize,
    ) -> u16 {
        let usable = width.saturating_sub(2).min(u16::MAX as usize) as u16;
        let lines = build_prompt_display_lines(
            edit_buf.source(),
            &edit_buf.attachment_ids,
            &self.input.store.lock().unwrap(),
        );
        let layout = WrappedLayout::from_lines_with_cursor_padding(&lines, usable, true);
        layout.visual_count().max(1) as u16
    }
}

/// Move every leaf buffer out of `ui` into a `RenderedLayout`. Missing buf ids fall back
/// to an empty placeholder so a registration race doesn't take down the frame.
pub(crate) fn extract_rendered_layout(
    layout: &BlockLayout,
    ui: &mut crate::smelt_edit::Ui,
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
    use crate::app::test_harness::TestApp;

    #[test]
    fn selection_highlights_subtract_virtual_row_base() {
        let mut harness = TestApp::builder().build();
        let buf_id = harness.app.transcript_win().buf;
        {
            let buf = harness.app.ui.buf_mut(buf_id).expect("transcript buffer");
            buf.set_all_lines((30..40).map(|i| format!("line {i}")).collect());
        }

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

        let ranges = harness.app.transcript_selection_highlights(39, 30, 5);
        assert!(
            ranges.iter().any(|(line, _, _)| *line == line_idx),
            "selection range should be expressed in materialized buffer rows"
        );
        assert!(ranges.iter().all(|(line, _, _)| *line < line_count));
    }
}
