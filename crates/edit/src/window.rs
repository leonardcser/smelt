use super::event::Status;
use super::text::{self, byte_to_cell, cell_to_byte};
use super::vim::{self, Action, VimContext, VimMode, VimWindowState};
use super::Buffer;
use super::Clipboard;
use super::{BufId, WinId};
use crate::Theme;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use smelt_buffer::buffer::VirtTextPos;
use smelt_term::grid::{GridSlice, Style};
use smelt_term::layout::{Gutters, Rect};

/// Per-frame paint context for `Window::render`.
#[derive(Default, Clone)]
pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
    /// Only meaningful when `focused` is true. `Block` paints at cursor pos;
    /// `Hardware` is handled by `Ui::render`; `Hidden` paints nothing.
    pub cursor_shape: CursorShape,
    /// `Arc` so per-leaf construction is a pointer bump, not a clone.
    pub theme: std::sync::Arc<Theme>,
    /// Used by `Window::render` to auto-derive visual selection ranges.
    pub vim_mode: VimMode,
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

    /// Click row → thumb top, centered on the click. Clamped to `[0, max_thumb_top()]`.
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

    /// Scroll offset → thumb top row (0-based). Inverse of `thumb_top_for_click`.
    pub(crate) fn thumb_top_for_scroll(&self, scroll_top: u16) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let scroll = scroll_top.min(max_scroll);
        ((scroll as u32 * max_thumb as u32 + max_scroll as u32 / 2) / max_scroll as u32) as u16
    }

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

/// Per-call context for [`Window::handle`]. Geometry is recomputed each frame and
/// supplied here so one `Window` drives heterogeneous backings.
pub struct EventCtx<'a> {
    pub rows: &'a [String],
    pub soft_breaks: &'a [usize],
    pub hard_breaks: &'a [usize],
    pub viewport: WindowViewport,
    pub click_count: u8,
    pub clipboard: &'a mut Clipboard,
}

/// Per-call context for [`Window::handle_mouse`].
pub struct MouseCtx<'a> {
    /// One per visual line; for unwrapped buffers equals `buffer.lines()`.
    pub rows: &'a [String],
    /// Soft-wrap byte positions in `rows.join("\n")`; word-select crosses these transparently.
    pub soft_breaks: &'a [usize],
    /// Hard `\n` byte positions; triple-click extends to the full source line.
    pub hard_breaks: &'a [usize],
    pub viewport: WindowViewport,
    pub click_count: u8,
}

#[derive(Clone, Debug)]
pub struct SplitConfig {
    pub region: String,
    pub gutters: Gutters,
}

