//! Buffer-backed list overlay for selectable items.
//!
//! Each item occupies one buffer line: `{indent}{prefix}{label}{padding}{description}`.
//! `prefix_style` and `label_style` paint their cell, and the description column
//! always renders dim. Rendering goes through `LineBuilder` so styled spans
//! compose the same way as everywhere else in the TUI.
//!
//! Reversed mode (prompt-docked completer) writes items in reverse order so
//! the best match sits at the bottom; logical → visual mapping is `n - 1 - logical`.
//! Closing the leaf cascades through `overlay_close` to remove the overlay.

use crate::app::TuiApp;
use crate::smelt_term::layout::Anchor;
use crate::smelt_term::BufCreateOpts;
use crate::smelt_term::{
    BufId, Constraint, Corner, Gutters, LayoutTree, Overlay, OverlayId, RowIndex, SplitConfig,
    WinId,
};
use smelt_core::content::builder::render_into;
use smelt_core::style::Style;

/// One row in a picker. Description (if any) is column-aligned after the
/// longest label across the set.
#[derive(Clone, Default, Debug)]
pub(crate) struct PickerItem {
    pub(crate) prefix: String,
    pub(crate) prefix_style: Style,
    pub(crate) label: String,
    pub(crate) label_style: Style,
    pub(crate) description: Option<String>,
}

impl PickerItem {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Default::default()
        }
    }

    pub(crate) fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub(crate) fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub(crate) fn with_prefix_style(mut self, style: Style) -> Self {
        self.prefix_style = style;
        self
    }

    pub(crate) fn with_label_style(mut self, style: Style) -> Self {
        self.label_style = style;
        self
    }
}

/// Where the picker overlay anchors on screen.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PickerPlacement {
    /// Centered on screen.
    ScreenCenter,
    /// Docked above the prompt; reversed so the best match is closest to the input.
    PromptDocked { max_rows: u16 },
    /// Anchored to the cursor.
    Cursor,
    /// Bottom of screen, full width, one row above the status bar.
    ScreenBottom,
}

/// Per-leaf picker state, keyed by `WinId`. Lets `set_items` / `set_selected`
/// resize the overlay and reverse logical → visual indices without re-deriving placement.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PickerState {
    pub(crate) overlay: OverlayId,
    pub(crate) placement: PickerPlacement,
    pub(crate) reversed: bool,
    pub(crate) max_rows: u16,
}

const INDENT: usize = 1;
const DESC_GAP: usize = 2;

/// Open a picker overlay. `selected` is a logical 0-based index into `items`.
/// Returns the leaf `WinId` for subsequent `set_items` / `set_selected` calls.
pub(crate) fn open(
    app: &mut TuiApp,
    items: Vec<PickerItem>,
    selected: usize,
    placement: PickerPlacement,
    focusable: bool,
    blocks_agent: bool,
    z: u16,
) -> Option<WinId> {
    let max_rows = match placement {
        PickerPlacement::PromptDocked { max_rows } => max_rows,
        _ => 32,
    };
    let reversed = matches!(placement, PickerPlacement::PromptDocked { .. });

    let buf = app.ui.buf_create(BufCreateOpts::default());
    write_buffer(app, buf, &items, reversed);

    let leaf = app.ui.win_open_split(
        buf,
        SplitConfig {
            region: "picker_overlay".into(),
            gutters: Gutters {
                scrollbar: false,
                ..Gutters::default()
            },
        },
    )?;
    let height = picker_height(items.len(), max_rows);
    let (cursor_row, scroll) = cursor_and_scroll(selected, items.len(), height, reversed, 0);
    let (w, buf_ref) = app.ui.win_and_buf_mut(leaf, buf);
    if let (Some(w), Some(buf_ref)) = (w, buf_ref) {
        w.selection_highlight = true;
        // Mouse-scroll opt-in: doubles as the caret-leaf opt-out so a click
        // doesn't commit `cpos` mid-line. Wheel pans the viewport and shifts
        // the highlight visually (same as `dialog.list` / resume).
        w.mouse_scroll = true;
        w.focusable = focusable;
        w.scroll_top = scroll;
        w.jump_to_row(buf_ref, cursor_row, height);
    }

    let layout = layout_for(leaf, height);
    let anchor = anchor_for(&app.ui, placement, height);
    let overlay = Overlay::new(layout, anchor)
        .with_z(z)
        .blocks_agent(blocks_agent);
    let overlay_id = app.ui.overlay_open(overlay);

    app.picker_state.insert(
        leaf,
        PickerState {
            overlay: overlay_id,
            placement,
            reversed,
            max_rows,
        },
    );
    if focusable {
        app.ui.set_focus(leaf);
    }
    Some(leaf)
}

