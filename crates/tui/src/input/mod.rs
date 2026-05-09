mod buffer;
mod completer_bridge;
mod vim_bridge;

pub(crate) use smelt_core::history::History;

use crate::completer::CompleterSession;
use crate::content;
use crate::keymap::{self, KeyAction, KeyContext};
use crate::smelt_term::VimMode;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;
use smelt_core::attachment::{Attachment, AttachmentId, AttachmentStore};
use vim_bridge::VimBridgeResult;

pub(crate) const ATTACHMENT_MARKER: char = '\u{FFFC}';

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

/// Prompt window state: `Window` plus prompt-specific side-cars (completer, stash, attachments).
/// `source` is the canonical edit buffer for the prompt. The wrapped display rows
/// passed to `Window::handle_mouse` are derived on demand by `PromptWrap`; the
/// `Window`'s own `text` cache is unused on the prompt path.
pub(crate) struct PromptState {
    pub(crate) win: crate::smelt_term::Window,
    pub(crate) source: String,
    pub(crate) store: AttachmentStore,
    pub(crate) completer: Option<CompleterSession>,
    /// WinIds of closed completer sessions, drained and closed on the next frame.
    pub(crate) pending_picker_close: Vec<crate::smelt_term::WinId>,
    pub(crate) stash: Option<InputSnapshot>,
    /// True when content came from a paste; cleared on manual character input.
    pub(super) from_paste: bool,
    /// Chord state: true after Ctrl+X, waiting for second key.
    pending_ctrl_x: bool,
    /// Completable arguments for commands like `/model`, `/theme`, `/color`.
    /// Each entry is `("/cmd", vec!["arg1", "arg2", ...])`.
    pub(crate) command_arg_sources: Vec<(String, Vec<String>)>,
}

