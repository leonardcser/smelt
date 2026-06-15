use super::event::Status;
use super::gutter::GutterProvider;
use super::text;
use super::vim::{self, Action, VimContext, VimMode, VimWindowState};
use super::Buffer;
use super::Clipboard;
use super::{BufId, UndoHistory, WinId};
use crate::row::{row_to_usize, DocPosition, DocRange, MaterializedRows, RowIndex};
use crate::Theme;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use smelt_buffer::buffer::VirtTextPos;
use smelt_buffer::wrap_layout::WrappedLayout;
use smelt_term::grid::{GridSlice, Style};
use smelt_term::layout::{Gutters, Rect};
use std::sync::Arc;

mod row_text;
pub use row_text::{RowTextState, RowYankFlash, ViewerCommand};

/// Per-frame paint context for `Window::render`.
#[derive(Default, Clone)]
pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    /// Whether keyboard focus is on this leaf. Drives focus-only paint decisions
    /// (e.g. some highlight groups). Independent of cursor ownership - a leaf
    /// can be focused without owning the cursor (e.g. while a drag on another
    /// leaf has captured it) or vice versa.
    pub focused: bool,
    /// `Block` paints a glyph at the cursor position; `Hidden` paints nothing.
    /// Set only on the leaf returned by `Ui::active_cursor_leaf` so exactly one
    /// leaf renders a block per frame.
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
pub struct TextHit<P> {
    pub row: RowIndex,
    pub cell_col: usize,
    pub cpos: P,
    pub kind: TextHitKind<P>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextHitKind<P> {
    Selectable { before: P, after: P },
    LeadingChrome { nearest: Option<P> },
    TrailingChrome { nearest: Option<P> },
    AllChrome,
    EmptyRow,
    Outside,
}

impl<P> TextHit<P> {
    pub fn is_selectable(&self) -> bool {
        matches!(self.kind, TextHitKind::Selectable { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MouseTextEndpoint {
    cpos: usize,
    includes_cell: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseEndpointMode {
    RequireContentHit,
    ClampToContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbarState {
    pub col: u16,
    pub total_rows: RowIndex,
    pub viewport_rows: u16,
}

impl ScrollbarState {
    pub fn new(col: u16, total_rows: RowIndex, viewport_rows: u16) -> Option<Self> {
        (viewport_rows > 0 && total_rows > viewport_rows as RowIndex).then_some(Self {
            col,
            total_rows,
            viewport_rows,
        })
    }

    fn max_scroll(&self) -> RowIndex {
        self.total_rows
            .saturating_sub(self.viewport_rows as RowIndex)
    }

    fn thumb_size(&self) -> u16 {
        let rows = self.viewport_rows as u128;
        let total = self.total_rows.max(1) as u128;
        ((rows * rows) / total).clamp(1, self.viewport_rows as u128) as u16
    }

    fn max_thumb_top(&self) -> u16 {
        self.viewport_rows.saturating_sub(self.thumb_size())
    }

    /// Click row → thumb top, centered on the click. Clamped to `[0, max_thumb_top()]`.
    pub(crate) fn thumb_top_for_click(&self, rel_row: u16) -> u16 {
        let half = self.thumb_size() / 2;
        rel_row.saturating_sub(half).min(self.max_thumb_top())
    }

    pub(crate) fn scroll_from_top_for_thumb(&self, thumb_top: u16) -> RowIndex {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let thumb_top = thumb_top.min(max_thumb);
        let from_top =
            (thumb_top as u128 * max_scroll as u128 + max_thumb as u128 / 2) / max_thumb as u128;
        from_top.min(RowIndex::MAX as u128) as RowIndex
    }

    pub(crate) fn contains(&self, rect: Rect, row: u16, col: u16) -> bool {
        col == self.col && row >= rect.top && row < rect.bottom()
    }

    /// Scroll offset → thumb top row (0-based). Inverse of `thumb_top_for_click`.
    pub(crate) fn thumb_top_for_scroll(&self, scroll_top: RowIndex) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let scroll = scroll_top.min(max_scroll);
        ((scroll as u128 * max_thumb as u128 + max_scroll as u128 / 2) / max_scroll as u128) as u16
    }

    pub(crate) fn is_thumb_at(&self, scroll_top: RowIndex, row: u16) -> bool {
        let thumb_top = self.thumb_top_for_scroll(scroll_top);
        let thumb_end = thumb_top + self.thumb_size();
        row >= thumb_top && row < thumb_end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowViewport {
    pub rect: Rect,
    pub content_width: u16,
    pub total_rows: RowIndex,
    pub scroll_top: RowIndex,
    pub scrollbar: Option<ScrollbarState>,
    /// Cells reserved on the left for the data-driven gutter column (line numbers, signs, …),
    /// before `Gutters::pad_left`. Zero when no `GutterProvider` is attached.
    pub gutter_width: u16,
}

impl WindowViewport {
    pub fn new(
        rect: Rect,
        content_width: u16,
        total_rows: RowIndex,
        scroll_top: RowIndex,
        scrollbar: Option<ScrollbarState>,
    ) -> Self {
        Self {
            rect,
            content_width,
            total_rows,
            scroll_top,
            scrollbar,
            gutter_width: 0,
        }
    }

    pub fn with_gutter_width(mut self, gutter_width: u16) -> Self {
        self.gutter_width = gutter_width;
        self
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

pub fn clamp_scroll(scroll_top: RowIndex, total_rows: RowIndex, viewport_rows: u16) -> RowIndex {
    scroll_top.min(total_rows.saturating_sub(viewport_rows as RowIndex))
}

pub fn scroll_to_show(scroll_top: RowIndex, target: RowIndex, viewport_rows: u16) -> RowIndex {
    let h = viewport_rows.max(1) as RowIndex;
    if target < scroll_top {
        target
    } else if target >= scroll_top.saturating_add(h) {
        target + 1 - h
    } else {
        scroll_top
    }
}

pub fn materialized_row_range(
    anchor: RowIndex,
    total_rows: RowIndex,
    viewport_rows: u16,
    overscan_rows: RowIndex,
) -> std::ops::Range<RowIndex> {
    if total_rows == 0 {
        return 0..0;
    }
    let visible = viewport_rows.max(1) as RowIndex;
    let len = visible
        .saturating_add(overscan_rows.saturating_mul(2))
        .min(total_rows);
    let max_start = total_rows.saturating_sub(len);
    let start = anchor.saturating_sub(overscan_rows).min(max_start);
    start..start.saturating_add(len)
}

/// Per-call context for [`Window::handle`]. Geometry is recomputed each frame and
/// supplied here so one `Window` drives heterogeneous backings.
pub struct EventCtx<'a> {
    pub soft_breaks: &'a [usize],
    pub hard_breaks: &'a [usize],
    pub viewport: WindowViewport,
    pub click_count: u8,
    pub clipboard: &'a mut Clipboard,
    /// Host clock at dispatch; threaded into [`VimContext::now`] so yank
    /// flash deadlines respect the virtual clock under sim/fuzz.
    pub now: std::time::Instant,
}

/// Per-call context for [`Window::handle_mouse`].
#[derive(Clone, Copy)]
pub struct MouseCtx<'a> {
    /// Soft-wrap byte positions in `buf.lines().join("\n")`; word-select crosses these transparently.
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
/// `Hidden` - no cursor. `Block { glyph, style, pos }` - paint a cell at `pos`
/// when set, else at the window-derived `(cursor_col, cursor_row - scroll_top)`.
/// `pos` is the host's projected (screen-relative) `(col, row)` override for
/// panes that wrap or format their buffer before paint (transcript, prompt).
/// The terminal's hardware caret is hidden for the lifetime of the app -
/// every cursor we show is painted into the grid as a styled cell, which keeps
/// large redraws atomic with the rest of the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Hidden,
    Block {
        glyph: char,
        style: Style,
        pos: Option<(u16, u16)>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerticalScroll {
    #[default]
    Pinned,
    Tail,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowTextState {
    pub(crate) cpos: usize,
    pub(crate) vim_enabled: bool,
    pub(crate) vim_mode: VimMode,
    pub(crate) vim_state: VimWindowState,
    /// Shift-selection / vim Visual anchor. `None` means no active selection.
    pub(crate) selection_anchor: Option<usize>,
    /// Preferred display column for vertical motion; measured in terminal cells.
    pub(crate) curswant: Option<usize>,
    /// Local visual-row index of the cursor (0-based in the backing buffer's
    /// materialized row space). Add `row_base` to compare with `scroll_top`.
    pub(crate) cursor_row: RowIndex,
    /// Virtual row/document state for readonly viewers whose backing buffer is a
    /// materialized slice of a larger row space. `None` means the backing buffer
    /// is the full scrollable extent.
    pub(crate) row_text: RowTextState,
    /// Cell-column of the cursor within its visual row. Derived from `cpos`
    /// via `sync_from_cpos`.
    pub(crate) cursor_col: u16,
    /// Last cpos seen by the renderer; distinguishes cursor-move from scroll-pan.
    pub(crate) last_render_cpos: Option<usize>,
    pub(crate) cursor_positioned: bool,
    /// Double-click word-select anchor; drag extends in word units while set.
    pub(crate) drag_anchor_word: Option<(usize, usize)>,
    /// Triple-click line-select anchor; drag extends in line units while set.
    pub(crate) drag_anchor_line: Option<(usize, usize)>,
    /// Cpos of a single-click press awaiting drag; promoted to a selection on the
    /// first `Drag` event. A bare press-release with no motion leaves no selection.
    pub(crate) pending_press: Option<usize>,
    pub(crate) pending_press_includes_cell: bool,
    pub(crate) selection_anchor_includes_cell: bool,
    pub(crate) drag_endpoint_includes_cell: bool,
    /// Moving end of an active mouse drag-select, in editable-byte space. `None`
    /// outside a drag. The renderer paints the cursor/CursorLine at this byte's
    /// projected row when set, and the selection range is `(selection_anchor,
    /// drag_endpoint)`. `mouse_up` commits this into `cpos` only for caret
    /// leaves (`is_caret_leaf`); otherwise the value is discarded and `cpos`
    /// returns to its pre-drag position.
    pub(crate) drag_endpoint: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowSurface {
    kind: WindowSurfaceKind,
    text: WindowTextState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowSurfaceKind {
    EditableText,
    ReadonlyText,
    SelectableText,
    List { focusable: bool },
    Inert,
}

impl Default for WindowSurface {
    fn default() -> Self {
        Self::editable_text()
    }
}

impl WindowSurface {
    pub fn editable_text() -> Self {
        Self::new(WindowSurfaceKind::EditableText)
    }

    pub fn readonly_text() -> Self {
        Self::new(WindowSurfaceKind::ReadonlyText)
    }

    pub fn selectable_text() -> Self {
        Self::new(WindowSurfaceKind::SelectableText)
    }

    pub fn inert() -> Self {
        Self::new(WindowSurfaceKind::Inert)
    }

    pub fn list(focusable: bool) -> Self {
        Self::new(WindowSurfaceKind::List { focusable })
    }

    fn new(kind: WindowSurfaceKind) -> Self {
        Self {
            kind,
            text: WindowTextState::default(),
        }
    }

    fn text(&self) -> &WindowTextState {
        &self.text
    }

    fn text_mut(&mut self) -> &mut WindowTextState {
        &mut self.text
    }

    fn with_text(mut self, text: WindowTextState) -> Self {
        self.text = text;
        self
    }

    pub fn accepts_focus(self) -> bool {
        match self.kind {
            WindowSurfaceKind::EditableText | WindowSurfaceKind::ReadonlyText => true,
            WindowSurfaceKind::List { focusable } => focusable,
            WindowSurfaceKind::SelectableText | WindowSurfaceKind::Inert => false,
        }
    }

    pub fn has_caret(self) -> bool {
        matches!(
            self.kind,
            WindowSurfaceKind::EditableText | WindowSurfaceKind::ReadonlyText
        )
    }

    pub fn supports_text_selection(self) -> bool {
        matches!(
            self.kind,
            WindowSurfaceKind::ReadonlyText | WindowSurfaceKind::SelectableText
        )
    }

    pub fn supports_search(self) -> bool {
        matches!(self.kind, WindowSurfaceKind::ReadonlyText)
    }

    pub fn accepts_wheel_scroll(self) -> bool {
        matches!(self.kind, WindowSurfaceKind::List { .. })
    }

    fn is_selectable_text(self) -> bool {
        matches!(self.kind, WindowSurfaceKind::SelectableText)
    }
}

pub struct Window {
    pub(crate) id: WinId,
    pub buf: BufId,
    pub config: SplitConfig,
    /// Optional per-row gutter (line numbers, signs, …). Paints into the leftmost
    /// `gutter_width()` cells of each row; content paint shifts right by the same.
    /// `None` = no gutter column, no width reserved.
    pub gutter: Option<Arc<dyn GutterProvider>>,
    /// Whether long lines wrap to the window's content width on render.
    pub wrap: bool,
    /// For editable wrapped inputs, reserve an empty visual row when a row ends
    /// exactly at the right edge so the caret has a visible end-of-line cell.
    /// Display-only wrapped panes leave this off to avoid synthetic blank rows.
    pub wrap_cursor_padding: bool,
    /// Visual-row layout derived from the buffer's lines + this window's width.
    /// Refreshed by `ensure_layout` before paint; render and coordinate helpers
    /// read it instead of indexing the buffer directly.
    pub(crate) layout: WrappedLayout,
    /// `(source_tick, width, wrap, wrap_cursor_padding)` last used to compute
    /// `layout`; `None` forces a rebuild on the next `ensure_layout`.
    layout_key: Option<(u64, u16, bool, bool)>,
    surface: WindowSurface,
    /// Caret-style cursorline: paints `CursorLine` bg on the cursor row
    /// **only when this window is focused**. Models Neovim's `'cursorline'`:
    /// the cursor lives in this window, this is where it is. Off by default.
    /// Set on caret leaves (transcript, code/diff viewers) where an unfocused
    /// sibling pane should not show a stale cursor row.
    pub cursor_line: bool,
    /// List-style selection highlight: paints `CursorLine` bg on the row at
    /// `cursor_row` **regardless of focus**. Models "this is the active
    /// option in the picker." Off by default. Set on list-shaped leaves
    /// whose selection state needs to remain visible while an external
    /// input (e.g. a sibling search box) drives navigation.
    pub selection_highlight: bool,
    /// Suppress the block caret even when this focusable leaf owns keyboard
    /// focus. Live dashboard panes use this so periodic text refreshes don't
    /// make a stale byte-position cursor appear to jitter across changing rows.
    pub hide_cursor: bool,

    /// Populated each frame by the host so scrollbar paint is available without a render-time channel.
    pub viewport: Option<WindowViewport>,

    scroll_top: RowIndex,
    /// Logical anchor for `scroll_top` - `(changedtick, logical_row, byte_offset)`
    /// of the chunk that was at the top of the viewport when scroll was last
    /// set. Restored after a width/wrap-driven layout rebuild so resize keeps
    /// the same logical row anchored at the top instead of letting the
    /// visual-row counter drift across reflow. The changedtick guard skips
    /// restoration when the buffer content was replaced under us (e.g. the
    /// transcript projection rebuilds its buffer on every frame), since the
    /// `(lrow, byte)` would no longer reference the same content.
    pub(crate) scroll_anchor: Option<(u64, usize, usize)>,
    /// Vertical scroll policy. `scroll_top` is the resolved viewport row used for paint;
    /// this records whether the next frame should preserve that row or resolve to tail.
    scroll_state: VerticalScroll,
    /// Leftmost visible cell column. `0` means the row starts at its first
    /// character; non-zero values pan the viewport rightward. Wrapped rows
    /// stay at `0` (wrapping moves overflow to the next visual row); pre-
    /// formatted rows are the primary user of horizontal scroll.
    pub scroll_left: u16,
    /// One-shot recenter request (vim `zz`); cleared after the next paint.
    pub pending_recenter: bool,
    /// One-shot "scroll so the cursor row is on-screen"; cleared after the next paint.
    /// Used when callers set a cursor position before the viewport height is known
    /// (e.g. opening a list dialog with an initial selection partway down the list).
    pub pending_scroll_to_cursor: bool,
    /// `(scroll_top, tail-follow)` last emitted via `WinEvent::Scrolled`.
    pub(crate) last_emitted_scroll: Option<(RowIndex, bool)>,
    /// `(rect, content_width)` last emitted via `WinEvent::Resized`. Tracked
    /// here so the dispatcher can fire only when the leaf's geometry
    /// actually changed since the last frame.
    pub(crate) last_emitted_resize: Option<(Rect, u16)>,
}

impl Window {
    pub fn new(id: WinId, buf: BufId, config: SplitConfig) -> Self {
        Self {
            id,
            buf,
            config,
            gutter: None,
            // Direct callers (host code, tests) default to no-wrap; the Lua API
            // overrides this to `true` so end-user content panes wrap by default.
            wrap: false,
            wrap_cursor_padding: false,
            layout: WrappedLayout::default(),
            layout_key: None,
            surface: WindowSurface::default(),
            cursor_line: false,
            selection_highlight: false,
            hide_cursor: false,
            viewport: None,
            scroll_top: 0,
            scroll_anchor: None,
            scroll_state: VerticalScroll::Pinned,
            scroll_left: 0,
            pending_recenter: false,
            pending_scroll_to_cursor: false,
            last_emitted_scroll: None,
            last_emitted_resize: None,
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    pub fn surface(&self) -> WindowSurface {
        self.surface
    }

    pub(crate) fn text_state(&self) -> &WindowTextState {
        self.surface.text()
    }

    pub(crate) fn text_state_mut(&mut self) -> &mut WindowTextState {
        self.surface.text_mut()
    }

    pub(crate) fn row_text_state(&self) -> &RowTextState {
        &self.surface.text().row_text
    }

    pub(crate) fn row_text_state_mut(&mut self) -> &mut RowTextState {
        &mut self.surface.text_mut().row_text
    }

    pub fn cpos(&self) -> usize {
        self.surface.text().cpos
    }

    pub fn set_cpos(&mut self, cpos: usize) {
        self.surface.text_mut().cpos = cpos;
    }

    pub fn selection_anchor(&self) -> Option<usize> {
        self.surface.text().selection_anchor
    }

    pub fn set_selection_anchor(&mut self, anchor: Option<usize>) {
        self.surface.text_mut().selection_anchor = anchor;
    }

    pub fn clear_selection_anchor(&mut self) {
        self.set_selection_anchor(None);
    }

    pub fn curswant(&self) -> Option<usize> {
        self.surface.text().curswant
    }

    pub fn set_curswant(&mut self, curswant: Option<usize>) {
        self.surface.text_mut().curswant = curswant;
    }

    pub fn vim_state(&self) -> &VimWindowState {
        &self.surface.text().vim_state
    }

    pub fn shift_vim_visual_anchor(&mut self, delta: usize) {
        self.surface.text_mut().vim_state.shift_visual_anchor(delta);
    }

    pub fn clear_vim_visual_anchor(&mut self) {
        self.surface.text_mut().vim_state.clear_visual_anchor();
    }

    pub fn has_drag_anchor(&self) -> bool {
        let text = self.surface.text();
        text.drag_anchor_word.is_some() || text.drag_anchor_line.is_some()
    }

    pub fn set_last_render_cpos(&mut self, cpos: Option<usize>) {
        self.surface.text_mut().last_render_cpos = cpos;
    }

    pub fn vim_enabled(&self) -> bool {
        self.surface.text().vim_enabled
    }

    pub fn vim_mode(&self) -> VimMode {
        self.surface.text().vim_mode
    }

    pub fn handle_vim_key(
        &mut self,
        buf: &mut Buffer,
        clipboard: &mut Clipboard,
        key: KeyEvent,
        now: std::time::Instant,
    ) -> Action {
        let (text, history) = buf.edit_refs();
        let text_state = self.surface.text_mut();
        let mut ctx = VimContext {
            buf: text,
            cpos: &mut text_state.cpos,
            history,
            clipboard,
            mode: &mut text_state.vim_mode,
            curswant: &mut text_state.curswant,
            vim_state: &mut text_state.vim_state,
            now,
        };
        vim::handle_key(key, &mut ctx)
    }

    pub fn set_surface(&mut self, surface: WindowSurface) {
        let text = *self.surface.text();
        self.surface = surface.with_text(text);
    }

    pub fn accepts_focus(&self) -> bool {
        self.surface.accepts_focus()
    }

    pub fn supports_text_selection(&self) -> bool {
        self.surface.supports_text_selection()
    }

    pub fn supports_search(&self) -> bool {
        self.surface.supports_search()
    }

    pub fn supports_wheel_scroll(&self) -> bool {
        self.surface.accepts_wheel_scroll() || self.config.gutters.scrollbar
    }

    pub fn scroll_top(&self) -> RowIndex {
        self.scroll_top
    }

    pub fn scroll_state(&self) -> VerticalScroll {
        self.scroll_state
    }

    /// Apply a resolved viewport row while preserving tail mode.
    pub fn set_resolved_scroll(&mut self, row: RowIndex) {
        self.scroll_top = row;
    }

    pub fn resolve_tail_scroll(&mut self, row: RowIndex) {
        self.scroll_top = row;
        if let Some(viewport) = self.viewport.as_mut() {
            viewport.scroll_top = row;
        }
        self.scroll_state = VerticalScroll::Tail;
    }

    /// Reserved cells for the gutter column for this buffer; `0` when no provider attached.
    pub fn gutter_width(&self, buf: &Buffer) -> u16 {
        self.gutter.as_ref().map(|g| g.width(buf)).unwrap_or(0)
    }

    /// Rebuild `layout` if the buffer's content or width changed. Called by the
    /// host before paint so render and hit-test see a consistent view. Rows
    /// whose decoration sets `pre_formatted = true` (parser output, markdown
    /// tables, diff hunks) stay as identity chunks regardless of `self.wrap`.
    pub fn ensure_layout(&mut self, buf: &Buffer, width: u16) {
        let key = (
            buf.changedtick(),
            width,
            self.wrap,
            self.wrap_cursor_padding,
        );
        if self.layout_key == Some(key) {
            return;
        }
        let prev_width_wrap = self
            .layout_key
            .map(|(_, w, wrap, cursor_padding)| (w, wrap, cursor_padding));
        if let Some((prev_tick, prev_width, prev_wrap, prev_cursor_padding)) = self.layout_key {
            if prev_width == width
                && prev_wrap == self.wrap
                && prev_cursor_padding == self.wrap_cursor_padding
            {
                if let Some(edit) = buf.last_line_edit() {
                    if edit.before_tick == prev_tick
                        && edit.after_tick == buf.changedtick()
                        && edit.old_end == edit.old_line_count
                    {
                        if self.wrap_cursor_padding {
                            self.layout.replace_suffix_from_buffer_with_cursor_padding(
                                buf, edit.start, width, self.wrap,
                            );
                        } else {
                            self.layout
                                .replace_suffix_from_buffer(buf, edit.start, width, self.wrap);
                        }
                        self.layout_key = Some(key);
                        return;
                    }
                }
            }
        }
        // Snapshot the cursor's distance from `scroll_top` before rebuild.
        // Only meaningful when the layout matches the buffer - otherwise
        // `cursor_row` was computed against a stale layout. Buffers replaced
        // under us mid-frame (transcript projection) reach this point with a
        // mismatched tick and skip the snapshot here; the render loop takes
        // it earlier via `cursor_screen_row_in_viewport`.
        let cursor_screen_row = if self.layout_matches(buf) {
            self.absolute_cursor_row()
                .checked_sub(self.scroll_top)
                .and_then(|rel| (rel <= u16::MAX as RowIndex).then_some(rel as u16))
        } else {
            None
        };
        self.layout = if self.wrap_cursor_padding {
            WrappedLayout::from_buffer_with_cursor_padding(buf, width, self.wrap)
        } else {
            WrappedLayout::from_buffer(buf, width, self.wrap)
        };
        self.layout_key = Some(key);
        // Restore the logical anchor only on width/wrap changes - a content
        // change (changedtick bump) shouldn't move the viewport behind the
        // user's back, and the anchor's `(lrow, byte)` may no longer reference
        // the same content.
        if let Some((prev_w, prev_wrap, prev_cursor_padding)) = prev_width_wrap {
            if prev_w != width
                || prev_wrap != self.wrap
                || prev_cursor_padding != self.wrap_cursor_padding
            {
                self.restore_scroll_from_anchor(buf);
                if let Some(screen_row) = cursor_screen_row {
                    self.restore_cursor_screen_row(buf, screen_row);
                }
            }
        }
    }

    /// Cursor's row offset from `scroll_top`, returned when the cursor sits
    /// inside the viewport. Callers that mutate the buffer mid-frame
    /// (transcript projection) capture this before the mutation so the
    /// cursor's screen row can be restored after the new layout lands.
    pub fn cursor_screen_row_in_viewport(&self) -> Option<u16> {
        let rel = self.absolute_cursor_row().checked_sub(self.scroll_top)?;
        (rel <= u16::MAX as RowIndex).then_some(rel as u16)
    }

    /// Single mutation entry for `scroll_top`. Stamps `scroll_anchor` at the
    /// `(changedtick, logical_row, chunk_start_byte)` currently at the top so
    /// a later width/wrap-driven layout rebuild can restore the same logical
    /// position. Skips the anchor stamp when the layout doesn't match the
    /// buffer (callers before the first `ensure_layout`); the next
    /// `set_scroll` post-layout will pick up the anchor correctly.
    pub fn set_scroll(&mut self, visual_row: RowIndex, buf: &Buffer) {
        self.scroll_top = visual_row;
        if !self.layout_matches(buf) {
            return;
        }
        let Some((lrow, chunk_idx)) = self
            .layout
            .logical_at_visual(row_to_usize(self.local_row(visual_row)))
        else {
            return;
        };
        let byte = self
            .layout
            .chunks_of(lrow)
            .get(chunk_idx)
            .map(|&(s, _)| s)
            .unwrap_or(0);
        self.scroll_anchor = Some((buf.changedtick(), lrow, byte));
    }

    /// Re-derive `scroll_top` from `scroll_anchor` against the freshly-built
    /// layout. Called after a width/wrap rebuild in `ensure_layout`. Skips
    /// when tail-follow is active (tail wins), when no anchor is set, when the
    /// buffer's content was replaced since the anchor was stamped (changedtick
    /// mismatch - the (lrow, byte) is no longer meaningful), or when the anchor
    /// row no longer exists in the buffer.
    fn restore_scroll_from_anchor(&mut self, buf: &Buffer) {
        if self.is_following_tail() {
            return;
        }
        let Some((tick, lrow, byte)) = self.scroll_anchor else {
            return;
        };
        if tick != buf.changedtick() {
            self.scroll_anchor = None;
            return;
        }
        if lrow >= self.layout.logical_count() {
            return;
        }
        let (vrow, _) = self.layout.visual_for_logical(lrow, byte);
        self.scroll_top = self.absolute_row(vrow as RowIndex);
    }

    /// Reposition the cursor so it sits at `scroll_top + screen_row` in the
    /// current layout - used after a layout/scroll restore so the cursor
    /// stays visually fixed relative to the viewport instead of drifting
    /// off-screen as reflow shifts visual rows. Reassigns `cpos` to whatever
    /// byte lands at that visual row using the preferred display column.
    pub fn restore_cursor_screen_row(&mut self, buf: &Buffer, screen_row: u16) {
        if self.selection_active() {
            return;
        }
        let total = self.visual_row_total(buf);
        if total == 0 {
            return;
        }
        let target_abs_row = self
            .scroll_top
            .saturating_add(screen_row as RowIndex)
            .min(self.scroll_row_total(buf).saturating_sub(1));
        if self.row_text_state().active {
            let mut state = *self.row_text_state();
            let mut cursor = state.cursor;
            self.move_row_cursor_to_row_preserving_cell(
                &mut state,
                buf,
                &mut cursor,
                target_abs_row,
            );
            state.cursor = cursor;
            self.project_row_cursor_to_local(state, buf);
            *self.row_text_state_mut() = state;
            return;
        }
        let target_vrow = self.local_row(target_abs_row).min(total.saturating_sub(1));
        let want = self.curswant().unwrap_or(self.cursor_col() as usize);
        let cpos = self.cpos_at_visual(buf, row_to_usize(target_vrow), want);
        let (row, col) = self.cursor_visual(buf, cpos);
        let text = self.text_state_mut();
        text.cpos = cpos;
        text.cursor_row = row;
        text.cursor_col = col;
    }

    /// `true` when `self.layout` was built against `buf`'s current
    /// `changedtick`. Row count alone isn't enough - a row whose text shrank in
    /// place (e.g. transcript reset, prompt clear) keeps the row count but
    /// invalidates the cached chunk byte offsets, so slicing the layout's
    /// chunks against the fresh `lines` panics. Callers that haven't run
    /// `ensure_layout` (tests, first-frame setup) fall back to identity
    /// coordinates in `cursor_visual` / `cpos_at_visual` / `visual_row_total`.
    fn layout_matches(&self, buf: &Buffer) -> bool {
        match self.layout_key {
            Some((tick, _, _, _)) => {
                tick == buf.changedtick() && self.layout.logical_count() == buf.lines().len()
            }
            None => false,
        }
    }

    /// Total visual rows for scroll/keep-visible math. Falls back to the
    /// buffer's logical row count when `layout` hasn't been refreshed yet.
    fn visual_row_total(&self, buf: &Buffer) -> RowIndex {
        if self.layout_matches(buf) {
            self.layout.visual_count() as RowIndex
        } else {
            buf.lines().len() as RowIndex
        }
    }

    pub fn max_scroll(&self, buf: &Buffer, viewport_rows: u16) -> RowIndex {
        self.scroll_row_total(buf)
            .saturating_sub(viewport_rows as RowIndex)
    }

    pub fn is_at_tail(&self, buf: &Buffer, viewport_rows: u16) -> bool {
        self.scroll_top >= self.max_scroll(buf, viewport_rows)
    }

    pub fn is_following_tail(&self) -> bool {
        matches!(self.scroll_state, VerticalScroll::Tail)
    }

    pub fn pin_scroll(&mut self, row: RowIndex) {
        self.scroll_top = row;
        self.scroll_state = VerticalScroll::Pinned;
    }

    pub fn pin_current_scroll(&mut self) {
        self.scroll_state = VerticalScroll::Pinned;
    }

    pub fn selection_active(&self) -> bool {
        self.selection_anchor().is_some()
            || (self.vim_enabled()
                && matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine))
    }

    pub fn update_tail_state(&mut self, buf: &Buffer, viewport_rows: u16) {
        if self.selection_active() {
            self.pin_current_scroll();
        } else if self.is_at_tail(buf, viewport_rows) {
            self.scroll_state = VerticalScroll::Tail;
        } else {
            self.pin_current_scroll();
        }
    }

    pub fn local_visual_row(&self, absolute_row: RowIndex) -> RowIndex {
        self.local_row(absolute_row)
    }

    fn local_row(&self, absolute_row: RowIndex) -> RowIndex {
        if self.row_text_state().active {
            self.row_text_state().materialized.local_row(absolute_row)
        } else {
            absolute_row
        }
    }

    fn absolute_row(&self, local_row: RowIndex) -> RowIndex {
        if self.row_text_state().active {
            self.row_text_state().materialized.absolute_row(local_row)
        } else {
            local_row
        }
    }

    fn local_scroll_top(&self) -> RowIndex {
        self.local_row(self.scroll_top)
    }

    fn absolute_cursor_row(&self) -> RowIndex {
        if self.row_text_state().active {
            self.row_text_state().cursor.row
        } else {
            self.absolute_row(self.cursor_row())
        }
    }

    fn backed_local_row(
        &self,
        state: RowTextState,
        buf: &Buffer,
        absolute_row: RowIndex,
    ) -> Option<RowIndex> {
        if absolute_row < state.materialized.row_base
            || absolute_row >= state.materialized.total_rows
        {
            return None;
        }
        let local = state.materialized.local_row(absolute_row);
        (local < self.visual_row_total(buf)).then_some(local)
    }

    fn backed_display_row(&self, buf: &Buffer, absolute_row: RowIndex) -> Option<RowIndex> {
        if self.row_text_state().active {
            self.backed_local_row(*self.row_text_state(), buf, absolute_row)
        } else {
            (absolute_row < self.visual_row_total(buf)).then_some(absolute_row)
        }
    }

    fn project_row_cursor_to_local(&mut self, state: RowTextState, buf: &Buffer) -> bool {
        let Some(local) = self.backed_local_row(state, buf, state.cursor.row) else {
            return false;
        };
        let Some(vcell) = state
            .preferred_cell_col
            .or_else(|| self.row_position_cell(state, buf, state.cursor))
        else {
            return false;
        };
        let cpos = self.cpos_at_visual(buf, row_to_usize(local), vcell);
        let (row, col) = self.cursor_visual(buf, cpos);
        let text = self.text_state_mut();
        text.cpos = cpos;
        text.cursor_row = row;
        text.cursor_col = col;
        true
    }

    fn move_row_cursor_to_row_preserving_cell(
        &self,
        state: &mut RowTextState,
        buf: &Buffer,
        cursor: &mut DocPosition,
        row: RowIndex,
    ) {
        let cell = state
            .preferred_cell_col
            .or_else(|| self.row_position_cell(*state, buf, *cursor));
        cursor.row = row.min(state.materialized.total_rows.saturating_sub(1));
        if let Some(cell) = cell {
            state.preferred_cell_col = Some(cell);
            if let Some(byte_col) = self.row_byte_col_at_cell(*state, buf, cursor.row, cell) {
                cursor.byte_col = byte_col;
            }
        }
    }

    fn row_position_cell(
        &self,
        state: RowTextState,
        buf: &Buffer,
        position: DocPosition,
    ) -> Option<usize> {
        let local = self.backed_local_row(state, buf, position.row)?;
        Some(self.visual_cell_for_byte_col(buf, local, position.byte_col))
    }

    fn row_byte_col_at_cell(
        &self,
        state: RowTextState,
        buf: &Buffer,
        row: RowIndex,
        cell: usize,
    ) -> Option<usize> {
        let local = self.backed_local_row(state, buf, row)?;
        let cpos = self.cpos_at_visual(buf, row_to_usize(local), cell);
        Some(buf.display_byte_pos(cpos).1)
    }

    fn visual_cell_for_byte_col(&self, buf: &Buffer, vrow: RowIndex, byte_col: usize) -> usize {
        let vrow = row_to_usize(vrow);
        if self.layout_matches(buf) {
            if let Some((lrow, chunk_idx)) = self.layout.logical_at_visual(vrow) {
                let line = buf.get_line(lrow).unwrap_or("");
                let byte = text::snap(line, byte_col.min(line.len()));
                let chunk_start = self
                    .layout
                    .chunks_of(lrow)
                    .get(chunk_idx)
                    .map(|&(start, _)| start)
                    .unwrap_or(0);
                return text::byte_to_cell(line, byte)
                    .saturating_sub(text::byte_to_cell(line, chunk_start));
            }
        }
        let line = buf.get_line(vrow).unwrap_or("");
        text::byte_to_cell(line, text::snap(line, byte_col.min(line.len())))
    }

    /// Project `cpos` to a visual `(row, cell_col)` through this window's
    /// layout. Used by cursor sync after navigation/edit so `cursor_row` stays
    /// in the backing buffer's local visual-row space.
    fn cursor_visual(&self, buf: &Buffer, cpos: usize) -> (RowIndex, u16) {
        let (lrow, byte_col) = buf.display_byte_pos(cpos);
        if !self.layout_matches(buf) {
            let line = buf.get_line(lrow).unwrap_or("");
            let cell_col = smelt_buffer::text::byte_to_cell(line, byte_col);
            return (lrow as RowIndex, cell_col as u16);
        }
        let (vrow, byte_in_chunk) = self.layout.visual_for_logical(lrow, byte_col);
        let line = self.layout.visual_line(buf.lines(), vrow).unwrap_or("");
        let cell_col = smelt_buffer::text::byte_to_cell(line, byte_in_chunk);
        (vrow as RowIndex, cell_col as u16)
    }

    fn adjust_vim_mouse_endpoint_for_hit(
        &self,
        text: &str,
        endpoint: usize,
        hit: TextHit<usize>,
    ) -> usize {
        if !self.vim_enabled() {
            return endpoint;
        }
        match hit.kind {
            TextHitKind::TrailingChrome {
                nearest: Some(nearest),
            } => {
                let nearest = text::snap(text, nearest.min(text.len()));
                text::prev_char_boundary(text, nearest).min(endpoint)
            }
            _ => endpoint,
        }
    }

    fn mouse_text_endpoint(
        &self,
        buf: &Buffer,
        event: MouseEvent,
        viewport: WindowViewport,
        text: &str,
        mode: MouseEndpointMode,
    ) -> Option<MouseTextEndpoint> {
        let viewport_hit = viewport.hit(event.row, event.column);
        let (rel_row, rel_col, hit_is_content) = match mode {
            MouseEndpointMode::RequireContentHit => {
                let ViewportHit::Content {
                    row,
                    col: viewport_rel_col,
                } = viewport_hit?
                else {
                    return None;
                };
                let col = viewport_rel_col
                    .saturating_sub(viewport.gutter_width)
                    .saturating_sub(self.config.gutters.pad_left);
                (row, col, true)
            }
            MouseEndpointMode::ClampToContent => {
                if viewport.rect.height == 0 {
                    return None;
                }
                let row = event
                    .row
                    .saturating_sub(viewport.rect.top)
                    .min(viewport.rect.height.saturating_sub(1));
                let col = event
                    .column
                    .saturating_sub(viewport.rect.left)
                    .saturating_sub(viewport.gutter_width)
                    .saturating_sub(self.config.gutters.pad_left)
                    .min(viewport.content_width.saturating_sub(1));
                (
                    row,
                    col,
                    matches!(viewport_hit, Some(ViewportHit::Content { .. })),
                )
            }
        };

        let visual_total = self.visual_row_total(buf);
        let vrow = self
            .local_row(self.scroll_top.saturating_add(rel_row as RowIndex))
            .min(visual_total.max(1).saturating_sub(1));
        let vcell = rel_col as usize + self.scroll_left as usize;
        let clamped_hit = self.text_hit_at_visual(buf, row_to_usize(vrow), vcell);
        let hit = if hit_is_content {
            clamped_hit
        } else {
            TextHit {
                row: self.absolute_row(vrow),
                cell_col: vcell,
                cpos: clamped_hit.cpos,
                kind: TextHitKind::Outside,
            }
        };
        let cpos = self.adjust_vim_mouse_endpoint_for_hit(text, hit.cpos, hit);
        Some(MouseTextEndpoint {
            cpos,
            includes_cell: hit.is_selectable(),
        })
    }

    pub fn text_hit_at_mouse(
        &self,
        buf: &Buffer,
        event: MouseEvent,
        viewport: WindowViewport,
    ) -> TextHit<usize> {
        let Some(hit) = viewport.hit(event.row, event.column) else {
            return TextHit {
                row: 0,
                cell_col: 0,
                cpos: 0,
                kind: TextHitKind::Outside,
            };
        };
        let ViewportHit::Content {
            row: rel_row,
            col: viewport_rel_col,
        } = hit
        else {
            return TextHit {
                row: 0,
                cell_col: 0,
                cpos: 0,
                kind: TextHitKind::Outside,
            };
        };
        let rel_col = viewport_rel_col
            .saturating_sub(viewport.gutter_width)
            .saturating_sub(self.config.gutters.pad_left);
        let visual_total = self.visual_row_total(buf);
        let vrow = self
            .local_row(self.scroll_top.saturating_add(rel_row as RowIndex))
            .min(visual_total.max(1).saturating_sub(1));
        let vcell = rel_col as usize + self.scroll_left as usize;
        self.text_hit_at_visual(buf, row_to_usize(vrow), vcell)
    }

    pub fn text_hit_at_visual(&self, buf: &Buffer, vrow: usize, vcell: usize) -> TextHit<usize> {
        let row = self.absolute_row(vrow as RowIndex);
        let Some((logical_row, cell)) = self.logical_cell_at_visual(buf, vrow, vcell) else {
            let cpos = buf
                .lines()
                .len()
                .checked_sub(1)
                .map(|last_logical| buf.byte_at_display_pos(last_logical, 0))
                .unwrap_or(0);
            return TextHit {
                row,
                cell_col: vcell,
                cpos,
                kind: TextHitKind::Outside,
            };
        };
        let Some(line) = buf.get_line(logical_row) else {
            return TextHit {
                row,
                cell_col: cell,
                cpos: 0,
                kind: TextHitKind::Outside,
            };
        };
        let cpos = |cell: usize| {
            let cell = snap_col_past_chrome(buf, logical_row, cell as u16) as usize;
            buf.byte_at_display_pos(logical_row, cell)
        };
        if line.is_empty() {
            return TextHit {
                row,
                cell_col: cell,
                cpos: cpos(cell),
                kind: TextHitKind::EmptyRow,
            };
        }

        let line_width = text::byte_to_cell(line, line.len());
        let line_start = buf.byte_at_display_pos(logical_row, 0);
        let line_end = buf.byte_at_display_pos(logical_row, line_width);
        if cell >= line_width {
            return TextHit {
                row,
                cell_col: cell,
                cpos: cpos(cell),
                kind: TextHitKind::TrailingChrome {
                    nearest: Some(line_end),
                },
            };
        }
        let spans = buf.highlights_at(logical_row);
        if cell_range_contains_selectable(&spans, cell, cell.saturating_add(1)) {
            let before = buf.byte_at_display_pos(logical_row, cell);
            return TextHit {
                row,
                cell_col: cell,
                cpos: before,
                kind: TextHitKind::Selectable {
                    before,
                    after: buf.byte_at_display_pos(logical_row, cell.saturating_add(1)),
                },
            };
        }

        let first_selectable = (0..line_width)
            .find(|col| cell_range_contains_selectable(&spans, *col, col.saturating_add(1)));
        let last_selectable_edge = (0..line_width)
            .filter(|col| cell_range_contains_selectable(&spans, *col, col.saturating_add(1)))
            .map(|col| col.saturating_add(1))
            .next_back();
        let kind = match (first_selectable, last_selectable_edge) {
            (None, _) => TextHitKind::AllChrome,
            (Some(first), _) if cell < first => TextHitKind::LeadingChrome {
                nearest: Some(line_start),
            },
            (_, Some(edge)) => TextHitKind::TrailingChrome {
                nearest: Some(buf.byte_at_display_pos(logical_row, edge)),
            },
            _ => TextHitKind::AllChrome,
        };
        TextHit {
            row,
            cell_col: cell,
            cpos: cpos(cell),
            kind,
        }
    }

    fn logical_cell_at_visual(
        &self,
        buf: &Buffer,
        vrow: usize,
        vcell: usize,
    ) -> Option<(usize, usize)> {
        if buf.lines().is_empty() {
            return None;
        }
        let last_logical = buf.lines().len() - 1;
        if !self.layout_matches(buf) {
            return Some((vrow.min(last_logical), vcell));
        }
        let (lrow, chunk_idx) = self.layout.logical_at_visual(vrow)?;
        let chunks = self.layout.chunks_of(lrow);
        let &(chunk_start_byte, _) = chunks.get(chunk_idx)?;
        let logical_line = buf.lines().get(lrow).map(String::as_str).unwrap_or("");
        let chunk_start_cell = smelt_buffer::text::byte_to_cell(logical_line, chunk_start_byte);
        Some((lrow, chunk_start_cell.saturating_add(vcell)))
    }

    /// Project a visual `(row, cell_col)` hit to a buffer cpos via the layout.
    /// Used by mouse hit-test and pan-preserving-cursor.
    fn cpos_at_visual(&self, buf: &Buffer, vrow: usize, vcell: usize) -> usize {
        if buf.lines().is_empty() {
            return 0;
        }
        let last_logical = buf.lines().len() - 1;
        let Some((lrow, cell)) = self.logical_cell_at_visual(buf, vrow, vcell) else {
            return buf.byte_at_display_pos(last_logical, 0);
        };
        let cell = snap_col_past_chrome(buf, lrow, cell as u16) as usize;
        buf.byte_at_display_pos(lrow, cell)
    }

    /// Text space that `cpos_at_visual` / `byte_at_display_pos` return offsets
    /// into for this buffer. Parsed or source-backed editable buffers use
    /// `source`; line-backed and readonly buffers use joined display rows.
    fn coordinate_text<'a>(buf: &'a Buffer) -> std::borrow::Cow<'a, str> {
        if buf.has_parser() || !buf.source().is_empty() {
            std::borrow::Cow::Borrowed(buf.source())
        } else {
            std::borrow::Cow::Owned(buf.text())
        }
    }

    /// Read access to the most recent layout. Always populated after the first
    /// `ensure_layout`; before that it's an empty identity layout.
    pub fn layout(&self) -> &WrappedLayout {
        &self.layout
    }

    // ── Vim ────────────────────────────────────────────────────────────

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        let text = self.text_state_mut();
        text.vim_enabled = enabled;
        if !enabled {
            text.selection_anchor = None;
            text.selection_anchor_includes_cell = false;
        }
    }

    /// Set this window's vim mode and clear any pending key sequence.
    pub fn set_vim_mode(&mut self, mode: VimMode) {
        let text = self.surface.text_mut();
        text.vim_state.set_mode(&mut text.vim_mode, mode);
    }

    /// Restore a previously observed vim mode without touching pending vim sub-state.
    pub fn restore_vim_mode(&mut self, mode: VimMode) {
        self.surface.text_mut().vim_mode = mode;
    }

    /// Anchor a visual selection at `cpos` and enter the given visual mode.
    /// Clears any shift-selection anchor.
    pub fn begin_visual(&mut self, mode: VimMode, cpos: usize) {
        let text = self.surface.text_mut();
        text.selection_anchor = None;
        text.selection_anchor_includes_cell = false;
        text.vim_state.begin_visual(&mut text.vim_mode, mode, cpos);
    }

    // ── Cursor ─────────────────────────────────────────────────────────

    pub fn cursor_abs_row(&self) -> RowIndex {
        self.absolute_cursor_row()
    }

    /// Screen row (relative to the viewport top) where the cursor should render.
    /// `None` when the buffer cursor lies outside the viewport - the renderer must
    /// suppress the cursor in that case.
    pub fn cursor_screen_row(&self, viewport_rows: u16) -> Option<u16> {
        self.cursor_screen_row_at(self.scroll_top, viewport_rows)
    }

    pub fn cursor_screen_row_at(&self, scroll_top: RowIndex, viewport_rows: u16) -> Option<u16> {
        Self::screen_row_at(self.absolute_cursor_row(), scroll_top, viewport_rows)
    }

    fn screen_row_at(row: RowIndex, scroll_top: RowIndex, viewport_rows: u16) -> Option<u16> {
        let rel = row.checked_sub(scroll_top)?;
        (rel < viewport_rows as RowIndex).then_some(rel as u16)
    }

    fn screen_row_or_edge(row: RowIndex, scroll_top: RowIndex, viewport_rows: u16) -> u16 {
        Self::screen_row_at(row, scroll_top, viewport_rows).unwrap_or_else(|| {
            if row < scroll_top {
                0
            } else {
                viewport_rows.saturating_sub(1)
            }
        })
    }

    pub fn cursor_row(&self) -> RowIndex {
        self.text_state().cursor_row
    }

    pub fn cursor_col(&self) -> u16 {
        self.text_state().cursor_col
    }

    /// Set cursor column on a single-line buffer (row is always 0).
    pub fn set_cursor_col_single_line(&mut self, col: u16) {
        let text = self.text_state_mut();
        text.cursor_col = col;
        text.cursor_row = 0;
        text.cpos = col as usize;
    }

    /// Set cursor from a byte offset in a single-line buffer. The byte offset is
    /// snapped to a UTF-8 boundary and the displayed column is measured in cells.
    pub fn set_cursor_byte_single_line(&mut self, line: &str, byte: usize) {
        let byte = text::snap(line, byte);
        let text_state = self.text_state_mut();
        text_state.cursor_col = text::byte_to_cell(line, byte) as u16;
        text_state.cursor_row = 0;
        text_state.cpos = byte;
        text_state.curswant = None;
    }

    /// Reset cursor to the origin.
    pub fn reset_cursor(&mut self) {
        let text = self.text_state_mut();
        text.cpos = 0;
        text.cursor_row = 0;
        text.cursor_col = 0;
        text.curswant = None;
    }

    /// Cancel a partially completed mouse gesture without moving the
    /// persistent cursor. Used when terminal focus or keyboard input makes it
    /// clear that a staged Down/Drag will not receive its matching Up event.
    pub fn clear_mouse_state(&mut self) {
        let text = self.text_state_mut();
        text.drag_endpoint = None;
        text.drag_endpoint_includes_cell = false;
        text.drag_anchor_word = None;
        text.drag_anchor_line = None;
        text.pending_press = None;
        text.pending_press_includes_cell = false;
        text.selection_anchor_includes_cell = false;
        if text.row_text.active {
            text.row_text.drag_endpoint = None;
        }
    }

    /// Finish a staged single-click because keyboard input is taking over.
    /// Terminals can deliver a Down without the matching Up around focus
    /// changes; committing here keeps the visible click location and the
    /// persistent insertion cursor in sync.
    pub fn commit_pending_caret_click(&mut self, buf: &Buffer) {
        let should_commit = {
            let text = self.text_state();
            text.pending_press.is_some() && text.drag_endpoint.is_some() && self.is_caret_leaf()
        };
        if should_commit {
            let end = self.text_state().drag_endpoint.unwrap_or(0);
            let source = Self::coordinate_text(buf);
            let cpos = text::snap(source.as_ref(), end.min(source.len()));
            let text = self.text_state_mut();
            text.cpos = cpos;
            text.selection_anchor = None;
            text.selection_anchor_includes_cell = false;
        }
        self.clear_mouse_state();
    }

    /// Re-derive `cursor_row` / `cursor_col` from the persisted `cpos` using the
    /// buffer's display mapping. The prompt layer calls this after a buffer
    /// mutation has moved `cpos` but no scroll/recenter decision has been made
    /// yet - it owns its own `keep_cursor_visible` vs `recenter` choice.
    pub fn resync_display_coords(&mut self, buf: &Buffer) {
        let cpos = self.cpos();
        let (row, col) = self.cursor_visual(buf, cpos);
        let text = self.text_state_mut();
        text.cursor_row = row;
        text.cursor_col = col;
    }

    /// Place the logical cursor at line `row`, column 0, writing `cpos` so all
    /// downstream readers (renderer's `effective_endpoint`, selection paint,
    /// copy_range) see the same position. Used by list leaves where j/k
    /// navigation owns the logical cursor - keeping `cpos` in sync means the
    /// global active-cursor machinery paints the block on the correct row when
    /// this leaf takes the cursor (e.g. when focus lands here, or when a drag
    /// ends here).
    pub fn jump_to_row(&mut self, buf: &Buffer, row: RowIndex, viewport_rows: u16) {
        self.jump_to_line_col(buf, row_to_usize(row), 0, viewport_rows);
    }

    /// Set `selection_anchor` to `cpos` if unset. Call before a shift-move.
    pub fn extend_selection(&mut self, cpos: usize) {
        self.pin_current_scroll();
        let text = self.text_state_mut();
        if text.selection_anchor.is_none() {
            text.selection_anchor = Some(cpos);
            text.selection_anchor_includes_cell = false;
        }
    }

    /// Snap-clamp every byte-offset anchor (`cpos`, `selection_anchor`,
    /// `vim_state.visual_anchor`) into `source` and onto a char boundary.
    /// Call after any in-place source shrink so a delete that consumed the
    /// bytes those offsets used to point at can't outlive them. Wholesale
    /// swaps belong in `PromptState::install_source`; this is for
    /// surgical edits (drains, replace-range) where keeping the cursor's
    /// neighborhood is the point.
    pub fn clamp_anchors_to_source(&mut self, source: &str) {
        let len = source.len();
        let text = self.text_state_mut();
        text.cpos = text::snap(source, text.cpos.min(len));
        if let Some(a) = text.selection_anchor {
            let snapped = text::snap(source, a.min(len));
            text.selection_anchor = if snapped == text.cpos {
                None
            } else {
                Some(snapped)
            };
        }
        text.vim_state.clamp_visual_anchor(source);
        debug_assert!(
            text.cpos <= len && source.is_char_boundary(text.cpos),
            "clamp_anchors_to_source postcondition: cpos {} not on char boundary in source len {}",
            text.cpos,
            len
        );
        debug_assert!(
            text.selection_anchor
                .is_none_or(|a| a <= len && source.is_char_boundary(a)),
            "clamp_anchors_to_source postcondition: selection_anchor {:?} not on char boundary",
            text.selection_anchor
        );
    }

    /// Resolve the shift-selection range against `src`. Both endpoints are clamped to
    /// `src.len()` and snapped to char boundaries - a stale anchor that survived a
    /// source mutation degrades to `None` instead of producing an out-of-bounds slice.
    pub fn selection_range_at(&self, cpos: usize, src: &str) -> Option<(usize, usize)> {
        let a = text::snap(src, self.selection_anchor()?);
        let c = text::snap(src, cpos);
        let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
        (lo != hi).then_some((lo, hi))
    }

    fn mouse_selection_range_at(
        &self,
        cpos: usize,
        src: &str,
        buf: &Buffer,
        cursor_includes_cell: bool,
    ) -> Option<(usize, usize)> {
        let anchor_includes_cell = self.text_state().selection_anchor_includes_cell;
        let a = text::snap(src, self.selection_anchor()?);
        let c = text::snap(src, cpos);
        let (lo, hi, include_hi) = if a <= c {
            (a, c, cursor_includes_cell)
        } else {
            (c, a, anchor_includes_cell)
        };
        let end = if include_hi {
            include_cursor_cell(buf, src, hi)
        } else {
            hi
        };
        (lo < end).then_some((lo, end))
    }

    pub fn selection_range(&self, buf: &Buffer) -> Option<(usize, usize)> {
        let endpoint = self
            .text_state()
            .drag_endpoint
            .unwrap_or_else(|| self.compute_cpos(buf));
        let text = buf.text();
        if self.vim_enabled() {
            if let Some(range) =
                vim::visual_range(self.vim_state(), &text, endpoint, self.vim_mode())
            {
                return Some(range);
            }
        }
        if self.is_plain_mouse_drag() {
            self.mouse_selection_range_at(
                endpoint,
                &text,
                buf,
                self.text_state().drag_endpoint_includes_cell,
            )
        } else {
            self.selection_range_at(endpoint, &text)
        }
    }

    /// Derive selection ranges from this window's own anchors; used when the buffer
    /// has no explicit `set_selection` override. Empty result means no paint.
    fn auto_selection_ranges(
        &self,
        buf: &Buffer,
        vim_mode: VimMode,
    ) -> Vec<smelt_buffer::buffer::SelectionRange> {
        // Row-backed documents manage selection via sync_row_render_state (which writes
        // the buffer's Selection range layer); falling back to byte-backed vim::visual_range
        // uses vim_state.visual_anchor which is never set for row documents and
        // would spuriously select from byte 0.
        if self.has_materialized_rows() {
            return Vec::new();
        }
        let in_vim_visual =
            self.vim_enabled() && matches!(vim_mode, VimMode::Visual | VimMode::VisualLine);
        if !in_vim_visual && self.selection_anchor().is_none() {
            return Vec::new();
        }
        let buf_text = buf.text();
        let endpoint = self.effective_endpoint();
        let range = if in_vim_visual {
            vim::visual_range(self.vim_state(), &buf_text, endpoint, vim_mode)
        } else if self.is_plain_mouse_drag() {
            self.mouse_selection_range_at(
                endpoint,
                &buf_text,
                buf,
                self.text_state().drag_endpoint_includes_cell,
            )
        } else {
            self.selection_range_at(endpoint, &buf_text)
        };
        let Some((s, e)) = range else {
            return Vec::new();
        };
        smelt_buffer::coords::byte_range_to_row_ranges(buf, s, e)
    }

    /// Select the WORD (whitespace-delimited, punctuation included) at `cpos`.
    /// `transparent` positions (soft-wrap joins) are treated as word chars.
    fn select_big_word_at_transparent(
        &mut self,
        cpos: usize,
        transparent: &[usize],
        buf: &Buffer,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let text = buf.text();
        let (start, end) = super::text::big_word_range_at_transparent(&text, cpos, transparent)?;
        self.finish_range_select(start, end, buf, viewport_rows);
        Some((start, end))
    }

    /// Select the table cell containing `cpos`. Returns `None` when the row
    /// does not look like a table (no `┃` border character). The returned
    /// range spans from the right edge of the left border/padding to the left
    /// edge of the right border/padding, so the mask in `Window::render` drops
    /// chrome and only the cell text is visually highlighted.
    fn select_cell_at(
        &mut self,
        cpos: usize,
        buf: &Buffer,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (row, col) = buf.display_cursor_pos(cpos);
        if !buf.decoration_at(row).cell_selectable {
            return None;
        }
        let line = buf.get_line(row)?;
        let highlights = buf.highlights_at(row);
        let line_width = text::byte_to_cell(line, line.len());

        // Collect boundaries of non-selectable spans (borders + padding).
        let mut boundaries = Vec::new();
        boundaries.push(0);
        boundaries.push(line_width);
        let mut saw_chrome_span = false;
        for span in &highlights {
            if !span.meta.selectable {
                saw_chrome_span = true;
                boundaries.push(span.col_start as usize);
                boundaries.push(span.col_end as usize);
            }
        }
        if !saw_chrome_span {
            return None;
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        // Find the gap between two chrome boundaries that contains `col`.
        let mut cell_start = None;
        let mut cell_end = None;
        for window in boundaries.windows(2) {
            let (s, e) = (window[0], window[1]);
            if s <= col && col < e {
                cell_start = Some(s);
                cell_end = Some(e);
                break;
            }
        }
        let (start, end) = (cell_start?, cell_end?);

        // Require at least one selectable character inside the gap. Plain text
        // often has no explicit highlight span, so unstyled characters are
        // selectable unless covered by non-selectable chrome.
        if !cell_range_contains_selectable(&highlights, start, end) {
            return None;
        }

        let start_byte = buf.byte_at_display_pos(row, start);
        let end_byte = buf.byte_at_display_pos(row, end);
        self.finish_range_select(start_byte, end_byte, buf, viewport_rows);
        Some((start_byte, end_byte))
    }

    /// Expand the selection to the full contiguous selectable block containing
    /// `cpos`. Structured renderers opt rows into this behavior through
    /// `LineDecoration::block_selectable`.
    fn select_block_at(
        &mut self,
        cpos: usize,
        buf: &Buffer,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (row, _col) = buf.display_cursor_pos(cpos);
        if !buf.decoration_at(row).block_selectable {
            return None;
        }

        let mut first = row;
        while first > 0 && buf.decoration_at(first - 1).block_selectable {
            first -= 1;
        }

        let mut last = row;
        while last + 1 < buf.line_count() && buf.decoration_at(last + 1).block_selectable {
            last += 1;
        }

        let start_byte = buf.byte_at_display_pos(first, 0);
        let last_line = buf.get_line(last)?;
        // In identity mode (transcript buffers) `byte_at_display_pos(last,0)` is the
        // start of the row and `+ line.len()` lands on the newline byte (or EOF),
        // which `copy_byte_range` maps to the end of the current display row.
        let end_byte = buf.byte_at_display_pos(last, 0) + last_line.len();
        self.finish_range_select(start_byte, end_byte, buf, viewport_rows);
        Some((start_byte, end_byte))
    }

    /// Select the source line at `cpos` and enter Visual mode anchored at its start.
    /// Uses `Visual` (not `VisualLine`) so soft-wrapped lines select correctly.
    fn select_line_at(
        &mut self,
        cpos: usize,
        hard_breaks: &[usize],
        buf: &Buffer,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let text = buf.text();
        let (start, end) = super::text::line_range_at(&text, cpos, hard_breaks)?;
        self.finish_range_select(start, end, buf, viewport_rows);
        Some((start, end))
    }

    /// Stage `[start, end)` as a mouse-selected range with the drag endpoint at `end`
    /// (or the last char's start byte for vim, since `visual_range` is inclusive).
    /// `mouse_up` decides whether to commit the endpoint into `cpos`.
    fn finish_range_select(&mut self, start: usize, end: usize, buf: &Buffer, _viewport_rows: u16) {
        let endpoint = if self.vim_enabled() {
            smelt_buffer::text::prev_char_boundary(&buf.text(), end).max(start)
        } else {
            end
        };
        self.text_state_mut().drag_endpoint = Some(endpoint);
        self.text_state_mut().drag_endpoint_includes_cell = false;
        if self.vim_enabled() {
            self.begin_visual(VimMode::Visual, start);
        } else {
            self.text_state_mut().selection_anchor = Some(start);
            self.text_state_mut().selection_anchor_includes_cell = false;
        }
    }

    pub fn resync(&mut self, buf: &Buffer, viewport_rows: u16) {
        if buf.lines().is_empty() {
            return;
        }
        self.sync_from_cpos(buf, viewport_rows);
    }

    pub fn refocus(&mut self, buf: &Buffer, viewport_rows: u16) {
        if buf.lines().is_empty() {
            self.reset_cursor();
            self.text_state_mut().cursor_positioned = false;
            return;
        }
        // Row-backed windows manage their own cursor; refocus would overwrite it
        // from the stale local cpos.
        if self.has_materialized_rows() {
            return;
        }
        if self.vim_enabled() && self.vim_mode() != VimMode::Normal {
            self.set_vim_mode(VimMode::Normal);
        }
        if !self.text_state().cursor_positioned {
            let rows = buf.lines();
            let last_line = rows.len().saturating_sub(1);
            self.set_cpos(buf.byte_at_display_pos(last_line, 0));
            self.sync_from_cpos(buf, viewport_rows);
            self.text_state_mut().cursor_positioned = true;
        } else {
            self.sync_from_cpos(buf, viewport_rows);
        }
        if self.curswant().is_none() {
            self.set_curswant(Some(self.cursor_col() as usize));
        }
    }

    // ── Follow-tail ────────────────────────────────────────────────────

    /// Re-engage sticky-bottom scrolling. The next tail-follow pass resolves
    /// `scroll_top` against the current row count.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_state = VerticalScroll::Tail;
    }

    // ── Navigation ─────────────────────────────────────────────────────

    pub fn compute_cpos(&self, buf: &Buffer) -> usize {
        self.cpos_at_visual(
            buf,
            row_to_usize(self.cursor_row()),
            self.cursor_col() as usize,
        )
    }

    /// Byte to use as the selection/cursor endpoint. Returns `drag_endpoint` if an
    /// active mouse drag is updating a transient endpoint (non-vim mouse path),
    /// otherwise the persistent `cpos`. Renderer, selection-range computation, and
    /// drag extends all read through this so they observe the drag position without
    /// the drag having to write `cpos` mid-gesture.
    pub fn effective_endpoint(&self) -> usize {
        self.text_state()
            .drag_endpoint
            .unwrap_or_else(|| self.cpos())
    }

    /// Display-row component of `effective_endpoint`, looked up through `buf`.
    /// Used by the renderer to paint CursorLine / caret at the drag end while a
    /// drag is in flight.
    pub fn effective_cursor_row(&self, buf: &Buffer) -> RowIndex {
        match self.text_state().drag_endpoint {
            Some(end) => self.cursor_visual(buf, end).0,
            None => self.cursor_row(),
        }
    }

    /// Absolute visual row of the drag endpoint (or cursor if no drag).
    /// For row-backed documents this is the document row; for byte-backed text it is
    /// the local row from `effective_cursor_row`, which is already absolute.
    pub fn drag_endpoint_absolute_row(&self, buf: &Buffer) -> RowIndex {
        if self.row_text_state().active {
            self.row_text_state()
                .drag_endpoint
                .map(|ep| ep.row)
                .unwrap_or(self.row_text_state().cursor.row)
        } else {
            self.effective_cursor_row(buf)
        }
    }

    /// Clamp `scroll_top` to the valid range `[0, total_rows - viewport_rows]`
    /// and stamp the scroll anchor. Returns `max_scroll`.
    fn clamp_scroll_top(
        &mut self,
        total_rows: RowIndex,
        viewport_rows: u16,
        buf: &Buffer,
    ) -> RowIndex {
        let max_scroll = total_rows.saturating_sub(viewport_rows as RowIndex);
        if self.scroll_top > max_scroll {
            self.set_scroll(max_scroll, buf);
        }
        max_scroll
    }

    /// Pan `scroll_top` and `scroll_left` so the cursor stays inside the
    /// viewport on both axes. Zero on either dimension treats that axis as
    /// "no viewport yet" and skips it - the host's `pending_scroll_to_cursor`
    /// retries once real dimensions land. Horizontal bounds come from the
    /// cached layout's widest visual row; vertical bounds use the caller-
    /// supplied `total_rows` so wrap-aware paths pass `visual_row_total` and
    /// plain row counters pass `lines().len()`.
    pub fn keep_cursor_visible(
        &mut self,
        buf: &Buffer,
        total_rows: RowIndex,
        viewport_rows: u16,
        viewport_cols: u16,
    ) {
        if viewport_rows > 0 {
            let max_scroll = self.clamp_scroll_top(total_rows, viewport_rows, buf);
            let viewport_bottom = self
                .scroll_top
                .saturating_add(viewport_rows.saturating_sub(1) as RowIndex);
            let cursor_row = self.absolute_cursor_row();
            if cursor_row > viewport_bottom {
                let target = cursor_row
                    .saturating_sub(viewport_rows.saturating_sub(1) as RowIndex)
                    .min(max_scroll);
                self.set_scroll(target, buf);
            } else if cursor_row < self.scroll_top {
                self.set_scroll(cursor_row, buf);
            }
        }
        if viewport_cols > 0 {
            let max_scroll_left = if self.layout_matches(buf) {
                let content_extent = self.layout.max_row_width();
                let scroll_extent = if buf.readonly {
                    content_extent
                } else {
                    content_extent.max(self.cursor_col().saturating_add(1))
                };
                Some(scroll_extent.saturating_sub(viewport_cols))
            } else {
                None
            };
            if let Some(max) = max_scroll_left {
                self.scroll_left = self.scroll_left.min(max);
            }
            let viewport_right = self
                .scroll_left
                .saturating_add(viewport_cols.saturating_sub(1));
            if self.cursor_col() > viewport_right {
                let target = self
                    .cursor_col()
                    .saturating_sub(viewport_cols.saturating_sub(1));
                self.scroll_left = max_scroll_left.map(|max| target.min(max)).unwrap_or(target);
            } else if self.cursor_col() < self.scroll_left {
                self.scroll_left = max_scroll_left
                    .map(|max| self.cursor_col().min(max))
                    .unwrap_or(self.cursor_col());
            }
        }
    }

    fn sync_from_cpos(&mut self, buf: &Buffer, viewport_rows: u16) {
        let cpos = self.cpos();
        let (row, col) = self.cursor_visual(buf, cpos);
        let byte_col = buf.display_byte_pos(cpos).1;
        {
            let text = self.text_state_mut();
            text.cursor_row = row;
            text.cursor_col = col;
            if text.row_text.active {
                text.row_text.cursor = DocPosition {
                    row: text.row_text.materialized.absolute_row(row),
                    byte_col,
                };
                text.row_text.preferred_cell_col = Some(col as usize);
            }
        }
        let total_rows = self.scroll_row_total(buf);
        let viewport_cols = self.viewport.map(|v| v.content_width).unwrap_or(0);
        self.keep_cursor_visible(buf, total_rows, viewport_rows, viewport_cols);
    }

    // ── Event dispatch ────────────────────────────────────────────────

    pub fn handle(
        &mut self,
        buf: &mut Buffer,
        ev: super::event::Event,
        ctx: EventCtx<'_>,
    ) -> Status {
        use super::event::Event;
        match ev {
            Event::Key(k) => {
                self.clear_mouse_state();
                self.handle_key(buf, k, ctx.clipboard, ctx.now)
            }
            Event::Mouse(me) => {
                let (status, _) = self.handle_mouse(
                    buf,
                    me,
                    MouseCtx {
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
    /// the buffer's coordinate text if a selection was active (host applies the copy primitive).
    pub fn handle_mouse(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        ctx: MouseCtx,
    ) -> (Status, Option<(usize, usize)>) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => (self.mouse_down(buf, event, &ctx), None),
            MouseEventKind::Drag(MouseButton::Left) => {
                let text = Self::coordinate_text(buf);
                (self.mouse_drag(buf, event, &ctx, text.as_ref()), None)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let text = Self::coordinate_text(buf);
                let range = self.mouse_yank_range(&ctx, buf, text.as_ref());
                let status = self.mouse_up(buf, ctx.viewport.rect.height);
                (status, range)
            }
            _ => (Status::Ignored, None),
        }
    }

    fn mouse_down(&mut self, buf: &Buffer, event: MouseEvent, ctx: &MouseCtx) -> Status {
        let Some(endpoint) = self.mouse_text_endpoint(
            buf,
            event,
            ctx.viewport,
            Self::coordinate_text(buf).as_ref(),
            MouseEndpointMode::RequireContentHit,
        ) else {
            return Status::Ignored;
        };
        if buf.lines().is_empty() {
            return if self.surface.is_selectable_text() {
                Status::Ignored
            } else {
                Status::Consumed
            };
        }
        let hit_includes_cell = endpoint.includes_cell;
        if self.surface.is_selectable_text() && !hit_includes_cell {
            return Status::Ignored;
        }

        let viewport_rows = ctx.viewport.rect.height;
        let click_byte = endpoint.cpos;
        // All leaves stage the click into `drag_endpoint`; `cpos` is committed on Up
        // only for caret-bearing leaves (see `is_caret_leaf`). Readers route through
        // `effective_endpoint` so the cursor and selection track the drag without
        // perturbing the persistent `cpos` mid-gesture.
        self.text_state_mut().drag_endpoint = Some(click_byte);
        self.text_state_mut().drag_endpoint_includes_cell = hit_includes_cell;
        if self.vim_enabled() && matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine) {
            self.set_vim_mode(VimMode::Normal);
        }

        match ctx.click_count {
            2 => {
                self.pin_current_scroll();
                if let Some((s, e)) = self.select_cell_at(click_byte, buf, viewport_rows) {
                    self.text_state_mut().drag_anchor_word = Some((s, e));
                    self.text_state_mut().drag_anchor_line = None;
                } else if let Some((s, e)) = self.select_big_word_at_transparent(
                    click_byte,
                    ctx.soft_breaks,
                    buf,
                    viewport_rows,
                ) {
                    self.text_state_mut().drag_anchor_word = Some((s, e));
                    self.text_state_mut().drag_anchor_line = None;
                }
                Status::Capture
            }
            3 => {
                self.pin_current_scroll();
                if let Some((s, e)) = self.select_block_at(click_byte, buf, viewport_rows) {
                    self.text_state_mut().drag_anchor_line = Some((s, e));
                    self.text_state_mut().drag_anchor_word = None;
                } else if let Some((s, e)) =
                    self.select_line_at(click_byte, ctx.hard_breaks, buf, viewport_rows)
                {
                    self.text_state_mut().drag_anchor_line = Some((s, e));
                    self.text_state_mut().drag_anchor_word = None;
                }
                Status::Capture
            }
            _ => {
                self.text_state_mut().drag_anchor_word = None;
                self.text_state_mut().drag_anchor_line = None;
                self.text_state_mut().selection_anchor = None;
                self.text_state_mut().selection_anchor_includes_cell = false;
                self.text_state_mut().pending_press = Some(click_byte);
                self.text_state_mut().pending_press_includes_cell = hit_includes_cell;
                Status::Capture
            }
        }
    }

    fn mouse_drag(
        &mut self,
        buf: &Buffer,
        event: MouseEvent,
        ctx: &MouseCtx,
        text: &str,
    ) -> Status {
        let viewport_rows = ctx.viewport.rect.height;
        if viewport_rows == 0 || buf.lines().is_empty() {
            return Status::Consumed;
        }
        let endpoint = self
            .mouse_text_endpoint(
                buf,
                event,
                ctx.viewport,
                text,
                MouseEndpointMode::ClampToContent,
            )
            .expect("non-empty viewport produces a mouse endpoint");
        let drag_byte = endpoint.cpos;
        let drag_includes_cell = endpoint.includes_cell;
        self.text_state_mut().drag_endpoint = Some(drag_byte);
        self.text_state_mut().drag_endpoint_includes_cell = drag_includes_cell;

        if self.text_state_mut().drag_anchor_word.is_some() {
            self.extend_word_anchored_drag(buf, ctx, text);
        } else if self.text_state_mut().drag_anchor_line.is_some() {
            self.extend_line_anchored_drag(buf, ctx, text);
        } else {
            if let Some(press) = self.text_state_mut().pending_press.take() {
                self.pin_current_scroll();
                let press_includes_cell = self.text_state().pending_press_includes_cell;
                let press = text::snap(text, press.min(text.len()));
                if self.vim_enabled() {
                    self.begin_visual(VimMode::Visual, press);
                } else {
                    self.text_state_mut().selection_anchor = Some(press);
                    self.text_state_mut().selection_anchor_includes_cell = press_includes_cell;
                }
            } else if !self.vim_enabled() {
                self.extend_selection(self.effective_endpoint());
            }
        }
        Status::Consumed
    }

    /// Selection byte range before `mouse_up` clears anchors; `None` if empty or absent.
    fn mouse_yank_range(
        &self,
        _ctx: &MouseCtx,
        buf: &Buffer,
        text: &str,
    ) -> Option<(usize, usize)> {
        let endpoint = self.effective_endpoint();
        let (start, end) = if self.vim_enabled() {
            vim::visual_range(self.vim_state(), text, endpoint, self.vim_mode())?
        } else if self.is_plain_mouse_drag() {
            self.mouse_selection_range_at(
                endpoint,
                text,
                buf,
                self.text_state().drag_endpoint_includes_cell,
            )?
        } else {
            self.selection_range_at(endpoint, text)?
        };
        if start >= end {
            return None;
        }
        Some((start, end))
    }

    fn mouse_up(&mut self, buf: &Buffer, viewport_rows: u16) -> Status {
        if self.vim_enabled() && matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine) {
            self.set_vim_mode(VimMode::Normal);
        }
        // Commit-or-discard the drag endpoint. Caret leaves keep a visible
        // text cursor, so a click should park that cursor at the release byte.
        // List, selectable-text, and inert surfaces skip the commit: their
        // `cpos` either has no visible meaning or is owned by another widget.
        if let Some(end) = self.text_state_mut().drag_endpoint.take() {
            if self.is_caret_leaf() {
                // The drag endpoint was computed at mouse-down against the
                // buffer state then; by mouse-up the source or projected lines
                // may have shifted (streaming delta into the transcript,
                // resize-driven re-layout). Snap in the same coordinate space
                // that produced the endpoint so the committed cpos never lands
                // past EOF or mid-codepoint.
                let text = Self::coordinate_text(buf);
                let mut cpos = text::snap(text.as_ref(), end.min(text.len()));
                if self.vim_enabled() && matches!(self.vim_mode(), VimMode::Normal) {
                    super::motions::clamp_normal(text.as_ref(), &mut cpos);
                }
                self.set_cpos(cpos);
                self.sync_from_cpos(buf, viewport_rows);
                // Mirror vim: a click or drag-release stamps curswant at the
                // landing column so a subsequent j/k keeps that column.
                self.set_curswant(Some(self.cursor_col() as usize));
            }
        }
        self.text_state_mut().selection_anchor = None;
        self.text_state_mut().selection_anchor_includes_cell = false;
        self.text_state_mut().drag_anchor_word = None;
        self.text_state_mut().drag_anchor_line = None;
        self.text_state_mut().pending_press = None;
        self.text_state_mut().pending_press_includes_cell = false;
        self.text_state_mut().drag_endpoint_includes_cell = false;
        Status::Consumed
    }

    /// Caret-leaf predicate. Mouse-up writes `cpos` only for these.
    fn is_caret_leaf(&self) -> bool {
        self.surface.has_caret()
    }

    /// True when the user is performing a plain (cell-granularity) mouse drag,
    /// as opposed to word- or line-anchored double/triple-click selections.
    fn is_plain_mouse_drag(&self) -> bool {
        self.text_state().drag_endpoint.is_some()
            && self.text_state().drag_anchor_word.is_none()
            && self.text_state().drag_anchor_line.is_none()
    }

    /// Extend drag by WORD units, keeping the original double-clicked word inside the selection.
    fn extend_word_anchored_drag(&mut self, _buf: &Buffer, ctx: &MouseCtx, text: &str) {
        let Some((ws, we)) = self.text_state_mut().drag_anchor_word else {
            return;
        };
        // Vim cursor sits on the last char's start byte; `prev_char_boundary`
        // is the correct step for ASCII and multibyte alike.
        let last_of = |end: usize| smelt_buffer::text::prev_char_boundary(text, end).max(ws);
        let p = self.effective_endpoint();
        let (new_endpoint, new_anchor) = if p >= we {
            let far = super::text::word_range_at_transparent(text, p, ctx.soft_breaks)
                .map(|(_, e)| last_of(e))
                .unwrap_or_else(|| p.max(last_of(we)));
            (far, ws)
        } else if p < ws {
            let near = super::text::word_range_at_transparent(text, p, ctx.soft_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, last_of(we))
        } else {
            (last_of(we), ws)
        };
        self.text_state_mut().drag_endpoint = Some(new_endpoint);
        if self.vim_enabled() {
            self.begin_visual(VimMode::Visual, new_anchor);
        } else {
            self.text_state_mut().selection_anchor = Some(new_anchor);
        }
    }

    fn extend_line_anchored_drag(&mut self, _buf: &Buffer, ctx: &MouseCtx, text: &str) {
        let Some((ls, le)) = self.text_state_mut().drag_anchor_line else {
            return;
        };
        let last_of = |end: usize| smelt_buffer::text::prev_char_boundary(text, end).max(ls);
        let p = self.effective_endpoint();
        let (new_endpoint, new_anchor) = if p >= le {
            let far = super::text::line_range_at(text, p, ctx.hard_breaks)
                .map(|(_, e)| last_of(e))
                .unwrap_or_else(|| p.max(last_of(le)));
            (far, ls)
        } else if p < ls {
            let near = super::text::line_range_at(text, p, ctx.hard_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, last_of(le))
        } else {
            (last_of(le), ls)
        };
        self.text_state_mut().drag_endpoint = Some(new_endpoint);
        if self.vim_enabled() {
            self.begin_visual(VimMode::Visual, new_anchor);
        } else {
            self.text_state_mut().selection_anchor = Some(new_anchor);
        }
    }

    // ── Key dispatch ───────────────────────────────────────────────────

    pub fn handle_key(
        &mut self,
        buf: &mut Buffer,
        k: KeyEvent,
        clipboard: &mut Clipboard,
        now: std::time::Instant,
    ) -> Status {
        let width = self.viewport.map(|v| v.content_width).unwrap_or(80);
        let viewport_rows = self.viewport.map(|v| v.rect.height).unwrap_or(1);
        let action = {
            let mut cpos = self.cpos();
            let action = if buf.readonly {
                let mut scratch = buf.text();
                let mut scratch_history = UndoHistory::default();
                let mut scratch_attachments = Vec::new();
                let text_state = self.surface.text_mut();
                let mut ctx = VimContext {
                    buf: smelt_buffer::attached::AttachedTextMut::new(
                        &mut scratch,
                        &mut scratch_attachments,
                    ),
                    cpos: &mut cpos,
                    history: &mut scratch_history,
                    clipboard,
                    mode: &mut text_state.vim_mode,
                    curswant: &mut text_state.curswant,
                    vim_state: &mut text_state.vim_state,
                    now,
                };
                vim::handle_key(k, &mut ctx)
            } else {
                let (text, history) = buf.edit_refs();
                let text_state = self.surface.text_mut();
                let mut ctx = VimContext {
                    buf: text,
                    cpos: &mut cpos,
                    history,
                    clipboard,
                    mode: &mut text_state.vim_mode,
                    curswant: &mut text_state.curswant,
                    vim_state: &mut text_state.vim_state,
                    now,
                };
                vim::handle_key(k, &mut ctx)
            };
            self.set_cpos(cpos);
            if matches!(action, Action::Passthrough) {
                return Status::Ignored;
            }
            action
        };
        if self.vim_enabled() && self.vim_mode() == VimMode::Insert {
            self.set_vim_mode(VimMode::Normal);
        }
        if buf.readonly {
            // Insertion-mode-entering keys (o, O, i, a, ...) run against a
            // scratch built from `buf.text()` and discard the write, but vim
            // still advances byte offsets inside the scratch. Snap persistent
            // offsets back into the real readonly text-space so follow-up
            // renders and invariant checks see valid anchors.
            let text = buf.text();
            let cpos = self.cpos();
            self.set_cpos(text::snap(&text, cpos.min(text.len())));
            self.text_state_mut().vim_state.clamp_visual_anchor(&text);
        } else {
            buf.sync_after_edit(width);
        }
        // Refresh layout for the possibly-mutated buffer so cursor projection
        // uses the post-edit chunk map.
        self.ensure_layout(buf, width);
        let cpos = self.cpos();
        let (row, col) = self.cursor_visual(buf, cpos);
        let text = self.text_state_mut();
        text.cursor_row = row;
        text.cursor_col = col;
        let total_rows = self.scroll_row_total(buf);
        let viewport_cols = self.viewport.map(|v| v.content_width).unwrap_or(0);
        match action {
            // `zz` recenters the cursor row; horizontal still tracks the cursor.
            Action::CenterScroll => {
                self.recenter_on_cursor(buf, total_rows, viewport_rows);
                self.keep_cursor_visible(buf, total_rows, 0, viewport_cols);
            }
            // `zh`/`zl` pan the horizontal viewport without moving the cursor -
            // the cursor is allowed to scroll off-screen, matching nvim.
            Action::PanColumns(delta) => {
                self.pan_by_columns(delta, viewport_cols);
                self.keep_cursor_visible(buf, total_rows, viewport_rows, 0);
            }
            _ => self.keep_cursor_visible(buf, total_rows, viewport_rows, viewport_cols),
        }
        self.update_tail_state(buf, viewport_rows);
        Status::Consumed
    }

    /// Vim `zz`: scroll so the cursor row sits at the vertical middle of the
    /// viewport, clamped to the available scroll range.
    fn recenter_on_cursor(&mut self, buf: &Buffer, total_rows: RowIndex, viewport_rows: u16) {
        if viewport_rows == 0 {
            return;
        }
        let half = viewport_rows as RowIndex / 2;
        let want = self.absolute_cursor_row().saturating_sub(half);
        let max_scroll = total_rows.saturating_sub(viewport_rows as RowIndex);
        self.set_scroll(want.min(max_scroll), buf);
        self.update_tail_state(buf, viewport_rows);
    }

    /// Cursor-led vertical motion: move `cpos` by `delta` rows; viewport pans only
    /// when the cursor would otherwise leave it. Used by j/k, arrow keys, Ctrl-U/D.
    pub fn move_cursor_by_lines(&mut self, buf: &Buffer, delta: isize, viewport_rows: u16) {
        if buf.lines().is_empty() || viewport_rows == 0 || delta == 0 {
            return;
        }
        let text = buf.text();
        let (new_cpos, new_want) = text::vertical_move(&text, self.cpos(), delta, self.curswant());
        self.set_curswant(Some(new_want));
        self.set_cpos(new_cpos);
        if self.vim_enabled() && self.vim_mode() == VimMode::Insert {
            self.set_vim_mode(VimMode::Normal);
        }
        self.sync_from_cpos(buf, viewport_rows);
        self.update_tail_state(buf, viewport_rows);
    }

    /// Viewport-led horizontal pan: bump `scroll_left` by `delta` cells,
    /// clamped to `[0, max_row_width - viewport_cols]`. The cursor's source-row
    /// column is unchanged; if it would land off-viewport after the pan, the
    /// caller is responsible for tugging it back (vim `zh/zl` keeps the cursor
    /// where it is by design - same as nvim).
    ///
    /// Assumes `ensure_layout` ran this frame; otherwise `max_row_width` is
    /// stale. The host's prep pass guarantees that for any window with a live
    /// viewport - tests that bypass the prep pass should call `ensure_layout`
    /// themselves before calling this.
    pub fn pan_by_columns(&mut self, delta: isize, viewport_cols: u16) {
        if viewport_cols == 0 || delta == 0 {
            return;
        }
        let max_scroll = self.layout.max_row_width().saturating_sub(viewport_cols);
        let cur = self.scroll_left.min(max_scroll);
        self.scroll_left = (cur as isize + delta).clamp(0, max_scroll as isize) as u16;
    }

    /// One step of edge-drag autoscroll. Pans `scroll_top` by `delta` rows and
    /// moves `drag_endpoint` to sit on the new leading edge of the viewport at
    /// the drag's current visual column, so the selection grows by one row per
    /// tick and the endpoint stays parked at the trigger edge for the next
    /// poll. `cpos` is untouched - mouse-up commits the final endpoint.
    /// Returns `true` when the viewport actually moved.
    pub fn drag_autoscroll_step(&mut self, buf: &Buffer, viewport_rows: u16, delta: isize) -> bool {
        if delta == 0 || viewport_rows == 0 {
            return false;
        }
        let total = self.scroll_row_total(buf);
        if total == 0 {
            return false;
        }
        let max_scroll = total.saturating_sub(viewport_rows as RowIndex);
        let cur_scroll = self.scroll_top.min(max_scroll);
        let new_scroll = add_signed_row(cur_scroll, delta).min(max_scroll);
        if new_scroll == self.scroll_top {
            return false;
        }

        // Row-document path: update the document cursor and drag endpoint.
        if self.row_text_state().active {
            let mut state = *self.row_text_state();
            let col = state.preferred_cell_col.unwrap_or_else(|| {
                self.row_position_cell(state, buf, state.drag_endpoint.unwrap_or(state.cursor))
                    .unwrap_or(0)
            });
            self.set_scroll(new_scroll, buf);
            let edge_vrow = if delta < 0 {
                new_scroll
            } else {
                new_scroll
                    .saturating_add(viewport_rows.saturating_sub(1) as RowIndex)
                    .min(total.saturating_sub(1))
            };
            state.cursor.row = edge_vrow;
            state.drag_endpoint = Some(DocPosition {
                row: edge_vrow,
                byte_col: state.cursor.byte_col,
            });
            if let Some(byte_col) = self.row_byte_col_at_cell(state, buf, edge_vrow, col) {
                state.cursor.byte_col = byte_col;
                if let Some(ref mut de) = state.drag_endpoint {
                    de.byte_col = byte_col;
                }
            }
            *self.row_text_state_mut() = state;
            self.project_row_cursor_to_local(state, buf);
            return true;
        }

        let col = self.cursor_visual(buf, self.effective_endpoint()).1 as usize;
        self.set_scroll(new_scroll, buf);
        let edge_vrow = if delta < 0 {
            new_scroll
        } else {
            new_scroll
                .saturating_add(viewport_rows.saturating_sub(1) as RowIndex)
                .min(total.saturating_sub(1))
        };
        // `cpos_at_visual` snaps past non-selectable chrome (transcript fold
        // markers, gutter cells), so no transcript-specific snap is needed here.
        self.text_state_mut().drag_endpoint =
            Some(self.cpos_at_visual(buf, row_to_usize(self.local_row(edge_vrow)), col));
        true
    }

    /// Viewport-led pan (mouse wheel / tmux copy-mode semantics): bump `scroll_top`
    /// by `delta` rows and keep the cursor on the same screen row. The cursor's
    /// buffer row changes to whatever row is now under that screen cell.
    /// To move the cursor first and reveal it afterward, use `move_cursor_by_lines`.
    pub fn pan_by_lines(&mut self, buf: &Buffer, delta: isize, viewport_rows: u16) {
        let total = self.scroll_row_total(buf);
        if total == 0 || viewport_rows == 0 || delta == 0 {
            return;
        }
        let max_scroll = self.clamp_scroll_top(total, viewport_rows, buf);
        if self.is_following_tail() && self.scroll_top != max_scroll {
            self.set_scroll(max_scroll, buf);
        }
        let new_scroll = add_signed_row(self.scroll_top, delta).min(max_scroll);
        self.scroll_to_preserving_cursor_screen_row(new_scroll, buf, viewport_rows);
    }

    /// Set `scroll_top` as a viewport operation: preserve the cursor's screen row
    /// and re-anchor the cursor to the buffer position now under that row.
    pub fn scroll_to_preserving_cursor_screen_row(
        &mut self,
        scroll_top: RowIndex,
        buf: &Buffer,
        viewport_rows: u16,
    ) {
        let total_visual = self.scroll_row_total(buf);
        if total_visual == 0 || viewport_rows == 0 {
            return;
        }
        let max_scroll = self.clamp_scroll_top(total_visual, viewport_rows, buf);
        let cur_scroll = self.scroll_top;

        let screen_row =
            Self::screen_row_or_edge(self.absolute_cursor_row(), cur_scroll, viewport_rows);
        let new_scroll = scroll_top.min(max_scroll);
        // Pin scroll for shift+mouse drag selection, but preserve cursor screen
        // row for vim visual/visual-line mode (the cursor should stay fixed
        // relative to the viewport while wheel-scrolling).
        if self.selection_anchor().is_some() {
            self.set_scroll(new_scroll, buf);
            self.pin_current_scroll();
            return;
        }
        if new_scroll == cur_scroll {
            self.set_scroll(new_scroll, buf);
            self.update_tail_state(buf, viewport_rows);
            return;
        }
        if self.row_text_state().active {
            let mut state = *self.row_text_state();
            let mut cursor = state.cursor;
            let target_row = new_scroll
                .saturating_add(screen_row as RowIndex)
                .min(total_visual.saturating_sub(1));
            self.move_row_cursor_to_row_preserving_cell(&mut state, buf, &mut cursor, target_row);
            state.cursor = cursor;
            self.project_row_cursor_to_local(state, buf);
            *self.row_text_state_mut() = state;
            self.set_scroll(new_scroll, buf);
            self.update_tail_state(buf, viewport_rows);
            return;
        }

        let target_vrow = self
            .local_row(new_scroll.saturating_add(screen_row as RowIndex))
            .min(self.visual_row_total(buf).saturating_sub(1));
        let want = self.curswant().unwrap_or(self.cursor_col() as usize);
        let cpos = self.cpos_at_visual(buf, row_to_usize(target_vrow), want);
        let (row, col) = self.cursor_visual(buf, cpos);
        {
            let text = self.text_state_mut();
            text.cpos = cpos;
            text.cursor_row = row;
            text.cursor_col = col;
            text.curswant = Some(want);
        }
        self.set_scroll(new_scroll, buf);
        self.update_tail_state(buf, viewport_rows);
    }

    /// One-shot positioning. Leaves tail-follow state alone - callers that want
    /// tail-follow re-engagement (transcript `<C-End>`) call `scroll_to_bottom`
    /// instead; non-streaming surfaces (pickers, dialog lists) get to stay at
    /// their default `false` even when the cursor lands on the last row.
    fn jump_to_line_col(&mut self, buf: &Buffer, line_idx: usize, col: usize, viewport_rows: u16) {
        let rows = buf.lines();
        if rows.is_empty() {
            return;
        }
        let line_idx = line_idx.min(rows.len() - 1);
        let cpos = buf.byte_at_display_pos(line_idx, col);
        self.set_cpos(cpos);
        let landed_col = buf.display_cursor_pos(cpos).1;
        self.set_curswant(Some(landed_col));
        self.sync_from_cpos(buf, viewport_rows);
    }

    pub fn visible_range(&self, viewport_rows: u16) -> std::ops::Range<RowIndex> {
        self.scroll_top..self.scroll_top.saturating_add(viewport_rows as RowIndex)
    }

    pub fn render(&self, buf: &Buffer, slice: &mut GridSlice<'_>, ctx: &DrawContext) {
        use unicode_width::UnicodeWidthChar;

        let width = slice.width();
        let height = slice.height();
        let scroll = row_to_usize(self.local_scroll_top());
        let gutter_width = self.gutter_width(buf).min(width);
        let pad_left = self
            .config
            .gutters
            .pad_left
            .min(width.saturating_sub(gutter_width));
        // `content_offset` is where source text begins on each row.
        let content_offset = gutter_width + pad_left;
        let content_width = self
            .config
            .gutters
            .content_width(width)
            .min(width.saturating_sub(content_offset));
        // The host's prep pass refreshes the layout before paint. Tests that
        // skip the prep pass build a one-shot fallback so render stays correct
        // without requiring &mut self.
        let fallback_layout;
        let layout_key = (
            buf.changedtick(),
            content_width,
            self.wrap,
            self.wrap_cursor_padding,
        );
        let layout = if self.layout_key == Some(layout_key) {
            &self.layout
        } else {
            fallback_layout = if self.wrap_cursor_padding {
                WrappedLayout::from_buffer_with_cursor_padding(buf, content_width, self.wrap)
            } else {
                WrappedLayout::from_buffer(buf, content_width, self.wrap)
            };
            &fallback_layout
        };
        let line_count = layout.visual_count();
        // Screen row of the cursor (or drag endpoint mid-gesture). Drives the
        // `CursorLine` bg fill below when `cursor_line` / `selection_highlight`
        // is on, and gates `on_cursor_row` extmark painting regardless so
        // selection-aware spans always work.
        let cursor_screen_row = {
            let effective_row = self.absolute_row(self.effective_cursor_row(buf));
            Self::screen_row_at(effective_row, self.scroll_top, height)
        };
        // Two opt-ins; `selection_highlight` always paints (picker semantics),
        // `cursor_line` paints only when this window owns focus (caret semantics).
        let fill_cursor_row = self.selection_highlight || (self.cursor_line && ctx.focused);
        let normal_style = ctx.theme.get("Normal");
        let cursor_style = ctx.theme.get("CursorLine");
        // Buffer override wins; fall back to window anchors.
        let selection_owned: Vec<smelt_buffer::buffer::SelectionRange>;
        let selection_layer = buf.range_layer(crate::RangeLayer::Selection);
        let selection_ranges: &[smelt_buffer::buffer::SelectionRange] =
            if !selection_layer.is_empty() {
                selection_layer
            } else {
                selection_owned = self.auto_selection_ranges(buf, ctx.vim_mode);
                &selection_owned[..]
            };
        // Reused per-row scratch - avoids `height` allocations of each Vec.
        let mut col_to_char: Vec<usize> = Vec::with_capacity(content_width as usize);
        let mut line_chars: Vec<char> = Vec::with_capacity(content_width as usize);
        let mut spans_buf: Vec<smelt_buffer::buffer::Span> = Vec::new();
        let mut vt_buf: Vec<smelt_buffer::buffer::VirtualText> = Vec::new();
        let mut mask_buf: Vec<bool> = Vec::with_capacity(content_width as usize);
        for row in 0..height {
            let visual_row = scroll + row as usize;
            // Map visual → logical for extmark lookup. `chunk_cell_offset` is how
            // many cells the preceding chunks of this logical line occupy; spans
            // and selections (stored in logical-line cells) shift by `-offset`
            // to land in this chunk's cell space.
            let logical = (visual_row < line_count)
                .then(|| layout.logical_at_visual(visual_row))
                .flatten();
            let (logical_row, chunk_idx) = logical.unwrap_or((0, 0));
            let chunk_cell_offset: u16 = if logical.is_some() && chunk_idx > 0 {
                let logical_line = buf.get_line(logical_row).unwrap_or("");
                let chunks = layout.chunks_of(logical_row);
                let (cs, _) = chunks[chunk_idx];
                smelt_buffer::text::byte_to_cell(logical_line, cs) as u16
            } else {
                0
            };
            let decoration = logical.map(|_| buf.decoration_at(logical_row));
            let base_row_style = if fill_cursor_row && cursor_screen_row == Some(row) {
                cursor_style
            } else {
                normal_style
            };
            let row_style = match decoration.and_then(|d| d.fill_bg) {
                Some(bg) => Style {
                    bg: Some(bg),
                    ..base_row_style
                },
                None => base_row_style,
            };
            // `base_row_style` covers the whole slice (gutter and right margin)
            // so cursor/normal highlights span every column. `fill_bg` is a
            // layout-scoped decoration: it only overrides the content region
            // so the bg lines up with `pad_row_to_layout_width`-padded rows
            // and doesn't leak into chrome columns.
            if base_row_style != Style::default() {
                for col in 0..width {
                    slice.set(col, row, ' ', base_row_style);
                }
            }
            if row_style != base_row_style {
                let end = content_offset.saturating_add(content_width).min(width);
                for col in content_offset..end {
                    slice.set(col, row, ' ', row_style);
                }
            }
            if let Some(provider) = self.gutter.as_ref() {
                if gutter_width > 0 {
                    // Continuation chunks (`chunk_idx > 0`) leave the gutter blank
                    // so a wrapped logical line doesn't repeat its line number.
                    let cell = if logical.is_some() && chunk_idx == 0 {
                        provider.cell(buf, &ctx.theme, logical_row)
                    } else {
                        None
                    };
                    if let Some(g) = cell {
                        let style = merge_styles(base_row_style, g.style);
                        let mut c: u16 = 0;
                        for ch in g.text.chars() {
                            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1) as u16;
                            if c + cw > gutter_width {
                                break;
                            }
                            slice.set(c, row, ch, style);
                            c += cw;
                        }
                        // Pad to gutter_width with the gutter's base style so
                        // partial cells inherit chrome bg, never `fill_bg`.
                        for fill in c..gutter_width {
                            slice.set(fill, row, ' ', base_row_style);
                        }
                    }
                }
            }
            let Some(line) = layout.visual_line(buf.lines(), visual_row) else {
                continue;
            };
            col_to_char.clear();
            line_chars.clear();
            line_chars.extend(line.chars());
            // Two-axis pan: `src_col` walks the source row in cell space (drives
            // skip/clip against `scroll_left`); `dst_col` is where each surviving
            // glyph lands in the viewport. A wide char straddling the left edge
            // collapses to a leading space so wide-char cells aren't smeared.
            //
            // `to_viewport_col` translates a span/selection column (in source-row
            // cell space, after chunk_cell_offset for wrapped continuations) into
            // the viewport's cell space - clamped so out-of-view spans collapse
            // to a no-op range rather than panicking on subtraction.
            let scroll_left = self.scroll_left;
            let to_viewport_col = |source: u16| -> u16 {
                source
                    .saturating_sub(chunk_cell_offset)
                    .saturating_sub(scroll_left)
                    .min(content_width)
            };
            let mut src_col: u16 = 0;
            let mut dst_col: u16 = 0;
            for (ci, ch) in line_chars.iter().enumerate() {
                let cw = UnicodeWidthChar::width(*ch).unwrap_or(0).max(1) as u16;
                if src_col.saturating_add(cw) <= scroll_left {
                    src_col = src_col.saturating_add(cw);
                    continue;
                }
                let (painted, eff_cw) = if src_col < scroll_left {
                    let visible = src_col.saturating_add(cw).saturating_sub(scroll_left);
                    (' ', visible)
                } else {
                    (*ch, cw)
                };
                if dst_col.saturating_add(eff_cw) > content_width {
                    break;
                }
                slice.set(content_offset + dst_col, row, painted, row_style);
                col_to_char.push(ci);
                for _ in 1..eff_cw {
                    col_to_char.push(ci);
                }
                dst_col = dst_col.saturating_add(eff_cw);
                src_col = src_col.saturating_add(cw);
            }
            let content_end_col = dst_col;
            spans_buf.clear();
            if logical.is_some() {
                buf.highlights_at_into(logical_row, &mut spans_buf);
            }
            let is_cursor_row = cursor_screen_row == Some(row);
            for span in &spans_buf {
                if span.on_cursor_row && !is_cursor_row {
                    continue;
                }
                let span_style = ctx.theme.resolve(span.hl);
                let style = merge_span_style(row_style, &span_style);
                let start = to_viewport_col(span.col_start);
                let end = to_viewport_col(span.col_end);
                if end > start {
                    paint_span_cells(
                        slice,
                        content_offset,
                        row,
                        start,
                        end,
                        &col_to_char,
                        &line_chars,
                        style,
                        None,
                    );
                }
                if span.hl_eol {
                    for c in end..content_width {
                        slice.set(content_offset + c, row, ' ', style);
                    }
                }
            }
            // Selection painting: after highlights (wins over base) but before virt-text.
            // Mask out cells under `selectable = false` spans so chrome (e.g. inline
            // gutter, line-number column) doesn't receive the Visual bg. Skip the mask
            // when the row has only chrome and no selectable cells - the fallback
            // selection span placed after the chrome by `byte_range_to_row_ranges` will
            // paint there, keeping multi-line selections visually continuous without
            // highlighting the chrome itself.
            let line_has_highlight = crate::RangeLayer::paint_order().into_iter().any(|layer| {
                let ranges = match layer {
                    crate::RangeLayer::Selection => selection_ranges,
                    _ => buf.range_layer(layer),
                };
                ranges.iter().any(|r| r.line == logical_row)
            });
            let any_chrome = spans_buf.iter().any(|s| !s.meta.selectable);
            let any_selectable =
                cell_range_contains_selectable(&spans_buf, 0, text::byte_to_cell(line, line.len()));
            let mask_slice: Option<&[bool]> = if line_has_highlight && any_chrome && any_selectable
            {
                mask_buf.clear();
                mask_buf.resize(content_width as usize, true);
                for span in spans_buf.iter().filter(|s| !s.meta.selectable) {
                    let start = to_viewport_col(span.col_start) as usize;
                    let end = to_viewport_col(span.col_end) as usize;
                    for slot in mask_buf.iter_mut().take(end).skip(start) {
                        *slot = false;
                    }
                }
                Some(mask_buf.as_slice())
            } else {
                None
            };
            if logical.is_some() {
                let mut paint_ranges =
                    |ranges: &[smelt_buffer::buffer::SelectionRange], style: crate::grid::Style| {
                        for r in ranges.iter().filter(|r| r.line == logical_row) {
                            let start = to_viewport_col(r.col_start);
                            let end = to_viewport_col(r.col_end);
                            if end > start {
                                paint_span_cells(
                                    slice,
                                    content_offset,
                                    row,
                                    start,
                                    end,
                                    &col_to_char,
                                    &line_chars,
                                    style,
                                    mask_slice,
                                );
                            }
                        }
                    };
                for layer in crate::RangeLayer::paint_order() {
                    let ranges = match layer {
                        crate::RangeLayer::Selection => selection_ranges,
                        _ => buf.range_layer(layer),
                    };
                    let style =
                        merge_span_style(row_style, &ctx.theme.get(layer.highlight_group()));
                    paint_ranges(ranges, style);
                }
            }
            vt_buf.clear();
            // Virtual text attaches to the logical row's first chunk only; we
            // don't want EOL/RightAlign virt-text duplicated on every wrapped row.
            if logical.is_some() && chunk_idx == 0 {
                buf.virtual_text_at_into(logical_row, &mut vt_buf);
            }
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
                    VirtTextPos::Inline | VirtTextPos::Overlay => (vt.col as u16)
                        .saturating_sub(scroll_left)
                        .min(content_width),
                    VirtTextPos::RightAlign => content_width.saturating_sub(vt_width),
                };
                let mut c = start_col;
                for ch in vt.text.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1) as u16;
                    if c + cw > content_width {
                        break;
                    }
                    slice.set(content_offset + c, row, ch, style);
                    c += cw;
                }
            }
        }

        if let Some(viewport) = self.viewport {
            paint_scrollbar(slice, viewport, &ctx.theme);
        }

        // `ctx.cursor_shape == Block` reaches this leaf only when it is the
        // `Ui::active_cursor_leaf` for the frame (drag-active leaf, else focus) -
        // exactly one leaf paints a block per frame. Position derives from
        // `effective_endpoint`: `drag_endpoint` during a drag (so the cursor tracks
        // the mouse, even on non-focusable leaves like notifications), `cpos`
        // otherwise.
        if let CursorShape::Block { glyph, style, pos } = ctx.cursor_shape {
            let resolved = pos.or_else(|| {
                let end = self.effective_endpoint();
                let (row, col) = self.cursor_visual(buf, end);
                // Snap visual cursor past chrome spans (e.g. inline gutter) so the
                // caret never paints inside non-selectable cells. Logical row is
                // resolved via the layout - when the layout doesn't match, fall
                // back to identity (vrow == logical).
                let logical_row = self
                    .layout
                    .logical_at_visual(row_to_usize(row))
                    .map(|(lr, _)| lr)
                    .unwrap_or_else(|| row_to_usize(row));
                let col = snap_col_past_chrome(buf, logical_row, col);
                Self::screen_row_at(self.absolute_row(row), self.scroll_top, height)
                    .map(|screen_row| (col, screen_row))
            });
            if let Some((col, screen_row)) = resolved {
                // `col` is in source-row cells; shift into viewport coords. Off
                // either side of the horizontal viewport drops the caret paint.
                if col >= self.scroll_left && screen_row < height {
                    let dst_col = col - self.scroll_left;
                    if dst_col < content_width {
                        let under = slice.cell(content_offset + dst_col, screen_row).symbol;
                        let painted = if under == '\0' || under == ' ' {
                            glyph
                        } else {
                            under
                        };
                        slice.set(content_offset + dst_col, screen_row, painted, style);
                    }
                }
            }
        }
    }
}

fn add_signed_row(row: RowIndex, delta: isize) -> RowIndex {
    if delta >= 0 {
        row.saturating_add(delta as RowIndex)
    } else {
        row.saturating_sub(delta.unsigned_abs() as RowIndex)
    }
}

fn include_cursor_cell(buf: &Buffer, src: &str, pos: usize) -> usize {
    let pos = text::snap(src, pos.min(src.len()));
    if cursor_cell_selectable(buf, pos) && pos < src.len() && src.as_bytes()[pos] != b'\n' {
        text::next_char_boundary(src, pos)
    } else {
        pos
    }
}

fn cursor_cell_selectable(buf: &Buffer, cpos: usize) -> bool {
    let (row, col) = buf.display_cursor_pos(cpos);
    let Some(line) = buf.get_line(row) else {
        return false;
    };
    let line_width = text::byte_to_cell(line, line.len());
    if col >= line_width {
        return false;
    }
    let spans = buf.highlights_at(row);
    cell_range_contains_selectable(&spans, col, col.saturating_add(1))
}

/// Advance `col` past any leading non-selectable (chrome) spans on `logical_row`.
/// Used by cursor positioning so the caret never lands inside an inline gutter
/// or other chrome - caller-supplied col is clamped forward to the first
/// selectable cell, repeated until no chrome span covers it. If advancing
/// would carry the cursor past every selectable cell on the row (e.g. click in
/// a trailing bg pad, or an all-chrome user-block padding row), clamp back to
/// the last selectable col_end so the cursor never escapes the row's content
/// edge and triggers horizontal pan.
fn cell_range_contains_selectable(
    spans: &[smelt_buffer::buffer::Span],
    start: usize,
    end: usize,
) -> bool {
    if start >= end {
        return false;
    }
    (start..end).any(|col| {
        !spans.iter().any(|span| {
            !span.meta.selectable && col >= span.col_start as usize && col < span.col_end as usize
        })
    })
}

fn snap_col_past_chrome(buf: &Buffer, logical_row: usize, col: u16) -> u16 {
    let mut spans = Vec::new();
    buf.highlights_at_into(logical_row, &mut spans);
    if spans.iter().all(|s| s.meta.selectable) {
        return col;
    }
    let mut col = col;
    loop {
        let mut advanced = false;
        for s in &spans {
            if !s.meta.selectable && s.col_start <= col && col < s.col_end {
                col = s.col_end;
                advanced = true;
            }
        }
        if !advanced {
            break;
        }
    }
    let Some(line) = buf.get_line(logical_row) else {
        return col;
    };
    let line_width = text::byte_to_cell(line, line.len()).min(u16::MAX as usize) as u16;
    if col < line_width && cell_range_contains_selectable(&spans, col as usize, col as usize + 1) {
        return col;
    }
    let last_selectable_edge = (0..line_width)
        .filter(|c| cell_range_contains_selectable(&spans, *c as usize, *c as usize + 1))
        .map(|c| c.saturating_add(1))
        .next_back();
    match last_selectable_edge {
        Some(edge) if col > edge => edge,
        None => u16::from(line_width == 1),
        _ => col.min(line_width),
    }
}

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
        reverse: base.reverse || top.reverse,
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
        reverse: base.reverse || span.reverse,
    }
}

#[cfg(test)]
mod tests {
    use super::BufId;
    use super::*;
    use crate::grid::Grid;
    use crate::BufCreateOpts;
    use crate::Theme;
    use crossterm::event::KeyCode;
    use std::time::{Duration, Instant};

    fn make_win() -> Window {
        Window::new(
            WinId(1),
            BufId(1),
            SplitConfig {
                region: "test".into(),
                // Default reserves a scrollbar column; tests below assert on
                // bare-width content layout, so opt out.
                gutters: Gutters {
                    scrollbar: false,
                    ..Default::default()
                },
            },
        )
    }

    fn sample_rows(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    fn make_buf(rows: Vec<String>) -> Buffer {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(rows);
        buf
    }

    fn viewport(width: u16, height: u16) -> WindowViewport {
        WindowViewport {
            rect: Rect::new(0, 0, width, height),
            gutter_width: 0,
            content_width: width,
            scroll_top: 0,
            total_rows: height as RowIndex,
            scrollbar: None,
        }
    }

    #[test]
    fn text_hit_at_mouse_distinguishes_selectable_text_from_chrome() {
        let w = make_win();
        let mut buf = make_buf(vec!["abc----xyz".into()]);
        buf.add_highlight_group_with_meta(
            0,
            3,
            7,
            smelt_buffer::theme::intern("Normal"),
            smelt_buffer::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        let vp = viewport(20, 1);

        let chrome = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 4,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert!(!w.text_hit_at_mouse(&buf, chrome, vp).is_selectable());

        let text = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 8,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert!(w.text_hit_at_mouse(&buf, text, vp).is_selectable());

        let trailing = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 11,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert!(!w.text_hit_at_mouse(&buf, trailing, vp).is_selectable());
    }

    #[test]
    fn selectable_text_mouse_down_ignores_empty_buffer() {
        let mut w = make_win();
        w.set_surface(WindowSurface::selectable_text());
        let buf = make_buf(Vec::new());
        let soft = Vec::new();
        let hard = Vec::new();
        let ctx = MouseCtx {
            soft_breaks: &soft,
            hard_breaks: &hard,
            viewport: viewport(20, 1),
            click_count: 1,
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };

        let (status, range) = w.handle_mouse(&buf, event, ctx);

        assert_eq!(status, Status::Ignored);
        assert!(range.is_none());
        assert!(w.text_state().pending_press.is_none());
        assert!(w.text_state().drag_endpoint.is_none());
    }

    #[test]
    fn selectable_text_mouse_down_ignores_chrome_in_edit_policy() {
        let mut w = make_win();
        w.set_surface(WindowSurface::selectable_text());
        let mut buf = make_buf(vec!["abc----xyz".into()]);
        buf.add_highlight_group_with_meta(
            0,
            3,
            7,
            smelt_buffer::theme::intern("Normal"),
            smelt_buffer::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        let vp = viewport(20, 1);
        let soft = Vec::new();
        let hard = Vec::new();
        let ctx = MouseCtx {
            soft_breaks: &soft,
            hard_breaks: &hard,
            viewport: vp,
            click_count: 1,
        };
        let chrome = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 4,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };

        let (status, range) = w.handle_mouse(&buf, chrome, ctx);

        assert_eq!(status, Status::Ignored);
        assert!(range.is_none());
        assert!(w.text_state().pending_press.is_none());
        assert!(w.text_state().drag_endpoint.is_none());

        let text = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 8,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let (status, range) = w.handle_mouse(&buf, text, ctx);

        assert_eq!(status, Status::Capture);
        assert!(range.is_none());
        assert!(w.text_state().pending_press.is_some());
        assert!(w.text_state().drag_endpoint.is_some());
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
    fn cursor_visual_survives_in_place_line_shrink() {
        // Regression: a line that mutates to be shorter than the cached chunk's
        // end byte must not slice into a stale `(start, end)` range. Before
        // tightening `layout_matches` to compare on `changedtick`, this panicked
        // with "end byte index N is out of bounds of empty string" because the
        // logical row count stayed the same.
        let mut w = make_win();
        w.wrap = true;
        let mut buf = make_buf(vec!["hello world hello world".to_string()]);
        w.ensure_layout(&buf, 10);
        // Mutate the line in place to empty (e.g. prompt clear after submit).
        buf.set_all_lines(vec![String::new()]);
        // No new `ensure_layout` yet - must not panic.
        let _ = w.cursor_visual(&buf, 0);
    }

    #[test]
    fn scroll_to_bottom_sets_tail_mode() {
        let mut w = make_win();
        w.pin_scroll(10);
        w.scroll_to_bottom();
        assert_eq!(w.scroll_top, 10);
        assert!(w.is_following_tail());
    }

    #[test]
    fn row_text_tail_checks_use_logical_total_rows() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        w.set_materialized_rows(80, 20, 100);
        w.scroll_top = 94;
        assert!(!w.is_at_tail(&buf, 5));
        w.scroll_top = 95;
        assert!(w.is_at_tail(&buf, 5));
    }

    #[test]
    fn scrollbar_maps_rows_beyond_u16() {
        let bar = ScrollbarState::new(0, 1_000_000, 10).expect("overflowing scrollbar");
        let bottom = bar.scroll_from_top_for_thumb(bar.max_thumb_top());
        assert_eq!(bottom, 999_990);
        assert_eq!(bar.thumb_top_for_scroll(999_990), bar.max_thumb_top());
        assert_eq!(bar.thumb_top_for_scroll(500_000), 5);
    }

    #[test]
    fn cursor_screen_row_handles_large_scroll_top() {
        let mut w = make_win();
        w.scroll_top = 70_000;
        w.text_state_mut().cursor_row = 70_003;
        assert_eq!(w.cursor_screen_row(10), Some(3));
        w.text_state_mut().cursor_row = 69_999;
        assert_eq!(w.cursor_screen_row(10), None);
    }

    #[test]
    fn window_viewport_keeps_large_total_rows() {
        let vp = WindowViewport::new(Rect::new(0, 0, 20, 10), 20, 100_000, 80_000, None);
        assert_eq!(vp.total_rows, 100_000);
        assert_eq!(vp.scroll_top, 80_000);
    }

    #[test]
    fn move_cursor_by_lines_advances_cursor_within_viewport() {
        let mut w = make_win();
        w.set_vim_enabled(true);
        w.set_vim_mode(VimMode::Normal);
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        w.jump_to_line_col(&buf, 0, 0, viewport);
        assert_eq!(w.cursor_row(), 0);
        assert_eq!(w.scroll_top, 0);
        w.move_cursor_by_lines(&buf, 1, viewport);
        // Cursor-led: cursor moves down, viewport stays put.
        assert_eq!(w.cursor_row(), 1);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn handle_key_vim_move_up_pins_scroll_when_leaving_tail() {
        // Vim key dispatch through handle_key must update tail state just like
        // move_cursor_by_lines does; otherwise the render loop can keep snapping
        // scroll_top to the bottom every frame after the cursor moves away.
        use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
        let mut w = make_win();
        w.set_vim_enabled(true);
        w.set_vim_mode(VimMode::Normal);
        let rows = sample_rows(11);
        let mut buf = make_buf(rows.clone());
        let viewport = 2;
        w.viewport = Some(WindowViewport::new(
            Rect::new(0, 0, 80, viewport),
            80,
            rows.len() as RowIndex,
            0,
            None,
        ));
        w.jump_to_line_col(&buf, 10, 0, viewport);
        w.scroll_to_bottom();
        assert!(w.is_following_tail());
        assert_eq!(w.scroll_top, 9);

        let k = KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        let mut clipboard = Clipboard::null();
        // One 'k' keeps us in the bottom region (scroll_top == max_scroll == 9),
        // so tail mode should still be active.
        w.handle_key(&mut buf, k, &mut clipboard, std::time::Instant::now());
        assert_eq!(w.cursor_row(), 9);
        assert_eq!(w.scroll_top, 9);
        assert!(
            w.is_following_tail(),
            "still at max_scroll -> tail mode stays active"
        );

        // Second 'k' moves above max_scroll; tail mode must clear.
        w.handle_key(&mut buf, k, &mut clipboard, std::time::Instant::now());
        assert_eq!(w.cursor_row(), 8);
        assert_eq!(w.scroll_top, 8);
        assert!(
            !w.is_following_tail(),
            "moving above max_scroll must pin scroll"
        );
    }

    #[test]
    fn pan_by_lines_pans_viewport_and_pins_cursor_screen_row() {
        // Tmux copy-mode semantics: pan moves the viewport and keeps the cursor
        // on the same screen row. The cursor's buffer row changes with the row
        // now visible under that screen cell.
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        w.jump_to_line_col(&buf, 5, 3, viewport);
        let screen_row = w.cursor_screen_row(viewport);
        w.pan_by_lines(&buf, 2, viewport);
        assert_eq!(w.scroll_top, 2);
        assert_eq!(w.cursor_screen_row(viewport), screen_row);
        assert_eq!(w.cursor_row(), 7);
        let offsets = smelt_buffer::text::line_start_offsets(&rows);
        assert_eq!(w.cpos(), offsets[7] + 3);
    }

    #[test]
    fn pan_by_lines_preserves_selection_endpoint() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows);
        let viewport = 10;
        w.jump_to_line_col(&buf, 5, 0, viewport);
        let endpoint = w.cpos();
        w.text_state_mut().selection_anchor = Some(0);
        w.pan_by_lines(&buf, 3, viewport);
        assert_eq!(w.scroll_top, 3);
        assert_eq!(w.cpos(), endpoint);
        assert_eq!(w.scroll_state(), VerticalScroll::Pinned);
    }

    #[test]
    fn pan_by_lines_clamps_at_top() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        w.jump_to_line_col(&buf, 0, 0, viewport);
        assert_eq!(w.scroll_top, 0);
        w.pan_by_lines(&buf, -3, viewport);
        assert_eq!(w.scroll_top, 0, "scroll clamps at 0");
    }

    #[test]
    fn pan_by_lines_clamps_to_wrapped_visual_bottom() {
        let mut w = make_win();
        w.wrap = true;
        w.wrap_cursor_padding = true;
        let rows: Vec<String> = (0..20).map(|_| "abcdef".to_string()).collect();
        let buf = make_buf(rows);
        let width = 3;
        let viewport = 5;
        w.ensure_layout(&buf, width);
        let total_visual_rows = w.layout().visual_count() as RowIndex;
        assert!(
            total_visual_rows > buf.line_count() as RowIndex,
            "test input must wrap to more visual rows than logical rows"
        );

        w.jump_to_line_col(&buf, 0, 0, viewport);
        w.pan_by_lines(&buf, 1000, viewport);

        assert_eq!(
            w.scroll_top,
            total_visual_rows.saturating_sub(viewport as RowIndex),
            "wheel/scrollbar panning must clamp against wrapped visual rows"
        );
    }

    #[test]
    fn pan_by_lines_reengages_tail_when_scrolling_to_bottom() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        let max_scroll = (30 - viewport) as RowIndex;
        w.jump_to_line_col(&buf, 15, 0, viewport);
        w.pin_scroll(max_scroll - 5);
        assert!(!w.is_following_tail());

        w.pan_by_lines(&buf, 5, viewport);

        assert_eq!(w.scroll_top, max_scroll, "scroll clamps at max");
        assert!(
            w.is_following_tail(),
            "normal scroll to the bottom re-engages tail follow"
        );
    }

    #[test]
    fn pan_by_lines_with_selection_does_not_reengage_tail_at_bottom() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        let max_scroll = (30 - viewport) as RowIndex;
        w.jump_to_line_col(&buf, 15, 0, viewport);
        w.pin_scroll(max_scroll - 5);
        w.text_state_mut().selection_anchor = Some(w.cpos().saturating_sub(1));

        w.pan_by_lines(&buf, 5, viewport);

        assert_eq!(w.scroll_top, max_scroll, "scroll clamps at max");
        assert!(
            !w.is_following_tail(),
            "selection at the bottom keeps the viewport pinned"
        );
    }

    #[test]
    fn pan_by_lines_down_from_tail_resolves_to_max_scroll() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        let max_scroll = (30 - viewport) as RowIndex;
        w.jump_to_line_col(&buf, 29, 0, viewport);
        w.pin_scroll(0);
        w.scroll_to_bottom();
        w.pan_by_lines(&buf, 3, viewport);
        assert_eq!(w.scroll_top, max_scroll, "scroll stays at max_scroll");
        assert!(w.is_following_tail());
    }

    #[test]
    fn pan_by_lines_up_from_tail_pins_scroll() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        let max_scroll = (30 - viewport) as RowIndex;
        w.jump_to_line_col(&buf, 29, 0, viewport);
        w.pin_scroll(0);
        w.scroll_to_bottom();
        w.pan_by_lines(&buf, -3, viewport);
        assert_eq!(w.scroll_top, max_scroll - 3);
        assert!(!w.is_following_tail());
    }

