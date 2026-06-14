mod buffer;
pub(crate) mod editor;
mod vim_bridge;

pub(crate) use smelt_core::history::History;

use crate::content;
use crate::keymap::{self, KeyAction, KeyContext};
use crate::smelt_edit::VimMode;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;
use smelt_core::attachment::{Attachment, AttachmentId, AttachmentStore};
use std::sync::{Arc, Mutex};
use vim_bridge::VimBridgeResult;

pub(crate) use smelt_buffer::ATTACHMENT_MARKER;

/// Mutable borrow bundle for prompt edits. Same shape that a Lua-created
/// editable buffer will eventually present, so methods written against `&mut PromptCtx`
/// transfer over without further signature churn.
pub(crate) struct PromptCtx<'a> {
    pub(crate) buf: &'a mut crate::smelt_edit::Buffer,
    pub(crate) win: &'a mut crate::smelt_edit::Window,
}

impl<'a> PromptCtx<'a> {
    pub(crate) fn as_ref(&self) -> PromptCtxRef<'_> {
        PromptCtxRef {
            buf: self.buf,
            win: self.win,
        }
    }
}

/// Read-only sibling of `PromptCtx`.
#[derive(Clone, Copy)]
pub(crate) struct PromptCtxRef<'a> {
    pub(crate) buf: &'a crate::smelt_edit::Buffer,
    pub(crate) win: &'a crate::smelt_edit::Window,
}

/// Disjoint-borrow helper: returns the prompt's `(win, buf)` bundle borrowed
/// from `ui` alone, leaving the rest of `TuiApp` (notably `self.input`)
/// independently borrowable at the call site. Replaces the
/// `ui.win_and_buf_mut(...)` + manual struct-ctor pattern.
pub(crate) fn prompt_ctx_mut(ui: &mut crate::smelt_edit::Ui) -> PromptCtx<'_> {
    let (win, buf) = ui.win_and_buf_mut(crate::app::PROMPT_WIN, crate::app::PROMPT_EDIT_BUF);
    PromptCtx {
        buf: buf.expect("prompt edit buffer"),
        win: win.expect("prompt window"),
    }
}

/// Read-only counterpart of [`prompt_ctx_mut`].
pub(crate) fn prompt_ctx_ref(ui: &crate::smelt_edit::Ui) -> PromptCtxRef<'_> {
    PromptCtxRef {
        buf: ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .expect("prompt edit buffer"),
        win: ui.win(crate::app::PROMPT_WIN).expect("prompt window"),
    }
}

/// Snapshot of the input buffer state (used for Ctrl+S stash).
/// Owns its attachment data so it survives store clears across sessions.
#[derive(Clone, Debug)]
pub(crate) struct InputSnapshot {
    pub(crate) buf: String,
    pub(crate) cpos: usize,
    pub(crate) attachments: Vec<Attachment>,
    from_paste: bool,
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Prompt-specific side-cars (stash, attachments).
/// The canonical edit buffer lives in `ui.bufs[PROMPT_EDIT_BUF]` and the
/// window in `ui.wins[PROMPT_WIN]`; methods that touch either take
/// `&mut Buffer`/`&mut Window` (or shared refs) as parameters. Display
/// coordinates (`cursor_row`, `cursor_col`, `scroll_top`) are synced from
/// `buf.source` via `sync_display_coords` before each render.
pub(crate) struct PromptState {
    pub(crate) store: Arc<Mutex<AttachmentStore>>,
    pub(crate) stash: Option<InputSnapshot>,
    /// True when content came from a paste; cleared on manual character input.
    pub(super) from_paste: bool,
    /// Chord state: true after Ctrl+X, waiting for second key.
    pending_ctrl_x: bool,
}

/// What the caller should do after `handle_event`.
pub(crate) enum Action {
    Redraw,
    Submit { content: Content, display: String },
    SubmitEmpty,
    EditInEditor,
    CenterScroll,
    PanColumns(isize),
    NotifyError(String),
    Noop,
}

impl Default for PromptState {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptState {
    pub(crate) fn new() -> Self {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        Self {
            store,
            stash: None,
            from_paste: false,
            pending_ctrl_x: false,
        }
    }

    /// Active selection range `(start_byte, end_byte)` for vim visual or shift+key selection.
    pub(crate) fn selection_range(&self, ctx: PromptCtxRef<'_>) -> Option<(usize, usize)> {
        let endpoint = ctx.win.effective_endpoint();
        if ctx.win.vim_enabled() {
            if let Some(range) = crate::smelt_edit::vim::visual_range(
                ctx.win.vim_state(),
                ctx.buf.source(),
                endpoint,
                ctx.win.vim_mode(),
            ) {
                return Some(range);
            }
        }
        ctx.win.selection_range_at(endpoint, ctx.buf.source())
    }

    /// Yank-flash range for rendering. The brief post-yank highlight (nvim's
    /// `vim.highlight.on_yank`) is painted via a separate theme group so it
    /// never affects editing/mutations.
    pub(crate) fn yank_flash_range(
        &self,
        ctx: PromptCtxRef<'_>,
        clipboard: &crate::smelt_edit::Clipboard,
        now: std::time::Instant,
    ) -> Option<(usize, usize)> {
        clipboard
            .kill_ring
            .yank_flash_range(now)
            .map(|(s, e)| {
                let src = ctx.buf.source();
                (
                    smelt_buffer::text::snap(src, s),
                    smelt_buffer::text::snap(src, e),
                )
            })
            .filter(|&(s, e)| s < e)
    }

    fn has_selection(&self, ctx: PromptCtxRef<'_>) -> bool {
        self.selection_range(ctx).is_some()
    }

    pub(crate) fn clear_selection(&mut self, win: &mut crate::smelt_edit::Window) {
        win.clear_selection_anchor();
    }

    fn extend_selection(&mut self, win: &mut crate::smelt_edit::Window) {
        win.extend_selection(win.cpos());
    }

    fn delete_selection(
        &mut self,
        ctx: &mut PromptCtx<'_>,
    ) -> Option<crate::smelt_edit::CopyOutput> {
        let (start, end) = self.selection_range(ctx.as_ref())?;
        let deleted = ctx.buf.copy_range(start..end);
        ctx.buf.text_mut().replace_range(start..end, "");
        ctx.win.set_cpos(start);
        ctx.win.clear_selection_anchor();
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
        Some(deleted)
    }

    pub(crate) fn vim_enabled(&self, win: &crate::smelt_edit::Window) -> bool {
        win.vim_enabled()
    }

    /// True when content originated from a paste; skips `!` shell-escape treatment.
    pub(crate) fn skip_shell_escape(&self) -> bool {
        self.from_paste
    }

    pub(crate) fn set_vim_enabled(&mut self, win: &mut crate::smelt_edit::Window, enabled: bool) {
        win.set_vim_enabled(enabled);
        // Prompt is the only writable vim surface - land in Insert when vim
        // turns on so typing works immediately. The global `VimMode` default
        // is Normal (right for transcript / read-only overlays); the prompt
        // overrides here.
        if enabled {
            win.set_vim_mode(VimMode::Insert);
        }
    }

    /// Set this prompt window's vim mode and reset the in-flight key sequence.
    pub(crate) fn set_vim_mode(&mut self, win: &mut crate::smelt_edit::Window, new: VimMode) {
        if win.vim_enabled() {
            win.set_vim_mode(new);
        }
    }

    /// Current vim mode for this prompt window.
    pub(crate) fn vim_mode(&self, win: &crate::smelt_edit::Window) -> VimMode {
        win.vim_mode()
    }

