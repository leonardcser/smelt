use super::buffer::Buffer;
use super::event::Status;
use super::grid::{GridSlice, Style};
use super::layout::{Gutters, Rect};
use super::text::{self, byte_to_cell, cell_to_byte};
use super::vim::{self, Action, VimContext, VimMode, VimWindowState};
use super::Clipboard;
use super::{BufId, WinId};
use crate::ui::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

/// Per-frame paint context handed to `Window::render`. Carries terminal
/// size + theme so renderers don't reach back into the host.
#[derive(Default, Clone)]
pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
    /// Global cursor shape for this frame. Only meaningful when
    /// `focused` is true — non-focused windows ignore it. `Block` paints
    /// the glyph + style at `(cursor_col, cursor_line)` after extmark
    /// layering; `Hardware` flows through `Ui::render` to the terminal
    /// caret and is inert in `Window::render`; `Hidden` paints nothing.
    pub cursor_shape: CursorShape,
    /// Theme registry resolved per-frame from `Ui`. Renderers read named
    /// highlight groups (`"Visual"`, `"SmeltAccent"`, …) via
    /// `theme.get(name)`; missing names return `Style::default()`.
    pub theme: Theme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewportHit {
    Scrollbar,
    Content { row: u16, col: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbarState {
    pub col: u16,
    pub total_rows: u16,
    pub viewport_rows: u16,
}

impl ScrollbarState {
    pub fn new(col: u16, total_rows: u16, viewport_rows: u16) -> Option<Self> {
        (viewport_rows > 0 && total_rows > viewport_rows).then_some(Self {
            col,
            total_rows,
            viewport_rows,
        })
    }

    fn max_scroll(&self) -> u16 {
        self.total_rows.saturating_sub(self.viewport_rows)
    }

    fn thumb_size(&self) -> u16 {
        let rows = self.viewport_rows as usize;
        let total = self.total_rows as usize;
        ((rows * rows) / total).max(1) as u16
    }

    fn max_thumb_top(&self) -> u16 {
        self.viewport_rows.saturating_sub(self.thumb_size())
    }

    /// Convert a click row (relative to viewport top) into the thumb
    /// top such that the thumb is centered on the click — the row the
    /// pointer is on lands on the middle of the thumb, not its first
    /// cell. Clamped to `[0, max_thumb_top()]`. Used by both the
    /// jump-scroll click and the in-flight drag tick so the thumb
    /// stays under the pointer.
    pub(crate) fn thumb_top_for_click(&self, rel_row: u16) -> u16 {
        let half = self.thumb_size() / 2;
        rel_row.saturating_sub(half).min(self.max_thumb_top())
    }

    pub(crate) fn scroll_from_top_for_thumb(&self, thumb_top: u16) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let thumb_top = thumb_top.min(max_thumb);
        let from_top =
            (thumb_top as u32 * max_scroll as u32 + max_thumb as u32 / 2) / max_thumb as u32;
        from_top.min(u16::MAX as u32) as u16
    }

    pub(crate) fn contains(&self, rect: Rect, row: u16, col: u16) -> bool {
        col == self.col && row >= rect.top && row < rect.bottom()
    }

    /// Thumb top row (0-based within the viewport) for a given
    /// scroll offset. Mirror of `thumb_top_for_click`'s inverse,
    /// rounded toward the nearest thumb cell. Used by paint paths
    /// to figure out where the thumb's first cell renders.
    pub(crate) fn thumb_top_for_scroll(&self, scroll_top: u16) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let scroll = scroll_top.min(max_scroll);
        ((scroll as u32 * max_thumb as u32 + max_scroll as u32 / 2) / max_scroll as u32) as u16
    }

    /// `true` if viewport-relative row `row` paints as the thumb
    /// (vs. the track) when scrolled to `scroll_top`. The thumb
    /// covers `[thumb_top, thumb_top + thumb_size)`.
    pub(crate) fn is_thumb_at(&self, scroll_top: u16, row: u16) -> bool {
        let thumb_top = self.thumb_top_for_scroll(scroll_top);
        let thumb_end = thumb_top + self.thumb_size();
        row >= thumb_top && row < thumb_end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowViewport {
    pub rect: Rect,
    pub content_width: u16,
    pub total_rows: u16,
    pub scroll_top: u16,
    pub scrollbar: Option<ScrollbarState>,
}

impl WindowViewport {
    pub fn new(
        rect: Rect,
        content_width: u16,
        total_rows: u16,
        scroll_top: u16,
        scrollbar: Option<ScrollbarState>,
    ) -> Self {
        Self {
            rect,
            content_width,
            total_rows,
            scroll_top,
            scrollbar,
        }
    }

    fn contains(&self, row: u16, col: u16) -> bool {
        self.rect.contains(row, col)
    }

    pub(crate) fn hit(&self, row: u16, col: u16) -> Option<ViewportHit> {
        if !self.contains(row, col) {
            return None;
        }
        if let Some(bar) = self.scrollbar {
            if bar.contains(self.rect, row, col) {
                return Some(ViewportHit::Scrollbar);
            }
        }
        let rel_row = row - self.rect.top;
        let rel_col = col.saturating_sub(self.rect.left);
        let max_col = self.content_width.saturating_sub(1);
        Some(ViewportHit::Content {
            row: rel_row,
            col: rel_col.min(max_col),
        })
    }
}

/// Per-call context for [`Window::handle`] and the per-event
/// helpers underneath it. The window itself does not store row
/// layout or viewport geometry — they're recomputed each frame and
/// supplied here so a single `Window` primitive can drive
/// heterogeneous backings (transcript display projection, dialog
/// buffer panel, plain split window).
///
/// Bundles per-pane data the host computed (rows, soft/hard breaks,
/// viewport, click count, vim mode, clipboard). Key dispatch reads
/// `rows` / `viewport.rect.height` / `vim_mode` / `clipboard`; mouse
/// dispatch reads everything except `clipboard`. Hosts pass full
/// data on every event — the unused fields are zero-cost references.
pub struct EventCtx<'a> {
    pub rows: &'a [String],
    pub soft_breaks: &'a [usize],
    pub hard_breaks: &'a [usize],
    pub viewport: WindowViewport,
    pub click_count: u8,
    pub vim_mode: &'a mut VimMode,
    pub clipboard: &'a mut Clipboard,
}

/// Per-call context for [`Window::handle_mouse`]. The window itself
/// does not store row layout or viewport geometry — they're recomputed
/// each frame and supplied here so a single `Window` primitive can
/// drive heterogeneous backings (transcript display projection,
/// dialog buffer panel, plain split window).
pub struct MouseCtx<'a> {
    /// Display rows, one per visual line. For buffers with no soft
    /// wrap, this is the buffer's `lines()`. For projected views
    /// (transcript) it's the post-projection rows.
    pub rows: &'a [String],
    /// Byte positions in `rows.join("\n")` of soft-wrap boundaries —
    /// the word selector treats these as transparent so a word split
    /// across two display rows still selects as one unit. Empty when
    /// the rows are not soft-wrapped.
    pub soft_breaks: &'a [usize],
    /// Byte positions in `rows.join("\n")` of *hard* line breaks —
    /// real `\n` characters in the source. Used by triple-click line
    /// selection to grow the range to the full source line.
    pub hard_breaks: &'a [usize],
    /// Painted viewport rect + content width + scrollbar geometry.
    pub viewport: WindowViewport,
    /// Click sequence number (1, 2, 3, …). Hosts that don't track
    /// click cadence can pass `1` and lose word/line click — that's
    /// fine for read-only views.
    pub click_count: u8,
    /// TuiApp-owned single-global VimMode reference. Vim mouse paths
    /// (begin Visual on click, exit Visual on mouse-up) write
    /// through this; non-vim windows ignore it.
    pub vim_mode: &'a mut VimMode,
}