    #[test]
    fn pan_by_columns_pans_viewport_without_moving_cursor() {
        // `zh`/`zl` and wheel-horizontal: pan the column viewport without
        // touching cpos. The cursor can scroll off-screen - that's what the
        // bindings are for.
        let mut w = make_win();
        let row = "x".repeat(100);
        let buf = make_buf(vec![row]);
        w.ensure_layout(&buf, 100);
        w.text_state_mut().cpos = 0;
        w.pan_by_columns(10, 20);
        assert_eq!(w.scroll_left, 10);
        assert_eq!(w.cpos(), 0, "pan must not touch cpos");
    }

    #[test]
    fn pan_by_columns_clamps_at_left_edge() {
        let mut w = make_win();
        let row = "x".repeat(100);
        let buf = make_buf(vec![row]);
        w.ensure_layout(&buf, 100);
        w.pan_by_columns(-5, 20);
        assert_eq!(w.scroll_left, 0);
    }

    #[test]
    fn pan_by_columns_clamps_to_max_row_width_minus_viewport() {
        let mut w = make_win();
        let row = "x".repeat(100);
        let buf = make_buf(vec![row]);
        w.ensure_layout(&buf, 100);
        w.pan_by_columns(1000, 20);
        assert_eq!(
            w.scroll_left, 80,
            "scroll_left clamps to max_row_width - viewport_cols"
        );
    }