    /// Sync kill ring from clipboard before `C-y` paste.
    /// If clipboard text differs from our last push, treat it as externally updated (charwise).
    fn sync_kill_ring_from_clipboard(clipboard: &mut crate::smelt_edit::Clipboard) {
        let Some(text) = clipboard.read() else {
            return;
        };
        if clipboard.kill_ring.last_clipboard_write() == Some(text.as_str()) {
            return;
        }
        clipboard.kill_ring.set(text.clone());
        clipboard.kill_ring.record_clipboard_write(text);
    }

    /// Wholesale buffer swap. Installs `text` + `ids` atomically (via
    /// `AttachedTextMut::install`, which debug-asserts that marker count in
    /// `text` matches `ids.len()`). Clamps `cpos` to a char boundary, clears
    /// selection + vim visual anchors.
    ///
    /// Passing `ids = vec![]` with marker-containing text trips the debug
    /// assert - exactly what you want, since the caller would otherwise be
    /// silently dropping attachment ids while leaving their markers in
    /// source. Callers that need to preserve attachments hand the
    /// corresponding `Vec<AttachmentId>` in; those that genuinely want a
    /// clean buffer pass `vec![]` and a marker-free `text`.
    pub(crate) fn install_source(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        text: String,
        cpos: usize,
        ids: Vec<AttachmentId>,
    ) {
        ctx.buf.text_mut().install(text, ids);
        let source = ctx.buf.source();
        ctx.win
            .set_cpos(crate::smelt_edit::text::snap(source, cpos));
        ctx.win.clear_selection_anchor();
        ctx.win.clear_vim_visual_anchor();
    }

    pub(crate) fn clear(&mut self, ctx: &mut PromptCtx<'_>) {
        self.install_source(ctx, String::new(), 0, Vec::new());
        self.from_paste = false;
        // Stash and store are intentionally preserved.
    }

    /// Wholesale buffer swap from user-supplied plain text (no attachments).
    /// Snapshots undo, installs new source with empty `attachment_ids`,
    /// cursor at end.
    ///
    /// Intended for callers feeding text from outside the prompt: Lua
    /// `smelt.prompt.set_text`, placeholder accept, `$EDITOR` re-import.
    /// Any `ATTACHMENT_MARKER` bytes in `text` are stripped - these
    /// callers can't carry attachment ids, so the markers would be
    /// orphaned (and trip the marker/id invariant in
    /// `AttachedTextMut::install`).
    pub(crate) fn replace_text(&mut self, ctx: &mut PromptCtx<'_>, text: String) {
        self.save_undo(ctx);
        let text = Self::strip_attachment_markers(&text);
        let cpos = text.len();
        self.install_source(ctx, text, cpos, Vec::new());
    }

    /// Strip `ATTACHMENT_MARKER` bytes from `s`. Used at every seam where
    /// source bytes leave the buffer for storage that can't carry the
    /// matching attachment ids: `History` draft slot, `replace_text`
    /// install path, `$EDITOR` round-trip. Persisting raw markers
    /// upstream would let them return without backing ids, violating
    /// the marker/id invariant on install.
    pub(crate) fn strip_attachment_markers(s: &str) -> String {
        s.replace(ATTACHMENT_MARKER, "")
    }

    /// Install a marker-stripped history entry into the prompt buffer.
    /// `end_cursor = true` places the cursor at end (used by history-down
    /// where shells conventionally land you after the recalled text).
    fn install_history_entry(&mut self, ctx: &mut PromptCtx<'_>, entry: &str, end_cursor: bool) {
        let text = Self::strip_attachment_markers(entry);
        let cpos = if end_cursor { text.len() } else { 0 };
        self.install_source(ctx, text, cpos, Vec::new());
    }

    /// Prepend `prefix` to the buffer, snapshot undo, shift cpos forward.
    /// Existing source bytes (including ATTACHMENT_MARKERs) and their
    /// `attachment_ids` are preserved - unlike `replace_text`, which wipes
    /// attachments by assuming the new text fully supplants the old.
    pub(crate) fn prepend_text(&mut self, ctx: &mut PromptCtx<'_>, prefix: String) {
        if prefix.is_empty() {
            return;
        }
        self.save_undo(ctx);
        let inserted = prefix.len();
        ctx.buf.text_mut().insert_str(0, &prefix);
        let cpos = ctx.win.cpos();
        ctx.win.set_cpos(cpos + inserted);
        ctx.win.clear_selection_anchor();
        // Every offset into the buffer shifts right by `inserted`. Cpos is
        // shifted above; vim's visual_anchor needs the same treatment or
        // it lands mid-codepoint when the prepended prefix straddles a
        // boundary that the anchor used to sit on.
        ctx.win.shift_vim_visual_anchor(inserted);
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
        self.from_paste = false;
    }

    /// Toggle stash. Attachments are cloned out of the store so the stash survives store clears.
    fn toggle_stash(&mut self, ctx: &mut PromptCtx<'_>) {
        if let Some(snap) = self.stash.take() {
            let ids: Vec<_> = snap
                .attachments
                .into_iter()
                .map(|a| self.store.lock().unwrap().insert(a))
                .collect();
            self.install_source(ctx, snap.buf, snap.cpos, ids);
            self.from_paste = snap.from_paste;
        } else {
            let source_empty = ctx.buf.source().is_empty();
            let no_attachments = ctx.buf.attachment_ids.is_empty();
            if !source_empty || !no_attachments {
                let attachments: Vec<_> = ctx
                    .buf
                    .attachment_ids
                    .iter()
                    .filter_map(|&id| self.store.lock().unwrap().get(id).cloned())
                    .collect();
                let stashed = ctx.buf.source().to_string();
                ctx.buf.text_mut().clear();
                let cpos = ctx.win.cpos();
                ctx.win.set_cpos(0);
                self.stash = Some(InputSnapshot {
                    buf: stashed,
                    cpos,
                    attachments,
                    from_paste: self.from_paste,
                });
                ctx.win.clear_selection_anchor();
                ctx.win.clear_vim_visual_anchor();
            }
        }
    }

    pub(crate) fn restore_stash(&mut self, ctx: &mut PromptCtx<'_>) {
        if let Some(snap) = self.stash.take() {
            let ids: Vec<_> = snap
                .attachments
                .into_iter()
                .map(|a| self.store.lock().unwrap().insert(a))
                .collect();
            self.install_source(ctx, snap.buf, snap.cpos, ids);
            self.from_paste = snap.from_paste;
        }
    }

    /// Restore rewind text: replace `[label]` placeholders with attachment markers.
    pub(crate) fn restore_from_rewind(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        mut text: String,
        images: Vec<(String, String)>,
    ) {
        let mut ids = Vec::new();
        for (label, data_url) in images {
            let display = format!("[{label}]");
            if let Some(pos) = text.find(&display) {
                text.replace_range(pos..pos + display.len(), &ATTACHMENT_MARKER.to_string());
                let id = self.store.lock().unwrap().insert_image(label, data_url);
                ids.push(id);
            }
        }
        let cpos = text.len();
        self.install_source(ctx, text, cpos, ids);
    }

