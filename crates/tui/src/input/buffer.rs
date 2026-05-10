//! Low-level buffer editing primitives for `PromptState`.

use super::{PromptState, ATTACHMENT_MARKER};
use crate::smelt_term::VimMode;
use smelt_core::attachment::AttachmentId;

impl PromptState {
    /// Save undo state. Skips during vim Insert — the session entry saved on insert-entry covers it.
    pub(crate) fn save_undo(&mut self) {
        if self.win.vim_enabled && self.win.vim_mode == VimMode::Insert {
            return; // insert session groups all edits into one undo step
        }
        self.win
            .history
            .save(crate::smelt_term::UndoEntry::snapshot(
                &self.source,
                self.win.cpos,
                &self.win.attachment_ids,
            ));
    }

    pub(super) fn insert_char(&mut self, c: char) {
        self.from_paste = false;
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
        }
        self.source.insert(self.win.cpos, c);
        self.win.cpos += c.len_utf8();
        self.recompute_completer();
    }

    pub(super) fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
            self.recompute_completer();
            return;
        }
        if self.win.cpos == 0 {
            return;
        }
        // Deleting the closing `"` of a `"@path"` token removes the whole token.
        if let Some(start) = self.quoted_at_ref_start() {
            if start == 0 {
                self.from_paste = false;
            }
            self.source.drain(start..self.win.cpos);
            self.win.cpos = start;
            self.recompute_completer();
            return;
        }
        let prev = self.source[..self.win.cpos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        if prev == 0 {
            self.from_paste = false;
        }
        self.maybe_remove_attachment(prev);
        self.source.drain(prev..self.win.cpos);
        self.win.cpos = prev;
        self.recompute_completer();
    }

    /// Byte offset of the opening `"` when the cursor is just after the closing `"` of a `"@path"` token.
    fn quoted_at_ref_start(&self) -> Option<usize> {
        let before = &self.source[..self.win.cpos];
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !self.source[..at_pos].ends_with(char::is_whitespace) {
            return None;
        }
        if inner[at_pos + 2..].contains('"') {
            return None;
        }
        Some(at_pos)
    }

    pub(super) fn delete_word_backward(&mut self) {
        if self.win.cpos == 0 {
            return;
        }
        let target = crate::smelt_term::text::word_backward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target == 0 {
            self.from_paste = false;
        }
        self.remove_attachments_in_range(target, self.win.cpos);
        self.source.drain(target..self.win.cpos);
        self.win.cpos = target;
        self.recompute_completer();
    }

    pub(super) fn delete_char_forward(&mut self) {
        if self.win.cpos >= self.source.len() {
            return;
        }
        self.maybe_remove_attachment(self.win.cpos);
        let next = self.source[self.win.cpos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.win.cpos + i)
            .unwrap_or(self.source.len());
        self.source.drain(self.win.cpos..next);
        self.recompute_completer();
    }

    pub(super) fn delete_word_forward(&mut self) {
        if self.win.cpos >= self.source.len() {
            return;
        }
        let target = crate::smelt_term::text::word_forward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        self.remove_attachments_in_range(self.win.cpos, target);
        self.source.drain(self.win.cpos..target);
        self.recompute_completer();
    }

    pub(super) fn kill_to_end_of_line(&mut self, clipboard: &mut crate::smelt_term::Clipboard) {
        let end = self.source[self.win.cpos..]
            .find('\n')
            .map(|i| self.win.cpos + i)
            .unwrap_or(self.source.len());
        let killed = self.source[self.win.cpos..end].to_string();
        self.remove_attachments_in_range(self.win.cpos, end);
        self.source.drain(self.win.cpos..end);
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer();
    }

    pub(super) fn kill_to_start_of_line(&mut self, clipboard: &mut crate::smelt_term::Clipboard) {
        let start = self.source[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let killed = self.source[start..self.win.cpos].to_string();
        self.remove_attachments_in_range(start, self.win.cpos);
        self.source.drain(start..self.win.cpos);
        self.win.cpos = start;
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer();
    }

    pub(super) fn delete_to_start_of_line(&mut self) {
        let start = self.source[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.remove_attachments_in_range(start, self.win.cpos);
        self.source.drain(start..self.win.cpos);
        self.win.cpos = start;
        self.recompute_completer();
    }

    pub(super) fn uppercase_word(&mut self) {
        let end = crate::smelt_term::text::word_forward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let upper: String = self.source[self.win.cpos..end].to_uppercase();
        self.source.replace_range(self.win.cpos..end, &upper);
        self.win.cpos += upper.len();
        self.recompute_completer();
    }

    pub(super) fn lowercase_word(&mut self) {
        let end = crate::smelt_term::text::word_forward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let lower: String = self.source[self.win.cpos..end].to_lowercase();
        self.source.replace_range(self.win.cpos..end, &lower);
        self.win.cpos += lower.len();
        self.recompute_completer();
    }

    pub(super) fn capitalize_word(&mut self) {
        let end = crate::smelt_term::text::word_forward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let word = &self.source[self.win.cpos..end];
        let mut cap = String::with_capacity(word.len());
        let mut first = true;
        for c in word.chars() {
            if first && c.is_alphabetic() {
                cap.extend(c.to_uppercase());
                first = false;
            } else {
                cap.push(c);
            }
        }
        self.source.replace_range(self.win.cpos..end, &cap);
        self.win.cpos += cap.len();
        self.recompute_completer();
    }

    pub(super) fn undo(&mut self) {
        let current = crate::smelt_term::UndoEntry::snapshot(
            &self.source,
            self.win.cpos,
            &self.win.attachment_ids,
        );
        if let Some(entry) = self.win.history.undo(current) {
            self.install_source(entry.buf, entry.cpos);
            self.win.attachment_ids = entry.attachments;
        }
        self.recompute_completer();
    }

    pub(super) fn move_word_forward(&mut self) -> bool {
        if self.win.cpos >= self.source.len() {
            return false;
        }
        let target = crate::smelt_term::text::word_forward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target != self.win.cpos {
            self.win.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    pub(super) fn move_word_backward(&mut self) -> bool {
        if self.win.cpos == 0 {
            return false;
        }
        let target = crate::smelt_term::text::word_backward_pos(
            &self.source,
            self.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target != self.win.cpos {
            self.win.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    pub(super) fn insert_paste(&mut self, data: String) {
        // Normalize `\r\n` and lone `\r` to `\n` (terminals in bracketed-paste mode send `\r`).
        let data = data.replace("\r\n", "\n").replace('\r', "\n");

        if data.is_empty() {
            return;
        }

        // Mark from_paste when inserting at the beginning of the current line
        // so pasted content starting with `!` isn't treated as a shell escape.
        let line_start = self.source[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if self.win.cpos == line_start {
            self.from_paste = true;
        }
        self.source.insert_str(self.win.cpos, &data);
        self.win.cpos += data.len();
    }

    pub(super) fn insert_attachment_id(&mut self, id: AttachmentId) {
        let idx = self.source[..self.win.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        self.win.attachment_ids.insert(idx, id);
        self.source.insert(self.win.cpos, ATTACHMENT_MARKER);
        self.win.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    pub(super) fn remove_attachments_in_range(&mut self, start: usize, end: usize) {
        let before = self.source[..start]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let count = self.source[start..end]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        for i in (0..count).rev() {
            let idx = before + i;
            if idx < self.win.attachment_ids.len() {
                self.win.attachment_ids.remove(idx);
            }
        }
    }

    pub(super) fn maybe_remove_attachment(&mut self, byte_pos: usize) {
        if self.source[byte_pos..].starts_with(ATTACHMENT_MARKER) {
            let idx = self.source[..byte_pos]
                .chars()
                .filter(|&c| c == ATTACHMENT_MARKER)
                .count();
            if idx < self.win.attachment_ids.len() {
                self.win.attachment_ids.remove(idx);
            }
        }
    }

    pub(super) fn move_to_line(&mut self, target_line: usize) {
        let mut line = 0;
        let mut pos = 0;
        for (i, c) in self.source.char_indices() {
            if line == target_line {
                pos = i;
                break;
            }
            if c == '\n' {
                line += 1;
                if line == target_line {
                    pos = i + 1;
                    break;
                }
            }
        }
        if line < target_line {
            // target beyond end, go to last line start
            pos = self.source.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        self.win.cpos = pos;
        self.recompute_completer();
    }

    /// Kill text into the kill ring and copy to clipboard.
    /// Records the write so subsequent pastes can distinguish our push from external updates.
    pub(super) fn kill_and_copy(
        &mut self,
        text: String,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        if !text.is_empty() && clipboard.write(&text).is_ok() {
            clipboard.kill_ring.record_clipboard_write(text.clone());
        }
        clipboard.kill_ring.kill(text);
    }
}