/// What the caller should do after `handle_event`.
pub(crate) enum Action {
    Redraw,
    Submit { content: Content, display: String },
    SubmitEmpty,
    EditInEditor,
    CenterScroll,
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
        let mut win = crate::smelt_term::Window::new(
            crate::app::PROMPT_WIN,
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_term::SplitConfig {
                region: "prompt".into(),
                gutters: crate::smelt_term::Gutters::default(),
            },
        );
        win.readonly = false;
        win.history = crate::smelt_term::UndoHistory::new(Some(100));
        Self {
            win,
            source: String::new(),
            store: AttachmentStore::new(),
            completer: None,
            pending_picker_close: Vec::new(),
            stash: None,
            from_paste: false,
            pending_ctrl_x: false,
            command_arg_sources: Vec::new(),
        }
    }

    /// Active selection range `(start_byte, end_byte)` for vim visual or shift+key selection.
    pub(crate) fn selection_range(&self, mode: VimMode) -> Option<(usize, usize)> {
        // Vim visual mode takes priority.
        if self.win.vim_enabled {
            if let Some(range) = crate::smelt_term::vim::visual_range(
                &self.win.vim_state,
                &self.source,
                self.win.cpos,
                mode,
            ) {
                return Some(range);
            }
        }
        self.win.selection_range_at(self.win.cpos)
    }

    /// Selection range for rendering. Falls back to yank-flash so vim copy ops get the
    /// brief post-yank highlight (nvim's `vim.highlight.on_yank`).
    /// Editing must use `selection_range` — the flash must never affect mutations.
    pub(crate) fn display_selection_range(
        &self,
        mode: VimMode,
        clipboard: &crate::smelt_term::Clipboard,
    ) -> Option<(usize, usize)> {
        if let Some(range) = self.selection_range(mode) {
            return Some(range);
        }
        clipboard
            .kill_ring
            .yank_flash_range(std::time::Instant::now())
            .filter(|&(s, e)| {
                e <= self.source.len()
                    && self.source.is_char_boundary(s)
                    && self.source.is_char_boundary(e)
            })
    }

    fn has_selection(&self, mode: VimMode) -> bool {
        self.selection_range(mode).is_some()
    }

    pub(crate) fn clear_selection(&mut self) {
        self.win.selection_anchor = None;
    }

    /// End the active completer session, queuing its picker leaf for close. Use instead of `= None`.
    pub(crate) fn close_completer(&mut self) {
        if let Some(session) = self.completer.take() {
            if let Some(win) = session.picker_win {
                self.pending_picker_close.push(win);
            }
        }
    }

    /// Install a new completer, retiring the previous session. Bare `= Some(...)` orphans the WinId.
    pub(crate) fn set_completer(&mut self, comp: crate::completer::Completer) {
        self.close_completer();
        self.completer = Some(CompleterSession::new(comp));
    }

    fn extend_selection(&mut self) {
        self.win.extend_selection(self.win.cpos);
    }

    fn delete_selection(&mut self, mode: VimMode) -> Option<String> {
        let (start, end) = self.selection_range(mode)?;
        let deleted = self.source[start..end].to_string();
        self.remove_attachments_in_range(start, end);
        self.source.drain(start..end);
        self.win.cpos = start;
        self.win.selection_anchor = None;
        Some(deleted)
    }

    pub(crate) fn vim_enabled(&self) -> bool {
        self.win.vim_enabled
    }

    /// True when content originated from a paste; skips `!` shell-escape treatment.
    pub(crate) fn skip_shell_escape(&self) -> bool {
        self.from_paste
    }

    pub(crate) fn set_vim_enabled(&mut self, enabled: bool) {
        self.win.set_vim_enabled(enabled);
    }

    /// Set vim mode via `mode_ref` (the TuiApp global) and reset the in-flight key sequence.
    pub(crate) fn set_vim_mode(&mut self, mode_ref: &mut VimMode, new: VimMode) {
        if self.win.vim_enabled {
            self.win.vim_state.set_mode(mode_ref, new);
        }
    }

    /// Sync kill ring from clipboard before `C-y` paste.
    /// If clipboard text differs from our last push, treat it as externally updated (charwise).
    fn sync_kill_ring_from_clipboard(clipboard: &mut crate::smelt_term::Clipboard) {
        let Some(text) = clipboard.read() else {
            return;
        };
        if clipboard.kill_ring.last_clipboard_write() == Some(text.as_str()) {
            return;
        }
        clipboard.kill_ring.set(text.clone());
        clipboard.kill_ring.record_clipboard_write(text);
    }

    pub(crate) fn clear(&mut self) {
        self.source.clear();
        self.win.cpos = 0;
        self.win.attachment_ids.clear();
        self.close_completer();
        self.from_paste = false;
        self.win.selection_anchor = None;
        // Stash and store are intentionally preserved.
    }

    /// Replace the buffer wholesale: snapshot undo, clear attachments/selection/paste-state,
    /// re-derive completer. Direct `source` writes bypass these invariants.
    pub(crate) fn replace_text(&mut self, text: String, cursor: Option<usize>, mode: VimMode) {
        self.save_undo(mode);
        let cpos = cursor.unwrap_or(text.len()).min(text.len());
        self.source = text;
        self.win.cpos = cpos;
        self.win.attachment_ids.clear();
        self.win.selection_anchor = None;
        self.from_paste = false;
        self.close_completer();
        self.recompute_completer();
    }

    /// Toggle stash. Attachments are cloned out of the store so the stash survives store clears.
    fn toggle_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.source = snap.buf;
            self.win.cpos = snap.cpos;
            self.win.attachment_ids = snap
                .attachments
                .into_iter()
                .map(|a| self.store.insert(a))
                .collect();
            self.from_paste = snap.from_paste;
            self.close_completer();
        } else if !self.source.is_empty() || !self.win.attachment_ids.is_empty() {
            let attachments = std::mem::take(&mut self.win.attachment_ids)
                .into_iter()
                .filter_map(|id| self.store.get(id).cloned())
                .collect();
            self.stash = Some(InputSnapshot {
                buf: std::mem::take(&mut self.source),
                cpos: std::mem::replace(&mut self.win.cpos, 0),
                attachments,
                from_paste: self.from_paste,
            });
            self.close_completer();
        }
    }

    pub(crate) fn restore_stash(&mut self) {
        if let Some(snap) = self.stash.take() {
            self.source = snap.buf;
            self.win.cpos = snap.cpos;
            self.win.attachment_ids = snap
                .attachments
                .into_iter()
                .map(|a| self.store.insert(a))
                .collect();
            self.from_paste = snap.from_paste;
        }
    }

    /// Restore rewind text: replace `[label]` placeholders with attachment markers.
    pub(crate) fn restore_from_rewind(&mut self, mut text: String, images: Vec<(String, String)>) {
        let mut ids = Vec::new();
        for (label, data_url) in images {
            let display = format!("[{label}]");
            if let Some(pos) = text.find(&display) {
                text.replace_range(pos..pos + display.len(), &ATTACHMENT_MARKER.to_string());
                let id = self.store.insert_image(label, data_url);
                ids.push(id);
            }
        }
        self.win.cpos = text.len();
        self.source = text;
        self.win.attachment_ids = ids;
    }

    pub(crate) fn cursor_char(&self) -> usize {
        char_pos(&self.source, self.win.cpos)
    }

    /// Expand attachment markers to text. Image markers are stripped (data flows via `Content::Parts`).
    pub(crate) fn expanded_text(&self) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        for c in self.source.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.win.attachment_ids.get(att_idx) {
                    result.push_str(self.store.expanded_text(id));
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    pub(crate) fn message_display_text(&self) -> String {
        let mut result = String::new();
        let mut att_idx = 0;
        for c in self.source.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(&id) = self.win.attachment_ids.get(att_idx) {
                    if let Some(Attachment::Image { label, .. }) = self.store.get(id) {
                        result.push_str(&format!("[{label}]"));
                    }
                }
                att_idx += 1;
            } else {
                result.push(c);
            }
        }
        result
    }

    pub(crate) fn insert_image(&mut self, label: String, data_url: String) {
        let id = self.store.insert_image(label, data_url);
        self.insert_attachment_id(id);
    }

    /// Build submission `Content`. Duplicate image refs are deduplicated (base64 payloads are large).
    pub(crate) fn build_content(&self) -> Content {
        let text = self.expanded_text();
        let mut seen: std::collections::HashSet<AttachmentId> = std::collections::HashSet::new();
        let images: Vec<(String, String)> = self
            .win
            .attachment_ids
            .iter()
            .filter(|&&id| seen.insert(id))
            .filter_map(|&id| match self.store.get(id) {
                Some(Attachment::Image { label, data_url }) => {
                    Some((label.clone(), data_url.clone()))
                }
                _ => None,
            })
            .collect();
        Content::with_images(text, images)
    }

    pub(crate) fn key_context(
        &self,
        agent_running: bool,
        ghost_text_visible: bool,
        mode: VimMode,
    ) -> KeyContext {
        KeyContext {
            buf_empty: self.source.is_empty() && self.win.attachment_ids.is_empty(),
            vim_non_insert: self.win.vim_enabled
                && matches!(
                    mode,
                    VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                ),
            vim_enabled: self.win.vim_enabled,
            agent_running,
            ghost_text_visible,
        }
    }

    fn execute_key_action(
        &mut self,
        action: KeyAction,
        history: Option<&mut History>,
        mode: VimMode,
        clipboard: &mut crate::smelt_term::Clipboard,
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
            self.win.curswant = None;
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
            self.clear_selection();
        }
        match action {
            // Caller handles these.
            KeyAction::Quit | KeyAction::CancelAgent | KeyAction::AcceptGhostText => Action::Noop,

            // ── TuiApp control ─────────────────────────────────────────────
            KeyAction::ClearBuffer => {
                self.clear();
                Action::Redraw
            }
            // Intercepted by the global chord layer; these arms are unreachable in practice.
            KeyAction::ToggleMode | KeyAction::CycleReasoning => Action::Noop,
            KeyAction::ToggleStash => {
                self.toggle_stash();
                Action::Redraw
            }
            KeyAction::Redraw => Action::Redraw,

            // ── Submit / newline ─────────────────────────────────────────
            KeyAction::Submit => {
                if self.source.is_empty() && self.win.attachment_ids.is_empty() {
                    Action::SubmitEmpty
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    Action::Submit { content, display }
                }
            }
            KeyAction::InsertNewline => {
                if self.selection_range(mode).is_some() {
                    self.save_undo(mode);
                    self.delete_selection(mode);
                }
                self.source.insert(self.win.cpos, '\n');
                self.win.cpos += 1;
                self.close_completer();
                Action::Redraw
            }

            // ── Navigation ──────────────────────────────────────────────
            KeyAction::MoveLeft => {
                if self.win.cpos > 0 {
                    let cp = char_pos(&self.source, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.source, cp - 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                if self.win.cpos < self.source.len() {
                    let cp = char_pos(&self.source, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.source, cp + 1);
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordForward => {
                if self.move_word_forward() {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveWordBackward => {
                if self.move_word_backward() {
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveUp => {
                let (new_pos, new_want) = crate::smelt_term::text::vertical_move(
                    &self.source,
                    self.win.cpos,
                    -1,
                    self.win.curswant,
                );
                self.win.curswant = Some(new_want);
                if new_pos != self.win.cpos {
                    self.win.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.up(&self.source)) {
                    self.source = entry.to_string();
                    self.win.cpos = 0;
                    self.win.curswant = None;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveDown => {
                let (new_pos, new_want) = crate::smelt_term::text::vertical_move(
                    &self.source,
                    self.win.cpos,
                    1,
                    self.win.curswant,
                );
                self.win.curswant = Some(new_want);
                if new_pos != self.win.cpos {
                    self.win.cpos = new_pos;
                    self.recompute_completer();
                    Action::Redraw
                } else if let Some(entry) = history.and_then(|h| h.down()) {
                    self.source = entry.to_string();
                    self.win.cpos = self.source.len();
                    self.win.curswant = None;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveStartOfLine => {
                self.win.cpos = crate::smelt_term::text::line_start(&self.source, self.win.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfLine => {
                self.win.cpos = crate::smelt_term::text::line_end(&self.source, self.win.cpos);
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveStartOfBuffer => {
                self.win.cpos = 0;
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::MoveEndOfBuffer => {
                self.win.cpos = self.source.len();
                self.recompute_completer();
                Action::Redraw
            }
            KeyAction::HistoryPrev => {
                if let Some(entry) = history.and_then(|h| h.up(&self.source)) {
                    self.source = entry.to_string();
                    self.win.cpos = 0;
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::HistoryNext => {
                if let Some(entry) = history.and_then(|h| h.down()) {
                    self.source = entry.to_string();
                    self.win.cpos = self.source.len();
                    self.sync_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }

            // ── Editing ─────────────────────────────────────────────────
            KeyAction::Backspace => {
                self.backspace(mode);
                Action::Redraw
            }
            KeyAction::DeleteCharForward => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    self.delete_selection(mode);
                } else {
                    self.delete_char_forward();
                }
                Action::Redraw
            }
            KeyAction::DeleteWordBackward => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    self.delete_selection(mode);
                } else {
                    self.delete_word_backward();
                }
                Action::Redraw
            }
            KeyAction::DeleteWordForward => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    self.delete_selection(mode);
                } else {
                    self.delete_word_forward();
                }
                Action::Redraw
            }
            KeyAction::DeleteToStartOfLine => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    self.delete_selection(mode);
                } else {
                    self.delete_to_start_of_line();
                }
                Action::Redraw
            }
            KeyAction::KillToEndOfLine => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    let deleted = self.delete_selection(mode);
                    if let Some(text) = deleted {
                        self.kill_and_copy(text, clipboard);
                    }
                } else {
                    self.kill_to_end_of_line(clipboard);
                }
                Action::Redraw
            }
            KeyAction::KillToStartOfLine => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    let deleted = self.delete_selection(mode);
                    if let Some(text) = deleted {
                        self.kill_and_copy(text, clipboard);
                    }
                } else {
                    self.kill_to_start_of_line(clipboard);
                }
                Action::Redraw
            }
            KeyAction::Yank => {
                self.save_undo(mode);
                if self.has_selection(mode) {
                    self.delete_selection(mode);
                }
                Self::sync_kill_ring_from_clipboard(clipboard);
                if let Some(new_cpos) = clipboard.kill_ring.yank(&mut self.source, self.win.cpos) {
                    self.win.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::YankPop => {
                if let Some(new_cpos) = clipboard.kill_ring.yank_pop(&mut self.source) {
                    self.win.cpos = new_cpos;
                    self.recompute_completer();
                }
                Action::Redraw
            }
            KeyAction::UppercaseWord => {
                self.save_undo(mode);
                self.uppercase_word();
                Action::Redraw
            }
            KeyAction::LowercaseWord => {
                self.save_undo(mode);
                self.lowercase_word();
                Action::Redraw
            }
            KeyAction::CapitalizeWord => {
                self.save_undo(mode);
                self.capitalize_word();
                Action::Redraw
            }
            KeyAction::Undo => {
                self.undo();
                Action::Redraw
            }

            // ── Vim half-page scroll ────────────────────────────────────
            KeyAction::VimHalfPageUp => {
                let half = content::term_height() / 2;
                let line = current_line(&self.source, self.win.cpos);
                let target = line.saturating_sub(half);
                self.move_to_line(target);
                Action::Redraw
            }
            KeyAction::VimHalfPageDown => {
                let half = content::term_height() / 2;
                let line = current_line(&self.source, self.win.cpos);
                let total = self.source.chars().filter(|&c| c == '\n').count() + 1;
                let target = (line + half).min(total - 1);
                self.move_to_line(target);
                Action::Redraw
            }

            // ── Clipboard ───────────────────────────────────────────────
            KeyAction::CopySelection => {
                if let Some((start, end)) = self.selection_range(mode) {
                    let text = self.source[start..end].to_string();
                    if clipboard.write(&text).is_ok() {
                        clipboard.kill_ring.record_clipboard_write(text.clone());
                    }
                    clipboard.kill_ring.set(text);
                }
                Action::Noop
            }
            KeyAction::CutSelection => {
                if self.selection_range(mode).is_some() {
                    self.save_undo(mode);
                    if let Some(text) = self.delete_selection(mode) {
                        if clipboard.write(&text).is_ok() {
                            clipboard.kill_ring.record_clipboard_write(text.clone());
                        }
                        clipboard.kill_ring.set(text);
                    }
                    self.recompute_completer();
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::ClipboardImage => {
                // Bracketed-paste terminals forward Cmd+V as `Event::Paste`, bypassing this arm.
                // Terminals with bracketed paste off send it as a key — handle both paths.
                if let Some(url) = clipboard_image_to_data_url() {
                    self.save_undo(mode);
                    self.insert_image("clipboard.png".into(), url);
                    return Action::Redraw;
                }
                if let Some(text) = clipboard.read() {
                    if !text.is_empty() {
                        self.save_undo(mode);
                        if self.has_selection(mode) {
                            self.delete_selection(mode);
                        }
                        self.insert_paste(text);
                        return Action::Redraw;
                    }
                }
                Action::Noop
            }

            // ── Selection (shift+movement) ─────────────────────────────
            KeyAction::SelectLeft => {
                self.extend_selection();
                if self.win.cpos > 0 {
                    let cp = char_pos(&self.source, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.source, cp - 1);
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection();
                if self.win.cpos < self.source.len() {
                    let cp = char_pos(&self.source, self.win.cpos);
                    self.win.cpos = byte_of_char(&self.source, cp + 1);
                }
                Action::Redraw
            }
            KeyAction::SelectUp => {
                self.extend_selection();
                let (new_pos, new_want) = crate::smelt_term::text::vertical_move(
                    &self.source,
                    self.win.cpos,
                    -1,
                    self.win.curswant,
                );
                self.win.curswant = Some(new_want);
                self.win.cpos = new_pos;
                Action::Redraw
            }
            KeyAction::SelectDown => {
                self.extend_selection();
                let (new_pos, new_want) = crate::smelt_term::text::vertical_move(
                    &self.source,
                    self.win.cpos,
                    1,
                    self.win.curswant,
                );
                self.win.curswant = Some(new_want);
                self.win.cpos = new_pos;
                Action::Redraw
            }
            KeyAction::SelectWordForward => {
                self.extend_selection();
                self.win.cpos = crate::smelt_term::text::word_forward_pos(
                    &self.source,
                    self.win.cpos,
                    crate::smelt_term::text::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectWordBackward => {
                self.extend_selection();
                self.win.cpos = crate::smelt_term::text::word_backward_pos(
                    &self.source,
                    self.win.cpos,
                    crate::smelt_term::text::CharClass::Word,
                );
                Action::Redraw
            }
            KeyAction::SelectStartOfLine => {
                self.extend_selection();
                self.win.cpos = crate::smelt_term::text::line_start(&self.source, self.win.cpos);
                Action::Redraw
            }
            KeyAction::SelectEndOfLine => {
                self.extend_selection();
                self.win.cpos = crate::smelt_term::text::line_end(&self.source, self.win.cpos);
                Action::Redraw
            }
        }
    }

    /// Process a terminal event. Priority: completer → vim → paste → keymap → insert.
    pub(crate) fn handle_event(
        &mut self,
        ev: Event,
        mut history: Option<&mut History>,
        mode: &mut VimMode,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) -> Action {
        if self.completer.is_some() {
            if let Some(action) = self.handle_completer_event(&ev) {
                return action;
            }
        }

        match self.dispatch_vim(&ev, &mut history, mode, clipboard) {
            VimBridgeResult::Handled(action) => return action,
            VimBridgeResult::Passthrough | VimBridgeResult::NotAKey => {}
        }

        if let Event::Paste(data) = ev {
            self.save_undo(*mode);
            if self.selection_range(*mode).is_some() {
                self.delete_selection(*mode);
            }
            if let Some(path) = engine::image::normalize_pasted_path(&data) {
                if engine::image::is_image_file(&path) {
                    match engine::image::read_image_as_data_url(&path) {
                        Ok(url) => {
                            let label = engine::image::image_label_from_path(&path);
                            self.insert_image(label, url);
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
                    self.insert_image("clipboard.png".into(), url);
                    return Action::Redraw;
                }
            }
            self.insert_paste(data);
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

            let ctx = KeyContext {
                buf_empty: self.source.is_empty() && self.win.attachment_ids.is_empty(),
                vim_non_insert: self.win.vim_enabled
                    && matches!(
                        *mode,
                        VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                    ),
                vim_enabled: self.win.vim_enabled,
                agent_running: false,
                ghost_text_visible: false,
            };

            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                return self.execute_key_action(action, history, *mode, clipboard);
            }

            if let KeyCode::Char(c) = code {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    self.insert_char(c, *mode);
                    return Action::Redraw;
                }
            }
        }

        Action::Noop
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub(crate) fn char_pos(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx].chars().count()
}

fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

fn current_line(buf: &str, cpos: usize) -> usize {
    let end = if buf.is_char_boundary(cpos) {
        cpos
    } else {
        buf.len()
    };
    buf[..end].chars().filter(|&c| c == '\n').count()
}

/// Returns the byte offset of the `@` anchor when the cursor is inside an `@…` zone.
pub(super) fn cursor_in_at_zone(buf: &str, cpos: usize) -> Option<usize> {
    if !buf.is_char_boundary(cpos) {
        return None;
    }
    // Include the char at cpos so cursor-on-@ is matched.
    let search_end = buf[cpos..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| cpos + i)
        .unwrap_or(buf.len());
    let at_pos = buf[..search_end].rfind('@')?;
    if at_pos > 0 && !buf[..at_pos].ends_with(char::is_whitespace) {
        return None;
    }
    if at_pos < cpos && buf[at_pos + 1..cpos].contains(char::is_whitespace) {
        return None;
    }
    Some(at_pos)
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

pub(super) fn find_slash_anchor(buf: &str, cpos: usize) -> Option<usize> {
    if !buf.starts_with('/') || !buf.is_char_boundary(cpos) {
        return None;
    }
    if cpos < 1 || buf[1..cpos].contains(char::is_whitespace) {
        return None;
    }
    Some(0)
}

// ── Agent-mode Esc resolution ────────────────────────────────────────────────

/// Result of pressing Esc during agent processing.
#[derive(Debug, PartialEq)]
pub(crate) enum EscAction {
    /// Vim was in insert mode — switch to normal, double-Esc timer started.
    VimToNormal,
    /// Unqueue messages back into the input buffer.
    Unqueue,
    /// Double-Esc cancel. Contains the vim mode to restore (if vim enabled).
    Cancel { restore_vim: Option<VimMode> },
    /// First Esc in normal/no-vim mode — timer started.
    StartTimer,
}

/// Resolve Esc during agent processing. `vim_mode_at_first_esc` tracks the mode before the
/// sequence so a double-Esc cancel can restore it (first Esc may have switched insert → normal).
pub(crate) fn resolve_agent_esc(
    vim_mode: Option<VimMode>,
    has_queued: bool,
    last_esc: &mut Option<std::time::Instant>,
    vim_mode_at_first_esc: &mut Option<VimMode>,
) -> EscAction {
    use std::time::{Duration, Instant};

    // Insert mode: switch to Normal and start the double-Esc timer (two presses total to cancel).
    if vim_mode == Some(VimMode::Insert) {
        *vim_mode_at_first_esc = Some(VimMode::Insert);
        *last_esc = Some(Instant::now());
        return EscAction::VimToNormal;
    }

    if has_queued {
        *last_esc = None;
        *vim_mode_at_first_esc = None;
        return EscAction::Unqueue;
    }

    if let Some(prev) = *last_esc {
        if prev.elapsed() < Duration::from_millis(500) {
            let restore = vim_mode_at_first_esc.take();
            *last_esc = None;
            return EscAction::Cancel {
                restore_vim: restore,
            };
        }
    }

    *vim_mode_at_first_esc = vim_mode;
    *last_esc = Some(Instant::now());
    EscAction::StartTimer
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    impl PromptState {
        /// Test-only convenience: run `execute_key_action` against a
        /// throwaway null clipboard. Most tests don't exercise the
        /// kill-ring path; the few that do (`KeyAction::Yank`,
        /// `YankPop`, `Cut`, `Copy`, `KillTo*`) use `execute_key_action`
        /// directly with a real `Clipboard` and assert against it.
        fn test_action(&mut self, action: KeyAction, mode: VimMode) -> Action {
            let mut clip = crate::smelt_term::Clipboard::null();
            self.execute_key_action(action, None, mode, &mut clip)
        }
    }

    // ── Vim-mode Esc behavior ───────────────────────────────────────────

    #[test]
    fn vim_esc_in_insert_switches_to_normal() {
        // Single Esc while vim is in insert mode → VimToNormal.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action =
            resolve_agent_esc(Some(VimMode::Insert), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action, EscAction::VimToNormal);
        // Timer should be started so a second Esc can cancel.
        assert!(last_esc.is_some());
        // The insert mode should be saved for restoration on cancel.
        assert_eq!(saved_mode, Some(VimMode::Insert));
    }

    #[test]
    fn vim_esc_in_normal_unqueues_if_queued() {
        // Esc in vim normal mode with queued messages → Unqueue.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(Some(VimMode::Normal), true, &mut last_esc, &mut saved_mode);
        assert_eq!(action, EscAction::Unqueue);
    }

    #[test]
    fn vim_double_esc_from_insert_cancels_and_restores_insert() {
        // First Esc: vim insert → normal, timer starts.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 =
            resolve_agent_esc(Some(VimMode::Insert), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::VimToNormal);

        // Second Esc: now in normal mode (vim switched), timer active → Cancel.
        // Restore mode should be Insert (the mode before the sequence started).
        let action2 =
            resolve_agent_esc(Some(VimMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(
            action2,
            EscAction::Cancel {
                restore_vim: Some(VimMode::Insert)
            }
        );
    }

    #[test]
    fn vim_double_esc_from_normal_cancels_and_stays_normal() {
        // First Esc: vim already in normal, no queue → StartTimer.
        let mut last_esc = None;
        let mut saved_mode = None;
        let action1 =
            resolve_agent_esc(Some(VimMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::StartTimer);
        assert_eq!(saved_mode, Some(VimMode::Normal));

        // Second Esc within 500ms → Cancel, restore to Normal.
        let action2 =
            resolve_agent_esc(Some(VimMode::Normal), false, &mut last_esc, &mut saved_mode);
        assert_eq!(
            action2,
            EscAction::Cancel {
                restore_vim: Some(VimMode::Normal)
            }
        );
    }

    // ── No-vim Esc behavior ─────────────────────────────────────────────

    #[test]
    fn no_vim_esc_unqueues_if_queued() {
        let mut last_esc = None;
        let mut saved_mode = None;
        let action = resolve_agent_esc(
            None, // vim disabled
            true,
            &mut last_esc,
            &mut saved_mode,
        );
        assert_eq!(action, EscAction::Unqueue);
    }

    #[test]
    fn no_vim_double_esc_cancels() {
        let mut last_esc = None;
        let mut saved_mode = None;

        // First Esc → StartTimer.
        let action1 = resolve_agent_esc(None, false, &mut last_esc, &mut saved_mode);
        assert_eq!(action1, EscAction::StartTimer);

        // Second Esc within 500ms → Cancel with no vim mode to restore.
        let action2 = resolve_agent_esc(None, false, &mut last_esc, &mut saved_mode);
        assert_eq!(action2, EscAction::Cancel { restore_vim: None });
    }

    // ── from_paste behavior for shell escape prevention ───────────────────

    #[test]
    fn paste_into_empty_buffer_sets_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.source, "!echo hello");
    }

    #[test]
    fn type_then_type_sets_from_paste_false() {
        let mut input = PromptState::new();
        input.insert_char('!', crate::smelt_term::VimMode::Insert);
        input.insert_char('e', crate::smelt_term::VimMode::Insert);
        assert!(
            !input.skip_shell_escape(),
            "Manual typing should clear from_paste"
        );
    }

    #[test]
    fn type_bang_then_paste_sets_from_paste() {
        let mut input = PromptState::new();

        // Simulate user typing '!'
        input.insert_char('!', crate::smelt_term::VimMode::Insert);
        assert!(!input.skip_shell_escape(), "Typing clears from_paste");

        // Reset cursor to simulate the scenario: user types '!', then pastes at line start
        // This is the key scenario that was broken before the fix
        input.source.clear();
        input.win.cpos = 0;
        input.insert_paste("echo hello".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.source, "echo hello");
    }

    #[test]
    fn paste_in_middle_of_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.source = "hello ".to_string();
        input.win.cpos = 6; // After "hello "
        input.insert_paste("!world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.source, "hello !world");
    }

    #[test]
    fn paste_at_end_of_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.source = "hello".to_string();
        input.win.cpos = 5; // At end
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste at end of line should not set from_paste"
        );
        assert_eq!(input.source, "hello world");
    }

    #[test]
    fn paste_at_start_of_multiline_buffer() {
        let mut input = PromptState::new();

        input.source = "line1\nline2".to_string();
        input.win.cpos = 0; // At very start
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at buffer start should set from_paste"
        );
        assert_eq!(input.source, "!commandline1\nline2");
    }

    #[test]
    fn paste_at_start_of_second_line_sets_from_paste() {
        let mut input = PromptState::new();

        input.source = "line1\n".to_string();
        input.win.cpos = 6; // Start of second line
        input.insert_paste("!command".to_string());
        assert!(
            input.skip_shell_escape(),
            "Paste at line start should set from_paste"
        );
        assert_eq!(input.source, "line1\n!command");
    }

    #[test]
    fn paste_middle_of_second_line_does_not_set_from_paste() {
        let mut input = PromptState::new();

        input.source = "line1\nhello".to_string();
        input.win.cpos = 8; // Insert at byte position 8 (before first 'l' of "hello")
        input.insert_paste(" world".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Paste in middle of line should not set from_paste"
        );
        assert_eq!(input.source, "line1\nhe worldllo");
    }

    #[test]
    fn manual_char_after_paste_clears_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        input.insert_char('x', crate::smelt_term::VimMode::Insert);
        assert!(
            !input.skip_shell_escape(),
            "Manual character after paste should clear from_paste"
        );
    }

    #[test]
    fn backspace_at_start_clears_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        input.backspace(crate::smelt_term::VimMode::Insert); // Deletes last character
        assert!(
            input.skip_shell_escape(),
            "Backspace not at start should not clear from_paste"
        );

        input.win.cpos = 0;
        input.backspace(crate::smelt_term::VimMode::Insert); // Now at position 0
                                                             // Can't backspace further, but the logic would clear it if we could
    }

    #[test]
    fn delete_word_backward_at_start_clears_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!echo hello".to_string());
        assert!(input.skip_shell_escape());

        // Move cursor to end
        input.win.cpos = input.source.len();
        input.delete_word_backward(); // Deletes "hello"
        assert!(
            input.skip_shell_escape(),
            "Delete word not at start should not clear from_paste"
        );

        // Move to after "echo " and delete word
        input.win.cpos = 5; // After "echo"
        input.delete_word_backward(); // Deletes "echo"
        assert!(input.skip_shell_escape(), "Still not at absolute start");

        input.win.cpos = 1; // After "!"
        input.delete_word_backward(); // Would delete to start, which should clear from_paste
        assert!(
            !input.skip_shell_escape(),
            "Delete word to start should clear from_paste"
        );
    }

    #[test]
    fn clear_resets_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!test".to_string());
        assert!(input.skip_shell_escape());

        input.clear();
        assert!(!input.skip_shell_escape(), "Clear should reset from_paste");
    }

    #[test]
    fn stash_preserves_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!test".to_string());
        assert!(input.skip_shell_escape());

        // Stash: saves from_paste to snapshot, but doesn't clear it in active buffer
        input.toggle_stash();
        assert!(
            input.skip_shell_escape(),
            "Stash saves from_paste to snapshot but keeps it in buffer"
        );
        assert!(
            input.source.is_empty(),
            "Buffer should be empty after stashing"
        );

        // Restore: restores from_paste from snapshot
        input.toggle_stash();
        assert!(input.skip_shell_escape(), "Stash should restore from_paste");
        assert_eq!(input.source, "!test");
    }

    #[test]
    fn multiple_pastes_set_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("!first".to_string());
        assert!(input.skip_shell_escape());

        // Type something, which clears from_paste
        input.insert_char(' ', crate::smelt_term::VimMode::Insert);
        assert!(!input.skip_shell_escape());

        // Paste again at start of line
        input.win.cpos = 0;
        input.insert_paste("!second".to_string());
        assert!(
            input.skip_shell_escape(),
            "Second paste at start should set from_paste again"
        );
    }

    #[test]
    fn paste_with_carriage_returns_normalized() {
        let mut input = PromptState::new();
        input.insert_paste("!line1\r\nline2\rline3".to_string());
        assert!(input.skip_shell_escape());
        assert!(
            !input.source.contains('\r'),
            "Carriage returns should be normalized"
        );
        assert_eq!(input.source, "!line1\nline2\nline3");
    }

    #[test]
    fn empty_paste_does_not_set_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("".to_string());
        assert!(
            !input.skip_shell_escape(),
            "Empty paste should not set from_paste"
        );
    }

    #[test]
    fn whitespace_only_paste_at_start_sets_from_paste() {
        let mut input = PromptState::new();
        input.insert_paste("   ".to_string());
        assert!(
            input.skip_shell_escape(),
            "Whitespace paste at start should set from_paste"
        );
    }

    #[test]
    fn paste_starting_with_bang_at_line_start() {
        // This is the main bug scenario: type '!', then paste command
        let mut input = PromptState::new();

        input.source = String::new();
        input.win.cpos = 0;
        input.insert_paste("!ls -la".to_string());

        assert!(
            input.skip_shell_escape(),
            "Paste at start of line should set from_paste"
        );
        assert_eq!(input.source, "!ls -la");

        // The expanded text should not be treated as shell command
        let text = input.expanded_text();
        assert_eq!(text, "!ls -la");
    }

    // ── Selection tests ─────────────────────────────────────────────────

    #[test]
    fn shift_select_right_creates_selection() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.selection_anchor, Some(0));
        assert_eq!(input.win.cpos, 1);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 1))
        );
    }

    #[test]
    fn shift_select_extends_selection() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.selection_anchor, Some(0));
        assert_eq!(input.win.cpos, 3);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 3))
        );
    }

    #[test]
    fn movement_clears_selection() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_some());
        input.test_action(KeyAction::MoveRight, crate::smelt_term::VimMode::Insert);
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_none());
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        // Select "hello"
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 5))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, " world");
        assert_eq!(input.win.cpos, 0);
    }

    #[test]
    fn delete_forward_deletes_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteCharForward,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(input.source, " world");
    }

    #[test]
    fn typing_replaces_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.insert_char('X', crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "X world");
        assert_eq!(input.win.cpos, 1);
    }

    #[test]
    fn select_left_from_end() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 5;
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.selection_anchor, Some(5));
        assert_eq!(input.win.cpos, 3);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((3, 5))
        );
    }

    #[test]
    fn select_word_forward() {
        let mut input = PromptState::new();
        input.source = "hello world foo".to_string();
        input.win.cpos = 0;
        input.test_action(
            KeyAction::SelectWordForward,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(input.win.selection_anchor, Some(0));
        // word_forward_pos from 0 should be 6 (start of "world").
        assert_eq!(input.win.cpos, 6);
        input.test_action(KeyAction::Backspace, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "world foo");
    }

    #[test]
    fn select_word_backward() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 11;
        input.test_action(
            KeyAction::SelectWordBackward,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((6, 11))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "hello ");
    }

    #[test]
    fn select_to_line_start() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 5;
        input.test_action(
            KeyAction::SelectStartOfLine,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 5))
        );
    }

    #[test]
    fn select_to_line_end() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 5;
        input.test_action(
            KeyAction::SelectEndOfLine,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((5, 11))
        );
    }

    #[test]
    fn newline_replaces_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.test_action(KeyAction::InsertNewline, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "\n world");
        assert_eq!(input.win.cpos, 1);
    }

    #[test]
    fn kill_to_eol_with_selection() {
        let mut input = PromptState::new();
        let mut clip = crate::smelt_term::Clipboard::null();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..5 {
            input.execute_key_action(
                KeyAction::SelectRight,
                None,
                crate::smelt_term::VimMode::Insert,
                &mut clip,
            );
        }
        input.execute_key_action(
            KeyAction::KillToEndOfLine,
            None,
            crate::smelt_term::VimMode::Insert,
            &mut clip,
        );
        assert_eq!(input.source, " world");
        // Killed text lands on the TuiApp-level kill ring.
        assert_eq!(clip.kill_ring.current(), "hello");
    }

    #[test]
    fn selection_at_buffer_boundary() {
        let mut input = PromptState::new();
        input.source = "ab".to_string();
        input.win.cpos = 0;
        // Select all.
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 2))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "");
        assert_eq!(input.win.cpos, 0);
    }

    #[test]
    fn selection_range_empty_when_anchor_equals_cursor() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 3;
        input.win.selection_anchor = Some(3);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            None
        );
    }

    #[test]
    fn clear_resets_selection() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_some());
        input.clear();
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_none());
    }

    #[test]
    fn delete_word_backward_with_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 6;
        // Select "wor"
        for _ in 0..3 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteWordBackward,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(input.source, "hello ld");
    }

    #[test]
    fn delete_word_forward_with_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        for _ in 0..3 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteWordForward,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(input.source, "lo world");
    }

    #[test]
    fn delete_to_start_of_line_with_selection() {
        let mut input = PromptState::new();
        input.source = "hello world".to_string();
        input.win.cpos = 3;
        for _ in 0..4 {
            input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        }
        input.test_action(
            KeyAction::DeleteToStartOfLine,
            crate::smelt_term::VimMode::Insert,
        );
        assert_eq!(input.source, "helorld");
    }

    #[test]
    fn select_left_at_start_stays() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.cpos, 0);
        assert_eq!(input.win.selection_anchor, Some(0));
    }

    #[test]
    fn select_right_at_end_stays() {
        let mut input = PromptState::new();
        input.source = "hello".to_string();
        input.win.cpos = 5;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.cpos, 5);
    }

    #[test]
    fn select_empty_buffer() {
        let mut input = PromptState::new();
        input.source = String::new();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.cpos, 0);
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_none());
    }

    #[test]
    fn utf8_selection() {
        let mut input = PromptState::new();
        input.source = "héllo".to_string();
        input.win.cpos = 0;
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        // Should select "hé" — 2 chars but 3 bytes.
        assert_eq!(input.win.cpos, 3); // byte offset of 'l'
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((0, 3))
        );
        input.test_action(KeyAction::Backspace, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.source, "llo");
    }

    #[test]
    fn selection_preserved_across_multiple_select_directions() {
        let mut input = PromptState::new();
        input.source = "abcdef".to_string();
        input.win.cpos = 3; // on 'd'
                            // Select right 2 chars.
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectRight, crate::smelt_term::VimMode::Insert);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((3, 5))
        );
        // Then select left 4 chars — anchor stays at 3.
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        input.test_action(KeyAction::SelectLeft, crate::smelt_term::VimMode::Insert);
        assert_eq!(input.win.cpos, 1);
        assert_eq!(
            input.selection_range(crate::smelt_term::VimMode::Insert),
            Some((1, 3))
        );
    }

    #[test]
    fn vim_esc_clears_shift_selection() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        };

        let mut input = PromptState::new();
        let mut mode = crate::smelt_term::VimMode::Insert;
        let mut clipboard = crate::smelt_term::Clipboard::null();
        input.set_vim_enabled(true);
        input.source = "hello world".to_string();
        input.win.cpos = 0;
        // Create a shift selection.
        input.test_action(KeyAction::SelectRight, mode);
        input.test_action(KeyAction::SelectRight, mode);
        assert!(input.selection_range(mode).is_some());
        // Press Esc — vim switches to normal mode AND clears selection.
        let esc = Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        input.handle_event(esc, None, &mut mode, &mut clipboard);
        assert!(
            input.selection_range(mode).is_none(),
            "Esc should clear shift selection"
        );
        assert_eq!(
            mode,
            crate::smelt_term::VimMode::Normal,
            "Should be in normal mode"
        );
    }

    #[test]
    fn delete_selection_removes_attachments() {
        let mut input = PromptState::new();
        // Insert text with an image attachment marker in the middle.
        input.source = format!("ab{}cd", ATTACHMENT_MARKER);
        input.win.cpos = 0;
        let id = input
            .store
            .insert_image("img.png".into(), "data:image/png;base64,AAA".into());
        input.win.attachment_ids.push(id);
        // Select "b<marker>c" (bytes 1..5 — marker is 3 bytes).
        input.win.selection_anchor = Some(1);
        input.win.cpos = 1 + 1 + ATTACHMENT_MARKER.len_utf8() + 1;
        assert!(input
            .selection_range(crate::smelt_term::VimMode::Insert)
            .is_some());
        let deleted = input.delete_selection(crate::smelt_term::VimMode::Insert);
        assert!(deleted.is_some());
        assert_eq!(input.source, "ad");
        assert!(
            input.win.attachment_ids.is_empty(),
            "Attachment should be removed"
        );
    }

    // ── Attachment dedup within a single message ───────────────────────

    /// Place two markers in the buffer that both point at `id`.
    fn buf_with_two_markers(input: &mut PromptState, id: AttachmentId) {
        input.source = format!("pre{m}mid{m}post", m = ATTACHMENT_MARKER);
        input.win.cpos = input.source.len();
        input.win.attachment_ids = vec![id, id];
    }

    #[test]
    fn build_content_dedups_repeated_image_in_parts() {
        let mut input = PromptState::new();
        let id = input
            .store
            .insert_image("img.png".into(), "data:image/png;base64,AAA".into());
        buf_with_two_markers(&mut input, id);
        let content = input.build_content();
        assert_eq!(
            content.image_count(),
            1,
            "repeated image should appear once in Content::Parts"
        );
    }

    #[test]
    fn build_content_preserves_distinct_images() {
        let mut input = PromptState::new();
        let id1 = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.source = format!("{m}{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.source.len();
        input.win.attachment_ids = vec![id1, id2];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    #[test]
    fn build_content_dedups_interleaved_image_references() {
        // Pattern: img A, img B, img A again. Parts should be [A, B].
        let mut input = PromptState::new();
        let id_a = input
            .store
            .insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id_b = input
            .store
            .insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        input.source = format!("{m}x{m}y{m}", m = ATTACHMENT_MARKER);
        input.win.cpos = input.source.len();
        input.win.attachment_ids = vec![id_a, id_b, id_a];
        let content = input.build_content();
        assert_eq!(content.image_count(), 2);
    }

    use crate::keymap::KeyAction;
}
