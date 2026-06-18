use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crate::app::transcript::{TranscriptDisplayDocument, TranscriptRenderContext};
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
enum RenderCacheDocument {
    Buffer(crate::smelt_edit::BufId),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct RenderCacheKey {
    document: RenderCacheDocument,
    generation: u64,
    width: u16,
    theme: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    start: RowIndex,
    count: RowIndex,
}

pub(crate) struct RenderCache {
    rows: HashMap<RenderCacheKey, DisplayRows>,
    order: VecDeque<RenderCacheKey>,
    limit: usize,
}

impl RenderCache {
    const DEFAULT_LIMIT: usize = 16;

    pub(crate) fn new() -> Self {
        Self {
            rows: HashMap::new(),
            order: VecDeque::new(),
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn get(&mut self, key: RenderCacheKey) -> Option<DisplayRows> {
        let rows = self.rows.get(&key)?.clone();
        self.order.retain(|existing| *existing != key);
        self.order.push_back(key);
        Some(rows)
    }

    fn insert(&mut self, key: RenderCacheKey, rows: DisplayRows) {
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
    ) -> Option<RenderCacheKey> {
        let handle = self.document_handle_for_win(win);
        let theme = theme_cache_key(self.ui.theme());
        match DocumentRegistry::resolve_optional(handle) {
            Some(RegisteredDocument::Transcript) => None,
            None if handle.is_some() => None,
            None => {
                let win = self.ui.win(win)?;
                let buf = self.ui.buf(win.buf)?;
                Some(RenderCacheKey {
                    document: RenderCacheDocument::Buffer(win.buf),
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
        if self.registered_document_for_win(win) == Some(RegisteredDocument::Transcript) {
            self.sync_transcript_renderer_generation();
            let width = self.transcript_width() as u16;
            let theme = self.ui.theme().clone();
            let theme_key = theme_cache_key(&theme);
            let inline_options = self.inline_options();
            let renderer_generation = self.lua.transcript_renderer_generation();
            let renderer_cache_key = crate::content::display_layout::transcript_renderer_cache_key(
                &self.lua,
                &inline_options,
            );
            return Some(self.transcript.cached_display_rows_for_range(
                &self.lua,
                &theme,
                TranscriptRenderContext {
                    width,
                    theme_key,
                    renderer_generation,
                    renderer_cache_key,
                },
                start,
                count,
            ));
        }

        let key = self.render_cache_key_for_win(win, start, count);
        if let Some(key) = key {
            if let Some(rows) = self.buffer_render_cache.get(key) {
                return Some(rows);
            }
        }
        let rows = self.with_display_document_for_win(win, |document| {
            document.materialize(start..start.saturating_add(count))
        })?;
        if let Some(key) = key {
            self.buffer_render_cache.insert(key, rows.clone());
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

        let (win_ref, buf_ref) = self.ui.win_and_buf_mut(win, buf);
        let win_ref = win_ref?;
        let buf_ref = buf_ref?;
        win_ref.set_document_view_state(state);
        if win_ref.vim_mode() != vim_mode {
            win_ref.set_vim_mode(vim_mode);
        }
        win_ref.scroll_left = scroll_left;
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
        copy
    }
}
