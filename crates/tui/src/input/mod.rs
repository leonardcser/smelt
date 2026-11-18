mod buffer;
pub(crate) mod editor;
mod file_drop;
mod vim_bridge;

pub(crate) use smelt_core::history::History;

use crate::app::QueueStage;
use crate::keymap::{self, KeyAction, KeyContext};
use crate::smelt_edit::AttachedText;
use crate::smelt_edit::VimMode;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use protocol::Content;
use smelt_core::attachment::Attachment;
use smelt_view as content;
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

/// Prompt text set aside with Ctrl+S: the vim visual selection or the
/// whole prompt. Owns its attachments so it survives store clears across
/// sessions.
struct Stash {
    text: AttachedText,
    /// Cursor offset into `text`.
    cpos: usize,
    from_paste: bool,
    /// The prompt's undo history, when the stash took the whole prompt. It
    /// returns with the text, so undo never mixes the draft with what was
    /// typed while it was stashed.
    history: Option<crate::smelt_edit::UndoHistory>,
}

/// The part of the prompt that a submit or a stash takes.
struct PromptScope {
    /// Bytes the action takes.
    taken: std::ops::Range<usize>,
    /// What leaves the prompt once the action commits.
    cut: PromptCut,
}

/// How the prompt changes once an action takes its [`PromptScope`].
pub(crate) enum PromptCut {
    /// The whole prompt went.
    Clear,
    /// A visual selection went, widened to whole lines when linewise.
    Range(std::ops::Range<usize>),
}

// ── Shared input state ───────────────────────────────────────────────────────

/// Prompt-specific side-cars (stash, paste state).
/// The canonical edit buffer lives in `ui.bufs[PROMPT_EDIT_BUF]` and the
/// window in `ui.wins[PROMPT_WIN]`; methods that touch either take
/// `&mut Buffer`/`&mut Window` (or shared refs) as parameters. Display
/// coordinates (`cursor_row`, `cursor_col`, `scroll_top`) are synced from
/// `buf.source` via `sync_display_coords` before each render.
pub(crate) struct PromptState {
    stash: Option<Stash>,
    cwd: std::path::PathBuf,
    runtime_dir: std::path::PathBuf,
    /// True when content came from a paste; cleared on manual character input.
    pub(super) from_paste: bool,
    /// Chord state: true after Ctrl+X, waiting for second key.
    pending_ctrl_x: bool,
}

/// What the caller should do after `handle_event`.
pub(crate) enum Action {
    Redraw,
    Submit {
        submission: PromptSubmission,
        cut: PromptCut,
        stage: QueueStage,
    },
    SubmitEmpty,
    EditInEditor,
    CenterScroll,
    PanColumns(isize),
    NotifyError(String),
    Noop,
}

pub(crate) struct PromptSubmission {
    pub(crate) content: Content,
    pub(crate) display: String,
    pub(crate) replay: Option<PromptReplay>,
}

#[derive(Clone)]
pub(crate) struct PromptReplay {
    pub(crate) source: String,
    pub(crate) attachments: Vec<Attachment>,
    pub(crate) image_indices: Vec<usize>,
}

impl PromptReplay {
    pub(crate) fn into_attached(self) -> AttachedText {
        AttachedText::new(self.source, self.attachments)
    }

    pub(crate) fn image_placement(&self) -> protocol::history::ImagePlacement {
        protocol::history::ImagePlacement {
            source: self.source.clone(),
            image_indices: self.image_indices.clone(),
        }
    }
}

fn rewind_source_and_attachments(
    mut text: String,
    images: Vec<(String, String)>,
) -> (String, Vec<Attachment>) {
    let mut attachments = Vec::new();
    for (label, data_url) in images {
        let attachment = Attachment::image(label, data_url);
        let display_label = attachment.display_label();
        let raw_label = format!("[{}]", attachment.label());
        let labels = if raw_label != display_label {
            vec![raw_label, display_label]
        } else {
            vec![display_label]
        };

        if let Some((pos, len)) = labels
            .iter()
            .find_map(|label| text.find(label).map(|pos| (pos, label.len())))
        {
            let range = smelt_buffer::text::covering_grapheme_range(&text, pos..pos + len);
            let mut marker = [0; 4];
            let marker = ATTACHMENT_MARKER.encode_utf8(&mut marker);
            smelt_buffer::text::replace_range(&mut text, range, marker);
        } else {
            if !text.is_empty() && !text.chars().last().is_some_and(char::is_whitespace) {
                text.push(' ');
            }
            text.push(ATTACHMENT_MARKER);
        }
        attachments.push(attachment);
    }
    (text, attachments)
}

impl Default for PromptState {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptState {
    pub(crate) fn new() -> Self {
        let runtime_dir = std::env::temp_dir();
        Self::new_for_runtime(runtime_dir.clone(), runtime_dir)
    }

    pub(crate) fn new_for_runtime(
        cwd: std::path::PathBuf,
        runtime_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            stash: None,
            cwd,
            runtime_dir,
            from_paste: false,
            pending_ctrl_x: false,
        }
    }