    #[test]
    fn keep_cursor_visible_pans_horizontally_when_off_right() {
        let mut w = make_win();
        let buf = make_buf(vec![]);
        w.scroll_left = 0;
        w.text_state_mut().cursor_col = 95;
        w.keep_cursor_visible(&buf, 0, 0, 20);
        assert_eq!(w.scroll_left, 76);
    }

    #[test]
    fn keep_cursor_visible_does_not_pan_readonly_full_width_row() {
        let mut w = make_win();
        let mut buf = make_buf(vec!["x".repeat(20)]);
        buf.readonly = true;
        w.ensure_layout(&buf, 20);
        w.text_state_mut().cursor_col = 20;
        w.keep_cursor_visible(&buf, 1, 1, 20);
        assert_eq!(w.scroll_left, 0);
    }

    #[test]
    fn keep_cursor_visible_snaps_back_horizontally_when_off_left() {
        let mut w = make_win();
        let buf = make_buf(vec![]);
        w.scroll_left = 50;
        w.text_state_mut().cursor_col = 10;
        w.keep_cursor_visible(&buf, 0, 0, 20);
        assert_eq!(w.scroll_left, 10);
    }

    #[test]
    fn keep_cursor_visible_clamps_scroll_when_content_shrinks() {
        let mut w = make_win();
        let buf = make_buf(vec![]);
        // 10 rows of content, 4-row viewport, cursor at end.
        w.text_state_mut().cursor_row = 9;
        w.keep_cursor_visible(&buf, 10, 4, 0);
        assert_eq!(w.scroll_top, 6);

        // Content shrinks to 8 rows: max_scroll becomes 4.
        // Cursor drops to row 7, still inside the old viewport [6, 9],
        // but scroll_top is now past the end. It must be clamped down.
        w.text_state_mut().cursor_row = 7;
        w.keep_cursor_visible(&buf, 8, 4, 0);
        assert_eq!(w.scroll_top, 4);
    }

