//! Transcript block history, streaming state, projection, and cursor glyph cache.

use crate::app::TuiApp;
use crate::content::prompt_parser::{
    build_prompt_display_lines, prompt_display_uses_cursor_padding,
};
use crate::smelt_edit::{Buffer, DisplayDocument, DisplaySnapshot, TextRange, Theme};
use smelt_buffer::wrap_layout::WrappedLayout;
use smelt_core::content::file_icons::FileIconOptions;
use smelt_core::content::highlight::InlineOptions;

use smelt_core::content::transcript::Transcript;
use smelt_core::lua::runtime::LuaRuntime;
use smelt_core::transcript_model::{Block, BlockHistory, BlockId, ToolOutputRef, ToolStatus};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LoadedTranscriptDescriptorSlice {
    pub(crate) start: smelt_store::TranscriptDescriptorIndex,
    pub(crate) end: smelt_store::TranscriptDescriptorIndex,
    pub(crate) total_count: usize,
    pub(crate) hydration: smelt_store::TranscriptDescriptorHydration,
}

pub(crate) struct LoadedTranscript {
    pub(crate) transcript: Transcript,
    pub(crate) descriptor_slice: Option<LoadedTranscriptDescriptorSlice>,
}

impl LoadedTranscript {
    pub(crate) fn full(transcript: Transcript) -> Self {
        Self {
            transcript,
            descriptor_slice: None,
        }
    }
}

pub(crate) struct TranscriptView {
    transcript: Transcript,
    projection: crate::content::transcript_buf::TranscriptProjection,
    compaction_preview_id: Option<BlockId>,
    descriptor_slice: Option<LoadedTranscriptDescriptorSlice>,
}

impl TranscriptView {
    pub(crate) fn new() -> Self {
        Self::from_transcript(Transcript::new())
    }

    pub(crate) fn from_transcript(transcript: Transcript) -> Self {
        Self::from_loaded_transcript(LoadedTranscript::full(transcript))
    }

    pub(crate) fn from_loaded_transcript(loaded: LoadedTranscript) -> Self {
        if let Some(slice) = loaded.descriptor_slice {
            smelt_perf::perf::record_value(
                "transcript:descriptor_slice:start",
                slice.start.get() as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:descriptor_slice:end",
                slice.end.get() as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:descriptor_slice:total",
                slice.total_count as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:descriptor_slice:object_backed",
                u64::from(matches!(
                    slice.hydration,
                    smelt_store::TranscriptDescriptorHydration::ObjectBacked
                )),
            );
        }
        Self {
            transcript: loaded.transcript,
            projection: crate::content::transcript_buf::TranscriptProjection::new(),
            compaction_preview_id: None,
            descriptor_slice: loaded.descriptor_slice,
        }
    }

    pub(crate) fn replace_transcript(&mut self, transcript: Transcript) {
        self.replace_loaded_transcript(LoadedTranscript::full(transcript));
    }

    pub(crate) fn replace_loaded_transcript(&mut self, loaded: LoadedTranscript) {
        let inline_options = self.projection.inline_options().clone();
        *self = Self::from_loaded_transcript(loaded);
        self.set_inline_options(inline_options);
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        self.projection.set_inline_options(options);
    }

    pub(crate) fn invalidate_theme(&mut self) {
        self.projection.invalidate_theme();
    }