#[derive(Clone, Debug)]
pub struct SplitConfig {
    pub region: String,
    pub gutters: Gutters,
}

/// How the focused window's cursor renders. Single global on `Ui`;
/// the focused window's `cursor_line` / `cursor_col` carry the
/// viewport-relative position.
///
/// * `Hidden` — no cursor paints anywhere. Read-only viewers, modal
///   dialogs without an input target, or any frame where focus does
///   not point at a window expecting a caret.
/// * `Hardware` — the terminal's native caret. `Ui::render` pulls
///   the absolute (col, row) for the focused window and emits a
///   `cursor::MoveTo` after the diff flush. Used for plain text-input
///   fields (cmdline, prompt in Insert mode, dialog input panels).
/// * `Block { glyph, style }` — paint the cell at the focused window's
///   `(cursor_col, cursor_line)` with `glyph` + `style` and suppress
///   the hardware caret. Used for vim Normal/Visual modes and the
///   ghost-text "cursor on prediction" preview.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Hidden,
    Hardware,
    Block {
        glyph: char,
        style: Style,
    },
}

pub struct Window {
    pub(crate) id: WinId,
    pub buf: BufId,
    pub config: SplitConfig,
    pub focusable: bool,
    /// Opt-in flag: paint a `CursorLine`-themed background under
    /// the visible cursor row when the window is focused. Defaults
    /// to `false` so generic content viewers (transcript, /help,
    /// /btw) stay clean. List-shaped Windows (option panels,
    /// `kind="list"` dialog leaves) flip this on so the selected
    /// row reads at a glance.
    pub cursor_line_highlight: bool,

    /// Per-frame viewport geometry. When `Some`, `Window::render`
    /// paints the scrollbar from the viewport's `scrollbar` state.
    /// Hosts repopulate each frame from layout state — the field
    /// lives on Window (not pushed in via render args) so painted
    /// splits and overlay leaves can both surface scrollbars
    /// without an extra render-time channel.
    pub viewport: Option<WindowViewport>,

    /// Flat text content for this window. For Buffer-backed windows
    /// this mirrors `buffer.text()`; for prompt editing it is the
    /// live editable source.
    pub text: String,
    /// Attachment markers inside `text`.
    pub attachment_ids: Vec<super::AttachmentId>,
    /// Undo/redo stack. `None` capacity disables undo (used for readonly
    /// buffers).
    pub history: super::undo::UndoHistory,
    /// Whether this window's text can be edited.
    pub readonly: bool,
    pub cpos: usize,
    /// Vim mode is enabled on this Window. Combined with `vim_state`
    /// it gates the keystroke dispatcher in `dispatch_vim_key`.
    pub vim_enabled: bool,
    /// Persistent per-Window vim state (Visual anchor, last `f`/`t`
    /// target, in-flight key-sequence state). Always present; the
    /// `vim_enabled` flag controls whether it's consulted.
    pub vim_state: VimWindowState,
    /// Shift-selection anchor. Vim Visual's `v`/`V` set this too (via
    /// `set_selection_anchor`) so paint/copy read one range. `None`
    /// means no active selection.
    pub selection_anchor: Option<usize>,
    /// Preferred display column for vertical motion. Set by the first
    /// vertical motion after a horizontal one; preserved across
    /// subsequent vertical motions so the cursor returns to the wanted
    /// column on longer lines. Measured in terminal cells, so wide
    /// glyphs (`⏺`, CJK) don't throw the column off.
    pub curswant: Option<usize>,
    pub scroll_top: u16,
    pub cursor_line: u16,
    pub cursor_col: u16,
    /// Autoscroll flag for append-only buffers. `true` keeps the
    /// viewport snapped to the newest row — new content flows in
    /// below and the viewport follows. Flipped to `false` as soon as
    /// the user scrolls away from the bottom; back to `true` when
    /// they scroll back to it.
    pub follow_tail: bool,
    /// One-shot "center cursor on next render" request, set by vim `zz`.
    /// Honored and cleared by the next paint that knows the viewport
    /// dimensions. Independent of `follow_tail` (which is sticky).
    pub pending_recenter: bool,
    /// Last `cpos` observed by the renderer. Compared against current
    /// `cpos` each frame to distinguish "cursor moved → ensure visible"
    /// from "wheel/scrollbar panned → leave scroll alone." `None` on
    /// first render forces an initial ensure-visible.
    pub last_render_cpos: Option<usize>,
    pub cursor_positioned: bool,
    /// Active drag-anchor span set by a double-click word-select
    /// (`[start, end)` byte range in the joined buffer). When set,
    /// drag extension grows in word units and keeps the original word
    /// inside the selection regardless of drag direction. Cleared on
    /// mouse-up or any non-mouse cursor motion.
    pub drag_anchor_word: Option<(usize, usize)>,
    /// Same as `drag_anchor_word` but for triple-click line-select.
    pub drag_anchor_line: Option<(usize, usize)>,
}