/// Replace picker items, resizing the overlay. No-op if `leaf` is not a known picker.
pub(crate) fn set_items(app: &mut TuiApp, leaf: WinId, items: Vec<PickerItem>, selected: usize) {
    let Some(state) = app.picker_state.get(&leaf).copied() else {
        return;
    };
    let prev_scroll = app.ui.win(leaf).map(|w| w.scroll_top).unwrap_or(0);
    let buf_id = app.ui.win(leaf).map(|w| w.buf);
    if let Some(buf_id) = buf_id {
        write_buffer(app, buf_id, &items, state.reversed);
    }
    let height = picker_height(items.len(), state.max_rows);
    let (cursor_row, scroll) =
        cursor_and_scroll(selected, items.len(), height, state.reversed, prev_scroll);
    if let Some(buf_id) = buf_id {
        let (w, buf_ref) = app.ui.win_and_buf_mut(leaf, buf_id);
        if let (Some(w), Some(buf_ref)) = (w, buf_ref) {
            w.scroll_top = scroll;
            w.jump_to_row(buf_ref, cursor_row, height);
        }
    }
    // Compute the anchor before grabbing the &mut on `Ui::overlay_mut` so
    // the immutable read of `app.ui.named_win(...)` doesn't overlap the
    // mutable borrow on the overlay record.
    let new_anchor = anchor_for(&app.ui, state.placement, height);
    if let Some(ov) = app.ui.overlay_mut(state.overlay) {
        ov.layout = layout_for(leaf, height);
        ov.anchor = new_anchor;
    }
}

/// Update the picker's logical selection (clamped to `n - 1`).
pub(crate) fn set_selected(app: &mut TuiApp, leaf: WinId, selected: usize) {
    let Some(state) = app.picker_state.get(&leaf).copied() else {
        return;
    };
    let buf_id = match app.ui.win(leaf).map(|w| w.buf) {
        Some(id) => id,
        None => return,
    };
    let n = app.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
    let prev_scroll = app.ui.win(leaf).map(|w| w.scroll_top).unwrap_or(0);
    let height = picker_height(n, state.max_rows);
    let (cursor_row, scroll) = cursor_and_scroll(selected, n, height, state.reversed, prev_scroll);
    let (w, buf_ref) = app.ui.win_and_buf_mut(leaf, buf_id);
    if let (Some(w), Some(buf_ref)) = (w, buf_ref) {
        w.scroll_top = scroll;
        w.jump_to_row(buf_ref, cursor_row, height);
    }
}

/// Remove picker state when its leaf closes. The overlay itself is removed
/// by `Ui::win_close → overlay_close`.
pub(crate) fn forget(app: &mut TuiApp, leaf: WinId) {
    app.picker_state.remove(&leaf);
}

/// Current logical selection index (0-based) for `leaf`. Resolves the buffer
/// cursor row through the picker's `reversed` mapping. `None` when `leaf` is
/// not a known picker or has no items.
pub(crate) fn selected_index(app: &TuiApp, leaf: WinId) -> Option<usize> {
    let state = app.picker_state.get(&leaf)?;
    let win = app.ui.win(leaf)?;
    let buf = app.ui.buf(win.buf)?;
    let n = buf.line_count();
    if n == 0 {
        return None;
    }
    let row = win.cursor_row() as usize;
    let row = row.min(n - 1);
    Some(if state.reversed { n - 1 - row } else { row })
}