    pub(crate) fn build_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        self.projection
            .build_rows(lua, &mut self.transcript.history, width, theme)
    }

    pub(crate) fn estimated_total_rows(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        self.projection
            .estimated_total_rows(lua, &mut self.transcript.history, width)
    }

    pub(crate) fn materialize_block_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> Vec<(
        BlockId,
        crate::smelt_edit::RowIndex,
        crate::smelt_edit::RowIndex,
    )> {
        self.projection
            .materialize_block_layout(lua, &mut self.transcript.history, width)
    }

    pub(crate) fn materialize_search_layout(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        self.projection
            .materialize_search_layout(lua, &mut self.transcript.history, width)
    }

    pub(crate) fn materialize_search_layout_for_blocks(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        block_indices: &[u64],
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        self.projection.materialize_search_layout_for_blocks(
            lua,
            &mut self.transcript.history,
            width,
            block_indices,
        )
    }

    pub(crate) fn block_id_at_or_before_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        forward: bool,
    ) -> Option<BlockId> {
        self.projection.block_id_at_or_before_row(
            lua,
            &mut self.transcript.history,
            width,
            row,
            forward,
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

    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        scroll_target: crate::content::transcript_buf::ScrollTarget,
        viewport_rows: u16,
    ) -> crate::content::transcript_buf::ProjectionPlan {
        self.projection.plan_projection_measured(
            lua,
            &mut self.transcript.history,
            width,
            theme,
            scroll_target,
            viewport_rows,
        )
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &LuaRuntime,
        buf: &mut Buffer,
        theme: &Theme,
        plan: crate::content::transcript_buf::ProjectionPlan,
    ) -> crate::smelt_edit::MaterializedRows {
        self.projection
            .project_planned(lua, buf, &mut self.transcript.history, theme, plan)
    }

    pub(crate) fn display_rows_for_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        self.projection.display_rows_for_range(
            lua,
            &mut self.transcript.history,
            width,
            theme,
            start..start.saturating_add(count),
        )
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.projection
            .node_metadata_at_row(lua, &mut self.transcript.history, width, row)
    }

    pub(crate) fn block_node_before_or_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        matches: impl FnMut(&BlockHistory, BlockId) -> bool,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.projection.block_node_before_or_at_row(
            lua,
            &mut self.transcript.history,
            width,
            row,
            matches,
        )
    }

    pub(crate) fn fold_node_at_row(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.projection.fold_node_at_row(
            lua,
            &mut self.transcript.history,
            width,
            crate::content::transcript_buf::FoldAtRow {
                row,
                action,
                activation,
            },
        )
    }

    pub(crate) fn fold_node(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.projection
            .rebuild_row_index(lua, &mut self.transcript.history, width);
        self.projection
            .fold_node(&self.transcript.history, id, action)
    }

    pub(crate) fn fold_all(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.projection
            .rebuild_row_index(lua, &mut self.transcript.history, width);
        self.projection.fold_all(&self.transcript.history, action)
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.projection
            .rebuild_row_index(lua, &mut self.transcript.history, width);
        self.projection
            .fold_block_kind(&self.transcript.history, kind, action)
    }

    pub(crate) fn copy_range(
        &mut self,
        lua: &LuaRuntime,
        width: u16,
        theme: &Theme,
        range: crate::smelt_edit::DocRange,
    ) -> crate::smelt_edit::CopyOutput {
        self.projection
            .copy_range(lua, &mut self.transcript.history, width, theme, range)
    }

    pub(crate) fn history(&self) -> &BlockHistory {
        &self.transcript.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut BlockHistory {
        &mut self.transcript.history
    }

    pub(crate) fn projection_generation(&self) -> u64 {
        self.projection.projection_generation()
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) -> bool {
        self.projection
            .invalidate_renderer_if_changed(generation, cache_key)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.transcript.history.is_empty()
            && self
                .descriptor_slice
                .is_none_or(|slice| slice.total_count == 0)
    }

    pub(crate) fn push(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub(crate) fn push_with_origin(
        &mut self,
        block: Block,
        origin: smelt_core::transcript_model::BlockOrigin,
    ) {
        self.transcript.push_with_origin(block, origin);
    }

    pub(crate) fn set_compaction_preview(&mut self, summary: String) -> Option<BlockId> {
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return self.clear_compaction_preview();
        }

        if let Some(id) = self.compaction_preview_id {
            if self.transcript.block(id).is_some() {
                self.transcript.rewrite_compaction_preview(id, summary);
                return Some(id);
            }
            self.compaction_preview_id = None;
        }

        let id = self.transcript.push_compaction_preview(summary)?;
        self.compaction_preview_id = Some(id);
        Some(id)
    }

    pub(crate) fn clear_compaction_preview(&mut self) -> Option<BlockId> {
        let id = self.compaction_preview_id.take()?;
        self.transcript.remove_compaction_preview(id);
        Some(id)
    }

    pub(crate) fn compaction_preview_id(&self) -> Option<BlockId> {
        self.compaction_preview_id
    }

    pub(crate) fn insert_checkpoint_marker_at(&mut self, block_index: usize, block: Block) {
        self.transcript
            .insert_checkpoint_marker_at(block_index, block);
    }

    pub(crate) fn remove_unoriginated_at(&mut self, block_index: usize) -> Option<Block> {
        self.transcript.remove_unoriginated_at(block_index)
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

pub(crate) struct TranscriptDocument<'a> {
    view: &'a mut TranscriptView,
    lua: &'a LuaRuntime,
    width: u16,
    theme: &'a Theme,
}

impl<'a> TranscriptDocument<'a> {
    pub(crate) fn new(
        view: &'a mut TranscriptView,
        lua: &'a LuaRuntime,
        width: u16,
        theme: &'a Theme,
    ) -> Self {
        Self {
            view,
            lua,
            width,
            theme,
        }
    }
}

impl DisplayDocument for TranscriptDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.view.projection_generation(),
            total_rows: self.view.estimated_total_rows(self.lua, self.width),
        }
    }

    fn materialize(
        &mut self,
        range: std::ops::Range<crate::smelt_edit::RowIndex>,
    ) -> crate::smelt_edit::DisplayRows {
        self.view.display_rows_for_range(
            self.lua,
            self.width,
            self.theme,
            range.start,
            range.end.saturating_sub(range.start),
        )
    }

    fn copy_range(&mut self, range: TextRange) -> Option<crate::smelt_edit::CopyOutput> {
        range.rows().map(|range| {
            self.view
                .copy_range(self.lua, self.width, self.theme, range)
        })
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

    pub(crate) fn clear(&mut self) {
        self.views.clear();
        self.order.clear();
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

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        for view in self.views.values_mut() {
            view.set_inline_options(options.clone());
        }
    }

    pub(crate) fn invalidate_theme(&mut self) {
        for view in self.views.values_mut() {
            view.invalidate_theme();
        }
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        for view in self.views.values_mut() {
            view.invalidate_renderer_if_changed(generation, cache_key);
        }
    }
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
        Block::ToolDraft { .. } | Block::ToolCall { .. } => "tool",
        Block::CodeLine { .. } => "code",
        Block::Exec { .. } => "exec",
        Block::Compacted { .. } => "compacted",
        Block::CompactionPreview { .. } => "compaction_preview",
    }
}

