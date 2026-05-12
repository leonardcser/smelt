//! Low-level buffer editing primitives for `PromptState`.

use super::{PromptCtx, PromptCtxRef, PromptState, ATTACHMENT_MARKER};
use crate::smelt_term::{Buffer, VimMode};
use smelt_core::attachment::AttachmentId;

impl PromptState {
    /// Save undo state. Skips during vim Insert — the session entry saved on insert-entry covers it.
    pub(crate) fn save_undo(&mut self, ctx: &mut PromptCtx<'_>) {
        if ctx.win.vim_enabled && ctx.win.vim_mode == VimMode::Insert {
            return; // insert session groups all edits into one undo step
        }
        ctx.buf.history.save(crate::smelt_term::UndoEntry::snapshot(
            ctx.buf.source(),
            ctx.win.cpos,
            &ctx.buf.attachment_ids,
        ));
    }

    pub(super) fn insert_char(&mut self, ctx: &mut PromptCtx<'_>, c: char) {
        self.from_paste = false;
        if self.selection_range(ctx.as_ref()).is_some() {
            self.save_undo(ctx);
            self.delete_selection(ctx);
        }
        ctx.buf.source_mut().insert(ctx.win.cpos, c);
        ctx.win.cpos += c.len_utf8();
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn backspace(&mut self, ctx: &mut PromptCtx<'_>) {
        if self.selection_range(ctx.as_ref()).is_some() {
            self.save_undo(ctx);
            self.delete_selection(ctx);
            self.recompute_completer(ctx.as_ref());
            return;
        }
        if ctx.win.cpos == 0 {
            return;
        }
        // Deleting the closing `"` of a `"@path"` token removes the whole token.
        if let Some(start) = self.quoted_at_ref_start(ctx.as_ref()) {
            if start == 0 {
                self.from_paste = false;
            }
            ctx.buf.source_mut().drain(start..ctx.win.cpos);
            ctx.win.cpos = start;
            self.recompute_completer(ctx.as_ref());
            return;
        }
        let prev = ctx.buf.source()[..ctx.win.cpos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        if prev == 0 {
            self.from_paste = false;
        }
        self.maybe_remove_attachment(ctx.buf, prev);
        ctx.buf.source_mut().drain(prev..ctx.win.cpos);
        ctx.win.cpos = prev;
        self.recompute_completer(ctx.as_ref());
    }

    /// Byte offset of the opening `"` when the cursor is just after the closing `"` of a `"@path"` token.
    pub(super) fn quoted_at_ref_start(&self, ctx: PromptCtxRef<'_>) -> Option<usize> {
        let before = &ctx.buf.source()[..ctx.win.cpos];
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !ctx.buf.source()[..at_pos].ends_with(char::is_whitespace) {
            return None;
        }
        if inner[at_pos + 2..].contains('"') {
            return None;
        }
        Some(at_pos)
    }

    pub(super) fn delete_word_backward(&mut self, ctx: &mut PromptCtx<'_>) {
        if ctx.win.cpos == 0 {
            return;
        }
        let target = crate::smelt_term::text::word_backward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target == 0 {
            self.from_paste = false;
        }
        self.remove_attachments_in_range(ctx.buf, target, ctx.win.cpos);
        ctx.buf.source_mut().drain(target..ctx.win.cpos);
        ctx.win.cpos = target;
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn delete_char_forward(&mut self, ctx: &mut PromptCtx<'_>) {
        if ctx.win.cpos >= ctx.buf.source().len() {
            return;
        }
        self.maybe_remove_attachment(ctx.buf, ctx.win.cpos);
        let next = ctx.buf.source()[ctx.win.cpos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| ctx.win.cpos + i)
            .unwrap_or(ctx.buf.source().len());
        ctx.buf.source_mut().drain(ctx.win.cpos..next);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn delete_word_forward(&mut self, ctx: &mut PromptCtx<'_>) {
        if ctx.win.cpos >= ctx.buf.source().len() {
            return;
        }
        let target = crate::smelt_term::text::word_forward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        self.remove_attachments_in_range(ctx.buf, ctx.win.cpos, target);
        ctx.buf.source_mut().drain(ctx.win.cpos..target);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn kill_to_end_of_line(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        let end = ctx.buf.source()[ctx.win.cpos..]
            .find('\n')
            .map(|i| ctx.win.cpos + i)
            .unwrap_or(ctx.buf.source().len());
        let killed = ctx.buf.copy_range(ctx.win.cpos..end);
        self.remove_attachments_in_range(ctx.buf, ctx.win.cpos, end);
        ctx.buf.source_mut().drain(ctx.win.cpos..end);
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn kill_to_start_of_line(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        let start = ctx.buf.source()[..ctx.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let killed = ctx.buf.copy_range(start..ctx.win.cpos);
        self.remove_attachments_in_range(ctx.buf, start, ctx.win.cpos);
        ctx.buf.source_mut().drain(start..ctx.win.cpos);
        ctx.win.cpos = start;
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn delete_to_start_of_line(&mut self, ctx: &mut PromptCtx<'_>) {
        let start = ctx.buf.source()[..ctx.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.remove_attachments_in_range(ctx.buf, start, ctx.win.cpos);
        ctx.buf.source_mut().drain(start..ctx.win.cpos);
        ctx.win.cpos = start;
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn uppercase_word(&mut self, ctx: &mut PromptCtx<'_>) {
        let end = crate::smelt_term::text::word_forward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == ctx.win.cpos {
            return;
        }
        let upper: String = ctx.buf.source()[ctx.win.cpos..end].to_uppercase();
        ctx.buf
            .source_mut()
            .replace_range(ctx.win.cpos..end, &upper);
        ctx.win.cpos += upper.len();
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn lowercase_word(&mut self, ctx: &mut PromptCtx<'_>) {
        let end = crate::smelt_term::text::word_forward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == ctx.win.cpos {
            return;
        }
        let lower: String = ctx.buf.source()[ctx.win.cpos..end].to_lowercase();
        ctx.buf
            .source_mut()
            .replace_range(ctx.win.cpos..end, &lower);
        ctx.win.cpos += lower.len();
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn capitalize_word(&mut self, ctx: &mut PromptCtx<'_>) {
        let end = crate::smelt_term::text::word_forward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if end == ctx.win.cpos {
            return;
        }
        let word = &ctx.buf.source()[ctx.win.cpos..end];
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
        ctx.buf.source_mut().replace_range(ctx.win.cpos..end, &cap);
        ctx.win.cpos += cap.len();
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn undo(&mut self, ctx: &mut PromptCtx<'_>) {
        let current = crate::smelt_term::UndoEntry::snapshot(
            ctx.buf.source(),
            ctx.win.cpos,
            &ctx.buf.attachment_ids,
        );
        if let Some(entry) = ctx.buf.history.undo(current) {
            self.install_source(ctx, entry.buf, entry.cpos);
            ctx.buf.attachment_ids = entry.attachments;
        }
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn move_word_forward(&mut self, ctx: &mut PromptCtx<'_>) -> bool {
        if ctx.win.cpos >= ctx.buf.source().len() {
            return false;
        }
        let target = crate::smelt_term::text::word_forward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target != ctx.win.cpos {
            ctx.win.cpos = target;
            self.recompute_completer(ctx.as_ref());
            true
        } else {
            false
        }
    }

    pub(super) fn move_word_backward(&mut self, ctx: &mut PromptCtx<'_>) -> bool {
        if ctx.win.cpos == 0 {
            return false;
        }
        let target = crate::smelt_term::text::word_backward_pos(
            ctx.buf.source(),
            ctx.win.cpos,
            crate::smelt_term::text::CharClass::Word,
        );
        if target != ctx.win.cpos {
            ctx.win.cpos = target;
            self.recompute_completer(ctx.as_ref());
            true
        } else {
            false
        }
    }

    pub(super) fn insert_paste(&mut self, ctx: &mut PromptCtx<'_>, data: String) {
        // Normalize `\r\n` and lone `\r` to `\n` (terminals in bracketed-paste mode send `\r`).
        let data = data.replace("\r\n", "\n").replace('\r', "\n");

        if data.is_empty() {
            return;
        }

        // Mark from_paste when inserting at the beginning of the current line
        // so pasted content starting with `!` isn't treated as a shell escape.
        let line_start = ctx.buf.source()[..ctx.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if ctx.win.cpos == line_start {
            self.from_paste = true;
        }
        ctx.buf.source_mut().insert_str(ctx.win.cpos, &data);
        ctx.win.cpos += data.len();
    }

    pub(super) fn insert_attachment_id(&mut self, ctx: &mut PromptCtx<'_>, id: AttachmentId) {
        let idx = ctx.buf.source()[..ctx.win.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        ctx.buf.attachment_ids.insert(idx, id);
        ctx.buf.source_mut().insert(ctx.win.cpos, ATTACHMENT_MARKER);
        ctx.win.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    pub(super) fn remove_attachments_in_range(
        &mut self,
        buf: &mut Buffer,
        start: usize,
        end: usize,
    ) {
        buf.remove_attachments_in_range(start, end);
    }

    pub(super) fn maybe_remove_attachment(&mut self, buf: &mut Buffer, byte_pos: usize) {
        buf.remove_attachment_at(byte_pos);
    }

    pub(super) fn move_to_line(&mut self, ctx: &mut PromptCtx<'_>, target_line: usize) {
        let mut line = 0;
        let mut pos = 0;
        for (i, c) in ctx.buf.source().char_indices() {
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
            pos = ctx.buf.source().rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        ctx.win.cpos = pos;
        self.recompute_completer(ctx.as_ref());
    }

    /// Kill text into the kill ring and copy to clipboard. `out.kill_ring`
    /// is paste-back text (raw, e.g. attachment markers survive); `out.clipboard`
    /// is the human-readable form pushed to the system clipboard.
    pub(super) fn kill_and_copy(
        &mut self,
        out: crate::smelt_term::CopyOutput,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        if !out.clipboard.is_empty() && clipboard.write(&out.clipboard).is_ok() {
            clipboard
                .kill_ring
                .record_clipboard_write(out.clipboard.clone());
        }
        clipboard.kill_ring.kill(out.kill_ring);
    }
}