fn picker_height(item_count: usize, max_rows: u16) -> u16 {
    let n = item_count.max(1) as u16;
    n.min(max_rows.max(1))
}

/// Compute `(cursor_row, scroll_top)` for a picker leaf. Adjusts scroll
/// so the selected buffer row stays in `[scroll, scroll + height)`.
/// `cursor_row` is buffer-absolute.
fn cursor_and_scroll(
    selected: usize,
    item_count: usize,
    height: u16,
    reversed: bool,
    prev_scroll: RowIndex,
) -> (RowIndex, RowIndex) {
    let buf_row = visual_cursor(selected, item_count, reversed);
    let h = height.max(1) as RowIndex;
    let max_scroll = (item_count as RowIndex).max(1).saturating_sub(h);
    let scroll = if buf_row < prev_scroll {
        buf_row
    } else if buf_row >= prev_scroll.saturating_add(h) {
        buf_row + 1 - h
    } else {
        prev_scroll
    }
    .min(max_scroll);
    (buf_row, scroll)
}

fn visual_cursor(logical: usize, n: usize, reversed: bool) -> RowIndex {
    if n == 0 {
        return 0;
    }
    let clamped = logical.min(n - 1);
    if reversed {
        (n - 1 - clamped) as RowIndex
    } else {
        clamped as RowIndex
    }
}

fn layout_for(leaf: WinId, height: u16) -> LayoutTree {
    LayoutTree::vbox(vec![(
        Constraint::Length(height),
        LayoutTree::hbox(vec![(Constraint::Percentage(100), LayoutTree::leaf(leaf))]),
    )])
}

fn anchor_for(ui: &crate::smelt_term::Ui, placement: PickerPlacement, height: u16) -> Anchor {
    match placement {
        // Float above the prompt's chrome stack. Anchoring at the top
        // bar's window (rather than offset-from-prompt-input) keeps the
        // picker correctly placed when queued messages or a stash row
        // grow the top bar past one row.
        PickerPlacement::PromptDocked { .. } => {
            crate::content::layout::anchor_above_prompt_chrome(ui, height)
        }
        PickerPlacement::ScreenCenter => Anchor::ScreenCenter,
        PickerPlacement::Cursor => Anchor::Cursor {
            corner: Corner::NW,
            row_offset: 1,
            col_offset: 0,
        },
        // Anchor at the very bottom of the screen. The host has no
        // opinion about the Lua-allocated statusline; callers that want
        // the picker to clear it pass an explicit `Anchor::Win` against
        // `require("smelt.statusline").win` from the Lua side instead
        // of relying on a Rust-side reservation.
        PickerPlacement::ScreenBottom => Anchor::ScreenBottom { above_rows: 0 },
    }
}

/// Longest `prefix + label` width across the item set, for description alignment.
fn max_label_chars(items: &[PickerItem]) -> usize {
    items
        .iter()
        .map(|i| i.prefix.chars().count() + i.label.chars().count())
        .max()
        .unwrap_or(0)
}