    #[test]
    fn pan_by_lines_keeps_cursor_visible_after_repeated_pan() {
        // Pan the viewport multiple times; cursor row stays inside viewport.
        let mut w = make_win();
        let rows = sample_rows(50);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        w.jump_to_line_col(&buf, 5, 0, viewport);
        let screen_row = w.cursor_screen_row(viewport);
        for _ in 0..3 {
            w.pan_by_lines(&buf, 3, viewport);
        }
        assert_eq!(w.scroll_top, 9);
        assert_eq!(w.cursor_screen_row(viewport), screen_row);
        assert_eq!(w.cursor_row(), 14);
    }

    #[test]
    fn refocus_on_empty_resets_cursor() {
        let mut w = make_win();
        w.text_state_mut().cursor_row = 5;
        w.text_state_mut().cursor_col = 3;
        w.refocus(&make_buf(vec![]), 20);
        assert_eq!(w.cursor_row(), 0);
        assert_eq!(w.cursor_col(), 0);
    }

    #[test]
    fn jump_to_last_line_scrolls_to_bottom() {
        let mut w = make_win();
        let rows = sample_rows(50);
        let buf = make_buf(rows.clone());
        let viewport = 10;
        w.jump_to_line_col(&buf, 49, 0, viewport);
        assert_eq!(w.scroll_top, 40);
        assert_eq!(w.cursor_row(), 49, "buffer-absolute row");
        assert_eq!(w.cursor_screen_row(viewport), Some(9));
    }