    pub(crate) fn set_cwd(&mut self, cwd: std::path::PathBuf) {
        self.cwd = cwd;
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
                    smelt_buffer::text::snap_grapheme(src, s),
                    smelt_buffer::text::snap_grapheme(src, e),
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

    fn replace_selection_for_insert(&mut self, ctx: &mut PromptCtx<'_>) {
        let was_vim_visual = ctx.win.vim_enabled()
            && matches!(ctx.win.vim_mode(), VimMode::Visual | VimMode::VisualLine);
        if self.selection_range(ctx.as_ref()).is_none() {
            return;
        }

        self.delete_selection(ctx);
        if was_vim_visual {
            ctx.win.set_vim_mode(VimMode::Normal);
        }
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
        clipboard
            .kill_ring
            .set(crate::smelt_edit::AttachedText::plain(text.clone()));
        clipboard.kill_ring.record_clipboard_write(text);
    }

    /// Wholesale buffer swap. Installs `text` + `attachments` atomically (via
    /// `AttachedTextMut::install`, which debug-asserts that marker count in
    /// `text` matches `attachments.len()`). Clamps `cpos` to a grapheme
    /// boundary, then clears selection + vim visual anchors.
    ///
    /// Passing no attachments with marker-containing text trips the debug
    /// assert - exactly what you want, since the caller would otherwise be
    /// silently dropping attachments while leaving their markers in
    /// source. Callers that need to preserve attachments hand them in;
    /// those that genuinely want a clean buffer pass `vec![]` and a
    /// marker-free `text`.
    pub(crate) fn install_source(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        text: String,
        cpos: usize,
        attachments: Vec<Attachment>,
    ) {
        ctx.buf.text_mut().install(text, attachments);
        let source = ctx.buf.source();
        ctx.win
            .set_cpos(crate::smelt_edit::text::snap_grapheme(source, cpos));
        ctx.win.clear_selection_anchor();
        ctx.win.clear_vim_visual_anchor();
    }

    pub(crate) fn clear(&mut self, ctx: &mut PromptCtx<'_>) {
        self.install_source(ctx, String::new(), 0, Vec::new());
        self.from_paste = false;
        // The stash is intentionally preserved.
    }

    pub(crate) fn clear_with_undo(&mut self, ctx: &mut PromptCtx<'_>) {
        if !ctx.buf.source().is_empty() || !ctx.buf.attachments.is_empty() {
            self.save_undo_force(ctx);
        }
        self.clear(ctx);
    }

    /// Wholesale buffer swap from user-supplied plain text (no attachments).
    /// Snapshots undo, installs new source without attachments, cursor at end.
    ///
    /// Intended for callers feeding text from outside the prompt: Lua
    /// `smelt.prompt.set_text`, placeholder accept, `$EDITOR` re-import.
    /// Any `ATTACHMENT_MARKER` bytes in `text` are stripped - these
    /// callers can't carry attachments, so the markers would be
    /// orphaned (and trip the marker/attachment invariant in
    /// `AttachedTextMut::install`).
    pub(crate) fn replace_text(&mut self, ctx: &mut PromptCtx<'_>, text: String) {
        self.save_undo(ctx);
        let text = Self::strip_attachment_markers(&text);
        let cpos = text.len();
        self.install_source(ctx, text, cpos, Vec::new());
    }

    /// Strip `ATTACHMENT_MARKER` bytes from `s`. Used at every seam where
    /// source bytes leave the buffer for storage that can't carry the
    /// matching attachments: `History` draft slot, `replace_text`
    /// install path, `$EDITOR` round-trip. Persisting raw markers
    /// upstream would let them return without backing attachments,
    /// violating the marker/attachment invariant on install.
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

    /// Bring `text` back without losing what the prompt holds. An empty
    /// prompt takes it with the cursor at `cpos`; otherwise it goes above the
    /// current text. Either way it is one step that can be undone.
    pub(crate) fn restore_attached(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        mut text: AttachedText,
        cpos: usize,
    ) {
        if text.is_empty() {
            return;
        }
        if ctx.buf.source().is_empty() {
            self.save_undo(ctx);
            let (text, attachments) = text.into_parts();
            self.install_source(ctx, text, cpos, attachments);
            self.from_paste = false;
        } else {
            text.push_str("\n");
            self.prepend_attached(ctx, text);
        }
    }

    /// Prepend `prefix` with its attachments, snapshot undo, and shift the
    /// cursor.
    fn prepend_attached(&mut self, ctx: &mut PromptCtx<'_>, prefix: AttachedText) {
        self.save_undo(ctx);
        let inserted = ctx.buf.text_mut().insert_attached(0, &prefix).len();
        let cpos = smelt_buffer::text::ceil_grapheme(ctx.buf.source(), ctx.win.cpos() + inserted);
        ctx.win.set_cpos(cpos);
        ctx.win.clear_selection_anchor();
        // Every offset into the buffer shifts right by `inserted`. If the
        // prefix joins the first old grapheme, keep both cursors after that
        // complete grapheme rather than inside it.
        ctx.win.shift_vim_visual_anchor(inserted, ctx.buf.source());
        ctx.win.clamp_anchors_to_source(ctx.buf.source());
        self.from_paste = false;
    }

    /// Restore a removed range into an unchanged surviving prompt.
    pub(crate) fn restore_removed_range(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        start: usize,
        removed: AttachedText,
    ) {
        self.save_undo_force(ctx);
        let cpos = ctx.win.cpos();
        let inserted = ctx.buf.text_mut().insert_attached(start, &removed);
        let cpos = if cpos >= inserted.start {
            cpos + inserted.len()
        } else {
            cpos
        };
        ctx.win
            .set_cpos(smelt_buffer::text::snap_grapheme(ctx.buf.source(), cpos));
        ctx.win.clear_selection_anchor();
        ctx.win.clear_vim_visual_anchor();
    }

    pub(crate) fn has_stash(&self) -> bool {
        self.stash.is_some()
    }

    /// Restore the stash if there is one, else stash the prompt's scope.
    fn toggle_stash(&mut self, ctx: &mut PromptCtx<'_>) {
        if self.stash.is_some() {
            self.restore_stash(ctx);
        } else {
            self.stash_scope(ctx);
        }
    }

    /// Move the visual selection, or else the whole prompt, into the stash.
    /// The stash keeps the attachments of the text it takes.
    fn stash_scope(&mut self, ctx: &mut PromptCtx<'_>) {
        let scope = self.scope(ctx.as_ref());
        let text =
            AttachedText::slice_of(ctx.buf.source(), &ctx.buf.attachments, scope.taken.clone());
        if text.is_empty() {
            return;
        }
        let cpos = ctx
            .win
            .cpos()
            .saturating_sub(scope.taken.start)
            .min(text.text().len());
        let history = matches!(scope.cut, PromptCut::Clear).then(|| ctx.buf.history.take());
        self.stash = Some(Stash {
            text,
            cpos,
            from_paste: self.from_paste,
            history,
        });
        self.apply_cut(ctx, scope.cut);
    }

    /// Put the stash back. Text already in the prompt stays, below the
    /// stashed text. Into an empty prompt, the stashed draft returns whole,
    /// with its undo history.
    pub(crate) fn restore_stash(&mut self, ctx: &mut PromptCtx<'_>) {
        let Some(stash) = self.stash.take() else {
            return;
        };
        let whole = ctx.buf.source().is_empty();
        self.restore_attached(ctx, stash.text, stash.cpos);
        if whole {
            self.from_paste = stash.from_paste;
            if let Some(history) = stash.history {
                ctx.buf.history = history;
            }
        }
    }

    /// Restore rewound text, with visible attachment labels turned back into
    /// prompt markers. Text already in the prompt stays, below it.
    pub(crate) fn restore_from_rewind(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        text: String,
        images: Vec<(String, String)>,
        placement: Option<protocol::history::ImagePlacement>,
    ) {
        let exact = placement.and_then(|placement| {
            (placement.source.matches(ATTACHMENT_MARKER).count() == placement.image_indices.len())
                .then(|| {
                    // A repeated image is one attachment in several places.
                    let pool: Vec<_> = images
                        .iter()
                        .map(|(label, data_url)| Attachment::image(label.clone(), data_url.clone()))
                        .collect();
                    placement
                        .image_indices
                        .iter()
                        .map(|&index| pool.get(index).cloned())
                        .collect::<Option<Vec<_>>>()
                        .map(|attachments| (placement.source, attachments))
                })
                .flatten()
        });
        let (text, attachments) =
            exact.unwrap_or_else(|| rewind_source_and_attachments(text, images));
        let cpos = text.len();
        self.restore_attached(ctx, AttachedText::new(text, attachments), cpos);
    }

    fn submission_from_range(
        &self,
        buf: &crate::smelt_edit::Buffer,
        range: std::ops::Range<usize>,
    ) -> PromptSubmission {
        let source = buf.source();
        let range = smelt_buffer::text::snapped_grapheme_range(source, range);
        let start = range.start;
        let selected = smelt_buffer::text::slice(source, range);
        let mut marker_idx = smelt_buffer::text::slice(source, 0..start)
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let mut text = String::new();
        let mut display = String::new();
        let mut images = Vec::new();
        let mut attachments = Vec::new();
        let mut image_indices = Vec::new();
        // One image per distinct content: base64 payloads are large.
        let mut seen = std::collections::HashMap::new();

        for c in selected.chars() {
            if c == ATTACHMENT_MARKER {
                if let Some(attachment) = buf.attachments.get(marker_idx) {
                    attachments.push(attachment.clone());
                    display.push_str(&attachment.display_label());
                    let index = *seen.entry(attachment.content_key()).or_insert_with(|| {
                        images.push((
                            attachment.label().to_string(),
                            attachment.data_url().to_string(),
                        ));
                        images.len() - 1
                    });
                    image_indices.push(index);
                }
                marker_idx += 1;
            } else {
                text.push(c);
                display.push(c);
            }
        }

        let replay = (!attachments.is_empty()).then(|| PromptReplay {
            source: selected.to_string(),
            attachments,
            image_indices,
        });
        PromptSubmission {
            content: Content::with_images(text, images),
            display,
            replay,
        }
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
            self.replace_selection_for_insert(ctx);
        }
        let attachment = Attachment::image(label, data_url);
        crate::app::attachment_preview::prewarm(attachment.clone());
        self.insert_attachment(ctx, attachment);
    }