/// How the focused window's cursor renders (single global on `Ui`).
/// `Hidden` — no cursor. `Hardware` — native terminal caret via `Ui::render`.
/// `Block { glyph, style }` — paint a cell at `(cursor_col, cursor_line)`.
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
    /// Paints `CursorLine` bg on the cursor row when focused. Off by default; list-shaped
    /// windows opt in so the selected row is visible regardless of focus.
    pub cursor_line_highlight: bool,

    /// Populated each frame by the host so scrollbar paint is available without a render-time channel.
    pub viewport: Option<WindowViewport>,

    pub text: String,
    pub attachment_ids: Vec<super::AttachmentId>,
    pub history: super::UndoHistory,
    pub readonly: bool,
    pub cpos: usize,
    /// Gates `dispatch_vim_key`; `vim_state` is always present.
    pub vim_enabled: bool,
    pub vim_mode: VimMode,
    pub vim_state: VimWindowState,
    /// Shift-selection / vim Visual anchor. `None` means no active selection.
    pub selection_anchor: Option<usize>,
    /// Preferred display column for vertical motion; measured in terminal cells.
    pub curswant: Option<usize>,
    pub scroll_top: u16,
    pub cursor_line: u16,
    pub cursor_col: u16,
    /// Keeps viewport snapped to newest row; cleared when the user scrolls away.
    pub follow_tail: bool,
    /// One-shot recenter request (vim `zz`); cleared after the next paint.
    pub pending_recenter: bool,
    /// Last cpos seen by the renderer; distinguishes cursor-move from scroll-pan.
    pub last_render_cpos: Option<usize>,
    pub cursor_positioned: bool,
    /// Double-click word-select anchor; drag extends in word units while set.
    pub drag_anchor_word: Option<(usize, usize)>,
    /// Triple-click line-select anchor; drag extends in line units while set.
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
            history: super::UndoHistory::default(),
            readonly: true,
            cpos: 0,
            vim_enabled: false,
            vim_mode: VimMode::default(),
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

    /// Set this window's vim mode and clear any pending key sequence.
    pub fn set_vim_mode(&mut self, mode: VimMode) {
        self.vim_state.set_mode(&mut self.vim_mode, mode);
    }

    /// Anchor a visual selection at `cpos` and enter the given visual mode.
    pub fn begin_visual(&mut self, mode: VimMode, cpos: usize) {
        self.vim_state.begin_visual(&mut self.vim_mode, mode, cpos);
    }

    // ── Cursor ─────────────────────────────────────────────────────────

    pub fn cursor_abs_row(&self) -> usize {
        self.scroll_top as usize + self.cursor_line as usize
    }

    /// Set `selection_anchor` to `cpos` if unset. Call before a shift-move.
    pub fn extend_selection(&mut self, cpos: usize) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(cpos);
        }
    }

    /// Resolve the shift-selection range against `src`. Both endpoints are clamped to
    /// `src.len()` and snapped to char boundaries — a stale anchor that survived a
    /// source mutation degrades to `None` instead of producing an out-of-bounds slice.
    pub fn selection_range_at(&self, cpos: usize, src: &str) -> Option<(usize, usize)> {
        let a = text::snap(src, self.selection_anchor?);
        let c = text::snap(src, cpos);
        let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
        (lo != hi).then_some((lo, hi))
    }

    pub fn selection_range(&self, rows: &[String]) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(rows);
        let buf = rows.join("\n");
        if self.vim_enabled {
            if let Some(range) = vim::visual_range(&self.vim_state, &buf, cpos, self.vim_mode) {
                return Some(range);
            }
        }
        self.selection_range_at(cpos, &buf)
    }

    /// Derive selection ranges from this window's own anchors; used when the buffer
    /// has no explicit `set_selection` override. Empty result means no paint.
    fn auto_selection_ranges(
        &self,
        buf: &Buffer,
        vim_mode: VimMode,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        let in_vim_visual =
            self.vim_enabled && matches!(vim_mode, VimMode::Visual | VimMode::VisualLine);
        if !in_vim_visual && self.selection_anchor.is_none() {
            return Vec::new();
        }
        let buf_text = buf.text();
        let range = if in_vim_visual {
            vim::visual_range(&self.vim_state, &buf_text, self.cpos, vim_mode)
        } else {
            self.selection_range_at(self.cpos, &buf_text)
        };
        let Some((s, e)) = range else {
            return Vec::new();
        };
        if s >= e {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut line_start = 0usize;
        for (idx, line) in buf.lines().iter().enumerate() {
            let line_end = line_start + line.len();
            if e > line_start && s <= line_end {
                let clip_s = s.saturating_sub(line_start).min(line.len());
                let clip_e = e.saturating_sub(line_start).min(line.len());
                let start_cell = byte_to_cell(line, clip_s) as u16;
                let end_cell = byte_to_cell(line, clip_e) as u16;
                if end_cell > start_cell {
                    out.push(smelt_buffer::buffer::SelectionRange {
                        line: idx,
                        col_start: start_cell,
                        col_end: end_cell,
                    });
                } else if line.is_empty() && s <= line_start && e > line_start {
                    out.push(smelt_buffer::buffer::SelectionRange {
                        line: idx,
                        col_start: 0,
                        col_end: 1,
                    });
                }
            }
            line_start = line_end + 1;
        }
        out
    }

    /// Select the WORD (whitespace-delimited, punctuation included) at `cpos`.
    /// `transparent` positions (soft-wrap joins) are treated as word chars.
    fn select_big_word_at_transparent(
        &mut self,
        cpos: usize,
        transparent: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (start, end) = super::text::big_word_range_at_transparent(buf, cpos, transparent)?;
        self.finish_range_select(start, end, rows, viewport_rows);
        Some((start, end))
    }

    /// Select the source line at `cpos` and enter Visual mode anchored at its start.
    /// Uses `Visual` (not `VisualLine`) so soft-wrapped lines select correctly.
    fn select_line_at(
        &mut self,
        cpos: usize,
        hard_breaks: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (start, end) = super::text::line_range_at(buf, cpos, hard_breaks)?;
        self.finish_range_select(start, end, rows, viewport_rows);
        Some((start, end))
    }

    /// Position cursor at end of `[start, end)` and anchor Visual at `start`.
    /// Re-syncs cursor coords so the highlight pass sees the correct position.
    fn finish_range_select(
        &mut self,
        start: usize,
        end: usize,
        rows: &[String],
        viewport_rows: u16,
    ) {
        // vim visual_range is inclusive at cpos; non-vim selection_range_at is exclusive.
        self.cpos = if self.vim_enabled {
            end.saturating_sub(1).max(start)
        } else {
            end
        };
        let offsets = Self::line_start_offsets(rows);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        if self.vim_enabled {
            self.vim_state
                .begin_visual(&mut self.vim_mode, VimMode::Visual, start);
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

    pub fn refocus(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            self.text.clear();
            self.cpos = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.cursor_positioned = false;
            return;
        }
        if self.vim_enabled && self.vim_mode != VimMode::Normal {
            self.vim_state.set_mode(&mut self.vim_mode, VimMode::Normal);
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

    /// Snap to the bottom. `u16::MAX` is the sentinel; the render loop clamps it.
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

    // ── Event dispatch ────────────────────────────────────────────────

    pub fn handle(&mut self, ev: super::event::Event, ctx: EventCtx<'_>) -> Status {
        use super::event::Event;
        match ev {
            Event::Key(k) => self.handle_key(k, ctx.rows, ctx.viewport.rect.height, ctx.clipboard),
            Event::Mouse(me) => {
                let (status, _) = self.handle_mouse(
                    me,
                    MouseCtx {
                        rows: ctx.rows,
                        soft_breaks: ctx.soft_breaks,
                        hard_breaks: ctx.hard_breaks,
                        viewport: ctx.viewport,
                        click_count: ctx.click_count,
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

    /// Handle a mouse event. On `Up`, returns the selected byte range `(start, end)` over
    /// the joined display rows if a selection was active (host applies the copy primitive).
    pub fn handle_mouse(
        &mut self,
        event: MouseEvent,
        ctx: MouseCtx,
    ) -> (Status, Option<(usize, usize)>) {
        let buf = ctx.rows.join("\n");
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => (self.mouse_down(event, &ctx, &buf), None),
            MouseEventKind::Drag(MouseButton::Left) => (self.mouse_drag(event, &ctx, &buf), None),
            MouseEventKind::Up(MouseButton::Left) => {
                let range = self.mouse_yank_range(&ctx, &buf);
                let status = self.mouse_up();
                (status, range)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn mouse_down(&mut self, event: MouseEvent, ctx: &MouseCtx, buf: &str) -> Status {
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
                if let Some((s, e)) = self.select_big_word_at_transparent(
                    cpos,
                    ctx.soft_breaks,
                    ctx.rows,
                    buf,
                    viewport_rows,
                ) {
                    self.drag_anchor_word = Some((s, e));
                    self.drag_anchor_line = None;
                }
                Status::Capture
            }
            3 => {
                if let Some((s, e)) =
                    self.select_line_at(cpos, ctx.hard_breaks, ctx.rows, buf, viewport_rows)
                {
                    self.drag_anchor_line = Some((s, e));
                    self.drag_anchor_word = None;
                }
                Status::Capture
            }
            _ => {
                self.drag_anchor_word = None;
                self.drag_anchor_line = None;
                if self.vim_enabled {
                    self.vim_state
                        .begin_visual(&mut self.vim_mode, VimMode::Visual, cpos);
                } else {
                    self.selection_anchor = Some(cpos);
                }
                Status::Capture
            }
        }
    }

    fn mouse_drag(&mut self, event: MouseEvent, ctx: &MouseCtx, buf: &str) -> Status {
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

    /// Selection byte range before `mouse_up` clears anchors; `None` if empty or absent.
    fn mouse_yank_range(&self, ctx: &MouseCtx, buf: &str) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(ctx.rows);
        let (start, end) = if self.vim_enabled {
            vim::visual_range(&self.vim_state, buf, cpos, self.vim_mode)?
        } else {
            self.selection_range_at(cpos, buf)?
        };
        if start >= end {
            return None;
        }
        Some((start, end))
    }

    fn mouse_up(&mut self) -> Status {
        if self.vim_enabled && matches!(self.vim_mode, VimMode::Visual | VimMode::VisualLine) {
            self.vim_state.set_mode(&mut self.vim_mode, VimMode::Normal);
        }
        self.selection_anchor = None;
        self.drag_anchor_word = None;
        self.drag_anchor_line = None;
        Status::Consumed
    }

    /// Extend drag by WORD units, keeping the original double-clicked word inside the selection.
    fn extend_word_anchored_drag(&mut self, ctx: &MouseCtx, buf: &str) {
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
                .begin_visual(&mut self.vim_mode, VimMode::Visual, new_anchor);
        } else {
            self.selection_anchor = Some(new_anchor);
        }
    }

    fn extend_line_anchored_drag(&mut self, ctx: &MouseCtx, buf: &str) {
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
                .begin_visual(&mut self.vim_mode, VimMode::Visual, new_anchor);
        } else {
            self.selection_anchor = Some(new_anchor);
        }
    }

    // ── Key dispatch ───────────────────────────────────────────────────

    pub fn handle_key(
        &mut self,
        k: KeyEvent,
        rows: &[String],
        viewport_rows: u16,
        clipboard: &mut Clipboard,
    ) -> Status {
        if rows.is_empty() {
            return Status::Ignored;
        }
        let offsets = self.mount(rows);
        if !self.dispatch_vim_key(k, clipboard) {
            return Status::Ignored;
        }
        if self.vim_enabled && self.vim_mode == VimMode::Insert {
            self.vim_state.set_mode(&mut self.vim_mode, VimMode::Normal);
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        Status::Consumed
    }

    fn dispatch_vim_key(&mut self, key: KeyEvent, clipboard: &mut Clipboard) -> bool {
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
            mode: &mut self.vim_mode,
            curswant: &mut self.curswant,
            vim_state: &mut self.vim_state,
        };
        let action = vim::handle_key(key, &mut ctx);
        self.cpos = cpos;
        !matches!(action, Action::Passthrough)
    }

    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() || delta == 0 {
            return;
        }
        let offsets = self.mount(rows);
        let (new_cpos, new_want) = text::vertical_move(&self.text, self.cpos, delta, self.curswant);
        self.curswant = Some(new_want);
        self.cpos = new_cpos;
        if self.vim_enabled && self.vim_mode == VimMode::Insert {
            self.vim_state.set_mode(&mut self.vim_mode, VimMode::Normal);
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
        let line = &rows[line_idx];
        let col_bytes = cell_to_byte(line, col);
        self.cpos = offsets[line_idx] + col_bytes;
        let landed_col = byte_to_cell(line, col_bytes);
        self.curswant = Some(landed_col);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        let max_scroll = (rows.len() as u16).saturating_sub(viewport_rows);
        self.follow_tail = self.scroll_top >= max_scroll;
    }

    pub fn render(&self, buf: &Buffer, slice: &mut GridSlice<'_>, ctx: &DrawContext) {
        use unicode_width::UnicodeWidthChar;

        let width = slice.width();
        let height = slice.height();
        let scroll = self.scroll_top as usize;
        let line_count = buf.line_count();
        let pad_left = self.config.gutters.pad_left.min(width);
        let pad_right = self
            .config
            .gutters
            .pad_right
            .min(width.saturating_sub(pad_left));
        let content_width = width.saturating_sub(pad_left).saturating_sub(pad_right);
        let cursor_row = if self.cursor_line_highlight {
            Some(self.cursor_line)
        } else {
            None
        };
        let normal_style = ctx.theme.get("Normal");
        let cursor_style = ctx.theme.get("CursorLine");
        let visual_style = ctx.theme.get("Visual");
        // Buffer override wins; fall back to window anchors.
        let selection_owned: Vec<smelt_buffer::buffer::SelectionRange>;
        let selection_ranges: &[smelt_buffer::buffer::SelectionRange] =
            if !buf.selection().is_empty() {
                buf.selection()
            } else {
                selection_owned = self.auto_selection_ranges(buf, ctx.vim_mode);
                &selection_owned[..]
            };
        // Reused per-row scratch — avoids `height` allocations of each Vec.
        let mut col_to_char: Vec<usize> = Vec::with_capacity(content_width as usize);
        let mut line_chars: Vec<char> = Vec::with_capacity(content_width as usize);
        let mut spans_buf: Vec<smelt_buffer::buffer::Span> = Vec::new();
        let mut vt_buf: Vec<smelt_buffer::buffer::VirtualText> = Vec::new();
        let mut mask_buf: Vec<bool> = Vec::with_capacity(content_width as usize);
        for row in 0..height {
            let idx = scroll + row as usize;
            let decoration = if idx < line_count {
                Some(buf.decoration_at(idx))
            } else {
                None
            };
            let mut row_style = if cursor_row == Some(row) {
                cursor_style
            } else {
                normal_style
            };
            if let Some(dec) = decoration {
                if let Some(bg) = dec.fill_bg {
                    row_style = Style {
                        bg: Some(bg),
                        ..row_style
                    };
                }
            }
            if row_style != Style::default() {
                for col in 0..width {
                    slice.set(col, row, ' ', row_style);
                }
            }
            if idx >= line_count {
                continue;
            }
            let Some(line) = buf.get_line(idx) else {
                continue;
            };
            col_to_char.clear();
            line_chars.clear();
            line_chars.extend(line.chars());
            let mut col: u16 = 0;
            for (ci, ch) in line_chars.iter().enumerate() {
                let cw = UnicodeWidthChar::width(*ch).unwrap_or(0).max(1) as u16;
                if col + cw > content_width {
                    break;
                }
                slice.set(pad_left + col, row, *ch, row_style);
                col_to_char.push(ci);
                for _ in 1..cw {
                    col_to_char.push(ci);
                }
                col += cw;
            }
            let content_end_col = col;
            spans_buf.clear();
            buf.highlights_at_into(idx, &mut spans_buf);
            for span in &spans_buf {
                let span_style = ctx.theme.resolve(span.hl);
                let style = merge_span_style(row_style, &span_style);
                let start = span.col_start.min(content_width);
                let end = span.col_end.min(content_width);
                paint_span_cells(
                    slice,
                    pad_left,
                    row,
                    start,
                    end,
                    &col_to_char,
                    &line_chars,
                    style,
                    None,
                );
                if span.hl_eol {
                    for c in end..content_width {
                        slice.set(pad_left + c, row, ' ', style);
                    }
                }
            }
            // Selection painting: after highlights (wins over base) but before virt-text.
            // Cells under `selectable = false` spans are skipped so chrome spans don't
            // receive the Visual bg. `spans_buf` is reused from the highlight pass above.
            let mask_slice: Option<&[bool]> = if selection_ranges.iter().any(|r| r.line == idx)
                && spans_buf.iter().any(|s| !s.meta.selectable)
            {
                mask_buf.clear();
                mask_buf.resize(content_width as usize, true);
                for span in spans_buf.iter().filter(|s| !s.meta.selectable) {
                    let start = span.col_start.min(content_width) as usize;
                    let end = span.col_end.min(content_width) as usize;
                    for slot in mask_buf.iter_mut().take(end).skip(start) {
                        *slot = false;
                    }
                }
                Some(mask_buf.as_slice())
            } else {
                None
            };
            for r in selection_ranges.iter().filter(|r| r.line == idx) {
                let style = merge_span_style(row_style, &visual_style);
                let start = r.col_start.min(content_width);
                let end = r.col_end.min(content_width);
                paint_span_cells(
                    slice,
                    pad_left,
                    row,
                    start,
                    end,
                    &col_to_char,
                    &line_chars,
                    style,
                    mask_slice,
                );
            }
            vt_buf.clear();
            buf.virtual_text_at_into(idx, &mut vt_buf);
            for vt in &vt_buf {
                let base = vt
                    .hl_group
                    .as_deref()
                    .map(|g| ctx.theme.get(g))
                    .unwrap_or_default();
                let style = merge_styles(row_style, base);
                let vt_width: u16 = vt
                    .text
                    .chars()
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0).max(1) as u16)
                    .sum();
                let start_col = match vt.pos {
                    VirtTextPos::Eol => content_end_col,
                    VirtTextPos::Inline | VirtTextPos::Overlay => vt.col as u16,
                    VirtTextPos::RightAlign => content_width.saturating_sub(vt_width),
                };
                let mut c = start_col;
                for ch in vt.text.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1) as u16;
                    if c + cw > content_width {
                        break;
                    }
                    slice.set(pad_left + c, row, ch, style);
                    c += cw;
                }
            }
        }

        if let Some(viewport) = self.viewport {
            paint_scrollbar(slice, viewport, &ctx.theme);
        }

        if ctx.focused {
            if let CursorShape::Block { glyph, style } = ctx.cursor_shape {
                if self.cursor_col < content_width && self.cursor_line < height {
                    slice.set(pad_left + self.cursor_col, self.cursor_line, glyph, style);
                }
            }
        }
    }
}

/// Paint a styled span across `[start, end)` columns of `row`.
/// Skips wide-char continuation cells: their `col_to_char` index repeats the leading
/// cell, and `Grid::set` for a wide char also marks the next slot as `\0`, so a second
/// paint at the continuation column would clobber the cell after the wide char.
/// `mask`, when present, gates each column (used by selection paint to honor
/// `selectable = false` spans).
#[allow(clippy::too_many_arguments)]
fn paint_span_cells(
    slice: &mut GridSlice<'_>,
    pad_left: u16,
    row: u16,
    start: u16,
    end: u16,
    col_to_char: &[usize],
    line_chars: &[char],
    style: Style,
    mask: Option<&[bool]>,
) {
    for c in start..end {
        if let Some(mask) = mask {
            if !mask.get(c as usize).copied().unwrap_or(true) {
                continue;
            }
        }
        let ci = col_to_char.get(c as usize).copied();
        if c > 0 && ci.is_some() && ci == col_to_char.get((c - 1) as usize).copied() {
            continue;
        }
        let ch = ci.and_then(|i| line_chars.get(i)).copied().unwrap_or(' ');
        slice.set(pad_left + c, row, ch, style);
    }
}

/// Paint the scrollbar. Row offset = `viewport.rect.top - slice.area().top` so splits whose
/// viewport is a sub-region of the window (e.g. prompt input area) position the bar correctly.
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
    let thumb_style = Style::new().bg(thumb.bg.or(thumb.fg).unwrap_or(crate::grid::Color::Reset));
    let track_style = Style::new().bg(track.bg.or(track.fg).unwrap_or(crate::grid::Color::Reset));
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

/// Layer `top` onto `base`; `Some` fields win, booleans OR. For full `Style` values.
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

/// Layer a `SpanStyle` onto a base `Style`. Same merge rules as `merge_styles`.
fn merge_span_style(base: Style, span: &crate::SpanStyle) -> Style {
    Style {
        fg: span.fg.or(base.fg),
        bg: span.bg.or(base.bg),
        bold: base.bold || span.bold,
        dim: base.dim || span.dim,
        italic: base.italic || span.italic,
        underline: base.underline || span.underline,
        crossedout: base.crossedout || span.crossedout,
    }
}

#[cfg(test)]
mod tests {
    use super::BufId;
    use super::*;
    use crate::grid::Grid;
    use crate::BufCreateOpts;
    use crate::Theme;

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
            theme: std::sync::Arc::new(Theme::default()),
            vim_mode: VimMode::default(),
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
        w.set_vim_mode(VimMode::Normal);
        let rows = sample_rows(30);
        let viewport = 10;
        w.jump_to_line_col(&rows, 0, 0, viewport);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.scroll_top, 0);
        w.scroll_by_lines(1, &rows, viewport);
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn refocus_on_empty_resets_cursor() {
        let mut w = make_win();
        w.cursor_line = 5;
        w.cursor_col = 3;
        w.refocus(&[], 20);
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
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 1,
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
        let bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        let bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        buf.add_highlight(0, 2, 5, crate::SpanStyle::new().dim());
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
    fn render_highlight_does_not_duplicate_wide_char_in_grid() {
        // ⚡ (U+26A1) reports unicode-width 2. The line walk paints the
        // glyph at col 1 with `\0` continuation at col 2, then the
        // highlight loop overlays the span style. Re-painting the
        // continuation cell would write a second ⚡ + clobber the next
        // char with `\0`. Pin the post-render grid: one ⚡, one \0,
        // then the next source char unscathed.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec![" \u{26A1}yolo".into()]);
        buf.add_highlight(0, 0, 7, crate::SpanStyle::new().bold());
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(1, 0).symbol, '\u{26A1}');
        assert_eq!(grid.cell(2, 0).symbol, '\0');
        assert_eq!(grid.cell(3, 0).symbol, 'y');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
        assert_eq!(grid.cell(5, 0).symbol, 'l');
        assert_eq!(grid.cell(6, 0).symbol, 'o');
        // Highlight style applied to the wide char's leading cell.
        assert!(grid.cell(1, 0).style.bold);
        assert!(grid.cell(3, 0).style.bold);
    }

    #[test]
    fn render_layers_highlight_attributes_on_cursor_row_bg() {
        // When `cursor_line_highlight` paints the cursor row with a
        // bg, a span's bold attribute layers on top: that cell ends
        // up bg=cursor and bold=true.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["hello".into()]);
        buf.add_highlight(0, 0, 3, crate::SpanStyle::new().bold());
        let mut w = make_win();
        w.cursor_line_highlight = true;
        w.cursor_line = 0;
        let mut theme = Theme::default();
        let bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        let bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        buf.set_extmark(ns, 0, 3, crate::ExtmarkOpts::virt_text("xy".into(), None));
        let w = make_win();
        let theme = Theme::default();
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        theme.set("Ghost", crate::grid::Style::new().dim());
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        let bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(238));
        theme.set("CursorLine", bg);
        // Ghost group only sets `dim`, not bg/fg.
        theme.set("Ghost", crate::grid::Style::new().dim());
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 10,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        let cursor_style = crate::grid::Style::new().bg(crate::grid::Color::White);
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
            style: crate::grid::Style::default(),
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
            style: crate::grid::Style::default(),
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
        let thumb_bg = crate::grid::Color::AnsiValue(220);
        let track_bg = crate::grid::Color::AnsiValue(238);
        theme.set(
            "SmeltScrollbarThumb",
            crate::grid::Style::new().bg(thumb_bg),
        );
        theme.set(
            "SmeltScrollbarTrack",
            crate::grid::Style::new().bg(track_bg),
        );
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        let track_bg = crate::grid::Color::AnsiValue(238);
        theme.set(
            "SmeltScrollbarTrack",
            crate::grid::Style::new().bg(track_bg),
        );
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 10,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
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
        w.set_vim_mode(VimMode::Normal);
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
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 7),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some((0, 7)));
    }

    #[test]
    fn mouse_double_click_yank_word() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
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
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 8),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some((6, 11)));
    }

    #[test]
    fn mouse_triple_click_yank_line() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
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
        };
        let (r, yank) = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 4),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some((0, 11)));
    }

    /// Highlight cols are visual columns, so a span anchored after a
    /// multi-byte glyph (here `µ`, 2 bytes / 1 col) only lands on the
    /// right cell when the upstream offset is computed from width, not
    /// byte length.
    #[test]
    fn highlight_anchored_after_multibyte_covers_next_glyph() {
        use crate::grid::Color;
        use unicode_width::UnicodeWidthStr;
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        let last_s = " 969\u{00B5}s";
        let p99_s = "2.69ms";
        buf.set_all_lines(vec![format!("  func {last_s}  {p99_s} 223")]);

        let last_col: u16 = 7;
        let last_w = UnicodeWidthStr::width(last_s) as u16;
        let p99_w = UnicodeWidthStr::width(p99_s) as u16;
        assert_ne!(last_w, last_s.len() as u16);

        let p99_col = last_col + last_w + 2;
        buf.add_highlight(
            0,
            p99_col,
            p99_col + p99_w,
            crate::SpanStyle::new().fg(Color::Yellow),
        );

        let w = make_win();
        let ctx = DrawContext {
            terminal_width: 60,
            terminal_height: 4,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(Theme::default()),
            vim_mode: VimMode::default(),
        };
        let mut grid = Grid::new(60, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 60, 1));
        w.render(&buf, &mut slice, &ctx);

        assert_eq!(grid.cell(p99_col, 0).symbol, '2');
        assert_eq!(grid.cell(p99_col, 0).style.fg, Some(Color::Yellow));
        assert_eq!(grid.cell(p99_col + p99_w, 0).style.fg, None);
    }
}