/// Write items into the buffer, one row each, via `LineBuilder`. Reversed mode
/// flips order so logical 0 lands on the last row.
fn write_buffer(app: &mut TuiApp, buf: BufId, items: &[PickerItem], reversed: bool) {
    let max_label = max_label_chars(items);
    let order: Vec<usize> = if reversed {
        (0..items.len()).rev().collect()
    } else {
        (0..items.len()).collect()
    };

    // Clone the theme Arc so the buffer can be borrowed mutably alongside it.
    let theme = app.ui.theme().clone();
    let Some(b) = app.ui.buf_mut(buf) else {
        return;
    };
    // Reset to a single blank seed line; `LineBuilder` will replace it on the
    // first emitted row and append the rest.
    b.set_all_lines(vec![]);

    render_into(b, u16::MAX, &theme, |out| {
        if items.is_empty() {
            out.push(None, Style::new().dim());
            out.print(" (no matches)");
            out.pop_style();
            out.newline();
            return;
        }
        for &src_idx in &order {
            let item = &items[src_idx];
            let label_chars = item.prefix.chars().count() + item.label.chars().count();

            out.print(&" ".repeat(INDENT));

            if !item.prefix.is_empty() {
                out.push(None, item.prefix_style);
                out.print(&item.prefix);
                out.pop_style();
            }
            if !item.label.is_empty() {
                out.push(None, item.label_style);
                out.print(&item.label);
                out.pop_style();
            }
            if let Some(desc) = item.description.as_deref() {
                let pad = max_label.saturating_sub(label_chars) + DESC_GAP;
                out.print(&" ".repeat(pad));
                out.push(None, Style::new().dim());
                out.print(desc);
                out.pop_style();
            }
            out.newline();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── picker_height ────────────────────────────────────────────────────

    #[test]
    fn picker_height_uses_item_count_when_below_cap() {
        assert_eq!(picker_height(3, 10), 3);
    }

    #[test]
    fn picker_height_clamps_to_max_rows_when_items_exceed_cap() {
        assert_eq!(picker_height(50, 8), 8);
    }

    #[test]
    fn picker_height_is_at_least_one_for_empty_lists() {
        // An empty picker still needs a row to show "(no matches)".
        assert_eq!(picker_height(0, 10), 1);
    }

    #[test]
    fn picker_height_treats_a_zero_cap_as_one() {
        // `max_rows = 0` is meaningless; fall back to a one-row picker.
        assert_eq!(picker_height(5, 0), 1);
    }

    // ── visual_cursor ────────────────────────────────────────────────────

    #[test]
    fn visual_cursor_returns_logical_index_when_not_reversed() {
        assert_eq!(visual_cursor(0, 5, false), 0);
        assert_eq!(visual_cursor(3, 5, false), 3);
    }

    #[test]
    fn visual_cursor_mirrors_index_when_reversed() {
        // Reversed: logical 0 → last row, logical n-1 → first row.
        assert_eq!(visual_cursor(0, 5, true), 4);
        assert_eq!(visual_cursor(4, 5, true), 0);
        assert_eq!(visual_cursor(2, 5, true), 2);
    }

    #[test]
    fn visual_cursor_clamps_logical_overflow_to_last_item() {
        assert_eq!(visual_cursor(99, 5, false), 4);
        assert_eq!(visual_cursor(99, 5, true), 0);
    }

    #[test]
    fn visual_cursor_returns_zero_for_empty_list() {
        assert_eq!(visual_cursor(0, 0, false), 0);
        assert_eq!(visual_cursor(0, 0, true), 0);
    }

    // ── cursor_and_scroll ────────────────────────────────────────────────

    #[test]
    fn cursor_and_scroll_keeps_visible_selection_without_scrolling() {
        // 10 items, 5-row viewport, selection at row 2 already on screen.
        let (cursor, scroll) = cursor_and_scroll(2, 10, 5, false, 0);
        assert_eq!(cursor, 2);
        assert_eq!(scroll, 0);
    }

    #[test]
    fn cursor_and_scroll_scrolls_down_when_selection_falls_below_viewport() {
        // Selection at row 7, viewport [0, 5) — needs to scroll so row 7 is visible.
        let (cursor, scroll) = cursor_and_scroll(7, 10, 5, false, 0);
        assert_eq!(cursor, 7);
        // Window slides so cursor is at the bottom: scroll = 7 + 1 - 5 = 3.
        assert_eq!(scroll, 3);
    }

    #[test]
    fn cursor_and_scroll_scrolls_up_when_selection_falls_above_viewport() {
        // Selection at row 1, viewport [4, 9) — needs to scroll up.
        let (cursor, scroll) = cursor_and_scroll(1, 10, 5, false, 4);
        assert_eq!(cursor, 1);
        // Window slides so cursor is at the top: scroll = 1.
        assert_eq!(scroll, 1);
    }

    #[test]
    fn cursor_and_scroll_clamps_scroll_to_max_when_selection_is_at_end() {
        // 10 items, 5-row viewport, selection at last item. max_scroll = 10 - 5 = 5.
        let (cursor, scroll) = cursor_and_scroll(9, 10, 5, false, 0);
        assert_eq!(cursor, 9);
        assert_eq!(scroll, 5);
    }

    #[test]
    fn cursor_and_scroll_inverts_buffer_row_in_reversed_mode() {
        // Reversed: logical 0 → bottom of buffer. 5 items, 3-row viewport.
        // Logical 0 → buffer row 4. Needs scroll to show: scroll = 4+1-3 = 2.
        let (cursor, scroll) = cursor_and_scroll(0, 5, 3, true, 0);
        assert_eq!(cursor, 4);
        assert_eq!(scroll, 2);
    }

    // ── max_label_chars ──────────────────────────────────────────────────

    #[test]
    fn max_label_chars_sums_prefix_and_label_widths() {
        let items = vec![
            PickerItem::new("abc").with_prefix("# "),
            PickerItem::new("xy").with_prefix(">> "),
        ];
        // "# abc" = 5 chars, ">> xy" = 5 chars. Max = 5.
        assert_eq!(max_label_chars(&items), 5);
    }

    #[test]
    fn max_label_chars_counts_chars_not_bytes_for_unicode() {
        // "日本語" = 3 chars, 9 bytes. Verify chars().count() semantics.
        let items = vec![PickerItem::new("日本語")];
        assert_eq!(max_label_chars(&items), 3);
    }

    #[test]
    fn max_label_chars_returns_zero_for_empty_list() {
        assert_eq!(max_label_chars(&[]), 0);
    }

    // ── anchor_for ───────────────────────────────────────────────────────

    fn fresh_ui() -> crate::smelt_term::Ui {
        crate::smelt_term::Ui::new()
    }

    #[test]
    fn anchor_for_prompt_docked_falls_back_to_prompt_win_when_top_bar_missing() {
        // Without the Lua-allocated top bar (no `smelt.prompt_bar.top`
        // named window), the anchor falls back to anchoring against the
        // prompt input itself with `-height` offset. That matches the
        // cold-start case where the Lua composer hasn't run yet.
        let ui = fresh_ui();
        let a = anchor_for(&ui, PickerPlacement::PromptDocked { max_rows: 8 }, 5);
        match a {
            Anchor::Win { row_offset, .. } => assert_eq!(row_offset, -5),
            other => panic!("expected Anchor::Win, got {other:?}"),
        }
    }

    #[test]
    fn anchor_for_screen_center_returns_centered_anchor() {
        let ui = fresh_ui();
        assert!(matches!(
            anchor_for(&ui, PickerPlacement::ScreenCenter, 4),
            Anchor::ScreenCenter
        ));
    }

    #[test]
    fn anchor_for_cursor_places_overlay_one_row_below_cursor() {
        let ui = fresh_ui();
        match anchor_for(&ui, PickerPlacement::Cursor, 3) {
            Anchor::Cursor {
                row_offset,
                col_offset,
                corner,
            } => {
                assert_eq!(row_offset, 1);
                assert_eq!(col_offset, 0);
                assert!(matches!(corner, Corner::NW));
            }
            other => panic!("expected Anchor::Cursor, got {other:?}"),
        }
    }

    #[test]
    fn anchor_for_screen_bottom_does_not_reserve_statusline_rows() {
        // The host has no `statusline` concept anymore — the Lua layer
        // owns the statusline window and any reservation lives on the
        // caller. `ScreenBottom` resolves to the literal screen bottom;
        // overlays that need to clear chrome anchor explicitly against
        // the Lua-allocated statusline window.
        let ui = fresh_ui();
        match anchor_for(&ui, PickerPlacement::ScreenBottom, 4) {
            Anchor::ScreenBottom { above_rows } => assert_eq!(above_rows, 0),
            other => panic!("expected Anchor::ScreenBottom, got {other:?}"),
        }
    }
}
