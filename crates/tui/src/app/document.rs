use crate::app::transcript::TranscriptDocument;
use crate::app::TuiApp;
use crate::smelt_edit::{
    CopyOutput, DisplayDocument, DisplayRows, DisplaySnapshot, DocPosition, DocRange,
    DocumentCommand, DocumentHandle, HostDisplayDocument, RowIndex, SpanAction, TextRange, VimMode,
    WinId,
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
                let mut document = HostDisplayDocument::new(&mut self.ui, win);
                Some(f(&mut document))
            }
        }
    }

    pub(crate) fn document_snapshot_for_win(&mut self, win: WinId) -> Option<DisplaySnapshot> {
        self.with_display_document_for_win(win, |document| document.snapshot())
    }

    pub(crate) fn materialize_document_rows(
        &mut self,
        win: WinId,
        start: RowIndex,
        count: RowIndex,
    ) -> Option<DisplayRows> {
        self.with_display_document_for_win(win, |document| {
            document.materialize(start..start.saturating_add(count))
        })
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
