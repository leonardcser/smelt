//! Low-level buffer editing primitives for `PromptState`.

use super::{PromptCtx, PromptCtxRef, PromptState, ATTACHMENT_MARKER};
use crate::smelt_term::VimMode;
use smelt_buffer::text::{
    next_char_boundary, prev_char_boundary, safe_drain, safe_insert, safe_insert_str,
    safe_replace_range, safe_slice,
};
use smelt_core::attachment::AttachmentId;

impl PromptState {
    /// Shrink prompt source over `range` and keep every byte-offset
    /// anchor valid: drop attachment_ids whose markers lived in the range,
    /// drain the source bytes, then clamp cpos / selection_anchor /
    /// visual_anchor onto the new source. Use this instead of bare
    /// `safe_drain` whenever the range is computed from offsets that
    /// might also live in other anchors.
    pub(super) fn safe_shrink(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        range: std::ops::Range<usize>,
    ) {
        ctx.buf.remove_attachments_in_range(range.start, range.end);
        safe_drain(ctx.buf.source_mut(), range);
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
    }

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
        // ATTACHMENT_MARKER is a private sentinel that must only enter the
        // buffer through `insert_attachment_id`, which keeps source markers
        // and `attachment_ids` in 1:1 sync. A raw keystroke or paste of the
        // sentinel char would desync the two and break every downstream
        // attachment indexing path.
        if c == ATTACHMENT_MARKER {
            return;
        }
        self.from_paste = false;
        if self.selection_range(ctx.as_ref()).is_some() {
            self.save_undo(ctx);
            self.delete_selection(ctx);
        }
        let p = safe_insert(ctx.buf.source_mut(), ctx.win.cpos, c);
        ctx.win.cpos = p + c.len_utf8();
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn backspace(&mut self, ctx: &mut PromptCtx<'_>) {
        if self.selection_range(ctx.as_ref()).is_some() {
            self.save_undo(ctx);
            self.delete_selection(ctx);
            self.recompute_completer(ctx.as_ref());
            return;
        }
        // A degenerate `selection_anchor` (set by a shift-key whose motion
        // resolved to no movement, e.g. Shift+End at EOL) lingers as a
        // byte position that the source mutation below would orphan.
        ctx.win.selection_anchor = None;
        if ctx.win.cpos == 0 {
            return;
        }
        // Deleting the closing `"` of a `"@path"` token removes the whole token.
        if let Some(start) = self.quoted_at_ref_start(ctx.as_ref()) {
            if start == 0 {
                self.from_paste = false;
            }
            let cpos = ctx.win.cpos;
            self.safe_shrink(ctx, start..cpos);
            ctx.win.cpos = start;
            self.recompute_completer(ctx.as_ref());
            return;
        }
        let prev = prev_char_boundary(ctx.buf.source(), ctx.win.cpos);
        if prev == 0 {
            self.from_paste = false;
        }
        let cpos = ctx.win.cpos;
        self.safe_shrink(ctx, prev..cpos);
        ctx.win.cpos = prev;
        self.recompute_completer(ctx.as_ref());
    }

    /// Byte offset of the opening `"` when the cursor is just after the closing `"` of a `"@path"` token.
    pub(super) fn quoted_at_ref_start(&self, ctx: PromptCtxRef<'_>) -> Option<usize> {
        let src = ctx.buf.source();
        let before = safe_slice(src, 0..ctx.win.cpos);
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !safe_slice(src, 0..at_pos).ends_with(char::is_whitespace) {
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
        let cpos = ctx.win.cpos;
        self.safe_shrink(ctx, target..cpos);
        ctx.win.cpos = target;
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn delete_char_forward(&mut self, ctx: &mut PromptCtx<'_>) {
        if ctx.win.cpos >= ctx.buf.source().len() {
            return;
        }
        let cpos = ctx.win.cpos;
        let next = next_char_boundary(ctx.buf.source(), cpos);
        self.safe_shrink(ctx, cpos..next);
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
        let cpos = ctx.win.cpos;
        self.safe_shrink(ctx, cpos..target);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn kill_to_end_of_line(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        let cpos = ctx.win.cpos;
        let end = safe_slice(ctx.buf.source(), cpos..ctx.buf.source().len())
            .find('\n')
            .map(|i| cpos + i)
            .unwrap_or(ctx.buf.source().len());
        let killed = ctx.buf.copy_range(cpos..end);
        self.safe_shrink(ctx, cpos..end);
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn kill_to_start_of_line(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        clipboard: &mut crate::smelt_term::Clipboard,
    ) {
        let start = safe_slice(ctx.buf.source(), 0..ctx.win.cpos)
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let cpos = ctx.win.cpos;
        let killed = ctx.buf.copy_range(start..cpos);
        self.safe_shrink(ctx, start..cpos);
        ctx.win.cpos = start;
        self.kill_and_copy(killed, clipboard);
        self.recompute_completer(ctx.as_ref());
    }

    pub(super) fn delete_to_start_of_line(&mut self, ctx: &mut PromptCtx<'_>) {
        let start = safe_slice(ctx.buf.source(), 0..ctx.win.cpos)
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let cpos = ctx.win.cpos;
        self.safe_shrink(ctx, start..cpos);
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
        let cpos = ctx.win.cpos;
        let upper: String = safe_slice(ctx.buf.source(), cpos..end).to_uppercase();
        let new_len = upper.len();
        // ATTACHMENT_MARKER has no case mapping so any marker in the
        // range survives at the same chars()-index → attachment_ids
        // stays aligned. We still clamp anchors because case mapping
        // can change byte length (e.g. ß → SS).
        safe_replace_range(ctx.buf.source_mut(), cpos..end, &upper);
        ctx.win.cpos = cpos + new_len;
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
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
        let cpos = ctx.win.cpos;
        let lower: String = safe_slice(ctx.buf.source(), cpos..end).to_lowercase();
        let new_len = lower.len();
        safe_replace_range(ctx.buf.source_mut(), cpos..end, &lower);
        ctx.win.cpos = cpos + new_len;
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
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
        let word = safe_slice(ctx.buf.source(), ctx.win.cpos..end);
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
        let cpos = ctx.win.cpos;
        let cap_len = cap.len();
        safe_replace_range(ctx.buf.source_mut(), cpos..end, &cap);
        ctx.win.cpos = cpos + cap_len;
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
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
        // Also strip ATTACHMENT_MARKER — that sentinel must only enter the
        // buffer through `insert_attachment_id` so source markers and
        // `attachment_ids` stay in 1:1 sync.
        let data = data
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace(ATTACHMENT_MARKER, "");

        if data.is_empty() {
            return;
        }

        // Mark from_paste when inserting at the beginning of the current line
        // so pasted content starting with `!` isn't treated as a shell escape.
        let line_start = safe_slice(ctx.buf.source(), 0..ctx.win.cpos)
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if ctx.win.cpos == line_start {
            self.from_paste = true;
        }
        let p = safe_insert_str(ctx.buf.source_mut(), ctx.win.cpos, &data);
        ctx.win.cpos = p + data.len();
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
    }

    pub(super) fn insert_attachment_id(&mut self, ctx: &mut PromptCtx<'_>, id: AttachmentId) {
        let idx = safe_slice(ctx.buf.source(), 0..ctx.win.cpos)
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        ctx.buf.attachment_ids.insert(idx, id);
        let p = safe_insert(ctx.buf.source_mut(), ctx.win.cpos, ATTACHMENT_MARKER);
        ctx.win.cpos = p + ATTACHMENT_MARKER.len_utf8();
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
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