    #[test]
    fn cursor_screen_row_subtracts_scroll() {
        let mut w = make_win();
        w.scroll_top = 10;
        w.text_state_mut().cursor_row = 15;
        assert_eq!(w.cursor_abs_row(), 15);
        assert_eq!(w.cursor_screen_row(10), Some(5));
        // Out of viewport above and below collapses to None.
        w.text_state_mut().cursor_row = 9;
        assert_eq!(w.cursor_screen_row(10), None);
        w.text_state_mut().cursor_row = 20;
        assert_eq!(w.cursor_screen_row(10), None);
    }

    #[test]
    fn row_text_goto_keeps_absolute_cursor_across_materialized_slice() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 100,
            row_base: 100,
            total_rows: 200,
            materialized_rows: 20,
        });
        w.set_resolved_scroll(100);
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(150), 10, now);

        assert_eq!(w.row_cursor().unwrap().row, 150);
        assert_eq!(w.scroll_top(), 141);

        let buf = make_buf(sample_rows(20));
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 141,
            row_base: 140,
            total_rows: 200,
            materialized_rows: 20,
        });
        w.sync_row_cursor_to_local(&buf, 10);
        assert_eq!(w.cursor_abs_row(), 150);
        assert_eq!(w.cursor_row(), 10);
    }

    #[test]
    fn row_text_yank_selection_returns_doc_range() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 40,
            row_base: 40,
            total_rows: 100,
            materialized_rows: 20,
        });
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(45), 10, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::StartVisual, 10, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(55), 10, now);
        let range = w
            .execute_row_viewer_command(&buf, ViewerCommand::YankSelection, 10, now)
            .unwrap();

        assert_eq!(range.start.row, 45);
        assert_eq!(range.end.row, 55);
        assert_eq!(range.end.byte_col, 1);
        assert_eq!(w.row_selection_range(&buf, now).unwrap(), range);
        assert!(w
            .row_selection_range(&buf, now + Duration::from_millis(250))
            .is_none());
    }

    #[test]
    fn row_text_visual_line_yank_returns_linewise_doc_range() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 40,
            row_base: 40,
            total_rows: 100,
            materialized_rows: 20,
        });
        w.set_vim_mode(VimMode::VisualLine);
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(45), 10, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::StartVisualLine, 10, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(47), 10, now);
        let range = w
            .execute_row_viewer_command(&buf, ViewerCommand::YankSelectionLinewise, 10, now)
            .unwrap();

        assert_eq!(
            range,
            DocRange {
                start: DocPosition {
                    row: 45,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 48,
                    byte_col: 0,
                },
            }
        );
    }

    #[test]
    fn row_text_line_end_lands_on_last_character() {
        let mut w = make_win();
        let buf = make_buf(vec!["abc".into()]);
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 20,
            materialized_rows: 1,
        });
        let now = Instant::now();

        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(10), 5, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::LineEnd, 5, now);

        assert_eq!(w.row_cursor().unwrap().byte_col, 2);
    }

    #[test]
    fn row_text_word_motion_updates_doc_column() {
        let mut w = make_win();
        let buf = make_buf(vec!["alpha beta gamma".into()]);
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 20,
            materialized_rows: 1,
        });
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(10), 5, now);
        w.execute_row_viewer_command(&buf, ViewerCommand::WordForward(2), 5, now);
        assert_eq!(w.row_cursor().unwrap().byte_col, "alpha beta ".len());
        w.execute_row_viewer_command(&buf, ViewerCommand::WordBackward(1), 5, now);
        assert_eq!(w.row_cursor().unwrap().byte_col, "alpha ".len());
        w.execute_row_viewer_command(&buf, ViewerCommand::WordEnd(1), 5, now);
        assert_eq!(w.row_cursor().unwrap().byte_col, "alpha beta".len() - 1);
    }

    #[test]
    fn row_text_mouse_drag_returns_doc_range() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 45,
            row_base: 40,
            total_rows: 100,
            materialized_rows: 20,
        });
        w.set_resolved_scroll(45);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: WindowViewport::new(Rect::new(0, 0, 20, 5), 20, 100, 45, None),
            click_count: 1,
        };

        let now = Instant::now();

        let (status, range) = w.handle_row_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ctx,
            now,
        );
        assert_eq!(status, Status::Capture);
        assert!(range.is_none());
        let (_, range) = w.handle_row_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 4, 3),
            ctx,
            now,
        );
        assert!(range.is_none());
        let (_, range) = w.handle_row_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 4, 3),
            ctx,
            now,
        );

        assert_eq!(
            range.unwrap(),
            DocRange {
                start: DocPosition {
                    row: 45,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 49,
                    byte_col: 4,
                },
            }
        );
    }

    #[test]
    fn row_text_mouse_click_sets_preferred_cell_for_vertical_motion() {
        let mut w = make_win();
        let buf = make_buf(vec!["abcdef".into(), "uvwxyz".into()]);
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 12,
            materialized_rows: 2,
        });
        w.set_resolved_scroll(10);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: WindowViewport::new(Rect::new(0, 0, 20, 2), 20, 12, 10, None),
            click_count: 1,
        };
        let now = Instant::now();

        w.handle_row_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 4),
            ctx,
            now,
        );
        assert_eq!(w.row_text_state().preferred_cell_col, Some(4));

        w.execute_row_viewer_command(&buf, ViewerCommand::MoveRows(1), 2, now);
        w.sync_row_cursor_to_local(&buf, 2);
        assert_eq!(w.row_cursor().unwrap().row, 11);
        assert_eq!(w.cursor_col(), 4);
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
        WindowViewport::new(rect, rect.width, rows.len() as RowIndex, 0, None)
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
    fn click_stages_press_and_captures_without_moving_cpos() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 1,
        };
        let click_byte = buf.byte_at_display_pos(1, 7);
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 1, 7),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());
        // Non-vim Down does not write `cpos` - it stashes the click byte in
        // `pending_press` and parks it in `drag_endpoint` for visual feedback.
        // `cpos` is committed only on Up, and only for caret leaves.
        assert_eq!(w.cpos(), 0);
        assert_eq!(w.cursor_row(), 0);
        assert_eq!(w.cursor_col(), 0);
        assert_eq!(w.text_state().pending_press, Some(click_byte));
        assert_eq!(w.text_state().drag_endpoint, Some(click_byte));
        assert!(w.selection_anchor().is_none());
    }

    #[test]
    fn mouse_drag_copy_includes_cursor_cell() {
        let mut w = make_win();
        let buf = make_buf(vec!["abc".into()]);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: viewport_for(buf.lines(), Rect::new(0, 0, 10, 1)),
            click_count: 1,
        };

        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ctx,
        );
        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 2),
            ctx,
        );

        let ranges = w.auto_selection_ranges(&buf, VimMode::Normal);
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 3));

        let (_, range) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 2),
            ctx,
        );
        assert_eq!(range, Some((0, 3)));
    }

    #[test]
    fn reverse_mouse_drag_copy_includes_cursor_cell() {
        let mut w = make_win();
        let buf = make_buf(vec!["abc".into()]);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: viewport_for(buf.lines(), Rect::new(0, 0, 10, 1)),
            click_count: 1,
        };

        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 2),
            ctx,
        );
        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 0),
            ctx,
        );
        let (_, range) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 0),
            ctx,
        );

        assert_eq!(range, Some((0, 3)));
    }

    #[test]
    fn vim_mouse_click_past_eol_clamps_to_last_char_in_normal_mode() {
        let mut w = make_win();
        w.set_vim_enabled(true);
        w.set_vim_mode(VimMode::Normal);
        let buf = make_buf(vec!["abc".into()]);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: viewport_for(buf.lines(), Rect::new(0, 0, 10, 1)),
            click_count: 1,
        };

        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 9),
            ctx,
        );
        w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 9),
            ctx,
        );

        assert_eq!(w.cpos(), 2);
        assert_eq!(w.cursor_col(), 2);
    }

    #[test]
    fn tail_mode_default_false() {
        let w = make_win();
        assert!(!w.is_following_tail());
    }

    #[test]
    fn selection_active_with_selection_anchor() {
        let mut w = make_win();
        assert!(!w.selection_active(), "no selection");
        w.text_state_mut().selection_anchor = Some(5);
        assert!(w.selection_active(), "selection anchor");
        w.text_state_mut().selection_anchor = None;
        assert!(!w.selection_active(), "cleared selection");
    }

    #[test]
    fn selection_active_with_visual_mode() {
        let mut w = make_win();
        w.text_state_mut().vim_enabled = true;
        w.set_vim_mode(VimMode::Normal);
        assert!(!w.selection_active(), "Normal mode");
        w.set_vim_mode(VimMode::Visual);
        assert!(w.selection_active(), "Visual mode");
        w.set_vim_mode(VimMode::VisualLine);
        assert!(w.selection_active(), "VisualLine mode");
        w.set_vim_mode(VimMode::Insert);
        assert!(!w.selection_active(), "Insert mode");
        w.text_state_mut().vim_enabled = false;
        w.set_vim_mode(VimMode::Visual);
        assert!(!w.selection_active(), "vim disabled");
    }

    #[test]
    fn selection_pins_tail_mode_on_update() {
        let mut w = make_win();
        let rows = sample_rows(30);
        let buf = make_buf(rows);
        let viewport = 10;
        w.jump_to_line_col(&buf, 29, 0, viewport);
        w.scroll_to_bottom();
        assert!(w.is_following_tail());
        w.text_state_mut().selection_anchor = Some(w.cpos().saturating_sub(1));
        w.update_tail_state(&buf, viewport);
        assert!(!w.is_following_tail());
        assert_eq!(w.scroll_state(), VerticalScroll::Pinned);
    }

    #[test]
    fn restore_cursor_screen_row_preserves_selection_endpoint() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows);
        let viewport = 5;
        w.jump_to_line_col(&buf, 10, 0, viewport);
        let endpoint = w.cpos();
        w.text_state_mut().selection_anchor = Some(0);
        w.scroll_top = 0;
        w.restore_cursor_screen_row(&buf, 0);
        assert_eq!(w.cpos(), endpoint);
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
    fn render_paints_row_text_from_absolute_scroll_top() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["row 20".into(), "row 21".into(), "row 22".into()]);
        let mut w = make_win();
        w.set_materialized_rows(20, 10, 30);
        w.scroll_top = 21;
        let mut grid = Grid::new(10, 2);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 2));
        w.render(&buf, &mut slice, &ctx());
        assert_eq!(grid.cell(0, 0).symbol, 'r');
        assert_eq!(grid.cell(4, 0).symbol, '2');
        assert_eq!(grid.cell(5, 0).symbol, '1');
        assert_eq!(grid.cell(4, 1).symbol, '2');
        assert_eq!(grid.cell(5, 1).symbol, '2');
    }

    #[test]
    fn row_text_pan_preserves_cursor_screen_row_in_absolute_space() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        let viewport = 5;
        w.set_materialized_rows(80, 20, 100);
        w.jump_to_line_col(&buf, 19, 0, viewport);
        assert_eq!(w.scroll_top, 95);
        assert_eq!(w.cursor_abs_row(), 99);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));

        w.pan_by_lines(&buf, -2, viewport);
        assert_eq!(w.scroll_top, 93);
        assert_eq!(w.cursor_abs_row(), 97);
        assert_eq!(w.cursor_row(), 17);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));
        let offsets = smelt_buffer::text::line_start_offsets(&rows);
        assert_eq!(w.cpos(), offsets[17]);
    }

    #[test]
    fn row_text_pan_preserves_screen_row_beyond_materialized_slice() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        let viewport = 5;
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 90,
            row_base: 80,
            total_rows: 200,
            materialized_rows: 20,
        });
        w.set_resolved_scroll(90);
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(94), viewport, now);
        assert_eq!(w.row_cursor().unwrap().row, 94);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));

        w.pan_by_lines(&buf, 20, viewport);

        assert_eq!(w.scroll_top, 110);
        assert_eq!(w.row_cursor().unwrap().row, 114);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));
    }

    #[test]
    fn row_text_pan_preserves_screen_row_before_materialized_slice() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        let viewport = 5;
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 90,
            row_base: 80,
            total_rows: 200,
            materialized_rows: 20,
        });
        w.set_resolved_scroll(90);
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(94), viewport, now);
        assert_eq!(w.row_cursor().unwrap().row, 94);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));

        w.pan_by_lines(&buf, -20, viewport);

        assert_eq!(w.scroll_top, 70);
        assert_eq!(w.row_cursor().unwrap().row, 74);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));
    }

    #[test]
    fn row_text_scroll_command_preserves_screen_row_beyond_materialized_slice() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        let viewport = 5;
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 90,
            row_base: 80,
            total_rows: 200,
            materialized_rows: 20,
        });
        w.set_resolved_scroll(90);
        let now = Instant::now();
        w.execute_row_viewer_command(&buf, ViewerCommand::GotoRow(94), viewport, now);

        w.execute_row_viewer_command(&buf, ViewerCommand::ScrollRows(20), viewport, now);

        assert_eq!(w.scroll_top, 110);
        assert_eq!(w.row_cursor().unwrap().row, 114);
        assert_eq!(w.cursor_screen_row(viewport), Some(4));
    }

    #[test]
    fn row_text_half_page_rows_moves_by_half_viewport() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(100));
        let viewport = 10;
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 0,
            row_base: 0,
            total_rows: 100,
            materialized_rows: 50,
        });
        w.set_resolved_scroll(0);
        let now = Instant::now();
        w.execute_row_viewer_command(
            &buf,
            ViewerCommand::GotoPosition(DocPosition {
                row: 5,
                byte_col: 0,
            }),
            viewport,
            now,
        );

        w.execute_row_viewer_command(&buf, ViewerCommand::HalfPageRows(1), viewport, now);
        assert_eq!(w.row_cursor().unwrap().row, 10);

        w.execute_row_viewer_command(&buf, ViewerCommand::HalfPageRows(-1), viewport, now);
        assert_eq!(w.row_cursor().unwrap().row, 5);
    }

    #[test]
    fn row_text_ctrl_u_and_ctrl_d_use_half_page_in_vim_handler() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut mode = VimMode::Normal;
        let mut state = VimWindowState::default();

        let up = vim::handle_row_viewer_key(
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            },
            &mut mode,
            &mut state,
        );
        assert_eq!(up, Some(ViewerCommand::HalfPageRows(-1)));

        let down = vim::handle_row_viewer_key(
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            },
            &mut mode,
            &mut state,
        );
        assert_eq!(down, Some(ViewerCommand::HalfPageRows(1)));

        let back = vim::handle_row_viewer_key(
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            },
            &mut mode,
            &mut state,
        );
        assert_eq!(back, Some(ViewerCommand::PageRows(-1)));

        let forward = vim::handle_row_viewer_key(
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                state: KeyEventState::empty(),
            },
            &mut mode,
            &mut state,
        );
        assert_eq!(forward, Some(ViewerCommand::PageRows(1)));
    }

    #[test]
    fn row_cursor_sync_keeps_byte_column_on_multibyte_chrome_rows() {
        let mut w = make_win();
        let mut buf = make_buf(vec!["│ hello".into()]);
        let chrome = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            ..Default::default()
        };
        buf.add_highlight_with_meta(0, 0, 2, crate::SpanStyle::new(), chrome);
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 42,
            row_base: 42,
            total_rows: 50,
            materialized_rows: 1,
        });
        w.set_resolved_scroll(42);
        let now = Instant::now();
        w.execute_row_viewer_command(
            &buf,
            ViewerCommand::GotoPosition(DocPosition {
                row: 42,
                byte_col: "│ he".len(),
            }),
            5,
            now,
        );

        w.sync_row_cursor_to_local(&buf, 5);
        assert_eq!(w.row_cursor().unwrap().byte_col, "│ he".len());
        assert_eq!(w.cursor_col(), 4);

        w.sync_row_cursor_to_local(&buf, 5);
        assert_eq!(w.row_cursor().unwrap().byte_col, "│ he".len());
        assert_eq!(w.cursor_col(), 4);
    }

    #[test]
    fn row_cursor_sync_does_not_mutate_authoritative_byte_column() {
        let mut w = make_win();
        let buf = make_buf(vec!["abcdef".into(), String::new(), "uvwxyz".into()]);
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 13,
            materialized_rows: 3,
        });
        w.set_resolved_scroll(10);
        {
            w.row_text_state_mut().cursor = DocPosition {
                row: 11,
                byte_col: 4,
            };
            w.row_text_state_mut().preferred_cell_col = Some(4);
        }

        w.sync_row_cursor_to_local(&buf, 3);

        assert_eq!(w.cursor_col(), 0);
        let state = w.row_text_state();
        assert_eq!(state.cursor.row, 11);
        assert_eq!(state.cursor.byte_col, 4);
        assert_eq!(state.preferred_cell_col, Some(4));
    }

    #[test]
    fn row_text_vertical_motion_preserves_preferred_cell_across_short_rows() {
        let mut w = make_win();
        let buf = make_buf(vec!["abcdef".into(), String::new(), "uvwxyz".into()]);
        let viewport = 3;
        let now = Instant::now();
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 13,
            materialized_rows: 3,
        });
        w.set_resolved_scroll(10);
        w.execute_row_viewer_command(
            &buf,
            ViewerCommand::GotoPosition(DocPosition {
                row: 10,
                byte_col: 4,
            }),
            viewport,
            now,
        );
        w.sync_row_cursor_to_local(&buf, viewport);
        assert_eq!(w.cursor_col(), 4);
        w.text_state_mut().curswant = Some(0);

        w.execute_row_viewer_command(&buf, ViewerCommand::MoveRows(1), viewport, now);
        w.sync_row_cursor_to_local(&buf, viewport);
        assert_eq!(w.row_cursor().unwrap().row, 11);
        assert_eq!(w.cursor_col(), 0);
        assert_eq!(w.row_text_state().preferred_cell_col, Some(4));

        w.execute_row_viewer_command(&buf, ViewerCommand::MoveRows(1), viewport, now);
        w.sync_row_cursor_to_local(&buf, viewport);
        assert_eq!(w.row_cursor().unwrap().row, 12);
        assert_eq!(w.cursor_col(), 4);
        assert_eq!(w.row_cursor().unwrap().byte_col, 4);
    }

    #[test]
    fn row_text_pan_preserves_preferred_cell_across_short_rows() {
        let mut w = make_win();
        let buf = make_buf(vec!["abcdef".into(), String::new(), "uvwxyz".into()]);
        let viewport = 1;
        let now = Instant::now();
        w.apply_materialized_rows(MaterializedRows {
            clamped_scroll: 10,
            row_base: 10,
            total_rows: 13,
            materialized_rows: 3,
        });
        w.set_resolved_scroll(10);
        w.execute_row_viewer_command(
            &buf,
            ViewerCommand::GotoPosition(DocPosition {
                row: 10,
                byte_col: 4,
            }),
            viewport,
            now,
        );
        w.sync_row_cursor_to_local(&buf, viewport);
        assert_eq!(w.cursor_col(), 4);

        w.pan_by_lines(&buf, 1, viewport);
        assert_eq!(w.row_cursor().unwrap().row, 11);
        assert_eq!(w.cursor_col(), 0);
        assert_eq!(w.row_text_state().preferred_cell_col, Some(4));

        w.pan_by_lines(&buf, 1, viewport);
        assert_eq!(w.row_cursor().unwrap().row, 12);
        assert_eq!(w.cursor_col(), 4);
        assert_eq!(w.row_cursor().unwrap().byte_col, 4);
    }

    #[test]
    fn row_text_mouse_hit_testing_subtracts_row_base() {
        let w = {
            let mut w = make_win();
            w.set_materialized_rows(80, 20, 100);
            w.scroll_top = 90;
            w
        };
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        let endpoint = w
            .mouse_text_endpoint(
                &buf,
                click_event(MouseEventKind::Down(MouseButton::Left), 3, 2),
                viewport(20, 5),
                buf.text().as_str(),
                MouseEndpointMode::RequireContentHit,
            )
            .expect("content mouse endpoint");
        let offsets = smelt_buffer::text::line_start_offsets(&rows);
        assert_eq!(endpoint.cpos, offsets[13] + 2);
    }

    #[test]
    fn row_text_scroll_row_total_uses_logical_extent() {
        let mut w = make_win();
        let buf = make_buf(sample_rows(20));
        assert_eq!(w.scroll_row_total(&buf), 20);
        w.set_materialized_rows(80, 20, 100);
        assert_eq!(w.scroll_row_total(&buf), 100);
    }

    #[test]
    fn visible_range_uses_large_row_indices() {
        let mut w = make_win();
        w.scroll_top = 100_000;
        assert_eq!(w.visible_range(3), 100_000..100_003);
    }

    #[test]
    fn row_text_mouse_double_click_selects_word() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 0;

        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 0,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 2,
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 3, // on "line 0", column 3 is the 'e'
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let (status, range) = w.handle_row_mouse(&buf, event, mouse_ctx, now);
        assert_eq!(status, Status::Capture);
        assert!(range.is_none());

        // selection_anchor should be set to the word range
        let state = w.row_text_state();
        assert_eq!(
            state.selection_anchor,
            Some(DocPosition {
                row: 0,
                byte_col: 0
            })
        );
        assert_eq!(
            state.cursor,
            DocPosition {
                row: 0,
                byte_col: 4
            }
        );
    }

    #[test]
    fn row_text_mouse_triple_click_selects_line() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 0;

        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 0,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 3,
        };
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let (status, range) = w.handle_row_mouse(&buf, event, mouse_ctx, now);
        assert_eq!(status, Status::Capture);
        assert!(range.is_none());

        let state = w.row_text_state();
        assert_eq!(
            state.selection_anchor,
            Some(DocPosition {
                row: 0,
                byte_col: 0
            })
        );
        assert_eq!(
            state.cursor,
            DocPosition {
                row: 0,
                byte_col: 6
            }
        );
    }

    #[test]
    fn row_text_drag_autoscroll_steps_scroll_and_updates_row_cursor() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 0;

        // Start a drag with the endpoint at the bottom edge (viewport height = 10).
        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 0,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 9,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 1,
        };
        w.handle_row_mouse(&buf, event, mouse_ctx, now);
        assert_eq!(w.row_text_state().cursor.row, 9);

        // Auto-scroll down by one row.
        let stepped = w.drag_autoscroll_step(&buf, 10, 1);
        assert!(stepped, "autoscroll should have stepped");
        assert_eq!(w.scroll_top, 1);
        let state = w.row_text_state();
        assert_eq!(
            state.cursor.row, 10,
            "row-text cursor should follow scroll to row 10"
        );
        assert_eq!(
            state.drag_endpoint,
            Some(DocPosition {
                row: 10,
                byte_col: state.cursor.byte_col
            })
        );

        // Auto-scroll up by one row.
        let stepped = w.drag_autoscroll_step(&buf, 10, -1);
        assert!(stepped, "autoscroll should have stepped back");
        assert_eq!(w.scroll_top, 0);
        let state = w.row_text_state();
        assert_eq!(
            state.cursor.row, 0,
            "row-text cursor should follow scroll to row 0"
        );
    }

    #[test]
    fn row_text_drag_autoscroll_top_edge_keeps_cursor_parked() {
        let mut w = make_win();
        let rows = sample_rows(100);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 100, 101);
        w.scroll_top = 50;

        let mut viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 50,
            total_rows: 100,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = |vp: WindowViewport| MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport: vp,
            click_count: 1,
        };

        // Mouse down in the middle of the viewport (terminal row 5 => abs 55).
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 5,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        w.handle_row_mouse(&buf, down, mouse_ctx(viewport), now);
        assert_eq!(w.row_text_state().cursor.row, 55);

        // Drag up to the top edge (terminal row 0 => abs 50).
        let drag_top = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: 0,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        w.handle_row_mouse(&buf, drag_top, mouse_ctx(viewport), now);
        assert_eq!(w.row_text_state().cursor.row, 50);

        // Simulate a few autoscroll ticks upward.
        for expected_scroll in [49, 48, 47, 46, 45] {
            let stepped = w.drag_autoscroll_step(&buf, 10, -1);
            assert!(
                stepped,
                "autoscroll should step when scroll_top={}",
                w.scroll_top
            );
            assert_eq!(w.scroll_top, expected_scroll);
            let state = w.row_text_state();
            assert_eq!(
                state.cursor.row, expected_scroll,
                "cursor should park at the new top edge after autoscroll to {}",
                expected_scroll
            );
            assert_eq!(
                state.drag_endpoint.map(|e| e.row),
                Some(expected_scroll),
                "drag endpoint should park at the new top edge"
            );

            // A fresh drag event at the same terminal row 0 must resolve to the
            // new absolute top row, not drift downward.
            viewport.scroll_top = w.scroll_top;
            w.handle_row_mouse(&buf, drag_top, mouse_ctx(viewport), now);
            let state = w.row_text_state();
            assert_eq!(
                state.cursor.row, expected_scroll,
                "cursor should stay at top edge after drag at scroll_top={}",
                expected_scroll
            );
        }
    }

    #[test]
    fn row_text_mouse_click_preserves_scroll_top() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 5;

        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 5,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 1,
        };
        // Click inside the viewport at terminal row 2, which maps to absolute row 7.
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 2,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        w.handle_row_mouse(&buf, event, mouse_ctx, now);
        // scroll_top must NOT be clobbered by the mouse handler.
        assert_eq!(w.scroll_top, 5, "scroll_top should remain 5 after click");
        let state = w.row_text_state();
        assert_eq!(state.cursor.row, 7, "cursor should be at absolute row 7");
    }

    #[test]
    fn row_text_mouse_drag_outside_viewport_does_not_desync_scroll() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 5;

        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 5,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 1,
        };
        // Drag to a row below the viewport (row 20, viewport bottom is 10).
        let event = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            row: 20,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        w.handle_row_mouse(&buf, event, mouse_ctx, now);
        // scroll_top must NOT be modified by the drag handler.
        assert_eq!(
            w.scroll_top, 5,
            "scroll_top should remain 5 after drag outside viewport"
        );
    }

    #[test]
    fn row_text_mouse_double_click_yank_flash_covers_full_word() {
        let mut w = make_win();
        let rows = sample_rows(20);
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 20, 21);
        w.scroll_top = 0;

        let viewport = WindowViewport {
            rect: Rect::new(0, 0, 40, 10),
            gutter_width: 0,
            content_width: 40,
            scroll_top: 0,
            total_rows: 20,
            scrollbar: None,
        };
        let now = std::time::Instant::now();
        let mouse_ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 2,
        };
        // Double-click on "line 0" at column 3 ('e').
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row: 0,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        w.handle_row_mouse(&buf, down, mouse_ctx, now);

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            row: 0,
            column: 3,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let (_, copy) = w.handle_row_mouse(&buf, up, mouse_ctx, now);
        let range = copy.expect("double-click should yield a range");
        // double-click selects the word under cursor: "line" → bytes 0..4
        assert_eq!(range.start.row, 0);
        assert_eq!(range.start.byte_col, 0);
        assert_eq!(range.end.row, 0);
        assert_eq!(range.end.byte_col, 4);

        // The yank flash must cover the same full range.
        let state = w.row_text_state();
        let flash = state.yank_flash.expect("yank flash should be set");
        assert_eq!(flash.range, range);
    }

    #[test]
    fn row_text_visual_mode_h_and_l_move_cursor_and_preserve_column() {
        let mut w = make_win();
        let rows = vec![
            "short".into(),
            "much much longer line".into(),
            "tiny".into(),
            "another long line here".into(),
        ];
        let buf = make_buf(rows.clone());
        w.set_materialized_rows(0, 4, 5);
        w.scroll_top = 0;
        w.text_state_mut().vim_enabled = true;
        w.set_vim_mode(VimMode::Normal);

        // Place cursor at row 1, byte_col 5 on the long line.
        w.row_text_state_mut().cursor = DocPosition {
            row: 1,
            byte_col: 5,
        };

        // Enter visual mode.
        let now = std::time::Instant::now();
        let cmd = w.handle_row_viewer_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('v'),
            modifiers: crossterm::event::KeyModifiers::empty(),
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(cmd, Some(crate::ViewerCommand::StartVisual));
        w.execute_row_viewer_command(&buf, cmd.unwrap(), 10, now);

        // Move down to the short row 2; byte_col should clamp to line width.
        let cmd = w.handle_row_viewer_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('j'),
            modifiers: crossterm::event::KeyModifiers::empty(),
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(cmd, Some(crate::ViewerCommand::MoveRows(1)));
        w.execute_row_viewer_command(&buf, cmd.unwrap(), 10, now);
        assert_eq!(w.row_text_state().cursor.row, 2);
        assert_eq!(
            w.row_text_state().cursor.byte_col,
            4,
            "cursor should clamp to end of short line"
        );

        // Move down to the longer row 3; byte_col should recover toward the
        // original column, not get stuck at the clamped short-line width.
        let cmd = w.handle_row_viewer_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('j'),
            modifiers: crossterm::event::KeyModifiers::empty(),
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(cmd, Some(crate::ViewerCommand::MoveRows(1)));
        w.execute_row_viewer_command(&buf, cmd.unwrap(), 10, now);
        assert_eq!(w.row_text_state().cursor.row, 3);
        assert_eq!(
            w.row_text_state().cursor.byte_col,
            5,
            "cursor should recover to original byte column after short line"
        );

        // Move right to the end of the long line with 'l'.
        let line3_len = rows[3].len();
        for _ in 0..(line3_len - 5) {
            let cmd = w.handle_row_viewer_key(crossterm::event::KeyEvent {
                code: crossterm::event::KeyCode::Char('l'),
                modifiers: crossterm::event::KeyModifiers::empty(),
                kind: crossterm::event::KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            });
            assert_eq!(cmd, Some(crate::ViewerCommand::MoveCursorCol(1)));
            w.execute_row_viewer_command(&buf, cmd.unwrap(), 10, now);
        }
        assert_eq!(
            w.row_text_state().cursor.byte_col,
            line3_len,
            "cursor should park past last char"
        );

        // Pressing 'l' again at the end should be a no-op.
        let cmd = w.handle_row_viewer_key(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('l'),
            modifiers: crossterm::event::KeyModifiers::empty(),
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(cmd, Some(crate::ViewerCommand::MoveCursorCol(1)));
        w.execute_row_viewer_command(&buf, cmd.unwrap(), 10, now);
        assert_eq!(
            w.row_text_state().cursor.byte_col,
            line3_len,
            "l at end of line should stay put"
        );
    }

    #[test]
    fn row_text_doc_range_to_row_ranges_skips_chrome_on_empty_line() {
        let mut w = make_win();
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        // Thinking-block style: chrome "│ " is non-selectable.
        buf.set_all_lines(vec!["│ hello".into(), "│ ".into(), "│ world".into()]);
        buf.add_highlight_group_with_meta(
            1,
            0,
            2,
            smelt_buffer::theme::intern("Normal"),
            smelt_buffer::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        w.set_materialized_rows(0, 3, 4);
        w.scroll_top = 0;

        // Select across all three rows.
        let ranges = w.doc_range_to_row_ranges(
            &buf,
            10,
            DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: DocPosition {
                    row: 2,
                    byte_col: 9,
                },
            },
        );
        assert_eq!(ranges.len(), 3);
        // Row 0: normal range.
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 7));
        // Row 1: empty chrome-only row; fallback span placed after the 2-char gutter.
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (2, 3));
        // Row 2: end row.
        assert_eq!((ranges[2].col_start, ranges[2].col_end), (0, 7));
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
        // Caret-style Window with `cursor_line = true`: the row at
        // `cursor_row` (buffer-absolute, converted to screen row) gets the
        // `CursorLine` theme bg whenever this window owns focus.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "bravo".into(), "charlie".into()]);
        let mut w = make_win();
        w.cursor_line = true;
        w.text_state_mut().cursor_row = 1; // second visible row
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
        // Both `cursor_line` and `selection_highlight` default to false -
        // focused content viewers (transcript, /help, /btw) stay clean.
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
        // Cell at col 5 is the exclusive end - not dim.
        assert!(!grid.cell(5, 0).style.dim);
        // Cell before the span - not dim.
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
        // When `cursor_line` paints the cursor row with a bg, a span's
        // bold attribute layers on top: that cell ends up bg=cursor and
        // bold=true.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["hello".into()]);
        buf.add_highlight(0, 0, 3, crate::SpanStyle::new().bold());
        let mut w = make_win();
        w.cursor_line = true;
        w.text_state_mut().cursor_row = 0;
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
    fn render_paints_selection_highlight_unfocused() {
        // List-shaped windows (`selection_highlight = true`) keep selection
        // painted regardless of focus - picker overlays may be driven by an
        // external input yet still need to show the selected row.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "bravo".into()]);
        let mut w = make_win();
        w.selection_highlight = true;
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
        // Set virt_text at col=2 ("hi") on row 0 - paints over the
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
        w.cursor_line = true;
        w.text_state_mut().cursor_row = 0;
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
        // Block cursor position derives from `effective_endpoint` (cpos when no
        // drag), so set cpos to the byte for column 1 of "abc".
        w.text_state_mut().cpos = 1;
        w.text_state_mut().cursor_row = 0;
        w.text_state_mut().cursor_col = 1;
        let cursor_style = crate::grid::Style::new().bg(crate::grid::Color::White);
        let mut ctx = ctx();
        ctx.focused = true;
        ctx.cursor_shape = CursorShape::Block {
            glyph: 'b',
            style: cursor_style,
            pos: None,
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
    fn render_skips_block_cursor_when_cursor_shape_hidden() {
        // The render dispatch (`Ui::render_with_paints`) sets `cursor_shape` to
        // `Hidden` on every leaf that isn't `Ui::active_cursor_leaf`, so exactly
        // one leaf paints a block per frame. `Window::render` trusts that gate
        // and paints if-and-only-if it receives a `Block` shape - the previous
        // `ctx.focused` guard was redundant.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.text_state_mut().cpos = 1;
        w.text_state_mut().cursor_row = 0;
        w.text_state_mut().cursor_col = 1;
        let mut ctx = ctx();
        ctx.focused = false;
        ctx.cursor_shape = CursorShape::Hidden;
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);
        // Buffer text stays; no block painted.
        assert_eq!(grid.cell(1, 0).symbol, 'b');
    }

    #[test]
    fn render_block_cursor_outside_slice_is_clipped() {
        // Block cursor row derives from `effective_endpoint`'s projection.
        // When `scroll_top` is past the cursor's logical row, the screen-row
        // subtraction underflows and the block is clipped to nothing.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["abc".into()]);
        let mut w = make_win();
        w.text_state_mut().cpos = 1;
        w.text_state_mut().cursor_row = 0;
        w.text_state_mut().cursor_col = 1;
        w.scroll_top = 100;
        let mut ctx = ctx();
        ctx.focused = true;
        ctx.cursor_shape = CursorShape::Block {
            glyph: '!',
            style: crate::grid::Style::default(),
            pos: None,
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
    fn mouse_single_click_without_drag_yields_no_selection() {
        // Down-then-Up with no Drag in between must leave no selection,
        // no Visual mode, and no clipboard yank - only the cursor moved.
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        w.text_state_mut().vim_enabled = true;
        let rows: Vec<String> = vec!["hello world".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);
        let hb = hard_breaks(&rows);
        let mk_ctx = || MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hb,
            viewport,
            click_count: 1,
        };
        let (_, _) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 4),
            mk_ctx(),
        );
        assert_eq!(w.vim_mode(), VimMode::Normal, "Down must not enter Visual");
        let (_, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 4),
            mk_ctx(),
        );
        assert!(yank.is_none(), "bare click must not produce a yank range");
        assert!(w.selection_anchor().is_none());
        assert!(
            w.text_state().pending_press.is_none(),
            "Up clears the staged press"
        );
    }

    #[test]
    fn mouse_click_stamps_curswant_at_landing_col() {
        // Click at col 4 must set curswant = 4 so a subsequent j/k keeps that
        // column rather than snapping back to whatever curswant was before.
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        w.text_state_mut().vim_enabled = true;
        w.text_state_mut().curswant = Some(10);
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);
        let hb = hard_breaks(&rows);
        let mk_ctx = || MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hb,
            viewport,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 4),
            mk_ctx(),
        );
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 4),
            mk_ctx(),
        );
        assert_eq!(w.curswant(), Some(4));
    }

    #[test]
    fn mouse_drag_release_stamps_curswant_at_endpoint_col() {
        // Drag from col 0 to col 7 - curswant must follow the endpoint, not
        // stay at the drag start.
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);
        let hb = hard_breaks(&rows);
        let mk_ctx = || MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hb,
            viewport,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            mk_ctx(),
        );
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 7),
            mk_ctx(),
        );
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 7),
            mk_ctx(),
        );
        assert_eq!(w.curswant(), Some(7));
    }

    #[test]
    fn mouse_drag_yank_on_up() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Down on 'h' (row 0, col 0)
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());

        // Drag to 'o' in "world" (row 0, col 7)
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 7),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert!(yank.is_none());

        // Up - selected text "hello wo" is returned
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 1,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 7),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some((0, 8)));
    }

    #[test]
    fn mouse_double_click_yank_word() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec!["hello world".into()];
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Double-click on "world" (row 0, col 8)
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 2,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 8),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none()); // yank on Down, not Up

        // Up returns the selected word
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 2,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
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
        let buf = make_buf(rows.clone());
        let rect = Rect::new(0, 0, 20, 5);
        let viewport = viewport_for(&rows, rect);

        // Triple-click on the first line
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 4),
            ctx,
        );
        assert_eq!(r, Status::Capture);
        assert!(yank.is_none());

        // Up returns the selected line
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 4),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        assert_eq!(yank, Some((0, 11)));
    }

    #[test]
    fn mouse_click_respects_horizontal_scroll() {
        // With scroll_left = 5, a click at viewport column 2 must land on
        // source column 7, not column 2. Mouse endpoint resolution must apply
        // the horizontal scroll offset or clicks drift left by exactly that
        // amount.
        let mut w = make_win();
        let row = "abcdefghijklmnopqrstuvwxyz".to_string();
        let buf = make_buf(vec![row.clone()]);
        w.ensure_layout(&buf, 100);
        w.scroll_left = 5;

        let rect = Rect::new(0, 0, 20, 5);
        let mut vp = viewport_for(std::slice::from_ref(&row), rect);
        vp.scrollbar = None;
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(std::slice::from_ref(&row)),
            viewport: vp,
            click_count: 1,
        };

        // Click at viewport column 2 → source column 7 → byte for 'h'
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 2),
            ctx,
        );
        assert_eq!(w.text_state().drag_endpoint, Some(row.find('h').unwrap()));
    }

    #[test]
    fn mouse_up_commits_projected_editable_buffer_cpos_in_source_space() {
        // Parsed editable buffers (the prompt) have a canonical source string
        // plus rendered display lines. Mouse hit-testing maps display cells
        // back to source byte offsets via ProjectionMaps; mouse-up must keep
        // that byte in source space. Snapping it against `buf.text()` clamps
        // to the rendered display length instead and parks the edit cursor too
        // far left, so follow-up typing inserts before existing text.
        let mut w = make_win();
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_source("abcdef".into());
        buf.set_all_lines(vec!["x".into()]);
        buf.set_projection_maps(smelt_buffer::coords::ProjectionMaps {
            source_char_to_display_char: vec![0, 1, 1, 1, 1, 1, 1],
            display_char_to_source_char: vec![0, 6],
            row_offsets: vec![0],
        });
        w.ensure_layout(&buf, 20);

        let rect = Rect::new(0, 0, 20, 1);
        let rows = vec!["x".to_string()];
        let viewport = viewport_for(&rows, rect);
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 1),
            ctx,
        );

        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &[],
            viewport,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 1),
            ctx,
        );

        assert_eq!(w.cpos(), 6);
    }

    #[test]
    fn mouse_drag_respects_horizontal_scroll() {
        // Drag end-point must also account for scroll_left.
        let mut w = make_win();
        let row = "abcdefghijklmnopqrstuvwxyz".to_string();
        let buf = make_buf(vec![row.clone()]);
        w.ensure_layout(&buf, 100);
        w.scroll_left = 5;

        let rect = Rect::new(0, 0, 20, 5);
        let mut vp = viewport_for(std::slice::from_ref(&row), rect);
        vp.scrollbar = None;
        let hb = hard_breaks(std::slice::from_ref(&row));

        // Down at viewport col 2
        let ctx_down = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hb,
            viewport: vp,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 2),
            ctx_down,
        );

        // Drag to viewport col 4 → source col 9 → byte for 'j'
        let ctx_drag = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hb,
            viewport: vp,
            click_count: 1,
        };
        let _ = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 4),
            ctx_drag,
        );
        assert_eq!(w.text_state().drag_endpoint, Some(row.find('j').unwrap()));
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

    #[test]
    fn snap_col_past_chrome_clamps_trailing_pad_to_selectable_edge() {
        // Click in a user-block trailing bg pad (chrome past the content) must
        // not push the cursor to layout_width - that would pan the viewport
        // horizontally to the row's right edge. Clamp to the last selectable
        // col_end instead.
        let mut buf = make_buf(vec![" hello                          ".into()]);
        let chrome = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            ..Default::default()
        };
        // Leading 1-col pad (chrome), selectable content 1..6, trailing pad 6..32.
        buf.add_highlight_with_meta(0, 0, 1, crate::SpanStyle::new(), chrome.clone());
        buf.add_highlight_with_meta(
            0,
            1,
            6,
            crate::SpanStyle::new(),
            smelt_buffer::buffer::SpanMeta::default(),
        );
        buf.add_highlight_with_meta(0, 6, 32, crate::SpanStyle::new(), chrome);

        assert_eq!(snap_col_past_chrome(&buf, 0, 0), 1, "lead chrome → content");
        assert_eq!(snap_col_past_chrome(&buf, 0, 3), 3, "selectable stays put");
        assert_eq!(
            snap_col_past_chrome(&buf, 0, 20),
            6,
            "trailing chrome clamps to selectable edge"
        );
    }

    #[test]
    fn snap_col_past_chrome_all_chrome_row_stays_put_except_single_pad() {
        // A one-cell user-block padding row should put the cursor just after the
        // pad. Wider all-chrome rows still stay at the left edge so a generic
        // non-selectable separator can't push the cursor to layout_width.
        let mut one_pad = make_buf(vec![" ".into()]);
        let chrome = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            ..Default::default()
        };
        one_pad.add_highlight_with_meta(0, 0, 1, crate::SpanStyle::new(), chrome.clone());
        assert_eq!(snap_col_past_chrome(&one_pad, 0, 0), 1);

        let mut wide = make_buf(vec![" ".repeat(40)]);
        wide.add_highlight_with_meta(0, 0, 40, crate::SpanStyle::new(), chrome);

        assert_eq!(snap_col_past_chrome(&wide, 0, 0), 0);
        assert_eq!(snap_col_past_chrome(&wide, 0, 20), 0);
        assert_eq!(snap_col_past_chrome(&wide, 0, 39), 0);
    }

    #[test]
    fn selection_paints_through_all_chrome_padding_row() {
        // Regression: a multi-line selection that passes through a row whose
        // only spans are non-selectable (e.g. the user-block bg padding row)
        // must still paint the 1-cell fallback selection span at col 0, so the
        // selection looks continuous. Before the paint-mask fix, the all-chrome
        // mask blocked every cell on the row and the selection visibly broke.
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), " ".repeat(10), "bravo".into()]);
        let chrome = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            ..Default::default()
        };
        buf.add_highlight_with_meta(1, 0, 10, crate::SpanStyle::new(), chrome);
        buf.set_range_layer(
            crate::RangeLayer::Selection,
            vec![
                smelt_buffer::buffer::SelectionRange {
                    line: 0,
                    col_start: 0,
                    col_end: 5,
                },
                smelt_buffer::buffer::SelectionRange {
                    line: 1,
                    col_start: 0,
                    col_end: 1,
                },
                smelt_buffer::buffer::SelectionRange {
                    line: 2,
                    col_start: 0,
                    col_end: 5,
                },
            ],
        );

        let w = make_win();
        let mut theme = Theme::default();
        let visual_bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(60));
        theme.set("Visual", visual_bg);
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

        assert_eq!(grid.cell(0, 0).style.bg, visual_bg.bg, "row 0 selected");
        assert_eq!(
            grid.cell(0, 1).style.bg,
            visual_bg.bg,
            "row 1 fallback cell selected despite being all-chrome"
        );
        assert_eq!(grid.cell(0, 2).style.bg, visual_bg.bg, "row 2 selected");
    }

    #[test]
    fn render_highlight_span_columns_are_display_cells_with_wide_text() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["你x".into()]);
        buf.add_highlight(0, 2, 3, crate::SpanStyle::new().bold());

        let w = make_win();
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 3,
            focused: false,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(Theme::default()),
            vim_mode: VimMode::default(),
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        w.render(&buf, &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).symbol, '你');
        assert!(!grid.cell(0, 0).style.bold);
        assert_eq!(grid.cell(2, 0).symbol, 'x');
        assert!(grid.cell(2, 0).style.bold);
    }

    #[test]
    fn render_selection_masks_table_chrome_for_plain_text_cells() {
        let line = "┃ a ┃ b ┃  ";
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec![line.into()]);
        let chrome = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            ..Default::default()
        };
        buf.add_highlight_with_meta(0, 0, 2, crate::SpanStyle::new(), chrome.clone());
        buf.add_highlight_with_meta(0, 3, 6, crate::SpanStyle::new(), chrome.clone());
        buf.add_highlight_with_meta(0, 7, 11, crate::SpanStyle::new(), chrome);
        buf.set_range_layer(
            crate::RangeLayer::Selection,
            vec![smelt_buffer::buffer::SelectionRange {
                line: 0,
                col_start: 0,
                col_end: unicode_width::UnicodeWidthStr::width(line) as u16,
            }],
        );

        let w = make_win();
        let mut theme = Theme::default();
        let visual_bg = crate::grid::Style::new().bg(crate::grid::Color::AnsiValue(60));
        theme.set("Visual", visual_bg);
        let ctx = DrawContext {
            terminal_width: 40,
            terminal_height: 3,
            focused: true,
            cursor_shape: CursorShape::Hidden,
            theme: std::sync::Arc::new(theme),
            vim_mode: VimMode::default(),
        };
        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        w.render(&buf, &mut slice, &ctx);

        for char_col in [0, 1, 3, 4, 5, 7, 8, 9, 10] {
            let cell = text::byte_to_cell(line, text::byte_of_char(line, char_col as usize)) as u16;
            assert_ne!(
                grid.cell(cell, 0).style.bg,
                visual_bg.bg,
                "chrome char at {char_col} should not be selected"
            );
        }
        for char_col in [2, 6] {
            let cell = text::byte_to_cell(line, text::byte_of_char(line, char_col as usize)) as u16;
            assert_eq!(
                grid.cell(cell, 0).style.bg,
                visual_bg.bg,
                "cell text at {char_col} should be selected"
            );
        }
    }

    #[test]
    fn scroll_anchor_restored_after_width_change() {
        // Build a wrapped buffer where each row wraps into 4 visual rows at
        // width=5, and 2 visual rows at width=10. With cursor at the top,
        // scroll to visual row 8 - that's logical row 2 at width=5. After
        // narrowing width to nothing (rebuild only), then widening to 10, the
        // anchor must restore scroll_top to the visual row that contains
        // logical row 2 at the new width - visual row 4.
        let mut w = make_win();
        w.wrap = true;
        let rows = vec![
            "aaaaaaaaaaaaaaaaaaaa".into(), // 20 cells
            "bbbbbbbbbbbbbbbbbbbb".into(),
            "cccccccccccccccccccc".into(),
            "dddddddddddddddddddd".into(),
        ];
        let buf = make_buf(rows);

        w.ensure_layout(&buf, 5);
        // At width=5, each 20-char row wraps to 4 chunks → 16 visual rows.
        assert_eq!(w.layout.visual_count(), 16);
        w.set_scroll(8, &buf);
        // Logical row 2, chunk 0 sits at visual row 8.
        assert_eq!(w.scroll_anchor.map(|(_, l, b)| (l, b)), Some((2, 0)));

        // Widen to 10. Layout rebuilds; restore_scroll_from_anchor should
        // remap (lrow=2, byte=0) to visual row 4 (each logical = 2 chunks).
        w.ensure_layout(&buf, 10);
        assert_eq!(w.layout.visual_count(), 8);
        assert_eq!(w.scroll_top, 4, "anchor restored after width change");
    }

    #[test]
    fn cursor_screen_row_preserved_across_width_change() {
        // Cursor sits 3 rows below scroll_top at width=5. After widening to
        // 10, the cursor must still appear 3 rows below the (anchor-restored)
        // scroll_top so it stays visually fixed under the user's gaze.
        let mut w = make_win();
        w.wrap = true;
        let rows = vec![
            "aaaaaaaaaaaaaaaaaaaa".into(),
            "bbbbbbbbbbbbbbbbbbbb".into(),
            "cccccccccccccccccccc".into(),
            "dddddddddddddddddddd".into(),
        ];
        let buf = make_buf(rows);

        w.ensure_layout(&buf, 5);
        w.set_scroll(8, &buf);
        // Place cursor 3 visual rows below scroll_top - logical row 2,
        // chunk 3.
        w.text_state_mut().cpos = buf.byte_at_display_pos(2, 15);
        let (r, c) = w.cursor_visual(&buf, w.cpos());
        w.text_state_mut().cursor_row = r;
        w.text_state_mut().cursor_col = c;
        assert_eq!(w.cursor_row().checked_sub(w.scroll_top), Some(3));

        w.ensure_layout(&buf, 10);
        assert_eq!(
            w.cursor_row().checked_sub(w.scroll_top),
            Some(3),
            "cursor's screen-row distance preserved across width change"
        );
    }

    #[test]
    fn scroll_anchor_skipped_on_changedtick_bump() {
        // Content change (changedtick bump) must invalidate the anchor - the
        // (lrow, byte) no longer references the same content, so silently
        // restoring would teleport the viewport.
        let mut w = make_win();
        w.wrap = true;
        let mut buf = make_buf(vec!["aaaaaaaaaa".into(), "bbbbbbbbbb".into()]);
        w.ensure_layout(&buf, 5);
        w.set_scroll(2, &buf);
        assert!(w.scroll_anchor.is_some());

        // Replace buffer content in place - bumps changedtick.
        buf.set_all_lines(vec!["x".into(); 5]);
        w.ensure_layout(&buf, 10);
        // Anchor cleared because changedtick mismatched.
        assert!(w.scroll_anchor.is_none());
    }

    #[test]
    fn select_cell_at_selects_between_table_borders() {
        let mut w = make_win();
        let mut buf = make_buf(vec!["┃ a ┃ b ┃".into()]);
        let hl = smelt_buffer::theme::intern("Normal");
        let unsel = smelt_buffer::buffer::SpanMeta {
            selectable: false,
            copy_as: None,
        };
        buf.set_decoration(
            0,
            smelt_buffer::buffer::LineDecoration {
                cell_selectable: true,
                ..Default::default()
            },
        );
        // cols: 0┃1 2a3 4┃5 6b7 8┃
        // Mark borders and padding as non-selectable. Cell text is plain and
        // implicitly selectable, matching normal markdown table rendering.
        buf.add_highlight_group_with_meta(0, 0, 2, hl, unsel.clone()); // ┃ +
        buf.add_highlight_group_with_meta(0, 3, 4, hl, unsel.clone()); //  (pad)
        buf.add_highlight_group_with_meta(0, 4, 5, hl, unsel.clone()); // ┃
        buf.add_highlight_group_with_meta(0, 5, 6, hl, unsel.clone()); //  (pad)
        buf.add_highlight_group_with_meta(0, 7, 8, hl, unsel.clone()); //  (pad)
        buf.add_highlight_group_with_meta(0, 8, 9, hl, unsel); // ┃
                                                               // Click on the first cell content 'a' (display col 2).
        let cpos = buf.byte_at_display_pos(0, 2);
        let (s, e) = w.select_cell_at(cpos, &buf, 10).expect("cell selected");
        let selected = &buf.text()[s..e];
        assert_eq!(selected, "a");
    }

    #[test]
    fn select_cell_at_returns_none_for_non_table_row() {
        let mut w = make_win();
        let buf = make_buf(vec!["hello world".into()]);
        let cpos = buf.byte_at_display_pos(0, 6);
        assert!(w.select_cell_at(cpos, &buf, 10).is_none());
    }

    #[test]
    fn select_block_at_selects_full_table_block() {
        let mut w = make_win();
        let mut buf = make_buf(vec![
            "before".into(),
            "┏━┓".into(),
            "┃a┃".into(),
            "┗━┛".into(),
            "after".into(),
        ]);
        for row in 1..=3 {
            buf.set_decoration(
                row,
                smelt_buffer::buffer::LineDecoration {
                    block_selectable: true,
                    ..Default::default()
                },
            );
        }
        // Click on the middle table row (row 2, the data row).
        let cpos = buf.byte_at_display_pos(2, 1);
        let (s, e) = w.select_block_at(cpos, &buf, 10).expect("table selected");
        let selected = &buf.text()[s..e];
        assert_eq!(selected, "┏━┓\n┃a┃\n┗━┛");
    }

    #[test]
    fn select_block_at_returns_none_for_plain_row() {
        let mut w = make_win();
        let buf = make_buf(vec!["hello world".into()]);
        let cpos = buf.byte_at_display_pos(0, 6);
        assert!(w.select_block_at(cpos, &buf, 10).is_none());
    }

    #[test]
    fn triple_click_on_table_row_selects_whole_table() {
        let mut w = make_win();
        w.set_vim_mode(VimMode::Normal);
        let rows: Vec<String> = vec![
            "before".into(),
            "┏━┓".into(),
            "┃a┃".into(),
            "┗━┛".into(),
            "after".into(),
        ];
        let mut buf = make_buf(rows.clone());
        for row in 1..=3 {
            buf.set_decoration(
                row,
                smelt_buffer::buffer::LineDecoration {
                    block_selectable: true,
                    ..Default::default()
                },
            );
        }
        let rect = Rect::new(0, 0, 20, 10);
        let viewport = viewport_for(&rows, rect);

        // Triple-click on the data row (display row 2).
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
        };
        let (r, _yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Down(MouseButton::Left), 2, 1),
            ctx,
        );
        assert_eq!(r, Status::Capture);

        // Up returns the selected table block.
        let ctx = MouseCtx {
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport,
            click_count: 3,
        };
        let (r, yank) = w.handle_mouse(
            &buf,
            click_event(MouseEventKind::Up(MouseButton::Left), 2, 1),
            ctx,
        );
        assert_eq!(r, Status::Consumed);
        let (s, e) = yank.expect("yank range");
        let selected = &buf.text()[s..e];
        assert_eq!(selected, "┏━┓\n┃a┃\n┗━┛");
    }
}