    /// Build submission `Content`. Duplicate image refs are deduplicated (base64 payloads are large).
    #[cfg(test)]
    pub(crate) fn build_content(&self, buf: &crate::smelt_edit::Buffer) -> Content {
        self.submission_from_range(buf, 0..buf.source().len())
            .content
    }

    /// What a submit or stash takes: the non-empty vim visual selection, or
    /// else the whole prompt.
    fn scope(&self, ctx: PromptCtxRef<'_>) -> PromptScope {
        self.visual_scope(ctx).unwrap_or_else(|| PromptScope {
            taken: 0..ctx.buf.source().len(),
            cut: PromptCut::Clear,
        })
    }

    fn visual_scope(&self, ctx: PromptCtxRef<'_>) -> Option<PromptScope> {
        let mode = ctx.win.vim_mode();
        if !ctx.win.vim_enabled() || !matches!(mode, VimMode::Visual | VimMode::VisualLine) {
            return None;
        }
        let (start, end) = self.selection_range(ctx)?;
        if start >= end {
            return None;
        }
        let taken = start..end;
        let cut = if mode == VimMode::VisualLine {
            crate::smelt_edit::vim::linewise_delete_range(ctx.buf.source(), taken.clone())
        } else {
            taken.clone()
        };
        Some(PromptScope {
            taken,
            cut: PromptCut::Range(cut),
        })
    }

    /// Submits the prompt's scope at `stage`. An empty prompt submits empty,
    /// which continues or advances work; steering an empty prompt does
    /// nothing.
    pub(crate) fn submit_action(&mut self, ctx: &mut PromptCtx<'_>, stage: QueueStage) -> Action {
        let scope = self.scope(ctx.as_ref());
        if scope.taken.is_empty() {
            return match stage {
                QueueStage::Turn => Action::SubmitEmpty,
                QueueStage::Request => Action::Noop,
            };
        }
        Action::Submit {
            submission: self.submission_from_range(ctx.buf, scope.taken),
            cut: scope.cut,
            stage,
        }
    }

