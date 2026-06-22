use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::json;

use crate::app::transcript::{TranscriptDisplayDocument, TranscriptProjectionRestore};
use crate::app::transcript_scroll_trace::TranscriptScrollIntent;
use crate::app::TuiApp;
use crate::smelt_edit::{
    BufferDisplayDocument, CopyOutput, DisplayDocument, DisplayRows, DocPosition, DocRange,
    DocumentCommand, DocumentHandle, DocumentViewExecutor, MaterializedRows, RowIndex, SpanAction,
    Status, TextRange, WinId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisteredDocument {
    Transcript,
}

impl RegisteredDocument {
    fn render_cache_key(
        self,
        app: &mut TuiApp,
        handle: DocumentHandle,
        theme: u64,
        start: RowIndex,
        count: RowIndex,
    ) -> DocumentRenderCacheKey {
        match self {
            Self::Transcript => {
                app.sync_transcript_renderer_generation();
                let inline_options = app.inline_options();
                let renderer_cache_key =
                    crate::content::display_layout::transcript_renderer_cache_key(
                        &app.lua,
                        &inline_options,
                    );
                DocumentRenderCacheKey {
                    document: DocumentRenderCacheDocument::Registered(handle),
                    generation: app.transcript.projection_generation(),
                    width: app.transcript_width() as u16,
                    theme,
                    renderer_generation: app.lua.transcript_renderer_generation(),
                    renderer_cache_key,
                    start,
                    count,
                }
            }
        }
    }
}

pub(crate) struct DocumentRegistry;

impl DocumentRegistry {
    pub(crate) fn resolve(handle: DocumentHandle) -> Option<RegisteredDocument> {
        match handle {
            crate::app::TRANSCRIPT_DOCUMENT => Some(RegisteredDocument::Transcript),
            _ => None,
        }
    }

    pub(crate) fn resolve_optional(handle: Option<DocumentHandle>) -> Option<RegisteredDocument> {
        handle.and_then(Self::resolve)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum DocumentRenderCacheDocument {
    Buffer(crate::smelt_edit::BufId),
    Registered(DocumentHandle),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct DocumentRenderCacheKey {
    document: DocumentRenderCacheDocument,
    generation: u64,
    width: u16,
    theme: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    start: RowIndex,
    count: RowIndex,
}

pub(crate) struct DocumentRenderCache {
    rows: HashMap<DocumentRenderCacheKey, DisplayRows>,
    order: VecDeque<DocumentRenderCacheKey>,
    limit: usize,
}

impl DocumentRenderCache {
    const DEFAULT_LIMIT: usize = 16;

    pub(crate) fn new() -> Self {
        Self {
            rows: HashMap::new(),
            order: VecDeque::new(),
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn get(&mut self, key: DocumentRenderCacheKey) -> Option<DisplayRows> {
        let rows = self.rows.get(&key)?.clone();
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key);
        Some(rows)
    }

    fn insert(&mut self, key: DocumentRenderCacheKey, rows: DisplayRows) {
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key);
        self.rows.insert(key, rows);
        while self.order.len() > self.limit {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if !self.order.contains(&oldest) {
                self.rows.remove(&oldest);
            }
        }
    }
}

fn theme_cache_key(theme: &crate::smelt_edit::Theme) -> u64 {
    let mut groups: Vec<_> = theme.iter().collect();
    groups.sort_by_key(|(group, _)| *group);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    theme.is_light().hash(&mut hasher);
    for (group, style) in groups {
        group.hash(&mut hasher);
        style.hash(&mut hasher);
    }
    hasher.finish()
}

impl TuiApp {
    fn document_handle_for_win(&self, win: WinId) -> Option<DocumentHandle> {
        self.ui.win(win).and_then(|win| win.document_handle())
    }

    pub(crate) fn registered_document_for_win(&self, win: WinId) -> Option<RegisteredDocument> {
        DocumentRegistry::resolve_optional(self.document_handle_for_win(win))
    }

    pub(crate) fn transcript_document_is_attached_to(&self, win: WinId) -> bool {
        self.registered_document_for_win(win) == Some(RegisteredDocument::Transcript)
    }

    pub(crate) fn with_display_document_for_win<R>(
        &mut self,
        win: WinId,
        f: impl FnOnce(&mut dyn DisplayDocument) -> R,
    ) -> Option<R> {
        let handle = self.document_handle_for_win(win);
        match DocumentRegistry::resolve_optional(handle) {
            Some(RegisteredDocument::Transcript) => {
                self.sync_transcript_renderer_generation();
                let width = self.transcript_width() as u16;
                let theme = self.ui.theme().clone();
                let mut document =
                    TranscriptDisplayDocument::new(&mut self.transcript, &self.lua, width, &theme);
                Some(f(&mut document))
            }
            None if handle.is_some() => None,
            None => {
                self.ui.win(win)?;
                let mut document = BufferDisplayDocument::new(&mut self.ui, win);
                Some(f(&mut document))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn document_snapshot_for_win(
        &mut self,
        win: WinId,
    ) -> Option<crate::smelt_edit::DisplaySnapshot> {
        self.with_display_document_for_win(win, |document| document.snapshot())
    }

    fn render_cache_key_for_win(
        &mut self,
        win: WinId,
        start: RowIndex,
        count: RowIndex,
    ) -> Option<DocumentRenderCacheKey> {
        let handle = self.document_handle_for_win(win);
        let theme = theme_cache_key(self.ui.theme());
        match DocumentRegistry::resolve_optional(handle) {
            Some(document) => Some(document.render_cache_key(self, handle?, theme, start, count)),
            None if handle.is_some() => None,
            None => {
                let win = self.ui.win(win)?;
                let buf = self.ui.buf(win.buf)?;
                Some(DocumentRenderCacheKey {
                    document: DocumentRenderCacheDocument::Buffer(win.buf),
                    generation: buf.changedtick(),
                    width: win
                        .viewport
                        .map(|viewport| viewport.content_width)
                        .unwrap_or(0),
                    theme,
                    renderer_generation: 0,
                    renderer_cache_key: None,
                    start,
                    count,
                })
            }
        }
    }

    pub(crate) fn materialize_document_rows(
        &mut self,
        win: WinId,
        start: RowIndex,
        count: RowIndex,
    ) -> Option<DisplayRows> {
        let key = self.render_cache_key_for_win(win, start, count);
        if let Some(key) = key {
            if let Some(rows) = self.document_render_cache.get(key) {
                return Some(rows);
            }
        }
        let rows = self.with_display_document_for_win(win, |document| {
            document.materialize(start..start.saturating_add(count))
        })?;
        if let Some(key) = key {
            self.document_render_cache.insert(key, rows.clone());
        }
        Some(rows)
    }

    pub(crate) fn copy_document_rows(&mut self, win: WinId, range: DocRange) -> Option<CopyOutput> {
        self.with_display_document_for_win(win, |document| {
            document.copy_range(TextRange::Rows(range))
        })
        .flatten()
    }

    pub(crate) fn document_action_at(
        &mut self,
        win: WinId,
        pos: DocPosition,
    ) -> Option<SpanAction> {
        self.with_display_document_for_win(win, |document| document.action_at(pos))
            .flatten()
    }

    pub(crate) fn document_view_position_at_mouse_for_win(
        &mut self,
        win: WinId,
        event: crossterm::event::MouseEvent,
    ) -> Option<DocPosition> {
        let (viewport, scroll_top, scroll_left, gutter_pad_left) = {
            let win_ref = self.ui.win(win)?;
            (
                win_ref.viewport?,
                win_ref.scroll_top(),
                win_ref.scroll_left,
                win_ref.config.gutters.pad_left,
            )
        };
        self.with_display_document_for_win(win, |document| {
            DocumentViewExecutor::position_at_mouse(
                document,
                event,
                viewport,
                gutter_pad_left,
                scroll_top,
                scroll_left,
            )
        })
        .flatten()
    }

    pub(crate) fn handle_document_view_mouse_for_win(
        &mut self,
        win: WinId,
        event: crossterm::event::MouseEvent,
        click_count: u8,
        now: std::time::Instant,
    ) -> (Status, Option<CopyOutput>) {
        let (
            buf,
            mut state,
            mut vim_mode,
            viewport,
            scroll_top,
            scroll_left,
            gutter_pad_left,
            vim_enabled,
            cursor,
        ) = {
            let Some(win_ref) = self.ui.win(win) else {
                return (Status::Ignored, None);
            };
            let buf = win_ref.buf;
            let Some(viewport) = win_ref.viewport else {
                return (Status::Ignored, None);
            };
            let Some(cursor) = self
                .ui
                .buf(buf)
                .and_then(|buf| win_ref.viewer_doc_cursor(buf))
            else {
                return (Status::Ignored, None);
            };
            (
                buf,
                win_ref.document_view_state(),
                win_ref.vim_mode(),
                viewport,
                win_ref.scroll_top(),
                win_ref.scroll_left,
                win_ref.config.gutters.pad_left,
                win_ref.vim_enabled(),
                cursor,
            )
        };

        let trace_transcript_mouse =
            win == crate::app::TRANSCRIPT_WIN && self.transcript.scroll_trace_enabled();
        if trace_transcript_mouse {
            let scroll_anchor =
                self.transcript
                    .trace_anchor_at_row(&self.lua, viewport.content_width, scroll_top);
            let cursor_anchor = self.transcript.trace_anchor_at_row(
                &self.lua,
                viewport.content_width,
                state.cursor.row,
            );
            self.transcript.record_scroll_trace_event(
                "document_mouse_before",
                json!({
                    "mouse_kind": format!("{:?}", event.kind),
                    "mouse_row": event.row,
                    "mouse_column": event.column,
                    "click_count": click_count,
                    "viewport_top": viewport.rect.top,
                    "viewport_height": viewport.rect.height,
                    "viewport_content_width": viewport.content_width,
                    "window_scroll_top": scroll_top,
                    "scroll_anchor": format!("{:?}", scroll_anchor),
                    "viewer_cursor_before": trace_doc_position_json(cursor),
                    "document_state_cursor_before": trace_doc_position_json(state.cursor),
                    "document_state_cursor_anchor_before": format!("{:?}", cursor_anchor),
                    "selection_anchor_before": state.selection_anchor.map(trace_doc_position_json),
                    "drag_endpoint_before": state.drag_endpoint.map(trace_doc_position_json),
                    "materialized_before": trace_materialized_rows_json(state.materialized),
                }),
            );
        }

        let Some((status, copy)) = self.with_display_document_for_win(win, |document| {
            let total_rows = document.snapshot().total_rows;
            if !state.active {
                state.active = true;
                state.materialized = MaterializedRows {
                    clamped_scroll: scroll_top,
                    row_base: 0,
                    total_rows,
                    materialized_rows: total_rows,
                };
                state.cursor = crate::smelt_edit::DocPosition {
                    row: cursor.row.min(total_rows.saturating_sub(1)),
                    byte_col: cursor.byte_col,
                };
            }
            let (status, range) = DocumentViewExecutor::handle_mouse(
                &mut state,
                document,
                event,
                viewport,
                gutter_pad_left,
                scroll_top,
                scroll_left,
                click_count,
                vim_enabled,
                &mut vim_mode,
                now,
            );
            let copy = range.and_then(|range| document.copy_range(TextRange::Rows(range)));
            (status, copy)
        }) else {
            return (Status::Ignored, None);
        };

        let viewport_rows = viewport.rect.height;
        if let (Some(win_ref), Some(buf_ref)) = self.ui.win_and_buf_mut(win, buf) {
            win_ref.set_document_view_state(state);
            if win_ref.vim_mode() != vim_mode {
                win_ref.set_vim_mode(vim_mode);
            }
            win_ref.sync_row_cursor_to_local(buf_ref, viewport_rows);
        }

        if trace_transcript_mouse {
            let window_scroll_after = self.transcript_scroll_top();
            let state_cursor_anchor = self.transcript.trace_anchor_at_row(
                &self.lua,
                viewport.content_width,
                state.cursor.row,
            );
            self.transcript.record_scroll_trace_event(
                "document_mouse_after",
                json!({
                    "mouse_kind": format!("{:?}", event.kind),
                    "status": format!("{:?}", status),
                    "window_scroll_before": scroll_top,
                    "window_scroll_after": window_scroll_after,
                    "document_state_cursor_after": trace_doc_position_json(state.cursor),
                    "document_state_cursor_anchor_after": format!("{:?}", state_cursor_anchor),
                    "selection_anchor_after": state.selection_anchor.map(trace_doc_position_json),
                    "drag_endpoint_after": state.drag_endpoint.map(trace_doc_position_json),
                    "materialized_after": trace_materialized_rows_json(state.materialized),
                    "copy_returned": copy.is_some(),
                }),
            );
        }

        (status, copy)
    }

    pub(crate) fn execute_document_view_command_for_win(
        &mut self,
        win: WinId,
        command: DocumentCommand,
        viewport_rows: u16,
        now: std::time::Instant,
    ) -> Option<DocRange> {
        let (
            buf,
            mut state,
            mut vim_mode,
            mut scroll_top,
            mut scroll_left,
            following_tail,
            viewport_cols,
            cursor,
        ) = {
            let win = self.ui.win(win)?;
            let buf = win.buf;
            let cursor = self
                .ui
                .buf(buf)
                .and_then(|buf| win.viewer_doc_cursor(buf))?;
            (
                buf,
                win.document_view_state(),
                win.vim_mode(),
                win.scroll_top(),
                win.scroll_left,
                win.is_following_tail(),
                win.viewport
                    .map(|viewport| viewport.content_width)
                    .unwrap_or(0),
                cursor,
            )
        };
        let local_transcript_command = win == crate::app::TRANSCRIPT_WIN
            && matches!(
                command,
                DocumentCommand::MoveRows(_)
                    | DocumentCommand::PageRows(_)
                    | DocumentCommand::HalfPageRows(_)
                    | DocumentCommand::ScrollRows(_)
            );
        let selection_active_before =
            state.selection_anchor.is_some() || state.drag_endpoint.is_some();
        let defer_local_transcript_scroll = local_transcript_command && !selection_active_before;
        let window_scroll_before = scroll_top;
        if defer_local_transcript_scroll {
            scroll_top = self.transcript.local_command_scroll_top(scroll_top);
        }
        let command_scroll_before = scroll_top;
        let pending_local_scroll_before =
            defer_local_transcript_scroll && self.transcript.has_pending_local_scroll_top();
        let trace_transcript_command =
            win == crate::app::TRANSCRIPT_WIN && self.transcript.scroll_trace_enabled();
        if trace_transcript_command {
            let scroll_anchor = self.transcript.trace_anchor_at_row(
                &self.lua,
                viewport_cols.max(1),
                command_scroll_before,
            );
            let cursor_anchor = self.transcript.trace_anchor_at_row(
                &self.lua,
                viewport_cols.max(1),
                state.cursor.row,
            );
            self.transcript.record_scroll_trace_event(
                "document_command_before",
                json!({
                    "command": format!("{:?}", command),
                    "window_scroll_before": window_scroll_before,
                    "command_scroll_before": command_scroll_before,
                    "scroll_anchor_before": format!("{:?}", scroll_anchor),
                    "viewer_cursor_before": trace_doc_position_json(cursor),
                    "document_state_cursor_before": trace_doc_position_json(state.cursor),
                    "document_state_cursor_anchor_before": format!("{:?}", cursor_anchor),
                    "materialized_before": trace_materialized_rows_json(state.materialized),
                    "following_tail_before": following_tail,
                }),
            );
        }
        let copy = self.with_display_document_for_win(win, |document| {
            let total_rows = document.snapshot().total_rows;
            if !state.active {
                state.active = true;
                state.materialized = MaterializedRows {
                    clamped_scroll: scroll_top,
                    row_base: 0,
                    total_rows,
                    materialized_rows: total_rows,
                };
                state.cursor = crate::smelt_edit::DocPosition {
                    row: cursor.row.min(total_rows.saturating_sub(1)),
                    byte_col: cursor.byte_col,
                };
            }
            DocumentViewExecutor::execute(
                &mut state,
                document,
                command,
                &mut vim_mode,
                &mut scroll_top,
                &mut scroll_left,
                viewport_rows,
                viewport_cols,
                following_tail,
                now,
            )
        })?;

        let transcript_scroll_intent =
            if win == crate::app::TRANSCRIPT_WIN && scroll_top != command_scroll_before {
                let rows = signed_row_delta(command_scroll_before, scroll_top);
                let intent = if local_transcript_command {
                    TranscriptScrollIntent::UserDelta { rows }
                } else {
                    let anchor = self.transcript.trace_anchor_at_row(
                        &self.lua,
                        viewport_cols.max(1),
                        scroll_top,
                    );
                    TranscriptScrollIntent::ExactContentAnchor(anchor)
                };
                let restore = if defer_local_transcript_scroll {
                    TranscriptProjectionRestore {
                        cursor_screen_row: Some(screen_row_or_edge(
                            state.cursor.row,
                            scroll_top,
                            viewport_rows,
                        )),
                        drag_endpoint_screen_row: None,
                    }
                } else {
                    TranscriptProjectionRestore::default()
                };
                Some(("document_command", intent, window_scroll_before, restore))
            } else {
                None
            };
        let defer_transcript_window_scroll = defer_local_transcript_scroll;
        if defer_transcript_window_scroll {
            if !pending_local_scroll_before {
                self.transcript.prime_local_scroll_base(
                    &self.lua,
                    viewport_cols.max(1),
                    viewport_rows,
                    command_scroll_before,
                );
            }
            if let Some((label, intent, before, restore)) = transcript_scroll_intent.as_ref() {
                self.record_transcript_scroll_intent_from_document_command(
                    *label,
                    intent.clone(),
                    *before,
                    *restore,
                    Some(scroll_top),
                );
            }
        }

        {
            let (win_ref, buf_ref) = self.ui.win_and_buf_mut(win, buf);
            let win_ref = win_ref?;
            let buf_ref = buf_ref?;
            win_ref.set_document_view_state(state);
            if win_ref.vim_mode() != vim_mode {
                win_ref.set_vim_mode(vim_mode);
            }
            win_ref.scroll_left = scroll_left;
            if !defer_transcript_window_scroll {
                match command {
                    DocumentCommand::ScrollRows(_) => {
                        win_ref.set_scroll(scroll_top, buf_ref);
                        win_ref.update_tail_state(buf_ref, viewport_rows);
                    }
                    DocumentCommand::CenterScroll => win_ref.pin_scroll(scroll_top),
                    DocumentCommand::PanColumns(_) => {}
                    _ => {
                        win_ref.set_resolved_scroll(scroll_top);
                        win_ref.pin_current_scroll();
                    }
                }
                win_ref.sync_row_cursor_to_local(buf_ref, viewport_rows);
            }
        }
        if !defer_transcript_window_scroll {
            self.transcript.clear_pending_local_scroll_top();
            if let Some((label, intent, before, restore)) = transcript_scroll_intent.as_ref() {
                self.record_transcript_scroll_intent_from_document_command(
                    *label,
                    intent.clone(),
                    *before,
                    *restore,
                    None,
                );
            }
        }
        if trace_transcript_command {
            let cursor_anchor = self.transcript.trace_anchor_at_row(
                &self.lua,
                viewport_cols.max(1),
                state.cursor.row,
            );
            self.transcript.record_scroll_trace_event(
                "document_command_after",
                json!({
                    "command": format!("{:?}", command),
                    "window_scroll_before": window_scroll_before,
                    "resolved_scroll_after_command": scroll_top,
                    "window_scroll_after_apply": self.transcript_scroll_top(),
                    "document_state_cursor_after": trace_doc_position_json(state.cursor),
                    "document_state_cursor_anchor_after": format!("{:?}", cursor_anchor),
                    "materialized_after": trace_materialized_rows_json(state.materialized),
                    "copy_returned": copy.is_some(),
                }),
            );
        }
        copy
    }
}

fn signed_row_delta(before: RowIndex, after: RowIndex) -> isize {
    if after >= before {
        after.saturating_sub(before).min(isize::MAX as RowIndex) as isize
    } else {
        -(before.saturating_sub(after).min(isize::MAX as RowIndex) as isize)
    }
}

fn screen_row_or_edge(row: RowIndex, scroll_top: RowIndex, viewport_rows: u16) -> u16 {
    let rel = row.checked_sub(scroll_top);
    rel.and_then(|rel| (rel < RowIndex::from(viewport_rows)).then_some(rel as u16))
        .unwrap_or_else(|| {
            if row < scroll_top {
                0
            } else {
                viewport_rows.saturating_sub(1)
            }
        })
}

fn trace_doc_position_json(position: DocPosition) -> serde_json::Value {
    json!({
        "row": position.row,
        "byte_col": position.byte_col,
    })
}

fn trace_materialized_rows_json(rows: MaterializedRows) -> serde_json::Value {
    json!({
        "clamped_scroll": rows.clamped_scroll,
        "row_base": rows.row_base,
        "total_rows": rows.total_rows,
        "materialized_rows": rows.materialized_rows,
    })
}