fn transcript_block_first_line(block: &Block) -> String {
    block
        .raw_text()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

fn transcript_raw_first_line(history: &BlockHistory, id: BlockId) -> String {
    history
        .raw_text(id)
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_default()
}

fn transcript_history_role(history: &BlockHistory, id: BlockId) -> &'static str {
    history.block_kind(id).unwrap_or_default()
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

    pub(crate) fn update_compaction_preview(&mut self, summary: String) {
        let existing = self.transcript.compaction_preview_id();
        let Some(id) = self.transcript.set_compaction_preview(summary) else {
            return;
        };
        if existing.is_none() {
            let width = self.transcript_width() as u16;
            self.transcript.fold_node(
                &self.lua,
                width,
                crate::content::render_plan::RenderNodeId::Block(id),
                crate::content::transcript_buf::FoldAction::Peek,
            );
        }
        self.transcript_win_mut().follow_tail();
        self.request_transient_render();
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.transcript.clear_compaction_preview();
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

    pub(crate) fn has_transcript_content(&mut self) -> bool {
        !self.transcript.is_empty()
    }

    /// Full transcript as one string per display row. Result is cached as an
    /// `Arc<Vec<String>>` until the transcript, width, renderer, or presentation changes.
    pub(crate) fn full_transcript_display_text(&mut self) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_full");
        self.sync_transcript_renderer_generation();
        let tw = self.transcript_width() as u16;
        let theme = self.ui.theme().clone();
        self.transcript.build_rows(&self.lua, tw, &theme)
    }

    #[cfg(test)]
    pub(crate) fn transcript_total_rows(&mut self) -> crate::smelt_edit::RowIndex {
        self.document_snapshot_for_win(crate::app::TRANSCRIPT_WIN)
            .map(|snapshot| snapshot.total_rows)
            .unwrap_or(0)
    }

    pub(crate) fn transcript_rows_and_breaks_range(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::DisplayRows {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_range");
        self.materialize_document_rows(crate::app::TRANSCRIPT_WIN, start, count)
            .unwrap_or_default()
    }

    pub(crate) fn transcript_visible_rows(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.transcript_rows_and_breaks_range(start, count)
            .into_text_rows()
    }

    pub(crate) fn transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript.node_metadata_at_row(&self.lua, width, row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript
            .fold_node_at_row(&self.lua, width, row, action, activation)
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        id: crate::content::render_plan::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript.fold_node(&self.lua, width, id, action)
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript.fold_all(&self.lua, width, action)
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        self.transcript
            .fold_block_kind(&self.lua, width, kind, action)
    }

    pub(crate) fn snap_cpos_to_selectable(&mut self, rows: &[String], cpos: usize) -> usize {
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
                let snapped = smelt_buffer::coords::snap_col_to_selectable(buf, r, col);
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
        self.sync_transcript_renderer_generation();
        let tw = self.transcript_width() as u16;
        let layout = self.transcript.materialize_block_layout(&self.lua, tw);
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
            let Some(block) = history.block(block_id) else {
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

    pub(crate) fn transcript_block_before_or_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        role: Option<&str>,
    ) -> Option<TranscriptBlockSnapshot> {
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let node =
            self.transcript
                .block_node_before_or_at_row(&self.lua, width, row, |history, id| {
                    role.is_none_or(|role| transcript_history_role(history, id) == role)
                        && !transcript_raw_first_line(history, id).is_empty()
                })?;
        let block_id = node.id.as_block_id()?;
        let history = self.transcript.history();
        let idx = history.order.iter().position(|id| *id == block_id)?;
        Some((
            idx,
            transcript_history_role(history, block_id),
            node.first_row,
            node.rows,
            transcript_raw_first_line(history, block_id),
        ))
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
            let mode_base = append.mode().map(|_| self.mode_history_base());
            let history_append = append.history_append(mode_base);
            let replace_note_kind = history_append.replacement_note_kind();
            let result = self.apply_history_append_to_history(&history_append);
            if let Some(block) = append.transcript_block(&self.lua) {
                self.commit_history_append_block(block, replace_note_kind, result);
            }
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
        let result = if append.replacement_note_kind().is_some() {
            protocol::HistoryAppendResult::ReplacedLast
        } else {
            protocol::HistoryAppendResult::Pushed
        };
        if let Some(block) = append.transcript_block(&self.lua) {
            self.commit_history_append_block(block, append.replacement_note_kind(), result);
        }
    }

    pub(crate) fn commit_history_append_block(
        &mut self,
        block: Block,
        replace_note_kind: Option<protocol::HistoryNoteKind>,
        result: protocol::HistoryAppendResult,
    ) {
        match result {
            protocol::HistoryAppendResult::Unchanged => {}
            protocol::HistoryAppendResult::RemovedLast => {
                self.remove_last_mode_block();
            }
            protocol::HistoryAppendResult::ReplacedLast => {
                if self.rewrite_last_mode_block(block.clone(), replace_note_kind) {
                    return;
                }
                self.push_block(block);
            }
            protocol::HistoryAppendResult::Pushed => self.push_block(block),
        }
    }

    fn rewrite_last_mode_block(
        &mut self,
        block: Block,
        replace_note_kind: Option<protocol::HistoryNoteKind>,
    ) -> bool {
        if replace_note_kind != Some(protocol::HistoryNoteKind::ModeChange) {
            return false;
        }
        let history = self.transcript.history();
        let Some(id) = history.order.last().copied() else {
            return false;
        };
        if !matches!(history.block(id), Some(Block::Mode { .. })) {
            return false;
        }
        self.transcript.history_mut().rewrite(id, block);
        true
    }

    fn remove_last_mode_block(&mut self) {
        let history = self.transcript.history();
        let Some((idx, id)) = history.order.iter().copied().enumerate().next_back() else {
            return;
        };
        if matches!(history.block(id), Some(Block::Mode { .. })) {
            self.truncate_to(idx);
        }
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

    pub(crate) fn inline_options(&self) -> InlineOptions {
        InlineOptions {
            file_icons: FileIconOptions::new(
                self.core.config.settings.file_icons,
                self.core.config.settings.file_icon_colors,
                self.ui.theme().is_light(),
                Some(std::path::PathBuf::from(&self.cwd)),
            ),
        }
    }

    pub(crate) fn sync_inline_options(&mut self) {
        let options = self.inline_options();
        self.transcript.set_inline_options(options.clone());
        self.resume_preview_cache.set_inline_options(options);
    }

    pub(crate) fn sync_transcript_renderer_generation(&mut self) {
        let generation = self.lua.transcript_renderer_generation();
        let inline_options = self.inline_options();
        let cache_key = crate::content::display_layout::transcript_renderer_cache_key(
            &self.lua,
            &inline_options,
        );
        self.transcript
            .invalidate_renderer_if_changed(generation, cache_key);
        self.resume_preview_cache
            .invalidate_renderer_if_changed(generation, cache_key);
    }

    /// Install a complete theme and publish it to the process-wide active slot.
    pub(crate) fn install_theme(&mut self, theme: Theme) {
        *self.ui.theme_mut() = theme;
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
        self.invalidate_for_theme();
    }

    /// Mutate the current theme and republish.
    pub(crate) fn mutate_theme(&mut self, f: impl FnOnce(&mut Theme)) {
        f(self.ui.theme_mut());
        smelt_core::theme::set_active(self.ui.theme().clone());
        self.sync_inline_options();
        self.sync_transcript_renderer_generation();
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

    /// Per-line selection ranges (line, col_start, col_end) in display-cell units.
    /// No-op when no vim visual or selection anchor is active.
    #[cfg(test)]
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: crate::smelt_edit::RowIndex,
        row_base: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let win = self.transcript_win();
        if win.has_materialized_rows() {
            let buf_id = win.buf;
            let buf = match self.ui.buf(buf_id) {
                Some(b) => b,
                None => return Vec::new(),
            };
            let now = self.core.clock.instant_now();
            return win
                .row_selection_ranges(buf, viewport_rows, now)
                .into_iter()
                .map(|r| (r.line, r.col_start, r.col_end))
                .collect();
        }
        let vim_visual = win.vim_enabled()
            && matches!(
                win.vim_mode(),
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine
            );
        let anchor_set = win.selection_anchor().is_some();
        if !vim_visual && !anchor_set {
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
        let active_selection = if win.vim_enabled() {
            match win.vim_mode() {
                crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => {
                    crate::smelt_edit::vim::visual_range(
                        win.vim_state(),
                        &text,
                        endpoint,
                        win.vim_mode(),
                    )
                }
                _ => win.selection_range_at(endpoint, &text),
            }
        } else {
            win.selection_range_at(endpoint, &text)
        };
        let (s, e) = match active_selection {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        // Route through the shared coord helper so the prompt's per-row
        // selection painting and the transcript's stay one implementation -
        // including the "1-cell fallback span on empty middle rows" rule.
        let first = scroll_top
            .saturating_sub(row_base)
            .min(usize::MAX as crate::smelt_edit::RowIndex) as usize;
        let last = first + viewport_rows as usize;
        smelt_buffer::coords::byte_range_to_row_ranges(buf, s, e)
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
        placeholder: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2).min(u16::MAX as usize) as u16;
        let store = self.input.store.lock().unwrap();
        let lines = build_prompt_display_lines(
            edit_buf.source(),
            &edit_buf.attachment_ids,
            &store,
            placeholder,
        );
        let cursor_padding = prompt_display_uses_cursor_padding(edit_buf.source(), placeholder);
        let layout = if cursor_padding {
            WrappedLayout::from_lines_with_cursor_padding(&lines, usable, true)
        } else {
            WrappedLayout::from_lines(&lines, usable, true)
        };
        layout.visual_count().max(1) as u16
    }
}

#[cfg(test)]
mod tests {
    use crate::app::test_harness::TestApp;

    #[test]
    fn selection_highlights_subtract_materialized_row_base() {
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
            win.set_selection_anchor(Some(start));
            win.set_cpos(end);
        }

        let ranges = harness.app.transcript_selection_highlights(39, 30, 5);
        assert!(
            ranges.iter().any(|(line, _, _)| *line == line_idx),
            "selection range should be expressed in materialized buffer rows"
        );
        assert!(ranges.iter().all(|(line, _, _)| *line < line_count));
    }
}