    /// Expand attachment markers to text. Image markers are stripped (data flows via `Content::Parts`).
    pub(crate) fn expanded_text(&self, buf: &crate::smelt_edit::Buffer) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        let source = buf.source().to_string();
        for c in source.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = buf.attachment_ids.get(att_idx) {
                    result.push_str(self.store.lock().unwrap().expanded_text(id));
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    pub(crate) fn message_display_text(&self, buf: &crate::smelt_edit::Buffer) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        let source = buf.source().to_string();
        for c in source.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = buf.attachment_ids.get(att_idx) {
                    if let Some(attachment) = self.store.lock().unwrap().get(id) {
                        result.push_str(&attachment.display_label());
                    }
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    pub(crate) fn insert_image(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        label: String,
        data_url: String,
    ) {
        // Mirror `insert_char`: an active selection is replaced by the new
        // attachment. Without this, a stale `selection_anchor` survives the
        // source mutation below and ends up mid-codepoint when the inserted
        // ATTACHMENT_MARKER (3 bytes) shifts byte offsets after the anchor.
        if self.selection_range(ctx.as_ref()).is_some() {
            self.save_undo(ctx);
            self.delete_selection(ctx);
        }
        let id = self.store.lock().unwrap().insert_image(label, data_url);
        self.insert_attachment_id(ctx, id);
    }

    /// Build submission `Content`. Duplicate image refs are deduplicated (base64 payloads are large).
    pub(crate) fn build_content(&self, buf: &crate::smelt_edit::Buffer) -> Content {
        let text = self.expanded_text(buf);
        let mut seen: std::collections::HashSet<AttachmentId> = std::collections::HashSet::new();
        let images: Vec<(String, String)> = buf
            .attachment_ids
            .iter()
            .filter(|&&id| seen.insert(id))
            .filter_map(|&id| match self.store.lock().unwrap().get(id) {
                Some(Attachment::Image { label, data_url }) => {
                    Some((label.clone(), data_url.clone()))
                }
                _ => None,
            })
            .collect();
        Content::with_images(text, images)
    }

    pub(crate) fn key_context(&self, ctx: PromptCtxRef<'_>, agent_running: bool) -> KeyContext {
        KeyContext {
            buf_empty: ctx.buf.source().is_empty() && ctx.buf.attachment_ids.is_empty(),
            vim_non_insert: ctx.win.vim_enabled()
                && matches!(
                    ctx.win.vim_mode(),
                    VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                ),
            vim_enabled: ctx.win.vim_enabled(),
            agent_running,
        }
    }

    /// Sync `win.cursor_row`, `win.cursor_col`, and `win.scroll_top` from the current
    /// source `cpos` using the buffer's parser (if any) or identity mapping.
    pub(crate) fn sync_display_coords(&mut self, ctx: &mut PromptCtx<'_>, viewport_rows: u16) {
        ctx.win.resync_display_coords(ctx.buf);
        let cursor_line = ctx.win.cursor_row();
        let total_rows = ctx.win.layout().visual_count() as crate::smelt_edit::RowIndex;
        if ctx.win.pending_recenter {
            let rows = viewport_rows.max(1) as crate::smelt_edit::RowIndex;
            let max_scroll = total_rows.saturating_sub(rows);
            let s = cursor_line.saturating_sub(rows / 2);
            ctx.win.pin_scroll(s.min(max_scroll));
        } else {
            let viewport_cols = ctx.win.viewport.map(|v| v.content_width).unwrap_or(0);
            ctx.win
                .keep_cursor_visible(ctx.buf, total_rows, viewport_rows, viewport_cols);
        }
    }

    fn execute_key_action(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        action: KeyAction,
        history: Option<&mut History>,
        clipboard: &mut crate::smelt_edit::Clipboard,
    ) -> Action {
        if !matches!(action, KeyAction::Yank | KeyAction::YankPop) {
            clipboard.kill_ring.clear_yank();
        }
        // Any non-vertical action abandons the preferred column so the
        // next vertical motion picks up wherever the user is now.
        if !matches!(
            action,
            KeyAction::MoveUp | KeyAction::MoveDown | KeyAction::SelectUp | KeyAction::SelectDown
        ) {
            ctx.win.set_curswant(None);
        }
        // Selection actions extend; editing actions consume; everything else clears.
        let is_select = matches!(
            action,
            KeyAction::SelectLeft
                | KeyAction::SelectRight
                | KeyAction::SelectUp
                | KeyAction::SelectDown
                | KeyAction::SelectWordForward
                | KeyAction::SelectWordBackward
                | KeyAction::SelectStartOfLine
                | KeyAction::SelectEndOfLine
        );
        let is_editing = matches!(
            action,
            KeyAction::Backspace
                | KeyAction::DeleteCharForward
                | KeyAction::DeleteWordBackward
                | KeyAction::DeleteWordForward
                | KeyAction::DeleteToStartOfLine
                | KeyAction::KillToEndOfLine
                | KeyAction::KillToStartOfLine
                | KeyAction::InsertNewline
                | KeyAction::Yank
                | KeyAction::CutSelection
        );
        let preserves_selection = matches!(action, KeyAction::CopySelection);
        if !is_select && !is_editing && !preserves_selection {
            self.clear_selection(ctx.win);
        }
        match action {
            // Caller handles these.
            KeyAction::Quit | KeyAction::CancelAgent => Action::Noop,

            // ── TuiApp control ─────────────────────────────────────────────
            KeyAction::ClearBuffer => {
                self.clear(ctx);
                Action::Redraw
            }
            // Intercepted by the global chord layer; these arms are unreachable in practice.
            KeyAction::ToggleMode | KeyAction::CycleReasoning => Action::Noop,
            KeyAction::ToggleStash => {
                self.toggle_stash(ctx);
                Action::Redraw
            }
            KeyAction::Redraw => Action::Redraw,

            // ── Submit / newline ─────────────────────────────────────────
            KeyAction::Submit => {
                let source_empty = ctx.buf.source().is_empty();
                let no_attachments = ctx.buf.attachment_ids.is_empty();
                if source_empty && no_attachments {
                    Action::SubmitEmpty
                } else {
                    let display = self.message_display_text(ctx.buf);
                    let content = self.build_content(ctx.buf);
                    self.clear(ctx);
                    Action::Submit { content, display }
                }
            }
            KeyAction::InsertNewline => {
                if self.selection_range(ctx.as_ref()).is_some() {
                    self.save_undo(ctx);
                    self.delete_selection(ctx);
                }
                let p = ctx.buf.text_mut().insert(ctx.win.cpos(), '\n');
                ctx.win.set_cpos(p + 1);
                ctx.win.clamp_anchors_to_source(ctx.buf.source());
                Action::Redraw
            }

            // ── Navigation ──────────────────────────────────────────────
            KeyAction::MoveLeft => {
                if ctx.win.cpos() > 0 {
                    let cpos = ctx.win.cpos();
                    let source = ctx.buf.source();
                    let cp = char_pos(source, cpos);
                    ctx.win.set_cpos(byte_of_char(source, cp - 1));
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                let cpos = ctx.win.cpos();
                if cpos < ctx.buf.source().len() {
                    let source = ctx.buf.source();
                    let cp = char_pos(source, cpos);
                    ctx.win.set_cpos(byte_of_char(source, cp + 1));
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordForward => {
                if self.move_word_forward(ctx) {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordBackward => {
                if self.move_word_backward(ctx) {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveUp => {
                let cpos = ctx.win.cpos();
                let source = ctx.buf.source();
                let (new_pos, new_want) =
                    crate::smelt_edit::text::vertical_move(source, cpos, -1, ctx.win.curswant());
                ctx.win.set_curswant(Some(new_want));
                if new_pos != ctx.win.cpos() {
                    ctx.win.set_cpos(new_pos);
                    Action::Redraw
                } else if let Some(entry) =
                    history.and_then(|h| h.up(&Self::strip_attachment_markers(ctx.buf.source())))
                {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, false);
                    ctx.win.set_curswant(None);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveDown => {
                let cpos = ctx.win.cpos();
                let source = ctx.buf.source();
                let (new_pos, new_want) =
                    crate::smelt_edit::text::vertical_move(source, cpos, 1, ctx.win.curswant());
                ctx.win.set_curswant(Some(new_want));
                if new_pos != ctx.win.cpos() {
                    ctx.win.set_cpos(new_pos);
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.down()) {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, true);
                    ctx.win.set_curswant(None);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveStartOfLine => {
                let cpos = ctx.win.cpos();
                ctx.win
                    .set_cpos(crate::smelt_edit::text::line_start(ctx.buf.source(), cpos));
                Action::Redraw
            }
            KeyAction::MoveEndOfLine => {
                let cpos = ctx.win.cpos();
                ctx.win
                    .set_cpos(crate::smelt_edit::text::line_end(ctx.buf.source(), cpos));
                Action::Redraw
            }
            KeyAction::MoveStartOfBuffer => {
                ctx.win.set_cpos(0);
                Action::Redraw
            }
            KeyAction::MoveEndOfBuffer => {
                ctx.win.set_cpos(ctx.buf.source().len());
                Action::Redraw
            }
            KeyAction::HistoryPrev => {
                if let Some(entry) =
                    history.and_then(|h| h.up(&Self::strip_attachment_markers(ctx.buf.source())))
                {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, false);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::HistoryNext => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    let entry = entry.to_string();
                    self.install_history_entry(ctx, &entry, true);
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }

            // ── Editing ─────────────────────────────────────────────────
            KeyAction::Backspace => {
                self.backspace(ctx);
                Action::Redraw
            }
            KeyAction::DeleteCharForward => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    self.delete_selection(ctx);
                } else {
                    self.delete_char_forward(ctx);
                }
                Action::Redraw
            }
            KeyAction::DeleteWordBackward => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    self.delete_selection(ctx);
                } else {
                    self.delete_word_backward(ctx);
                }
                Action::Redraw
            }
            KeyAction::DeleteWordForward => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    self.delete_selection(ctx);
                } else {
                    self.delete_word_forward(ctx);
                }
                Action::Redraw
            }
            KeyAction::DeleteToStartOfLine => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    self.delete_selection(ctx);
                } else {
                    self.delete_to_start_of_line(ctx);
                }
                Action::Redraw
            }
            KeyAction::KillToEndOfLine => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    let deleted = self.delete_selection(ctx);
                    if let Some(text) = deleted {
                        self.kill_and_copy(text, clipboard);
                    }
                } else {
                    self.kill_to_end_of_line(ctx, clipboard);
                }
                Action::Redraw
            }
            KeyAction::KillToStartOfLine => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    let deleted = self.delete_selection(ctx);
                    if let Some(text) = deleted {
                        self.kill_and_copy(text, clipboard);
                    }
                } else {
                    self.kill_to_start_of_line(ctx, clipboard);
                }
                Action::Redraw
            }
            KeyAction::Yank => {
                self.save_undo(ctx);
                if self.has_selection(ctx.as_ref()) {
                    self.delete_selection(ctx);
                }
                Self::sync_kill_ring_from_clipboard(clipboard);
                let cpos = ctx.win.cpos();
                if let Some(new_cpos) = clipboard.kill_ring.yank(&mut ctx.buf.text_mut(), cpos) {
                    ctx.win.set_cpos(new_cpos);
                    ctx.win.clamp_anchors_to_source(ctx.buf.source());
                }
                Action::Redraw
            }
            KeyAction::YankPop => {
                if let Some(new_cpos) = clipboard.kill_ring.yank_pop(&mut ctx.buf.text_mut()) {
                    ctx.win.set_cpos(new_cpos);
                    ctx.win.clamp_anchors_to_source(ctx.buf.source());
                }
                Action::Redraw
            }
            KeyAction::UppercaseWord => {
                self.save_undo(ctx);
                self.uppercase_word(ctx);
                Action::Redraw
            }
            KeyAction::LowercaseWord => {
                self.save_undo(ctx);
                self.lowercase_word(ctx);
                Action::Redraw
            }
            KeyAction::CapitalizeWord => {
                self.save_undo(ctx);
                self.capitalize_word(ctx);
                Action::Redraw
            }
            KeyAction::Undo => {
                self.undo(ctx);
                Action::Redraw
            }

            // ── Page motion ─────────────────────────────────────────────
            // The prompt is a single edit buffer; page motion clamps to its
            // bounds. Used in vim Normal (Ctrl-B/F/U/D/Y/E) and emacs
            // (Ctrl-V/Alt-V) - both surface through the shared keymap.
            KeyAction::PageUp => {
                self.scroll_lines(ctx, -(content::term_height() as isize));
                Action::Redraw
            }
            KeyAction::PageDown => {
                self.scroll_lines(ctx, content::term_height() as isize);
                Action::Redraw
            }
            KeyAction::HalfPageUp => {
                self.scroll_lines(ctx, -((content::term_height() / 2) as isize));
                Action::Redraw
            }
            KeyAction::HalfPageDown => {
                self.scroll_lines(ctx, (content::term_height() / 2) as isize);
                Action::Redraw
            }
            KeyAction::ScrollLineUp => {
                self.scroll_lines(ctx, -1);
                Action::Redraw
            }
            KeyAction::ScrollLineDown => {
                self.scroll_lines(ctx, 1);
                Action::Redraw
            }

            // ── Clipboard ───────────────────────────────────────────────
            KeyAction::CopySelection => {
                if let Some((start, end)) = self.selection_range(ctx.as_ref()) {
                    let out = ctx.buf.copy_range(start..end);
                    if clipboard.write(&out.clipboard).is_ok() {
                        clipboard
                            .kill_ring
                            .record_clipboard_write(out.clipboard.clone());
                    }
                    clipboard.kill_ring.set(out.kill_ring);
                }
                Action::Noop
            }
            KeyAction::CutSelection => {
                if self.selection_range(ctx.as_ref()).is_some() {
                    self.save_undo(ctx);
                    if let Some(out) = self.delete_selection(ctx) {
                        if clipboard.write(&out.clipboard).is_ok() {
                            clipboard
                                .kill_ring
                                .record_clipboard_write(out.clipboard.clone());
                        }
                        clipboard.kill_ring.set(out.kill_ring);
                    }
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::ClipboardImage => {
                // Bracketed-paste terminals forward Cmd+V as `Event::Paste`, bypassing this arm.
                // Terminals with bracketed paste off send it as a key - handle both paths.
                if let Some(url) = clipboard_image_to_data_url() {
                    self.save_undo(ctx);
                    self.insert_image(ctx, "clipboard.png".into(), url);
                    return Action::Redraw;
                }
                if let Some(text) = clipboard.read() {
                    if !text.is_empty() {
                        self.save_undo(ctx);
                        if self.has_selection(ctx.as_ref()) {
                            self.delete_selection(ctx);
                        }
                        self.insert_paste(ctx, text);
                        return Action::Redraw;
                    }
                }
                Action::Noop
            }

            // ── Selection (shift+movement) ─────────────────────────────
            KeyAction::SelectLeft => {
                self.extend_selection(ctx.win);
                if ctx.win.cpos() > 0 {
                    let cpos = ctx.win.cpos();
                    let source = ctx.buf.source();
                    let cp = char_pos(source, cpos);
                    ctx.win.set_cpos(byte_of_char(source, cp - 1));
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection(ctx.win);
                if ctx.win.cpos() < ctx.buf.source().len() {
                    let cpos = ctx.win.cpos();
                    let source = ctx.buf.source();
                    let cp = char_pos(source, cpos);
                    ctx.win.set_cpos(byte_of_char(source, cp + 1));
                }
                Action::Redraw
            }
            KeyAction::SelectUp => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                let source = ctx.buf.source();
                let (new_pos, new_want) =
                    crate::smelt_edit::text::vertical_move(source, cpos, -1, ctx.win.curswant());
                ctx.win.set_curswant(Some(new_want));
                ctx.win.set_cpos(new_pos);
                Action::Redraw
            }
            KeyAction::SelectDown => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                let source = ctx.buf.source();
                let (new_pos, new_want) =
                    crate::smelt_edit::text::vertical_move(source, cpos, 1, ctx.win.curswant());
                ctx.win.set_curswant(Some(new_want));
                ctx.win.set_cpos(new_pos);
                Action::Redraw
            }
            KeyAction::SelectWordForward => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                let new_cpos = crate::smelt_edit::text::word_forward_pos(
                    ctx.buf.source(),
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                );
                ctx.win.set_cpos(new_cpos);
                Action::Redraw
            }
            KeyAction::SelectWordBackward => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                let new_cpos = crate::smelt_edit::text::word_backward_pos(
                    ctx.buf.source(),
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                );
                ctx.win.set_cpos(new_cpos);
                Action::Redraw
            }
            KeyAction::SelectStartOfLine => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                ctx.win
                    .set_cpos(crate::smelt_edit::text::line_start(ctx.buf.source(), cpos));
                Action::Redraw
            }
            KeyAction::SelectEndOfLine => {
                self.extend_selection(ctx.win);
                let cpos = ctx.win.cpos();
                ctx.win
                    .set_cpos(crate::smelt_edit::text::line_end(ctx.buf.source(), cpos));
                Action::Redraw
            }
        }
    }

    /// Process a terminal event. Priority: vim → paste → keymap → insert.
    pub(crate) fn handle_event(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        ev: Event,
        mut history: Option<&mut History>,
        clipboard: &mut crate::smelt_edit::Clipboard,
        now: std::time::Instant,
    ) -> Action {
        if matches!(ev, Event::Key(_) | Event::Paste(_)) {
            ctx.win.commit_pending_caret_click(ctx.buf);
        }

        match self.dispatch_vim(ctx, &ev, &mut history, clipboard, now) {
            VimBridgeResult::Handled(action) => return action,
            VimBridgeResult::Passthrough | VimBridgeResult::NotAKey => {}
        }

        if let Event::Paste(data) = ev {
            self.save_undo(ctx);
            if self.selection_range(ctx.as_ref()).is_some() {
                self.delete_selection(ctx);
            }
            if let Some(path) = engine::image::normalize_pasted_path(&data) {
                if engine::image::is_image_file(&path) && std::path::Path::new(&path).exists() {
                    match engine::image::read_image_as_data_url(&path) {
                        Ok(url) => {
                            let label = engine::image::image_label_from_path(&path);
                            self.insert_image(ctx, label, url);
                            return Action::Redraw;
                        }
                        Err(e) => {
                            return Action::NotifyError(format!("cannot read image: {e}"));
                        }
                    }
                }
            }
            if data.trim().is_empty() {
                if let Some(url) = clipboard_image_to_data_url() {
                    self.insert_image(ctx, "clipboard.png".into(), url);
                    return Action::Redraw;
                }
            }
            self.insert_paste(ctx, data);
            return Action::Redraw;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            // C-x C-e chord → edit in $EDITOR.
            if self.pending_ctrl_x {
                self.pending_ctrl_x = false;
                if code == KeyCode::Char('e') && modifiers.contains(KeyModifiers::CONTROL) {
                    return Action::EditInEditor;
                }
            }
            if code == KeyCode::Char('x') && modifiers.contains(KeyModifiers::CONTROL) {
                self.pending_ctrl_x = true;
                return Action::Noop;
            }

            let key_ctx = KeyContext {
                buf_empty: ctx.buf.source().is_empty() && ctx.buf.attachment_ids.is_empty(),
                vim_non_insert: ctx.win.vim_enabled()
                    && matches!(
                        ctx.win.vim_mode(),
                        VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                    ),
                vim_enabled: ctx.win.vim_enabled(),
                agent_running: false,
            };

            if let Some(action) = keymap::lookup(code, modifiers, &key_ctx) {
                return self.execute_key_action(ctx, action, history, clipboard);
            }

            if let KeyCode::Char(c) = code {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    self.insert_char(ctx, c);
                    return Action::Redraw;
                }
            }
        }

        Action::Noop
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub(crate) use smelt_buffer::text::{byte_of_char, char_pos};