impl Window {
    pub fn new(id: WinId, buf: BufId, config: SplitConfig) -> Self {
        Self {
            id,
            buf,
            config,
            focusable: true,
            cursor_line_highlight: false,
            viewport: None,
            text: String::new(),
            attachment_ids: Vec::new(),
            history: super::undo::UndoHistory::default(),
            readonly: true,
            cpos: 0,
            vim_enabled: false,
            vim_state: VimWindowState::default(),
            selection_anchor: None,
            curswant: None,
            scroll_top: 0,
            cursor_line: 0,
            cursor_col: 0,
            follow_tail: true,
            pending_recenter: false,
            last_render_cpos: None,
            cursor_positioned: false,
            drag_anchor_word: None,
            drag_anchor_line: None,
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    // ── Vim ────────────────────────────────────────────────────────────

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        self.vim_enabled = enabled;
        if !enabled {
            self.selection_anchor = None;
        }
    }

    // ── Cursor ─────────────────────────────────────────────────────────

    pub fn cursor_abs_row(&self) -> usize {
        self.scroll_top as usize + self.cursor_line as usize
    }

    /// Latch `selection_anchor` at `cpos` if none is set. Called before
    /// a shift-movement so the first extension anchors where the
    /// cursor was before the key.
    pub fn extend_selection(&mut self, cpos: usize) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(cpos);
        }
    }

    /// Selection as a `(start, end)` byte pair against `cpos`. Returns
    /// `None` when no anchor is set or the anchor equals `cpos`.
    pub fn selection_range_at(&self, cpos: usize) -> Option<(usize, usize)> {
        let a = self.selection_anchor?;
        let (lo, hi) = if a <= cpos { (a, cpos) } else { (cpos, a) };
        (lo != hi).then_some((lo, hi))
    }

    pub fn selection_range(&self, rows: &[String], mode: VimMode) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(rows);
        if self.vim_enabled {
            if let Some(range) = vim::visual_range(&self.vim_state, &rows.join("\n"), cpos, mode) {
                return Some(range);
            }
        }
        self.selection_range_at(cpos)
    }

    /// Vim "WORD" (capital W) selection: the token at `cpos` is any
    /// whitespace-delimited run, punctuation included. `transparent`
    /// byte positions are crossed by the boundary walk as if they
    /// were word chars (used for soft-wrap `\n` so a word broken
    /// across display rows selects as one unit); must be sorted
    /// ascending. Cursor lands at the last char of the selection so
    /// the visual-range render covers the whole word.
    fn select_big_word_at_transparent(
        &mut self,
        cpos: usize,
        transparent: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
        mode: &mut VimMode,
    ) -> Option<(usize, usize)> {
        let (start, end) = super::text::big_word_range_at_transparent(buf, cpos, transparent)?;
        self.finish_range_select(start, end, rows, viewport_rows, mode);
        Some((start, end))
    }

    /// Select the source line containing `cpos` and enter Visual mode
    /// anchored at the line's start. `hard_breaks` lists byte positions
    /// of `\n` characters that are real line breaks (not soft-wrap
    /// continuations), sorted ascending. We use plain `Visual` rather
    /// than `VisualLine` because `VisualLine` snaps to display rows
    /// (every `\n`), which would collapse selection to a single
    /// wrapped row for soft-wrapped content. Returns the byte range of
    /// the selected source line.
    fn select_line_at(
        &mut self,
        cpos: usize,
        hard_breaks: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
        mode: &mut VimMode,
    ) -> Option<(usize, usize)> {
        let (start, end) = super::text::line_range_at(buf, cpos, hard_breaks)?;
        self.finish_range_select(start, end, rows, viewport_rows, mode);
        Some((start, end))
    }

    /// Park the cursor at the last char of `[start, end)` and anchor a
    /// Visual selection at `start`. Re-syncs `cursor_line`/`cursor_col`
    /// from the new `cpos` so the selection-highlight pass (which
    /// derives cpos via `compute_cpos`) sees the moved cursor — without
    /// this, the highlight extends only to the original click position.
    fn finish_range_select(
        &mut self,
        start: usize,
        end: usize,
        rows: &[String],
        viewport_rows: u16,
        mode: &mut VimMode,
    ) {
        // Vim visual_range uses next_char_boundary to include the char
        // at cpos, so cpos lands on the last selected char. Non-vim
        // selection_range_at is exclusive at cpos, so cpos must land
        // one past the last selected char to include it.
        self.cpos = if self.vim_enabled {
            end.saturating_sub(1).max(start)
        } else {
            end
        };
        let offsets = Self::line_start_offsets(rows);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        if self.vim_enabled {
            self.vim_state.begin_visual(mode, VimMode::Visual, start);
        } else {
            self.selection_anchor = Some(start);
        }
    }

    pub fn resync(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.text = rows.join("\n");
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    pub fn refocus(&mut self, rows: &[String], viewport_rows: u16, mode: &mut VimMode) {
        if rows.is_empty() {
            self.text.clear();
            self.cpos = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.cursor_positioned = false;
            return;
        }
        if self.vim_enabled && *mode != VimMode::Normal {
            self.vim_state.set_mode(mode, VimMode::Normal);
        }
        if !self.cursor_positioned {
            let total = rows.len();
            let last_line = total.saturating_sub(1);
            let offsets = Self::line_start_offsets(rows);
            self.text = rows.join("\n");
            self.cpos = offsets[last_line];
            self.sync_from_cpos(rows, &offsets, viewport_rows);
            self.cursor_positioned = true;
        } else {
            let offsets = self.mount(rows);
            self.sync_from_cpos(rows, &offsets, viewport_rows);
        }
        if self.curswant.is_none() {
            self.curswant = Some(self.cursor_col as usize);
        }
    }

    pub fn reanchor_to_visible_row(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.text = rows.join("\n");
        let total = rows.len() as u16;
        let max = total.saturating_sub(viewport_rows);
        self.scroll_top = self.scroll_top.min(max);
        let cursor_line = self.cursor_line.min(viewport_rows.saturating_sub(1));
        let target_line = (self.scroll_top + cursor_line) as usize;
        let target_line = target_line.min(rows.len() - 1);
        let line = &rows[target_line];
        let want = self.curswant.unwrap_or(self.cursor_col as usize);
        let col_bytes = cell_to_byte(line, want);
        self.cpos = offsets[target_line] + col_bytes;
        self.cursor_col = byte_to_cell(line, col_bytes) as u16;
        self.cursor_line = cursor_line;
    }

    // ── Follow-tail ────────────────────────────────────────────────────

    /// Snap to the bottom of the buffer and re-enable autoscroll.
    /// `scroll_top = u16::MAX` is the "go to bottom" sentinel; the
    /// per-frame clamp in the render loop resolves it to `max_scroll`.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_top = u16::MAX;
        self.follow_tail = true;
    }

    // ── Navigation ─────────────────────────────────────────────────────

    pub fn compute_cpos(&self, rows: &[String]) -> usize {
        let offsets = Self::line_start_offsets(rows);
        self.visible_cpos(rows, &offsets)
    }

    fn line_start_offsets(rows: &[String]) -> Vec<usize> {
        let mut v = Vec::with_capacity(rows.len());
        let mut acc = 0usize;
        for r in rows {
            v.push(acc);
            acc += r.len() + 1;
        }
        v
    }

    fn visible_cpos(&self, rows: &[String], offsets: &[usize]) -> usize {
        let total = rows.len();
        if total == 0 {
            return 0;
        }
        let line_idx = (self.scroll_top as usize + self.cursor_line as usize).min(total - 1);
        offsets[line_idx] + cell_to_byte(&rows[line_idx], self.cursor_col as usize)
    }

    fn sync_from_cpos(&mut self, rows: &[String], offsets: &[usize], viewport_rows: u16) {
        let total = rows.len();
        if total == 0 {
            return;
        }
        let tail_byte = *offsets.last().unwrap() + rows.last().map_or(0, |r| r.len());
        self.cpos = self.cpos.min(tail_byte);
        let line_idx = match offsets.binary_search(&self.cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = &rows[line_idx];
        let byte_col = self.cpos.saturating_sub(offsets[line_idx]);
        self.cursor_col = byte_to_cell(line, byte_col) as u16;
        let line_idx = line_idx as u16;
        let viewport_bottom = self
            .scroll_top
            .saturating_add(viewport_rows.saturating_sub(1));
        if line_idx > viewport_bottom {
            self.scroll_top = line_idx.saturating_sub(viewport_rows.saturating_sub(1));
        } else if line_idx < self.scroll_top {
            self.scroll_top = line_idx;
        }
        self.cursor_line = line_idx.saturating_sub(self.scroll_top);
    }

    fn mount(&mut self, rows: &[String]) -> Vec<usize> {
        let offsets = Self::line_start_offsets(rows);
        self.text = rows.join("\n");
        self.cpos = self.visible_cpos(rows, &offsets);
        offsets
    }

    // ── Unified event dispatch ────────────────────────────────────────

    /// Single public entry consuming an `Event` plus per-pane `ctx`.
    /// Dispatches `Key` events to the vim/edit key path and `Mouse`
    /// events to the cursor/selection path; non-input events return
    /// `Status::Ignored` so the host can route them itself
    /// (terminal-focus tracking, paste-side effects, resize bookkeeping).
    ///
    /// Hosts populate `ctx` from `UiHost::rows_for` / `breaks_for` /
    /// `viewport_for` plus the App-owned `vim_mode` and clipboard
    /// before calling. Unused fields per event kind (e.g. clipboard
    /// for mouse, breaks for keys) are passed through as zero-cost
    /// references — Window simply doesn't read them.
    pub fn handle(&mut self, ev: super::event::Event, ctx: EventCtx<'_>) -> Status {
        use super::event::Event;
        match ev {
            Event::Key(k) => self.handle_key(
                k,
                ctx.rows,
                ctx.viewport.rect.height,
                ctx.vim_mode,
                ctx.clipboard,
            ),
            Event::Mouse(me) => {
                let (status, _) = self.handle_mouse(
                    me,
                    MouseCtx {
                        rows: ctx.rows,
                        soft_breaks: ctx.soft_breaks,
                        hard_breaks: ctx.hard_breaks,
                        viewport: ctx.viewport,
                        click_count: ctx.click_count,
                        vim_mode: ctx.vim_mode,
                    },
                );
                status
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
                Status::Ignored
            }
        }
    }

    // ── Mouse dispatch ─────────────────────────────────────────────────

    /// Handle a single mouse event using the supplied `MouseCtx`
    /// (rows, soft/hard line breaks, viewport, click count).
    /// Encapsulates the cursor and Visual selection logic that the
    /// transcript pane has used for ages: click-to-position cursor,
    /// double-click word-select, triple-click line-select, drag
    /// extension anchored to the original word/line when applicable.
    /// The window's `drag_anchor_*` fields are managed internally so
    /// successive `Drag` events extend by the right unit.
    ///
    /// On `MouseUp`, if a selection was active, the selected text is
    /// returned as `Some(text)` so the host can push it to the
    /// clipboard. Window still clears its own selection state.
    pub fn handle_mouse(
        &mut self,
        event: MouseEvent,
        mut ctx: MouseCtx,
    ) -> (Status, Option<String>) {
        // Build the joined buffer once and pass it down. Mouse helpers
        // operate on this `&str` instead of `self.text`, which
        // lets surfaces whose `self.text` is *not* `rows.join("\n")`
        // (the prompt — source buffer ≠ wrapped display rows) reuse
        // `Window::handle_mouse` directly. The transcript and dialog
        // buffer panels still keep `self.text == rows.join("\n")`
        // via their existing sync paths; the buffer arg just doesn't
        // need it to be true.
        let buf = ctx.rows.join("\n");
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                (self.mouse_down(event, &mut ctx, &buf), None)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                (self.mouse_drag(event, &mut ctx, &buf), None)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let yank = self.mouse_yank_text(&ctx, &buf);
                let status = self.mouse_up(&mut ctx, &buf);
                (status, yank)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn mouse_down(&mut self, event: MouseEvent, ctx: &mut MouseCtx, buf: &str) -> Status {
        // Hit-test against the painted viewport: anything that lands
        // on the scrollbar or outside the rect is the host's problem
        // (scrollbar drag latching, focus shift, …).
        let Some(hit) = ctx.viewport.hit(event.row, event.column) else {
            return Status::Ignored;
        };
        let ViewportHit::Content {
            row: rel_row,
            col: rel_col,
        } = hit
        else {
            return Status::Ignored;
        };
        if ctx.rows.is_empty() {
            return Status::Consumed;
        }

        let viewport_rows = ctx.viewport.rect.height;
        let line_idx = (self.scroll_top as usize + rel_row as usize).min(ctx.rows.len() - 1);
        self.jump_to_line_col(ctx.rows, line_idx, rel_col as usize, viewport_rows);
        let cpos = self.cpos;

        match ctx.click_count {
            2 => {
                // Vim "WORD" (whitespace-delimited, punctuation in)
                // matches what users expect from a double-click in
                // a code preview: `foo.bar(baz)` selects whole.
                if let Some((s, e)) = self.select_big_word_at_transparent(
                    cpos,
                    ctx.soft_breaks,
                    ctx.rows,
                    buf,
                    viewport_rows,
                    ctx.vim_mode,
                ) {
                    self.drag_anchor_word = Some((s, e));
                    self.drag_anchor_line = None;
                }
                Status::Capture
            }
            3 => {
                if let Some((s, e)) = self.select_line_at(
                    cpos,
                    ctx.hard_breaks,
                    ctx.rows,
                    buf,
                    viewport_rows,
                    ctx.vim_mode,
                ) {
                    self.drag_anchor_line = Some((s, e));
                    self.drag_anchor_word = None;
                }
                Status::Capture
            }
            _ => {
                // Single click: anchor a Visual selection at the click
                // so a subsequent drag grows from this point. Vim and
                // non-vim paths anchor differently (vim's Visual range
                // reads cpos directly; non-vim uses `selection_anchor`).
                self.drag_anchor_word = None;
                self.drag_anchor_line = None;
                if self.vim_enabled {
                    self.vim_state
                        .begin_visual(ctx.vim_mode, VimMode::Visual, cpos);
                } else {
                    self.selection_anchor = Some(cpos);
                }
                Status::Capture
            }
        }
    }

    fn mouse_drag(&mut self, event: MouseEvent, ctx: &mut MouseCtx, buf: &str) -> Status {
        // Drag past the rect edges still extends — clamp the cell to
        // the viewport's content area so the cursor lands on the
        // nearest visible position. Host handles edge-autoscroll on
        // a separate timer.
        let viewport_rows = ctx.viewport.rect.height;
        if viewport_rows == 0 || ctx.rows.is_empty() {
            return Status::Consumed;
        }
        let rel_row = event
            .row
            .saturating_sub(ctx.viewport.rect.top)
            .min(viewport_rows.saturating_sub(1));
        let rel_col = event
            .column
            .saturating_sub(ctx.viewport.rect.left)
            .min(ctx.viewport.content_width.saturating_sub(1));
        let line_idx = (self.scroll_top as usize + rel_row as usize).min(ctx.rows.len() - 1);
        self.jump_to_line_col(ctx.rows, line_idx, rel_col as usize, viewport_rows);

        if self.drag_anchor_word.is_some() {
            self.extend_word_anchored_drag(ctx, buf);
        } else if self.drag_anchor_line.is_some() {
            self.extend_line_anchored_drag(ctx, buf);
        } else if !self.vim_enabled {
            self.extend_selection(self.cpos);
        }
        Status::Consumed
    }

    /// Compute the text to yank from the current selection state,
    /// *before* `mouse_up` clears the anchors. Returns `None` when
    /// no selection is active or the range is empty.
    fn mouse_yank_text(&self, ctx: &MouseCtx, buf: &str) -> Option<String> {
        let cpos = self.compute_cpos(ctx.rows);
        let (start, end) = if self.vim_enabled {
            vim::visual_range(&self.vim_state, buf, cpos, *ctx.vim_mode)?
        } else {
            self.selection_range_at(cpos)?
        };
        let start = start.min(buf.len());
        let end = end.min(buf.len());
        if start >= end {
            return None;
        }
        Some(buf[start..end].to_string())
    }

    fn mouse_up(&mut self, ctx: &mut MouseCtx, _buf: &str) -> Status {
        // The user's gesture is over: clear all selection state so a
        // fresh click starts a fresh selection. Owning this here means
        // every consumer (transcript, prompt, dialog buffer) gets the
        // same lifecycle for free — no bespoke clear-anchor code in
        // the host adapters. Clipboard side effects are the host's
        // job; Window only owns its selection state.
        if self.vim_enabled && matches!(*ctx.vim_mode, VimMode::Visual | VimMode::VisualLine) {
            self.vim_state.set_mode(ctx.vim_mode, VimMode::Normal);
        }
        self.selection_anchor = None;
        self.drag_anchor_word = None;
        self.drag_anchor_line = None;
        Status::Consumed
    }

    /// Word-anchored drag extension: keep the originally-double-clicked
    /// word inside the selection while the drag grows by full WORD
    /// units, flipping the visual anchor as the drag crosses back over
    /// the original word.
    fn extend_word_anchored_drag(&mut self, ctx: &mut MouseCtx, buf: &str) {
        let Some((ws, we)) = self.drag_anchor_word else {
            return;
        };
        let p = self.compute_cpos(ctx.rows);
        let (new_cpos, new_anchor) = if p >= we {
            let far = super::text::word_range_at_transparent(buf, p, ctx.soft_breaks)
                .map(|(_, e)| e.saturating_sub(1).max(ws))
                .unwrap_or(p.max(we.saturating_sub(1)));
            (far, ws)
        } else if p < ws {
            let near = super::text::word_range_at_transparent(buf, p, ctx.soft_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, we.saturating_sub(1).max(ws))
        } else {
            (we.saturating_sub(1).max(ws), ws)
        };
        self.cpos = new_cpos;
        if self.vim_enabled {
            self.vim_state
                .begin_visual(ctx.vim_mode, VimMode::Visual, new_anchor);
        } else {
            self.selection_anchor = Some(new_anchor);
        }
    }

    fn extend_line_anchored_drag(&mut self, ctx: &mut MouseCtx, buf: &str) {
        let Some((ls, le)) = self.drag_anchor_line else {
            return;
        };
        let p = self.compute_cpos(ctx.rows);
        let (new_cpos, new_anchor) = if p >= le {
            let far = super::text::line_range_at(buf, p, ctx.hard_breaks)
                .map(|(_, e)| e.saturating_sub(1).max(ls))
                .unwrap_or(p.max(le.saturating_sub(1)));
            (far, ls)
        } else if p < ls {
            let near = super::text::line_range_at(buf, p, ctx.hard_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, le.saturating_sub(1).max(ls))
        } else {
            (le.saturating_sub(1).max(ls), ls)
        };
        self.cpos = new_cpos;
        if self.vim_enabled {
            self.vim_state
                .begin_visual(ctx.vim_mode, VimMode::Visual, new_anchor);
        } else {
            self.selection_anchor = Some(new_anchor);
        }
    }

    // ── Key dispatch ───────────────────────────────────────────────────

    pub(crate) fn handle_key(
        &mut self,
        k: KeyEvent,
        rows: &[String],
        viewport_rows: u16,
        mode: &mut VimMode,
        clipboard: &mut Clipboard,
    ) -> Status {
        if rows.is_empty() {
            return Status::Ignored;
        }
        let offsets = self.mount(rows);
        if !self.dispatch_vim_key(k, mode, clipboard) {
            return Status::Ignored;
        }
        if self.vim_enabled && *mode == VimMode::Insert {
            self.vim_state.set_mode(mode, VimMode::Normal);
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        Status::Consumed
    }

    fn dispatch_vim_key(
        &mut self,
        key: KeyEvent,
        mode: &mut VimMode,
        clipboard: &mut Clipboard,
    ) -> bool {
        if !self.vim_enabled {
            return false;
        }
        let key = match key.code {
            KeyCode::Up => KeyEvent {
                code: KeyCode::Char('k'),
                ..key
            },
            KeyCode::Down => KeyEvent {
                code: KeyCode::Char('j'),
                ..key
            },
            KeyCode::Left => KeyEvent {
                code: KeyCode::Char('h'),
                ..key
            },
            KeyCode::Right => KeyEvent {
                code: KeyCode::Char('l'),
                ..key
            },
            _ => key,
        };
        let mut cpos = self.cpos;
        let mut ctx = VimContext {
            buf: &mut self.text,
            cpos: &mut cpos,
            attachments: &mut self.attachment_ids,
            history: &mut self.history,
            clipboard,
            mode,
            curswant: &mut self.curswant,
            vim_state: &mut self.vim_state,
        };
        let action = vim::handle_key(key, &mut ctx);
        self.cpos = cpos;
        !matches!(action, Action::Passthrough)
    }

    /// Shift `scroll_top` by `delta` rows, clamped to
    pub fn scroll_by_lines(
        &mut self,
        delta: isize,
        rows: &[String],
        viewport_rows: u16,
        mode: &mut VimMode,
    ) {
        if rows.is_empty() || delta == 0 {
            return;
        }
        let offsets = self.mount(rows);
        let (new_cpos, new_want) = text::vertical_move(&self.text, self.cpos, delta, self.curswant);
        self.curswant = Some(new_want);
        self.cpos = new_cpos;
        if self.vim_enabled && *mode == VimMode::Insert {
            self.vim_state.set_mode(mode, VimMode::Normal);
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        let max_scroll = (rows.len() as u16).saturating_sub(viewport_rows);
        self.follow_tail = self.scroll_top >= max_scroll;
    }

    fn jump_to_line_col(
        &mut self,
        rows: &[String],
        line_idx: usize,
        col: usize,
        viewport_rows: u16,
    ) {
        if rows.is_empty() {
            return;
        }
        let line_idx = line_idx.min(rows.len() - 1);
        let offsets = Self::line_start_offsets(rows);
        // Intentionally do NOT write `self.text` here. Mouse
        // helpers are pure over `(rows, &str buf)` so the prompt — whose
        // `self.text` is the source buffer, not the wrapped display
        // rows — can run through `Window::handle_mouse` without losing
        // its source content.
        let line = &rows[line_idx];
        let col_bytes = cell_to_byte(line, col);
        self.cpos = offsets[line_idx] + col_bytes;
        let landed_col = byte_to_cell(line, col_bytes);
        self.curswant = Some(landed_col);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        let max_scroll = (rows.len() as u16).saturating_sub(viewport_rows);
        self.follow_tail = self.scroll_top >= max_scroll;
    }

    /// Paint visible buffer lines into `slice`, starting at this
    /// window's `scroll_top`. Each row of the slice maps 1:1 to a
    /// buffer line; lines longer than `slice.width()` truncate at
    /// the right edge.
    ///
    /// When `cursor_line_highlight` is on, the cursor row
    /// (`cursor_line` viewport offset) gets a `CursorLine`
    /// theme-driven background — the seam list-shaped Buffer Windows
    /// use for "selected item" highlighting. The flag is off by
    /// default so generic content viewers (transcript, /help, /btw)
    /// stay clean; list-shaped Windows opt in. Selection paints
    /// regardless of focus so non-focusable list leaves (picker
    /// overlays) still show their selection while keys flow
    /// elsewhere.
    ///
    /// When `viewport.scrollbar` is set, the scrollbar paints over
    /// the right edge column the viewport designates. When
    /// `ctx.focused` is true and `ctx.cursor_shape == Block`, the
    /// block cursor cell paints over `(cursor_col, cursor_line)`
    /// after extmark layering. The `Hardware` cursor variant is read
    /// by `Ui::render` and emitted as a `cursor::MoveTo` after the
    /// diff flush — `Window::render` itself does nothing for it.
    pub fn render(&self, buf: &Buffer, slice: &mut GridSlice<'_>, ctx: &DrawContext) {
        let width = slice.width();
        let height = slice.height();
        let scroll = self.scroll_top as usize;
        let line_count = buf.line_count();
        let cursor_row = if self.cursor_line_highlight {
            Some(self.cursor_line)
        } else {
            None
        };
        let cursor_style = ctx.theme.get("CursorLine");
        for row in 0..height {
            let row_style = if cursor_row == Some(row) {
                cursor_style
            } else {
                Style::default()
            };
            // Paint the row background first so trailing space
            // beyond the line content also picks up the cursor
            // highlight.
            if cursor_row == Some(row) {
                for col in 0..width {
                    slice.set(col, row, ' ', row_style);
                }
            }
            let idx = scroll + row as usize;
            if idx >= line_count {
                continue;
            }
            let Some(line) = buf.get_line(idx) else {
                continue;
            };
            for (col, ch) in line.chars().take(width as usize).enumerate() {
                slice.set(col as u16, row, ch, row_style);
            }
            // Layered highlight painting: walk highlight extmarks
            // anchored on this row and overlay each span's style on
            // top of `row_style`. Span styles carry resolved colors
            // (parsers/Lua have already looked up theme groups), so
            // no theme reads happen here. Cell symbols stay as the
            // already-painted line glyphs; only attributes change.
            let line_chars: Vec<char> = line.chars().take(width as usize).collect();
            for span in buf.highlights_at(idx) {
                let style = merge_span_style(row_style, &span.style);
                let start = span.col_start.min(width);
                let end = span.col_end.min(width);
                for col in start..end {
                    let ch = line_chars.get(col as usize).copied().unwrap_or(' ');
                    slice.set(col, row, ch, style);
                }
            }
            // Virtual text painting: walk virt_text extmarks anchored
            // on this row and overwrite cells starting at the
            // extmark's `col`. Style resolves the extmark's `hl_group`
            // through the theme (missing → `Style::default()`) and
            // layers on top of `row_style` so cursor-row highlight
            // bg shows through when the virt_text style omits bg.
            for vt in buf.virtual_text_at(idx) {
                let base = vt
                    .hl_group
                    .as_deref()
                    .map(|g| ctx.theme.get(g))
                    .unwrap_or_default();
                let style = merge_styles(row_style, base);
                for (col, ch) in (vt.col as u16..).zip(vt.text.chars()) {
                    if col >= width {
                        break;
                    }
                    slice.set(col, row, ch, style);
                }
            }
        }

        if let Some(viewport) = self.viewport {
            paint_scrollbar(slice, viewport, &ctx.theme);
        }

        if ctx.focused {
            if let CursorShape::Block { glyph, style } = ctx.cursor_shape {
                if self.cursor_col < width && self.cursor_line < height {
                    slice.set(self.cursor_col, self.cursor_line, glyph, style);
                }
            }
        }
    }
}

/// Paint the scrollbar described by `viewport.scrollbar` into the
/// window's `slice`. `viewport.rect` is in absolute terminal
/// coordinates; the scrollbar paints at `viewport.rect.top -
/// slice.area().top` rows down from the slice origin so painted
/// splits whose viewport covers only a sub-region of the window
/// (the prompt's input area) place the scrollbar correctly. For
/// surfaces where window rect == viewport rect (transcript,
/// overlay leaves) the row offset is zero and behaviour matches
/// the prior version.
fn paint_scrollbar(slice: &mut GridSlice<'_>, viewport: WindowViewport, theme: &super::Theme) {
    let Some(bar) = viewport.scrollbar else {
        return;
    };
    let width = slice.width();
    let height = slice.height();
    let area = slice.area();
    let local_col = bar.col.saturating_sub(area.left);
    if local_col >= width {
        return;
    }
    let row_offset = viewport.rect.top.saturating_sub(area.top);
    if row_offset >= height {
        return;
    }
    let thumb = theme.get("SmeltScrollbarThumb");
    let track = theme.get("SmeltScrollbarTrack");
    let thumb_style = Style::bg(
        thumb
            .bg
            .or(thumb.fg)
            .unwrap_or(crossterm::style::Color::Reset),
    );
    let track_style = Style::bg(
        track
            .bg
            .or(track.fg)
            .unwrap_or(crossterm::style::Color::Reset),
    );
    let avail = height.saturating_sub(row_offset);
    let rows = bar.viewport_rows.min(avail);
    for row in 0..rows {
        let style = if bar.is_thumb_at(viewport.scroll_top, row) {
            thumb_style
        } else {
            track_style
        };
        slice.set(local_col, row_offset + row, ' ', style);
    }
}

/// Layer a foreground `Style` on top of a base `Style`. Same merge
/// rule as `merge_span_style` but for full `Style` values (used by
/// virt-text painting where the extmark's `hl_group` resolves through
/// the theme to a `Style`, not a `SpanStyle`).
fn merge_styles(base: Style, top: Style) -> Style {
    Style {
        fg: top.fg.or(base.fg),
        bg: top.bg.or(base.bg),
        bold: base.bold || top.bold,
        dim: base.dim || top.dim,
        italic: base.italic || top.italic,
        underline: base.underline || top.underline,
        crossedout: base.crossedout || top.crossedout,
    }
}

/// Layer a `SpanStyle` (extmark highlight) on top of a base `Style`
/// (row default). `Some` fields on the span override; `None` keeps
/// the base. Boolean attributes OR together so `bold` / `dim` /
/// `italic` accumulate across layers.
fn merge_span_style(base: Style, span: &crate::ui::buffer::SpanStyle) -> Style {
    Style {
        fg: span.fg.or(base.fg),
        bg: span.bg.or(base.bg),
        bold: base.bold || span.bold,
        dim: base.dim || span.dim,
        italic: base.italic || span.italic,
        underline: base.underline,
        crossedout: base.crossedout,
    }
}

#[cfg(test)]
mod tests {
    use super::BufId;
    use super::*;
    use crate::ui::buffer::BufCreateOpts;
    use crate::ui::grid::Grid;
    use crate::ui::theme::Theme;

    fn make_win() -> Window {
        Window::new(
            WinId(1),
            BufId(1),
            SplitConfig {
                region: "test".into(),
                gutters: Gutters::default(),
            },
        )
    }

    fn sample_rows(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    fn ctx() -> DrawContext {
        DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: Theme::default(),
        }
    }

    #[test]
    fn scroll_to_bottom_sets_follow_tail() {
        let mut w = make_win();
        w.follow_tail = false;
        w.scroll_top = 10;
        w.scroll_to_bottom();
        assert_eq!(w.scroll_top, u16::MAX);
        assert!(w.follow_tail);
    }

    #[test]
    fn scroll_by_lines_moves_cursor_down() {
        let mut w = make_win();
        w.set_vim_enabled(true);
        let rows = sample_rows(30);
        let viewport = 10;
        let mut mode = VimMode::Normal;
        w.jump_to_line_col(&rows, 0, 0, viewport);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.scroll_top, 0);
        w.scroll_by_lines(1, &rows, viewport, &mut mode);
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn refocus_on_empty_resets_cursor() {
        let mut w = make_win();
        let mut mode = VimMode::Normal;
        w.cursor_line = 5;
        w.cursor_col = 3;
        w.refocus(&[], 20, &mut mode);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.cursor_col, 0);
    }

    #[test]
    fn jump_to_last_line_scrolls_to_bottom() {
        let mut w = make_win();
        let rows = sample_rows(50);
        let viewport = 10;
        w.jump_to_line_col(&rows, 49, 0, viewport);
        assert_eq!(w.scroll_top, 40);
        assert_eq!(w.cursor_line, 9);
    }

    #[test]
    fn cursor_abs_row_top_relative() {
        let mut w = make_win();
        w.scroll_top = 10;
        w.cursor_line = 5;
        assert_eq!(w.cursor_abs_row(), 15);
    }

    fn click_event(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        use crossterm::event::KeyModifiers;
        MouseEvent {
            kind,
            row,
            column: col,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn viewport_for(rows: &[String], rect: Rect) -> WindowViewport {
        WindowViewport::new(rect, rect.width, rows.len() as u16, 0, None)
    }

    fn hard_breaks(rows: &[String]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut acc = 0usize;
        for (i, r) in rows.iter().enumerate() {
            if i + 1 < rows.len() {
                acc += r.len();
                out.push(acc);
                acc += 1;
            }
        }
        out
    }

    #[test]
    fn click_positions_cursor_and_captures() {
        let mut w = make_win();
        let mut mode = VimMode::Normal;
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 1,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 1, 7),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.cursor_col, 7);
        assert!(w.selection_anchor.is_some());
    }

    #[test]
    fn follow_tail_default_true() {
        let w = make_win();
        assert!(w.follow_tail);
    }

    #[test]
    fn render_paints_visible_lines_from_scroll_top() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec![
            "alpha".into(),
            "bravo".into(),
            "charlie".into(),
            "delta".into(),
        ]);
        let mut w = make_win();
        w.scroll_top = 1;
        let mut grid = Grid::new(10, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 2));
        w.render(&buf, &mut slice, &ctx());
        assert_eq!(grid.cell(0, 0).symbol, 'b');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
        assert_eq!(grid.cell(0, 1).symbol, 'c');
        assert_eq!(grid.cell(6, 1).symbol, 'e');
    }

    #[test]
    fn render_truncates_at_slice_width() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abcdefghij".into()]);
        let w = make_win();
        let mut grid = Grid::new(5, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 5, 1));
        w.render(&buf, &mut slice, &ctx());
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(4, 0).symbol, 'e');
    }

    #[test]
    fn render_stops_when_buffer_runs_short() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["only".into()]);
        let w = make_win();
        let mut grid = Grid::new(8, 4);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 8, 4));
        w.render(&buf, &mut slice, &ctx());
        assert_eq!(grid.cell(0, 0).symbol, 'o');
        // Rows 1..3 stay empty.
        assert_eq!(grid.cell(0, 1).symbol, ' ');
        assert_eq!(grid.cell(0, 3).symbol, ' ');
    }

    #[test]
    fn render_highlights_cursor_row_when_opted_in_and_focused() {
        // List-shaped Window with `cursor_line_highlight = true`:
        // the row at `cursor_line` (relative to the viewport) gets
        // the `CursorLine` theme bg.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "bravo".into(), "charlie".into()]);
        let mut w = make_win();
        w.cursor_line_highlight = true;
        w.cursor_line = 1; // second visible row
        let mut theme = Theme::default();
        let bg = crate::ui::grid::Style::bg(crossterm::style::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 3);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 3));
        w.render(&buf, &mut slice, &ctx);
        // Cursor row text picks up the highlight bg.
        assert_eq!(grid.cell(0, 1).symbol, 'b');
        assert_eq!(grid.cell(0, 1).style.bg, bg.bg);
        // Trailing cells of the cursor row also pick up the bg.
        assert_eq!(grid.cell(9, 1).style.bg, bg.bg);
        // Non-cursor rows stay default.
        assert_ne!(grid.cell(0, 0).style.bg, bg.bg);
        assert_ne!(grid.cell(0, 2).style.bg, bg.bg);
    }

    #[test]
    fn render_skips_cursor_highlight_without_opt_in() {
        // Default `cursor_line_highlight = false` — focused content
        // viewers (transcript, /help, /btw) stay clean.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "bravo".into()]);
        let w = make_win();
        let mut theme = Theme::default();
        let bg = crate::ui::grid::Style::bg(crossterm::style::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 2));
        w.render(&buf, &mut slice, &ctx);
        // No cursor highlight even when focused, because opt-in flag is off.
        assert_ne!(grid.cell(0, 0).style.bg, bg.bg);
    }

    #[test]
    fn render_paints_highlight_extmarks_over_row_style() {
        // Buffer carries a single highlight extmark on row 0
        // covering cols 2..5 with `dim`. After render, cells in
        // that range have `dim = true`; cells outside don't.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abcdefgh".into()]);
        buf.add_highlight(0, 2, 5, crate::ui::buffer::SpanStyle::dim());
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(2, 0).symbol, 'c');
        assert!(grid.cell(2, 0).style.dim);
        assert!(grid.cell(4, 0).style.dim);
        // Cell at col 5 is the exclusive end — not dim.
        assert!(!grid.cell(5, 0).style.dim);
        // Cell before the span — not dim.
        assert!(!grid.cell(1, 0).style.dim);
    }

    #[test]
    fn render_layers_highlight_attributes_on_cursor_row_bg() {
        // When `cursor_line_highlight` paints the cursor row with a
        // bg, a span's bold attribute layers on top: that cell ends
        // up bg=cursor and bold=true.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["hello".into()]);
        buf.add_highlight(0, 0, 3, crate::ui::buffer::SpanStyle::bold());
        let mut w = make_win();
        w.cursor_line_highlight = true;
        w.cursor_line = 0;
        let mut theme = Theme::default();
        let bg = crate::ui::grid::Style::bg(crossterm::style::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Span-covered cell: bg from cursor row + bold from span.
        assert_eq!(grid.cell(0, 0).style.bg, bg.bg);
        assert!(grid.cell(0, 0).style.bold);
        // Outside span: bg from cursor row, no bold.
        assert_eq!(grid.cell(4, 0).style.bg, bg.bg);
        assert!(!grid.cell(4, 0).style.bold);
    }

    #[test]
    fn render_paints_cursor_highlight_unfocused() {
        // List-shaped windows (`cursor_line_highlight = true`) keep
        // selection painted regardless of focus — picker overlays are
        // non-focusable yet still need to show the selected row.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "bravo".into()]);
        let mut w = make_win();
        w.cursor_line_highlight = true;
        let mut theme = Theme::default();
        let bg = crate::ui::grid::Style::bg(crossterm::style::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 2));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).style.bg, bg.bg);
    }

    #[test]
    fn render_paints_virt_text_after_line_content() {
        // Set virt_text at col=2 ("hi") on row 0 — paints over the
        // cells starting at col 2 with the virt_text characters.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        buf.set_virtual_text(0, "ghost".into(), None);
        // `set_virtual_text` anchors at col=0; rewrite that extmark to
        // anchor at col=3 (past line end) so it paints in the trailing
        // space cells.
        buf.clear_virtual_text(0);
        let ns = buf.create_namespace("test");
        buf.set_extmark(
            ns,
            0,
            3,
            crate::ui::buffer::ExtmarkOpts::virt_text("xy".into(), None),
        );
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Line content paints first (a, b, c) then virt_text starting
        // at col 3 paints "xy".
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(2, 0).symbol, 'c');
        assert_eq!(grid.cell(3, 0).symbol, 'x');
        assert_eq!(grid.cell(4, 0).symbol, 'y');
    }

    #[test]
    fn render_virt_text_resolves_hl_group_through_theme() {
        // virt_text with `hl_group = "Ghost"` picks up the theme's
        // `Ghost` style (dim) when painting.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["".into()]);
        buf.set_virtual_text(0, "ghost".into(), Some("Ghost".into()));
        let w = make_win();
        let mut theme = Theme::default();
        theme.set("Ghost", crate::ui::grid::Style::dim());
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'g');
        assert!(grid.cell(0, 0).style.dim);
        assert!(grid.cell(4, 0).style.dim);
        // No virt_text paints past col 5; cell still default.
        assert!(!grid.cell(5, 0).style.dim);
    }

    #[test]
    fn render_clips_virt_text_at_slice_width() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["".into()]);
        buf.set_virtual_text(0, "abcdefghij".into(), None);
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(5, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 5, 1));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(4, 0).symbol, 'e');
        // Slice is only 5 cells wide; the rest of "fghij" never
        // reaches the grid.
    }

    #[test]
    fn render_layers_virt_text_on_cursor_row_bg() {
        // virt_text with no bg of its own, painted on the cursor-
        // highlighted row, picks up the cursor row bg through the
        // merge.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["".into()]);
        buf.set_virtual_text(0, "g".into(), Some("Ghost".into()));
        let mut w = make_win();
        w.cursor_line_highlight = true;
        w.cursor_line = 0;
        let mut theme = Theme::default();
        let bg = crate::ui::grid::Style::bg(crossterm::style::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        // Ghost group only sets `dim`, not bg/fg.
        theme.set("Ghost", crate::ui::grid::Style::dim());
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'g');
        assert_eq!(grid.cell(0, 0).style.bg, bg.bg);
        assert!(grid.cell(0, 0).style.dim);
    }

    #[test]
    fn render_paints_block_cursor_glyph_over_buffer_cell() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.cursor_line = 0;
        w.cursor_col = 1;
        let cursor_style = crate::ui::grid::Style::bg(crossterm::style::Color::White);
        let mut ctx = ctx();
        ctx.focused = true;
        ctx.cursor_shape = CursorShape::Block {
            glyph: 'b',
            style: cursor_style,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Block cursor paints the glyph and overrides the buffer-text bg.
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(1, 0).style.bg, cursor_style.bg);
        // Adjacent cells keep the buffer text untouched.
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(2, 0).symbol, 'c');
    }

    #[test]
    fn render_skips_block_cursor_when_unfocused() {
        // Block cursor only paints on the focused window — non-focused
        // windows (other splits, overlay leaves under modals) ignore
        // the global cursor_shape.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.cursor_line = 0;
        w.cursor_col = 1;
        let mut ctx = ctx();
        ctx.focused = false;
        ctx.cursor_shape = CursorShape::Block {
            glyph: 'X',
            style: crate::ui::grid::Style::default(),
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Buffer text stays; no `X` painted.
        assert_eq!(grid.cell(1, 0).symbol, 'b');
    }

    #[test]
    fn render_block_cursor_outside_slice_is_clipped() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.cursor_line = 5;
        w.cursor_col = 99;
        let mut ctx = ctx();
        ctx.focused = true;
        ctx.cursor_shape = CursorShape::Block {
            glyph: '!',
            style: crate::ui::grid::Style::default(),
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        // Should not panic, no `!` written anywhere.
        w.render(&buf, &mut slice, &ctx);
        for col in 0..10 {
            assert_ne!(grid.cell(col, 0).symbol, '!');
        }
    }

    #[test]
    fn render_hardware_cursor_is_inert_in_window_render() {
        // Hardware cursor flows through Ui::render to the terminal
        // caret; Window::render itself paints nothing extra for it.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.cursor_line = 0;
        w.cursor_col = 1;
        let mut ctx = ctx();
        ctx.focused = true;
        ctx.cursor_shape = CursorShape::Hardware;
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Buffer text untouched at the cursor col.
        assert_eq!(grid.cell(1, 0).symbol, 'b');
    }

    #[test]
    fn render_paints_scrollbar_thumb_at_top_for_scroll_zero() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(sample_rows(40));
        let mut w = make_win();
        w.viewport = Some(WindowViewport::new(
            Rect::new(0, 0, 20, 10),
            19,
            40,
            0,
            ScrollbarState::new(19, 40, 10),
        ));
        let mut theme = Theme::default();
        let thumb_bg = crossterm::style::Color::AnsiValue(220);
        let track_bg = crossterm::style::Color::AnsiValue(238);
        theme.set("SmeltScrollbarThumb", crate::ui::grid::Style::bg(thumb_bg));
        theme.set("SmeltScrollbarTrack", crate::ui::grid::Style::bg(track_bg));
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(20, 10);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 10));
        w.render(&buf, &mut slice, &ctx);
        // At scroll_top=0, thumb paints from row 0; track fills lower rows.
        assert_eq!(grid.cell(19, 0).style.bg, Some(thumb_bg));
        assert_eq!(grid.cell(19, 9).style.bg, Some(track_bg));
    }

    #[test]
    fn render_skips_scrollbar_when_no_overflow() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(sample_rows(5));
        let mut w = make_win();
        // 5 rows fit in 10 row viewport; ScrollbarState::new returns None.
        w.viewport = Some(WindowViewport::new(
            Rect::new(0, 0, 20, 10),
            20,
            5,
            0,
            ScrollbarState::new(19, 5, 10),
        ));
        let mut theme = Theme::default();
        let track_bg = crossterm::style::Color::AnsiValue(238);
        theme.set("SmeltScrollbarTrack", crate::ui::grid::Style::bg(track_bg));
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme,
        };
        let mut grid = Grid::new(20, 10);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 10));
        w.render(&buf, &mut slice, &ctx);
        // No scrollbar: rightmost column's bg untouched (Reset/None).
        assert_ne!(grid.cell(19, 0).style.bg, Some(track_bg));
    }

    #[test]
    fn mouse_drag_yank_on_up() {
        let mut w = make_win();
        let mut mode = VimMode::Normal;
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Down on 'h' (row 0, col 0)
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());

        // Drag to 'o' in "world" (row 0, col 7)
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 7),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert!(yank.is_none());

        // Up — selected text "hello wo" is returned
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 7),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some("hello w".into()));
    }

    #[test]
    fn mouse_double_click_yank_word() {
        let mut w = make_win();
        let mut mode = VimMode::Normal;
        let rows: Vec<String> = vec!["hello world".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Double-click on "world" (row 0, col 8)
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 2,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 8),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none()); // yank on Down, not Up

        // Up returns the selected word
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 2,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 8),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some("world".into()));
    }

    #[test]
    fn mouse_triple_click_yank_line() {
        let mut w = make_win();
        let mut mode = VimMode::Normal;
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Triple-click on the first line
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 4),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());

        // Up returns the selected line
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
            vim_mode: &mut mode,
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 4),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some("hello world".into()));
    }
}
