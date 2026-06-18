use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crate::app::transcript::TranscriptDocument;
use crate::app::TuiApp;
use crate::smelt_edit::{
    BufferDisplayDocument, CopyOutput, DisplayDocument, DisplayRows, DisplaySnapshot, DocPosition,
    DocRange, DocumentCommand, DocumentHandle, RowIndex, SpanAction, TextRange, VimMode, WinId,
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
    Handle(DocumentHandle),
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
                    TranscriptDocument::new(&mut self.transcript, &self.lua, width, &theme);
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

    pub(crate) fn document_snapshot_for_win(&mut self, win: WinId) -> Option<DisplaySnapshot> {
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
            Some(RegisteredDocument::Transcript) => {
                self.sync_transcript_renderer_generation();
                let inline_options = self.inline_options();
                Some(RenderCacheKey {
                    document: RenderCacheDocument::Handle(crate::app::TRANSCRIPT_DOCUMENT),
                    generation: self.transcript.projection_generation(),
                    width: self.transcript_width() as u16,
                    theme,
                    renderer_generation: self.lua.transcript_renderer_generation(),
                    renderer_cache_key:
                        crate::content::display_layout::transcript_renderer_cache_key(
                            &self.lua,
                            &inline_options,
                        ),
                    start,
                    count,
                })
            }
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

    pub(crate) fn resolve_document_command_for_win(
        &mut self,
        win: WinId,
        command: DocumentCommand,
        cursor: DocPosition,
        vim_mode: VimMode,
    ) -> Option<DocumentCommand> {
        self.with_display_document_for_win(win, |document| {
            crate::smelt_edit::resolve_document_command(document, command, cursor, vim_mode)
        })
        .flatten()
    }
}