pub(super) fn current_line(buf: &str, cpos: usize) -> usize {
    let end = smelt_buffer::text::snap(buf, cpos);
    buf[..end].chars().filter(|&c| c == '\n').count()
}

/// Read an image from the system clipboard and return a data URL.
/// macOS: uses `osascript`; Linux: tries `xclip` then `wl-paste`.
fn clipboard_image_to_data_url() -> Option<String> {
    use base64::Engine;

    let tmp = std::env::temp_dir().join("agent_clipboard.png");
    let tmp_str = tmp.to_string_lossy();

    let ok = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "set f to (open for access POSIX file \"{}\" with write permission)\n\
                     try\n\
                       write (the clipboard as «class PNGf») to f\n\
                     end try\n\
                     close access f",
                    tmp_str
                ),
            ])
            .output()
            .ok()
            .is_some_and(|o| o.status.success())
    } else {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .stdout(std::fs::File::create(&tmp).ok()?)
            .status()
            .ok()
            .is_some_and(|s| s.success())
            || std::process::Command::new("wl-paste")
                .args(["--type", "image/png"])
                .stdout(std::fs::File::create(&tmp).ok()?)
                .status()
                .ok()
                .is_some_and(|s| s.success())
    };

    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }

    let bytes = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    if bytes.is_empty() {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:image/png;base64,{b64}"))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Pair a `PromptState` with its own `Buffer` and `Window` for test
    /// convenience. Mirrors the runtime layout where the buffer lives in
    /// `ui.bufs` and the window in `ui.wins`. Derefs to `PromptState` so tests
    /// can call `harness.set_vim_enabled(...)` while still passing
    /// `&mut harness.buf` / `&mut harness.win` to methods that need them.
    pub(super) struct Harness {
        pub(super) state: PromptState,
        pub(super) buf: crate::smelt_edit::Buffer,
        pub(super) win: crate::smelt_edit::Window,
    }

    impl std::ops::Deref for Harness {
        type Target = PromptState;
        fn deref(&self) -> &PromptState {
            &self.state
        }
    }
    impl std::ops::DerefMut for Harness {
        fn deref_mut(&mut self) -> &mut PromptState {
            &mut self.state
        }
    }

    impl Harness {
        pub(super) fn new() -> Self {
            let state = PromptState::new();
            let parser = Arc::new(crate::content::prompt_parser::PromptBufferParser::new(
                state.store.clone(),
            ));
            let mut buf = crate::smelt_edit::Buffer::new(
                crate::app::PROMPT_EDIT_BUF,
                crate::smelt_edit::BufCreateOpts::default(),
            );
            buf.set_parser(parser);
            buf.history = crate::smelt_edit::UndoHistory::new(Some(100));
            let win = crate::smelt_edit::Window::new(
                crate::app::PROMPT_WIN,
                crate::app::PROMPT_EDIT_BUF,
                crate::smelt_edit::SplitConfig {
                    region: "prompt".into(),
                    gutters: crate::smelt_edit::Gutters::default(),
                },
            );
            Self { state, buf, win }
        }

        fn test_action(&mut self, action: KeyAction, mode: VimMode) -> Action {
            self.win.set_vim_mode(mode);
            let mut clip = crate::smelt_edit::Clipboard::null();
            let mut ctx = PromptCtx {
                buf: &mut self.buf,
                win: &mut self.win,
            };
            self.state
                .execute_key_action(&mut ctx, action, None, &mut clip)
        }
    }

    // ── from_paste behavior for shell escape prevention ───────────────────

    #[test]
    fn paste_into_empty_buffer_sets_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!echo hello".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf.source(), "!echo hello");
    }

    #[test]
    fn type_then_type_sets_from_paste_false() {
        let mut input = Harness::new();
        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            '!',
        );
        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            'e',
        );
        assert!(
            !input.skip_shell_escape(),
            "Manual typing should clear from_paste"
        );
    }

    #[test]
    fn type_bang_then_paste_sets_from_paste() {
        let mut input = Harness::new();

        // Simulate user typing '!'
        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            '!',
        );
        assert!(!input.skip_shell_escape(), "Typing clears from_paste");

        // Reset cursor to simulate the scenario: user types '!', then pastes at line start
        // This is the key scenario that was broken before the fix
        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "echo hello".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf.source(), "echo hello");
    }

    #[test]
    fn paste_in_middle_of_line_does_not_set_from_paste() {
        let mut input = Harness::new();

        input.buf.set_source("hello ".to_string());
        input.win.set_cpos(6); // After "hello "
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!world".to_string(),
        );
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf.source(), "hello !world");
    }

    #[test]
    fn paste_at_end_of_line_does_not_set_from_paste() {
        let mut input = Harness::new();

        input.buf.set_source("hello".to_string());
        input.win.set_cpos(5); // At end
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            " world".to_string(),
        );
        assert!(
            !input.skip_shell_escape(),
            "Paste at end of line should not set from_paste"
        );
        assert_eq!(input.buf.source(), "hello world");
    }

    #[test]
    fn paste_at_start_of_multiline_buffer() {
        let mut input = Harness::new();

        input.buf.set_source("line1\nline2".to_string());
        input.win.set_cpos(0); // At very start
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!command".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.buf.source(), "!commandline1\nline2");
    }

    #[test]
    fn paste_at_start_of_second_line_sets_from_paste() {
        let mut input = Harness::new();

        input.buf.set_source("line1\n".to_string());
        input.win.set_cpos(6); // Start of second line
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!command".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.buf.source(), "line1\n!command");
    }

    #[test]
    fn paste_middle_of_second_line_does_not_set_from_paste() {
        let mut input = Harness::new();

        input.buf.set_source("line1\nhello".to_string());
        input.win.set_cpos(8); // Insert at byte position 8 (before first 'l' of "hello")
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            " world".to_string(),
        );
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.buf.source(), "line1\nhe worldllo");
    }

    #[test]
    fn manual_char_after_paste_clears_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!echo hello".to_string(),
        );
        assert!(input.skip_shell_escape());

        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            'x',
        );
        assert!(
            !input.skip_shell_escape(),
            "Manual character after paste should clear from_paste"
        );
    }

    #[test]
    fn backspace_at_start_clears_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!echo hello".to_string(),
        );
        assert!(input.skip_shell_escape());

        input.state.backspace(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // Deletes last character
        assert!(
            input.skip_shell_escape(),
            "Backspace not at start should not clear from_paste"
        );

        input.win.set_cpos(0);
        input.state.backspace(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // Now at position 0
            // Can't backspace further, but the logic would clear it if we could
    }

    #[test]
    fn delete_word_backward_at_start_clears_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!echo hello".to_string(),
        );
        assert!(input.skip_shell_escape());

        // Move cursor to end
        input.win.set_cpos(input.buf.source().len());
        input.state.delete_word_backward(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // Deletes "hello"
        assert!(
            input.skip_shell_escape(),
            "Delete word not at start should not clear from_paste"
        );

        // Move to after "echo " and delete word
        input.win.set_cpos(5); // After "echo"
        input.state.delete_word_backward(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // Deletes "echo"
        assert!(input.skip_shell_escape(), "Still not at absolute start");

        input.win.set_cpos(1); // After "!"
        input.state.delete_word_backward(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // Would delete to start, which should clear from_paste
        assert!(
            !input.skip_shell_escape(),
            "Delete word to start should clear from_paste"
        );
    }

    #[test]
    fn clear_resets_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!test".to_string(),
        );
        assert!(input.skip_shell_escape());

        input.state.clear(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(!input.skip_shell_escape(), "Clear should reset from_paste");
    }

    #[test]
    fn stash_preserves_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!test".to_string(),
        );
        assert!(input.skip_shell_escape());

        // Stash: saves from_paste to snapshot, but doesn't clear it in active buffer
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(
            input.skip_shell_escape(),
            "Stash saves from_paste to snapshot but keeps it in buffer"
        );
        assert!(
            input.buf.source().is_empty(),
            "Buffer should be empty after stashing"
        );

        // Restore: restores from_paste from snapshot
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(input.skip_shell_escape(), "Stash should restore from_paste");
        assert_eq!(input.buf.source(), "!test");
    }

    #[test]
    fn multiple_pastes_set_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!first".to_string(),
        );
        assert!(input.skip_shell_escape());

        // Type something, which clears from_paste
        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            ' ',
        );
        assert!(!input.skip_shell_escape());

        // Paste again at start of line
        input.win.set_cpos(0);
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!second".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Second paste at start should set from_paste again"
        );
    }

    #[test]
    fn paste_with_carriage_returns_normalized() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!line1\r\nline2\rline3".to_string(),
        );
        assert!(input.skip_shell_escape());
        assert!(
            !input.buf.source().contains('\r'),
            "Carriage returns should be normalized"
        );
        assert_eq!(input.buf.source(), "!line1\nline2\nline3");
    }

    #[test]
    fn empty_paste_does_not_set_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "".to_string(),
        );
        assert!(
            !input.skip_shell_escape(),
            "Empty paste should not set from_paste"
        );
    }

    #[test]
    fn whitespace_only_paste_at_start_sets_from_paste() {
        let mut input = Harness::new();
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "   ".to_string(),
        );
        assert!(
            input.skip_shell_escape(),
            "Whitespace paste at start should set from_paste"
        );
    }

    #[test]
    fn paste_starting_with_bang_at_line_start() {
        // This is the main bug scenario: type '!', then paste command
        let mut input = Harness::new();

        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "!ls -la".to_string(),
        );

        assert!(
            input.skip_shell_escape(),
            "Paste at start of line should set from_paste"
        );
        assert_eq!(input.buf.source(), "!ls -la");

        // The expanded text should not be treated as shell command
        let text = input.state.expanded_text(&input.buf);
        assert_eq!(text, "!ls -la");
    }

    // ── Selection tests ─────────────────────────────────────────────────

    #[test]
    fn shift_select_right_creates_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.selection_anchor(), Some(0));
        assert_eq!(input.win.cpos(), 1);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 1))
        );
    }

    #[test]
    fn shift_select_extends_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.selection_anchor(), Some(0));
        assert_eq!(input.win.cpos(), 3);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 3))
        );
    }

    #[test]
    fn movement_clears_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_some());
        input.test_action(KeyAction::MoveRight, crate::smelt_edit::VimMode::Insert);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_none());
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        // Select "hello"
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 5))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), " world");
        assert_eq!(input.win.cpos(), 0);
    }

    #[test]
    fn delete_forward_deletes_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteCharForward,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(input.buf.source(), " world");
    }

    #[test]
    fn typing_replaces_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.state.insert_char(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            'X',
        );
        assert_eq!(input.buf.source(), "X world");
        assert_eq!(input.win.cpos(), 1);
    }

    #[test]
    fn select_left_from_end() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(5);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.selection_anchor(), Some(5));
        assert_eq!(input.win.cpos(), 3);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((3, 5))
        );
    }

    #[test]
    fn select_word_forward() {
        let mut input = Harness::new();
        input.buf.set_source("hello world foo".to_string());
        input.win.set_cpos(0);
        input.test_action(
            KeyAction::SelectWordForward,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(input.win.selection_anchor(), Some(0));
        // word_forward_pos from 0 should be 6 (start of "world").
        assert_eq!(input.win.cpos(), 6);
        input.test_action(KeyAction::Backspace, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), "world foo");
    }

    #[test]
    fn select_word_backward() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(11);
        input.test_action(
            KeyAction::SelectWordBackward,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((6, 11))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), "hello ");
    }

    #[test]
    fn select_to_line_start() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(5);
        input.test_action(
            KeyAction::SelectStartOfLine,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 5))
        );
    }

    #[test]
    fn select_to_line_end() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(5);
        input.test_action(
            KeyAction::SelectEndOfLine,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((5, 11))
        );
    }

    #[test]
    fn newline_replaces_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.test_action(KeyAction::InsertNewline, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), "\n world");
        assert_eq!(input.win.cpos(), 1);
    }

    #[test]
    fn kill_to_eol_with_selection() {
        let mut input = Harness::new();
        let mut clip = crate::smelt_edit::Clipboard::null();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        for _ in 0..5 {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input
                .state
                .execute_key_action(&mut ctx, KeyAction::SelectRight, None, &mut clip);
        }
        {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input
                .state
                .execute_key_action(&mut ctx, KeyAction::KillToEndOfLine, None, &mut clip);
        }
        assert_eq!(input.buf.source(), " world");
        // Killed text lands on the TuiApp-level kill ring.
        assert_eq!(clip.kill_ring.current(), "hello");
    }

    #[test]
    fn selection_at_buffer_boundary() {
        let mut input = Harness::new();
        input.buf.set_source("ab".to_string());
        input.win.set_cpos(0);
        // Select all.
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 2))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), "");
        assert_eq!(input.win.cpos(), 0);
    }

    #[test]
    fn selection_range_empty_when_anchor_equals_cursor() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(3);
        input.win.set_selection_anchor(Some(3));
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            None
        );
    }

    #[test]
    fn clear_resets_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_some());
        input.state.clear(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_none());
    }

    #[test]
    fn delete_word_backward_with_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(6);
        // Select "wor"
        for _ in 0..3 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteWordBackward,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(input.buf.source(), "hello ld");
    }

    #[test]
    fn delete_word_forward_with_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        for _ in 0..3 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteWordForward,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(input.buf.source(), "lo world");
    }

    #[test]
    fn delete_to_start_of_line_with_selection() {
        let mut input = Harness::new();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(3);
        for _ in 0..4 {
            input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteToStartOfLine,
            crate::smelt_edit::VimMode::Insert,
        );
        assert_eq!(input.buf.source(), "helorld");
    }

    #[test]
    fn select_left_at_start_stays() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.cpos(), 0);
        assert_eq!(input.win.selection_anchor(), Some(0));
    }

    #[test]
    fn select_right_at_end_stays() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(5);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.cpos(), 5);
    }

    #[test]
    fn select_empty_buffer() {
        let mut input = Harness::new();
        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.cpos(), 0);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_none());
    }

    #[test]
    fn utf8_selection() {
        let mut input = Harness::new();
        input.buf.set_source("héllo".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        // Should select "hé" - 2 chars but 3 bytes.
        assert_eq!(input.win.cpos(), 3); // byte offset of 'l'
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 3))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.buf.source(), "llo");
    }

    #[test]
    fn selection_preserved_across_multiple_select_directions() {
        let mut input = Harness::new();
        input.buf.set_source("abcdef".to_string());
        input.win.set_cpos(3); // on 'd'
                               // Select right 2 chars.
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((3, 5))
        );
        // Then select left 4 chars - anchor stays at 3.
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_edit::VimMode::Insert);
        assert_eq!(input.win.cpos(), 1);
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((1, 3))
        );
    }

    #[test]
    fn vim_esc_clears_shift_selection() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };

        let mut input = Harness::new();
        let mut clipboard = crate::smelt_edit::Clipboard::null();
        input.state.set_vim_enabled(&mut input.win, true);
        input
            .state
            .set_vim_mode(&mut input.win, crate::smelt_edit::VimMode::Insert);
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(0);
        // Create a shift selection.
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_edit::VimMode::Insert);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_some());
        // Press Esc - vim switches to normal mode AND clears selection.
        let esc = Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input.state.handle_event(
                &mut ctx,
                esc,
                None,
                &mut clipboard,
                std::time::Instant::now(),
            );
        }
        assert!(
            input
                .state
                .selection_range(PromptCtxRef {
                    buf: &input.buf,
                    win: &input.win
                })
                .is_none(),
            "Esc should clear shift selection"
        );
        assert_eq!(
            input.state.vim_mode(&input.win),
            crate::smelt_edit::VimMode::Normal,
            "Should be in normal mode"
        );
    }

    #[test]
    fn delete_selection_removes_attachments() {
        let mut input = Harness::new();
        // Insert text with an image attachment marker in the middle.
        input.buf.set_source(format!("ab{}cd", ATTACHMENT_MARKER));
        input.win.set_cpos(0);
        let id = input
            .store
            .lock()
            .unwrap()
            .insert_image("img.png".into(), "data:image/png;base64,AAA".into());
        input.buf.attachment_ids.push(id);
        // Select "b<marker>c" (bytes 1..5 - marker is 3 bytes).
        input.win.set_selection_anchor(Some(1));
        input.win.set_cpos(1 + 1 + ATTACHMENT_MARKER.len_utf8() + 1);
        assert!(input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            })
            .is_some());
        let deleted = input.state.delete_selection(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(deleted.is_some());
        assert_eq!(input.buf.source(), "ad");
        assert!(
            input.buf.attachment_ids.is_empty(),
            "Attachment should be removed"
        );
    }

    // ── Attachment dedup within a single message ───────────────────────

    /// Place two markers in the buffer that both point at `id`.
    fn buf_with_two_markers(input: &mut Harness, id: AttachmentId) {
        input
            .buf
            .set_source(format!("pre{m}mid{m}post", m = ATTACHMENT_MARKER));
        input.win.set_cpos(input.buf.source().len());
        input.buf.attachment_ids = vec![id, id];
    }

    #[test]
    fn build_content_dedups_repeated_image_in_parts() {
        let mut input = Harness::new();
        let id = input
            .store
            .lock()
            .unwrap()
            .insert_image("img.png".into(), "data:image/png;base64,AAA".into());
        buf_with_two_markers(&mut input, id);
        let content = input.state.build_content(&input.buf);
        assert_eq!(
            content.image_count(),
            1,
            "repeated image should appear once in Content::Parts"
        );
    }

    #[test]
    fn build_content_preserves_distinct_images() {
        let mut input = Harness::new();
        let id1 = input
            .store
            .lock()
            .unwrap()
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = input
            .store
            .lock()
            .unwrap()
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input
            .buf
            .set_source(format!("{m}{m}", m = ATTACHMENT_MARKER));
        input.win.set_cpos(input.buf.source().len());
        input.buf.attachment_ids = vec![id1, id2];
        let content = input.state.build_content(&input.buf);
        assert_eq!(content.image_count(), 2);
    }

    #[test]
    fn build_content_dedups_interleaved_image_references() {
        // Pattern: img A, img B, img A again. Parts should be [A, B].
        let mut input = Harness::new();
        let id_a = input
            .store
            .lock()
            .unwrap()
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id_b = input
            .store
            .lock()
            .unwrap()
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input
            .buf
            .set_source(format!("{m}x{m}y{m}", m = ATTACHMENT_MARKER));
        input.win.set_cpos(input.buf.source().len());
        input.buf.attachment_ids = vec![id_a, id_b, id_a];
        let content = input.state.build_content(&input.buf);
        assert_eq!(content.image_count(), 2);
    }

    use crate::keymap::KeyAction;

    // ── Stale-anchor regression suite ──────────────────────────────────
    //
    // Selection anchors are byte offsets into `source`. Any path that replaces
    // `source` wholesale must clear the anchor, OR the read seam must clamp;
    // both layers exist. These tests pin both invariants.

    #[test]
    fn stale_anchor_past_source_end_clamps_to_source_len() {
        let mut input = Harness::new();
        input.buf.set_source("hi".to_string());
        input.win.set_cpos(0);
        input.win.set_selection_anchor(Some(5808));
        // Stale anchor is clamped to source.len(); slice is in-bounds, no panic.
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 2))
        );
        // And the slice that would previously panic now succeeds.
        let (s, e) = input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win,
            })
            .unwrap();
        let _ = input.buf.source()[s..e];
    }

    #[test]
    fn stale_anchor_against_empty_source_returns_none() {
        let mut input = Harness::new();
        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.win.set_selection_anchor(Some(371));
        // Both endpoints clamp to 0 → range collapses to None.
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            None
        );
    }

    #[test]
    fn delete_selection_with_stale_anchor_does_not_panic() {
        let mut input = Harness::new();
        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.win.set_selection_anchor(Some(5808));
        assert_eq!(
            input.state.delete_selection(&mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win
            }),
            None
        );
        assert_eq!(input.buf.source(), "");
    }

    #[test]
    fn copy_selection_with_stale_anchor_does_not_panic() {
        let mut input = Harness::new();
        input.buf.set_source(String::new());
        input.win.set_cpos(0);
        input.win.set_selection_anchor(Some(5808));
        input.test_action(KeyAction::CopySelection, crate::smelt_edit::VimMode::Insert);
    }

    #[test]
    fn stale_anchor_mid_codepoint_snaps_to_boundary() {
        // Anchor lands inside a multi-byte codepoint; snap pulls it to a boundary
        // so the resulting slice can never split a UTF-8 sequence.
        let mut input = Harness::new();
        input.buf.set_source("héllo".to_string()); // 'é' occupies bytes 1..3
        input.win.set_cpos(0);
        input.win.set_selection_anchor(Some(2)); // mid-codepoint
        assert_eq!(
            input.state.selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win
            }),
            Some((0, 1))
        );
        let (s, e) = input
            .state
            .selection_range(PromptCtxRef {
                buf: &input.buf,
                win: &input.win,
            })
            .unwrap();
        let _ = input.buf.source()[s..e]; // would panic without the snap
    }

    #[test]
    fn replace_text_strips_attachment_markers() {
        let mut input = Harness::new();
        input.buf.set_source(String::new());
        let text = format!("hi{m}there", m = ATTACHMENT_MARKER);
        input.state.replace_text(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            text,
        );
        assert_eq!(input.buf.source(), "hithere");
        assert!(input.buf.attachment_ids.is_empty());
    }

    #[test]
    fn replace_text_clears_anchor() {
        let mut input = Harness::new();
        input.buf.set_source("long source".to_string());
        input.win.set_selection_anchor(Some(7));
        input.win.set_cpos(0);
        input.state.replace_text(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            String::new(),
        );
        assert_eq!(input.win.selection_anchor(), None);
    }

    #[test]
    fn toggle_stash_both_branches_clear_anchor() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.win.set_cpos(5);
        input.win.set_selection_anchor(Some(2));
        // Stash branch: source moves into stash.
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert_eq!(
            input.win.selection_anchor(),
            None,
            "stashing must clear stale anchor"
        );
        // Re-set an anchor against the (now empty) source and unstash.
        input.win.set_selection_anchor(Some(99));
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert_eq!(
            input.win.selection_anchor(),
            None,
            "unstashing must clear stale anchor"
        );
        assert_eq!(input.buf.source(), "hello");
    }

    #[test]
    fn restore_stash_clears_anchor() {
        let mut input = Harness::new();
        input.buf.set_source("hello".to_string());
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        }); // moves "hello" into stash
        input.win.set_selection_anchor(Some(99)); // stale against current empty source
        input.state.restore_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert_eq!(input.win.selection_anchor(), None);
        assert_eq!(input.buf.source(), "hello");
    }

    #[test]
    fn restore_from_rewind_clears_anchor() {
        let mut input = Harness::new();
        input.buf.set_source("long buffer".to_string());
        input.win.set_selection_anchor(Some(7));
        input.state.restore_from_rewind(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "hi".to_string(),
            Vec::new(),
        );
        assert_eq!(input.win.selection_anchor(), None);
        assert_eq!(input.buf.source(), "hi");
    }

    #[test]
    fn undo_clears_anchor_when_replacing_with_shorter_source() {
        let mut input = Harness::new();
        // Save undo with empty buffer.
        input.state.save_undo(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        // Type some text and create a selection.
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(11);
        input.win.set_selection_anchor(Some(6));
        // Undo back to empty.
        input.state.undo(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert_eq!(input.buf.source(), "");
        assert_eq!(
            input.win.selection_anchor(),
            None,
            "undo must clear anchor that no longer fits the restored source"
        );
    }

    #[test]
    fn install_source_snaps_cpos_to_char_boundary() {
        let mut input = Harness::new();
        input.state.install_source(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "héllo".to_string(),
            2,
            Vec::new(),
        ); // mid 'é'
        assert_eq!(input.win.cpos(), 1, "cpos must snap to a char boundary");
    }

    #[test]
    fn backspace_deletes_whole_utf8_codepoint() {
        let mut input = Harness::new();
        input.buf.set_source("aéb".to_string());
        input.win.set_cpos("aé".len());

        input.state.backspace(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });

        assert_eq!(input.buf.source(), "ab");
        assert_eq!(input.win.cpos(), 1);
    }

    #[test]
    fn forward_delete_deletes_whole_utf8_codepoint() {
        let mut input = Harness::new();
        input.buf.set_source("aéb".to_string());
        input.win.set_cpos(1);

        input.state.delete_char_forward(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });

        assert_eq!(input.buf.source(), "ab");
        assert_eq!(input.win.cpos(), 1);
    }

    #[test]
    fn paste_normalizes_crlf_and_strips_attachment_markers() {
        let mut input = Harness::new();
        let data = format!("a\r\nb\rc{m}", m = ATTACHMENT_MARKER);

        input.state.insert_paste(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            data,
        );

        assert_eq!(input.buf.source(), "a\nb\nc");
        assert!(input.buf.attachment_ids.is_empty());
        assert_eq!(input.win.cpos(), input.buf.source().len());
    }

    #[test]
    fn line_motion_clamps_to_existing_lines() {
        let mut input = Harness::new();
        input.buf.set_source("one\ntwo\nthree".to_string());
        input.win.set_cpos(0);

        input.state.scroll_lines(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            99,
        );
        assert_eq!(input.win.cpos(), "one\ntwo\n".len());

        input.state.scroll_lines(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            -99,
        );
        assert_eq!(input.win.cpos(), 0);
    }
}