    /// Remove what an action took from the prompt.
    pub(crate) fn apply_cut(&mut self, ctx: &mut PromptCtx<'_>, cut: PromptCut) {
        match cut {
            PromptCut::Clear => self.clear(ctx),
            PromptCut::Range(range) => {
                self.save_undo(ctx);
                ctx.buf.text_mut().replace_range(range.clone(), "");
                let cpos = smelt_buffer::text::snap_grapheme(ctx.buf.source(), range.start);
                ctx.win.set_cpos(cpos);
                ctx.win.clear_selection_anchor();
                ctx.win.clear_vim_visual_anchor();
                if ctx.win.vim_enabled()
                    && matches!(ctx.win.vim_mode(), VimMode::Visual | VimMode::VisualLine)
                {
                    ctx.win.set_vim_mode(VimMode::Normal);
                }
                ctx.win.clamp_anchors_to_source(ctx.buf.source());
            }
        }
    }

    pub(crate) fn key_context(&self, ctx: PromptCtxRef<'_>, agent_running: bool) -> KeyContext {
        KeyContext {
            buf_empty: ctx.buf.source().is_empty() && ctx.buf.attachments.is_empty(),
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
        now: std::time::Instant,
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
        let preserves_selection = matches!(
            action,
            KeyAction::CopySelection | KeyAction::Submit | KeyAction::SubmitToRequestQueue
        );
        if !is_select && !is_editing && !preserves_selection {
            self.clear_selection(ctx.win);
        }
        match action {
            // Caller handles these.
            KeyAction::Quit | KeyAction::CancelAgent => Action::Noop,

            // ── TuiApp control ─────────────────────────────────────────────
            KeyAction::ClearBuffer => {
                self.clear_with_undo(ctx);
                Action::Redraw
            }
            // Intercepted by the global chord layer; these arms are unreachable in practice.
            KeyAction::ToggleMode | KeyAction::CycleReasoning => Action::Noop,
            KeyAction::ToggleStash => {
                self.toggle_stash(ctx);
                Action::Redraw
            }

            // ── Submit / newline ─────────────────────────────────────────
            KeyAction::Submit => self.submit_action(ctx, QueueStage::Turn),
            KeyAction::SubmitToRequestQueue => self.submit_action(ctx, QueueStage::Request),
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
                    ctx.win
                        .set_cpos(smelt_buffer::text::prev_grapheme_boundary(source, cpos));
                    Action::Redraw
                } else {
                    Action::Noop
                }
            }
            KeyAction::MoveRight => {
                let cpos = ctx.win.cpos();
                if cpos < ctx.buf.source().len() {
                    let source = ctx.buf.source();
                    ctx.win
                        .set_cpos(smelt_buffer::text::next_grapheme_boundary(source, cpos));
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
            KeyAction::Redo => {
                self.redo(ctx);
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
                    clipboard.kill_ring.flash_range(start, end, now);
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
                if let Some(url) = clipboard_image_to_data_url(&self.cwd, &self.runtime_dir) {
                    self.save_undo(ctx);
                    self.replace_selection_for_insert(ctx);
                    self.insert_image(ctx, "clipboard.png".into(), url);
                    return Action::Redraw;
                }
                if let Some(text) = clipboard.read() {
                    if !text.is_empty() {
                        self.save_undo(ctx);
                        self.replace_selection_for_insert(ctx);
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
                    ctx.win
                        .set_cpos(smelt_buffer::text::prev_grapheme_boundary(source, cpos));
                }
                Action::Redraw
            }
            KeyAction::SelectRight => {
                self.extend_selection(ctx.win);
                if ctx.win.cpos() < ctx.buf.source().len() {
                    let cpos = ctx.win.cpos();
                    let source = ctx.buf.source();
                    ctx.win
                        .set_cpos(smelt_buffer::text::next_grapheme_boundary(source, cpos));
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
            let images = match file_drop::read_images(&data) {
                Ok(images) => images,
                Err(e) => return Action::NotifyError(e),
            };
            let clipboard_image = if images.is_none() && data.trim().is_empty() {
                clipboard_image_to_data_url(&self.cwd, &self.runtime_dir)
            } else {
                None
            };

            self.save_undo(ctx);
            self.replace_selection_for_insert(ctx);
            if let Some(images) = images {
                for (label, url) in images {
                    self.insert_image(ctx, label, url);
                }
                return Action::Redraw;
            }
            if let Some(url) = clipboard_image {
                self.insert_image(ctx, "clipboard.png".into(), url);
                return Action::Redraw;
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
                buf_empty: ctx.buf.source().is_empty() && ctx.buf.attachments.is_empty(),
                vim_non_insert: ctx.win.vim_enabled()
                    && matches!(
                        ctx.win.vim_mode(),
                        VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                    ),
                vim_enabled: ctx.win.vim_enabled(),
                agent_running: false,
            };

            if let Some(action) = keymap::lookup(code, modifiers, &key_ctx) {
                return self.execute_key_action(ctx, action, history, clipboard, now);
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

pub(super) fn current_line(buf: &str, cpos: usize) -> usize {
    let end = smelt_buffer::text::snap_grapheme(buf, cpos);
    buf[..end].chars().filter(|&c| c == '\n').count()
}

/// Read an image from the system clipboard and return a data URL.
/// macOS: uses `osascript`; Linux: tries `xclip` then `wl-paste`.
fn clipboard_image_to_data_url(
    cwd: &std::path::Path,
    runtime_dir: &std::path::Path,
) -> Option<String> {
    use base64::Engine;

    let tmp = tempfile::Builder::new()
        .prefix("clipboard-")
        .suffix(".png")
        .tempfile_in(runtime_dir)
        .ok()?;
    let tmp_str = tmp.path().to_string_lossy();

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
            .current_dir(cwd)
            .output()
            .ok()
            .is_some_and(|o| o.status.success())
    } else {
        std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .current_dir(cwd)
            .stdout(tmp.reopen().ok()?)
            .status()
            .ok()
            .is_some_and(|s| s.success())
            || std::process::Command::new("wl-paste")
                .args(["--type", "image/png"])
                .current_dir(cwd)
                .stdout(tmp.reopen().ok()?)
                .status()
                .ok()
                .is_some_and(|s| s.success())
    };

    if !ok {
        return None;
    }

    let bytes = std::fs::read(tmp.path()).ok()?;
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
            let parser =
                std::sync::Arc::new(smelt_view::prompt_parser::PromptBufferParser::default());
            let mut buf = crate::smelt_edit::Buffer::new(
                crate::app::PROMPT_EDIT_BUF,
                crate::smelt_edit::BufCreateOpts::default(),
            );
            buf.set_parser(parser);
            buf.hold_attachments();
            buf.history = crate::smelt_edit::UndoHistory::new(Some(100));
            let win = crate::smelt_edit::Window::new(
                crate::app::PROMPT_WIN,
                crate::app::PROMPT_EDIT_BUF,
                crate::smelt_edit::SplitConfig {
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
            self.state.execute_key_action(
                &mut ctx,
                action,
                None,
                &mut clip,
                std::time::Instant::now(),
            )
        }
    }

    #[test]
    fn prepend_attached_places_cursors_after_a_joined_grapheme() {
        let mut input = Harness::new();
        input.state.replace_text(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "\u{301}x".into(),
        );
        input.win.set_cpos(0);

        input.state.prepend_attached(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            AttachedText::plain("e"),
        );

        assert_eq!(input.buf.source(), "e\u{301}x");
        assert_eq!(input.win.cpos(), "e\u{301}".len());
        assert_eq!(input.win.vim_state().visual_anchor_raw(), "e\u{301}".len());
    }

    // ── rewind attachment restoration ────────────────────────────────────────

    #[test]
    fn rewind_restore_replaces_visible_attachment_label_with_marker() {
        let (source, attachments) = rewind_source_and_attachments(
            "look [shot.png]".into(),
            vec![("shot.png".into(), "data:image/png;base64,AAA".into())],
        );

        assert_eq!(source, format!("look {ATTACHMENT_MARKER}"));
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn rewind_restore_replaces_complete_attachment_label_graphemes() {
        for text in ["[shot.png]\u{301}", "\u{600}[shot.png]"] {
            let (source, attachments) = rewind_source_and_attachments(
                text.into(),
                vec![("shot.png".into(), "data:image/png;base64,AAA".into())],
            );

            assert_eq!(source, ATTACHMENT_MARKER.to_string());
            assert_eq!(attachments.len(), 1);
        }
    }

    #[test]
    fn rewind_restore_keeps_attachment_when_text_lacks_label() {
        let (source, attachments) = rewind_source_and_attachments(
            "look".into(),
            vec![("shot.png".into(), "data:image/png;base64,AAA".into())],
        );

        assert_eq!(source, format!("look {ATTACHMENT_MARKER}"));
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn rewind_restore_installs_prompt_attachment_id_when_label_missing() {
        let mut input = Harness::new();
        input.state.restore_from_rewind(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "look".into(),
            vec![("shot.png".into(), "data:image/png;base64,AAA".into())],
            None,
        );

        assert_eq!(input.buf.source(), &format!("look {ATTACHMENT_MARKER}"));
        assert_eq!(input.buf.attachments.len(), 1);
    }

    #[test]
    fn rewind_restore_matches_sanitized_display_label() {
        let (source, attachments) = rewind_source_and_attachments(
            "look [bad�label]".into(),
            vec![("bad\nlabel".into(), "data:image/png;base64,AAA".into())],
        );

        assert_eq!(source, format!("look {ATTACHMENT_MARKER}"));
        assert_eq!(attachments.len(), 1);
    }

    // ── from_paste behavior for shell escape prevention ───────────────────

    #[test]
    fn paste_sets_from_paste_only_at_line_start() {
        // (source, cursor, pasted, from_paste, resulting source)
        let cases = [
            ("", 0, "!echo hello", true, "!echo hello"),
            ("", 0, "", false, ""),
            ("", 0, "   ", true, "   "),
            ("hello ", 6, "!world", false, "hello !world"),
            ("hello", 5, " world", false, "hello world"),
            ("line1\nline2", 0, "!command", true, "!commandline1\nline2"),
            ("line1\n", 6, "!command", true, "line1\n!command"),
            ("line1\nhello", 8, " world", false, "line1\nhe worldllo"),
        ];
        for (source, cpos, pasted, from_paste, result) in cases {
            let mut input = Harness::new();
            input.buf.set_source(source.to_string());
            input.win.set_cpos(cpos);
            input.state.insert_paste(
                &mut PromptCtx {
                    buf: &mut input.buf,
                    win: &mut input.win,
                },
                pasted.to_string(),
            );
            assert_eq!(
                input.skip_shell_escape(),
                from_paste,
                "{source:?} + {pasted:?}"
            );
            assert_eq!(input.buf.source(), result);
        }
    }

    #[test]
    fn paste_cursor_moves_after_graphemes_formed_with_following_text() {
        for (pasted, following) in [
            ("e", "\u{301}z"),
            ("9", "\u{fe0f}z"),
            ("👩", "\u{200d}💻z"),
            ("🇨", "🇦z"),
        ] {
            let mut input = Harness::new();
            input.buf.set_source(following.to_string());
            input.win.set_cpos(0);
            input.state.insert_paste(
                &mut PromptCtx {
                    buf: &mut input.buf,
                    win: &mut input.win,
                },
                pasted.to_string(),
            );

            assert_eq!(input.buf.source(), format!("{pasted}{following}"));
            assert_eq!(
                input.win.cpos(),
                smelt_buffer::text::next_grapheme_boundary(input.buf.source(), 0)
            );
        }
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

        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(
            !input.skip_shell_escape(),
            "the emptied prompt holds no pasted text"
        );
        assert!(
            input.buf.source().is_empty(),
            "Buffer should be empty after stashing"
        );

        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        assert!(input.skip_shell_escape(), "Stash should restore from_paste");
        assert_eq!(input.buf.source(), "!test");
    }

    #[test]
    fn unstash_keeps_current_prompt_below_stashed_text() {
        let mut input = Harness::new();
        input.buf.set_source("stashed".to_string());
        input.win.set_cpos(7);
        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });
        input.buf.set_source("current".to_string());
        input.win.set_cpos(3);

        input.state.toggle_stash(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });

        assert_eq!(input.buf.source(), "stashed\ncurrent");
        assert_eq!(input.win.cpos(), 3 + "stashed\n".len());
        assert!(!input.state.has_stash());
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
    fn pasted_svg_path_inserts_text_not_image_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.svg");
        std::fs::write(&path, r#"<svg><text>relay</text></svg>"#).unwrap();
        let pasted = path.to_string_lossy().into_owned();
        let mut input = Harness::new();
        let mut clipboard = crate::smelt_edit::Clipboard::null();
        let mut ctx = PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        };

        let action = input.state.handle_event(
            &mut ctx,
            Event::Paste(pasted.clone()),
            None,
            &mut clipboard,
            std::time::Instant::now(),
        );

        assert!(matches!(action, Action::Redraw));
        assert_eq!(input.buf.source(), pasted);
        assert!(input.buf.attachments.is_empty());
    }

    // ── Selection tests ─────────────────────────────────────────────────

    #[test]
    fn submission_ranges_never_split_grapheme_clusters() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let mut input = Harness::new();
            input.buf.set_source(format!("a{grapheme}b"));
            let inside = 1 + grapheme.chars().next().unwrap().len_utf8();

            let head = input.state.submission_from_range(&input.buf, 0..inside);
            assert_eq!(head.content.text_content(), "a", "{grapheme:?}");
            assert_eq!(head.display, "a", "{grapheme:?}");

            let tail = input
                .state
                .submission_from_range(&input.buf, inside..input.buf.source().len());
            assert_eq!(tail.content.text_content(), format!("{grapheme}b"));
            assert_eq!(tail.display, format!("{grapheme}b"));
        }
    }

    #[test]
    fn selected_submit_carries_only_selected_attachments() {
        let mut input = Harness::new();
        let keep = Attachment::image("keep.png".into(), "data:image/png;base64,KEEP".into());
        let send = Attachment::image("send.png".into(), "data:image/png;base64,SEND".into());
        let source = format!("keep {ATTACHMENT_MARKER}\nsend {ATTACHMENT_MARKER}");
        let start = source.find("send").expect("send line");
        let end = source.len();
        input
            .buf
            .text_mut()
            .install(source, vec![keep.clone(), send.clone()]);
        input.state.set_vim_enabled(&mut input.win, true);
        input.win.begin_visual(VimMode::Visual, start);
        input.win.set_cpos(end);

        let action = input.state.submit_action(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            QueueStage::Turn,
        );

        let Action::Submit {
            submission, cut, ..
        } = action
        else {
            panic!("expected selected submit");
        };
        assert_eq!(submission.display, "send [send.png]");
        assert_eq!(
            submission.replay.as_ref().unwrap().attachments,
            vec![send.clone()]
        );
        match submission.content {
            Content::Parts(parts) => {
                assert!(matches!(
                    &parts[0],
                    protocol::ContentPart::Text { text } if text == "send "
                ));
                assert!(matches!(
                    &parts[1],
                    protocol::ContentPart::ImageUrl { label: Some(label), url }
                        if label == "send.png" && url == "data:image/png;base64,SEND"
                ));
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected multipart content, got {other:?}"),
        }
        assert_eq!(input.buf.attachments, vec![keep.clone(), send]);
        input.state.apply_cut(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            cut,
        );
        assert_eq!(input.buf.attachments, vec![keep]);
    }

    #[test]
    fn shift_selection_submit_uses_whole_prompt() {
        let mut input = Harness::new();
        input.buf.set_source("alpha beta".to_string());
        input.win.set_cpos(0);
        input.test_action(KeyAction::SelectRight, VimMode::Insert);
        input.test_action(KeyAction::SelectRight, VimMode::Insert);

        let action = input.state.submit_action(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            QueueStage::Turn,
        );

        let Action::Submit {
            submission, cut, ..
        } = action
        else {
            panic!("expected whole prompt submit");
        };
        assert_eq!(submission.content.text_content(), "alpha beta");
        assert!(matches!(cut, PromptCut::Clear));
        assert_eq!(input.buf.source(), "alpha beta");
    }

    #[test]
    fn shift_actions_extend_selection_from_the_anchor() {
        use KeyAction::*;
        // (source, cursor, actions, cursor after, selection range)
        type Case = (
            &'static str,
            usize,
            &'static [KeyAction],
            usize,
            Option<(usize, usize)>,
        );
        let cases: [Case; 12] = [
            ("hello", 0, &[SelectRight], 1, Some((0, 1))),
            ("hello", 0, &[SelectRight; 3], 3, Some((0, 3))),
            ("hello", 5, &[SelectLeft; 2], 3, Some((3, 5))),
            ("hello world foo", 0, &[SelectWordForward], 6, Some((0, 6))),
            ("hello world", 11, &[SelectWordBackward], 6, Some((6, 11))),
            ("hello world", 5, &[SelectStartOfLine], 0, Some((0, 5))),
            ("hello world", 5, &[SelectEndOfLine], 11, Some((5, 11))),
            // Reversing direction keeps the anchor.
            (
                "abcdef",
                3,
                &[
                    SelectRight,
                    SelectRight,
                    SelectLeft,
                    SelectLeft,
                    SelectLeft,
                    SelectLeft,
                ],
                1,
                Some((1, 3)),
            ),
            // Multibyte characters move by character.
            ("héllo", 0, &[SelectRight; 2], 3, Some((0, 3))),
            ("hello", 0, &[SelectLeft], 0, None),
            ("hello", 5, &[SelectRight], 5, None),
            ("", 0, &[SelectRight], 0, None),
        ];
        for (source, cpos, actions, after, range) in cases {
            let mut input = Harness::new();
            input.buf.set_source(source.to_string());
            input.win.set_cpos(cpos);
            for action in actions {
                input.test_action(*action, VimMode::Insert);
            }
            assert_eq!(input.win.cpos(), after, "{source:?} {actions:?}");
            assert_eq!(
                input.state.selection_range(PromptCtxRef {
                    buf: &input.buf,
                    win: &input.win
                }),
                range,
                "{source:?} {actions:?}"
            );
        }
    }

    #[test]
    fn edit_actions_replace_the_selection() {
        use KeyAction::*;
        // (source, cursor, selected chars, action, resulting source)
        let cases = [
            ("hello world", 0, 5, Backspace, " world"),
            ("hello world", 0, 5, DeleteCharForward, " world"),
            ("hello world", 0, 5, InsertNewline, "\n world"),
            ("hello world", 6, 3, DeleteWordBackward, "hello ld"),
            ("hello world", 0, 3, DeleteWordForward, "lo world"),
            ("hello world", 3, 4, DeleteToStartOfLine, "helorld"),
            ("ab", 0, 2, Backspace, ""),
            ("héllo", 0, 2, Backspace, "llo"),
        ];
        for (source, cpos, selected, action, result) in cases {
            let mut input = Harness::new();
            input.buf.set_source(source.to_string());
            input.win.set_cpos(cpos);
            for _ in 0..selected {
                input.test_action(SelectRight, VimMode::Insert);
            }
            input.test_action(action, VimMode::Insert);
            assert_eq!(input.buf.source(), result, "{source:?} {action:?}");
        }
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
            input.state.execute_key_action(
                &mut ctx,
                KeyAction::SelectRight,
                None,
                &mut clip,
                std::time::Instant::now(),
            );
        }
        {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input.state.execute_key_action(
                &mut ctx,
                KeyAction::KillToEndOfLine,
                None,
                &mut clip,
                std::time::Instant::now(),
            );
        }
        assert_eq!(input.buf.source(), " world");
        // Killed text lands on the TuiApp-level kill ring.
        assert_eq!(clip.kill_ring.current().text(), "hello");
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

    struct ReadClipboard(Option<String>);

    impl smelt_buffer::clipboard::Sink for ReadClipboard {
        fn read(&mut self) -> Option<String> {
            self.0.take()
        }

        fn write(&mut self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn clipboard_key_paste_replaces_visual_selection_and_exits_visual() {
        let mut input = Harness::new();
        let mut clipboard =
            crate::smelt_edit::Clipboard::new(Box::new(ReadClipboard(Some("X".into()))));
        input.state.set_vim_enabled(&mut input.win, true);
        input.buf.set_source("abc".to_string());
        input.win.set_cpos(1);
        input
            .win
            .begin_visual(crate::smelt_edit::VimMode::Visual, 1);

        let action = input.state.execute_key_action(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            KeyAction::ClipboardImage,
            None,
            &mut clipboard,
            std::time::Instant::now(),
        );

        assert!(matches!(action, Action::Redraw));
        assert_eq!(input.buf.source(), "aXc");
        assert_eq!(input.win.vim_mode(), crate::smelt_edit::VimMode::Normal);
    }

    #[test]
    fn clipboard_key_paste_replaces_visual_line_selection_and_exits_visual() {
        let mut input = Harness::new();
        let mut clipboard =
            crate::smelt_edit::Clipboard::new(Box::new(ReadClipboard(Some("TWO".into()))));
        input.state.set_vim_enabled(&mut input.win, true);
        input.buf.set_source("one\ntwo\nthree".to_string());
        input.win.set_cpos(4);
        input
            .win
            .begin_visual(crate::smelt_edit::VimMode::VisualLine, 4);

        let action = input.state.execute_key_action(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            KeyAction::ClipboardImage,
            None,
            &mut clipboard,
            std::time::Instant::now(),
        );

        assert!(matches!(action, Action::Redraw));
        assert_eq!(input.buf.source(), "one\nTWO\nthree");
        assert_eq!(input.win.vim_mode(), crate::smelt_edit::VimMode::Normal);
    }

    /// With `system_clipboard` on, a sink whose `read()` returns stale external
    /// text (the OSC 52 write never round-tripped) clobbers a fresh internal
    /// kill on `C-y`. This is the bug `system_clipboard = false` avoids.
    #[test]
    fn yank_with_system_clipboard_is_clobbered_by_stale_external_read() {
        let mut input = Harness::new();
        let mut clipboard =
            crate::smelt_edit::Clipboard::new(Box::new(ReadClipboard(Some("OUTSIDE".into()))));
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(6); // after "hello "
        for action in [KeyAction::KillToEndOfLine, KeyAction::Yank] {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input.state.execute_key_action(
                &mut ctx,
                action,
                None,
                &mut clipboard,
                std::time::Instant::now(),
            );
        }
        // C-k put "world" in the ring, but C-y pulled the stale OS clipboard.
        assert_eq!(input.buf.source(), "hello OUTSIDE");
    }

    /// A `NullSink` (what `system_clipboard = false` installs) reads `None`, so
    /// the paste-sync never clobbers: `C-k` then `C-y` round-trips the internal
    /// kill ring regardless of what the OS clipboard holds.
    #[test]
    fn yank_with_null_sink_round_trips_internal_kill_ring() {
        let mut input = Harness::new();
        let mut clipboard = crate::smelt_edit::Clipboard::null();
        input.buf.set_source("hello world".to_string());
        input.win.set_cpos(6); // after "hello "
        for action in [KeyAction::KillToEndOfLine, KeyAction::Yank] {
            let mut ctx = PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            };
            input.state.execute_key_action(
                &mut ctx,
                action,
                None,
                &mut clipboard,
                std::time::Instant::now(),
            );
        }
        assert_eq!(input.buf.source(), "hello world");
    }

    #[test]
    fn delete_selection_removes_attachments() {
        let mut input = Harness::new();
        // Insert text with an image attachment marker in the middle.
        let id = Attachment::image("img.png".into(), "data:image/png;base64,AAA".into());
        input
            .buf
            .text_mut()
            .install(format!("ab{}cd", ATTACHMENT_MARKER), vec![id]);
        input.win.set_cpos(0);
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
            input.buf.attachments.is_empty(),
            "Attachment should be removed"
        );
    }

    // ── Attachment dedup within a single message ───────────────────────

    /// Place two markers in the buffer that both carry `id`.
    fn buf_with_two_markers(input: &mut Harness, id: Attachment) {
        input.buf.text_mut().install(
            format!("pre{m}mid{m}post", m = ATTACHMENT_MARKER),
            vec![id.clone(), id],
        );
        input.win.set_cpos(input.buf.source().len());
    }

    #[test]
    fn build_content_dedups_repeated_image_in_parts() {
        let mut input = Harness::new();
        let id = Attachment::image("img.png".into(), "data:image/png;base64,AAA".into());
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
        let id1 = Attachment::image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = Attachment::image("b.png".into(), "data:image/png;base64,BBB".into());
        input
            .buf
            .text_mut()
            .install(format!("{m}{m}", m = ATTACHMENT_MARKER), vec![id1, id2]);
        input.win.set_cpos(input.buf.source().len());
        let content = input.state.build_content(&input.buf);
        assert_eq!(content.image_count(), 2);
    }

    #[test]
    fn build_content_dedups_interleaved_image_references() {
        // Pattern: img A, img B, img A again. Parts should be [A, B].
        let mut input = Harness::new();
        let id_a = Attachment::image("a.png".into(), "data:image/png;base64,AAA".into());
        let id_b = Attachment::image("b.png".into(), "data:image/png;base64,BBB".into());
        input.buf.text_mut().install(
            format!("{m}x{m}y{m}", m = ATTACHMENT_MARKER),
            vec![id_a.clone(), id_b, id_a],
        );
        input.win.set_cpos(input.buf.source().len());
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
        assert!(input.buf.attachments.is_empty());
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
            None,
        );
        assert_eq!(input.win.selection_anchor(), None);
        assert_eq!(input.buf.source(), "hi\nlong buffer");
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
    fn install_source_snaps_cpos_to_grapheme_boundary() {
        let mut input = Harness::new();
        input.state.install_source(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            "he\u{301}llo".to_string(),
            2,
            Vec::new(),
        ); // between the base `e` and its combining accent
        assert_eq!(input.win.cpos(), 1, "cpos must snap to a grapheme boundary");
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
    fn prompt_paste_and_deletion_keep_graphemes_atomic() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let mut input = Harness::new();
            let pasted = format!("a{grapheme}b");
            input.state.insert_paste(
                &mut PromptCtx {
                    buf: &mut input.buf,
                    win: &mut input.win,
                },
                pasted.clone(),
            );
            assert_eq!(input.buf.source(), pasted);

            input.win.set_cpos(1 + grapheme.len());
            input.state.backspace(&mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            });
            assert_eq!(input.buf.source(), "ab", "{grapheme:?}");
            assert_eq!(input.win.cpos(), 1, "{grapheme:?}");
        }
    }

    #[test]
    fn prompt_deletion_cursor_stays_valid_when_neighbors_form_a_grapheme() {
        let mut input = Harness::new();
        input.buf.set_source("🇨 🇦z".into());
        input.win.set_cpos("🇨 ".len());
        input.state.backspace(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });

        assert_eq!(input.buf.source(), "🇨🇦z");
        assert_eq!(
            smelt_buffer::text::snap_grapheme(input.buf.source(), input.win.cpos()),
            input.win.cpos()
        );
    }

    #[test]
    fn selection_deletion_snaps_back_when_neighbors_form_a_grapheme() {
        let mut input = Harness::new();
        input.buf.set_source("🇨 x 🇦z".into());
        let start = "🇨".len();
        let end = "🇨 x ".len();
        input.win.set_cpos(start);
        input.win.extend_selection(start);
        input.win.set_cpos(end);

        input.state.delete_selection(&mut PromptCtx {
            buf: &mut input.buf,
            win: &mut input.win,
        });

        assert_eq!(input.buf.source(), "🇨🇦z");
        assert_eq!(input.win.cpos(), 0);
    }

    #[test]
    fn attachment_insertion_advances_past_a_joined_grapheme() {
        let mut input = Harness::new();
        input.buf.set_source("\u{301}x".into());
        input.state.insert_attachment(
            &mut PromptCtx {
                buf: &mut input.buf,
                win: &mut input.win,
            },
            Attachment::image("7.png".into(), "data:7".into()),
        );

        assert_eq!(input.buf.source(), format!("{ATTACHMENT_MARKER}\u{301}x"));
        assert_eq!(
            input.win.cpos(),
            ATTACHMENT_MARKER.len_utf8() + "\u{301}".len()
        );
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
        assert!(input.buf.attachments.is_empty());
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
