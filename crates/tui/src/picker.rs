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
use crate::smelt_edit::layout::Anchor;
use crate::smelt_edit::BufCreateOpts;
use crate::smelt_edit::{
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
    /// Docked directly above the `:` cmdline; reversed so the best match is closest to input.
    CmdlineDocked { max_rows: u16 },
}

/// Per-leaf picker state, keyed by `WinId`. The picker owns the full logical
/// item set, but its backing buffer materializes only the visible slice.
#[derive(Clone, Debug)]
pub(crate) struct PickerState {
    pub(crate) overlay: OverlayId,
    pub(crate) placement: PickerPlacement,
    pub(crate) reversed: bool,
    pub(crate) items: Vec<PickerItem>,
    pub(crate) selected: usize,
    pub(crate) materialized: std::ops::Range<RowIndex>,
    pub(crate) max_label: usize,
}

const INDENT: usize = 1;
const DESC_GAP: usize = 2;
const PICKER_OVERSCAN_ROWS: RowIndex = 32;

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
    let reversed = matches!(
        placement,
        PickerPlacement::PromptDocked { .. } | PickerPlacement::CmdlineDocked { .. }
    );
    let total = items.len();
    let selected = clamp_selected(selected, total);

    let buf = app.ui.buf_create(BufCreateOpts::default());

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
    let height = picker_height(total, placement, &app.ui);
    let overlay = Overlay::new(
        layout_for(leaf, height),
        anchor_for(&app.ui, placement, height),
    )
    .with_z(z)
    .blocks_agent(blocks_agent);
    let overlay_id = app.ui.overlay_open(overlay);

    app.picker_state.insert(
        leaf,
        PickerState {
            overlay: overlay_id,
            placement,
            reversed,
            max_label: max_label_chars(&items),
            items,
            selected,
            materialized: 0..0,
        },
    );

    sync_selected(app, leaf, selected);
    if let Some(w) = app.ui.win_mut(leaf) {
        w.set_list_selection_highlight(true);
        w.set_surface(crate::smelt_edit::WindowSurface::list(focusable));
    }
    if focusable {
        app.ui.set_focus(leaf);
    }
    Some(leaf)
}

/// Replace picker items, resizing the overlay. No-op if `leaf` is not a known picker.
pub(crate) fn set_items(app: &mut TuiApp, leaf: WinId, items: Vec<PickerItem>, selected: usize) {
    let Some(mut state) = app.picker_state.remove(&leaf) else {
        return;
    };
    state.max_label = max_label_chars(&items);
    state.items = items;
    state.selected = clamp_selected(selected, state.items.len());
    state.materialized = 0..0;
    let height = picker_height(state.items.len(), state.placement, &app.ui);
    let new_anchor = anchor_for(&app.ui, state.placement, height);
    let overlay = state.overlay;
    app.picker_state.insert(leaf, state);

    if let Some(ov) = app.ui.overlay_mut(overlay) {
        ov.layout = layout_for(leaf, height);
        ov.anchor = new_anchor;
    }
    sync_selected(app, leaf, selected);
}

/// Update the picker's logical selection (clamped to `n - 1`).
pub(crate) fn set_selected(app: &mut TuiApp, leaf: WinId, selected: usize) {
    sync_selected(app, leaf, selected);
}

/// Move the picker's logical selection by `delta`, clamped to the item set.
pub(crate) fn move_selected(app: &mut TuiApp, leaf: WinId, delta: isize) {
    let Some(state) = app.picker_state.get(&leaf) else {
        return;
    };
    let total = state.items.len();
    if total == 0 {
        return;
    }
    let selected = if delta.is_negative() {
        state.selected.saturating_sub(delta.unsigned_abs())
    } else {
        state
            .selected
            .saturating_add(delta as usize)
            .min(total.saturating_sub(1))
    };
    sync_selected(app, leaf, selected);
}

/// Refresh materialized picker buffers after viewport-led scrolling.
pub(crate) fn sync_scrolled(app: &mut TuiApp) {
    let leaves: Vec<WinId> = app.picker_state.keys().copied().collect();
    for leaf in leaves {
        let Some(state) = app.picker_state.get(&leaf) else {
            continue;
        };
        let total = state.items.len();
        if total == 0 {
            continue;
        }
        let Some(abs_cursor) = app.ui.win(leaf).map(|w| w.cursor_abs_row()) else {
            continue;
        };
        let selected = logical_from_visual(abs_cursor, total, state.reversed);
        sync_to_view(app, leaf, selected, SyncAnchor::ScrollTop);
    }
}

/// Recompute prompt-docked picker sizes after the main layout changes.
/// Called from `refresh_main_layout` so pickers shrink or grow as the
/// available headroom above the prompt changes.
pub(crate) fn sync_layouts(app: &mut TuiApp) {
    let leaves: Vec<WinId> = app.picker_state.keys().copied().collect();
    for leaf in leaves {
        let Some(state) = app.picker_state.get(&leaf) else {
            continue;
        };
        if !matches!(
            state.placement,
            PickerPlacement::PromptDocked { .. } | PickerPlacement::CmdlineDocked { .. }
        ) {
            continue;
        }
        let total = state.items.len();
        let height = picker_height(total, state.placement, &app.ui);
        let new_anchor = anchor_for(&app.ui, state.placement, height);
        if let Some(ov) = app.ui.overlay_mut(state.overlay) {
            ov.layout = layout_for(leaf, height);
            ov.anchor = new_anchor;
        }
        // Re-materialize with the new height while keeping the current
        // logical selection visible.
        sync_selected(app, leaf, state.selected);
    }
}

/// Remove picker state when its leaf closes. The overlay itself is removed
/// by `Ui::win_close → overlay_close`.
pub(crate) fn forget(app: &mut TuiApp, leaf: WinId) {
    app.picker_state.remove(&leaf);
}

/// Current logical selection index (0-based) for `leaf`.
pub(crate) fn selected_index(app: &TuiApp, leaf: WinId) -> Option<usize> {
    let state = app.picker_state.get(&leaf)?;
    (!state.items.is_empty()).then_some(state.selected)
}

#[derive(Clone, Copy)]
enum SyncAnchor {
    Selected,
    ScrollTop,
}

fn sync_selected(app: &mut TuiApp, leaf: WinId, selected: usize) {
    sync_to_view(app, leaf, selected, SyncAnchor::Selected);
}

fn sync_to_view(app: &mut TuiApp, leaf: WinId, selected: usize, anchor: SyncAnchor) {
    let Some(mut state) = app.picker_state.remove(&leaf) else {
        return;
    };
    let total = state.items.len();
    state.selected = clamp_selected(selected, total);
    let height = picker_height(total, state.placement, &app.ui);
    let selected_visual = visual_cursor(state.selected, total, state.reversed);
    let prev_scroll = app.ui.win(leaf).map(|w| w.scroll_top()).unwrap_or(0);
    let scroll = match anchor {
        SyncAnchor::Selected => {
            crate::smelt_edit::scroll_to_show(prev_scroll, selected_visual, height)
        }
        SyncAnchor::ScrollTop => {
            crate::smelt_edit::clamp_scroll(prev_scroll, total as RowIndex, height)
        }
    };
    let range_anchor = match anchor {
        SyncAnchor::Selected => selected_visual,
        SyncAnchor::ScrollTop => scroll,
    };
    let range = crate::smelt_edit::materialized_row_range(
        range_anchor,
        total as RowIndex,
        height,
        PICKER_OVERSCAN_ROWS,
    );
    let buf_id = app.ui.win(leaf).map(|w| w.buf);
    if let Some(buf_id) = buf_id {
        write_buffer_range(
            app,
            buf_id,
            &state.items,
            range.clone(),
            state.reversed,
            state.max_label,
        );
        let (w, buf_ref) = app.ui.win_and_buf_mut(leaf, buf_id);
        if let (Some(w), Some(buf_ref)) = (w, buf_ref) {
            w.pin_scroll(scroll);
            w.apply_materialized_rows(crate::smelt_edit::MaterializedRows {
                clamped_scroll: scroll,
                row_base: range.start,
                total_rows: total as RowIndex,
                materialized_rows: range.end.saturating_sub(range.start),
            });
            let local_cursor = selected_visual
                .saturating_sub(range.start)
                .min(buf_ref.line_count().saturating_sub(1) as RowIndex);
            w.jump_to_row(buf_ref, local_cursor, height);
        }
    }
    state.materialized = range;
    app.picker_state.insert(leaf, state);
}

fn clamp_selected(selected: usize, total: usize) -> usize {
    selected.min(total.saturating_sub(1))
}

/// Effective row cap for docked pickers, limited by available headroom so
/// overlays never cover their input chrome. Non-docked placements use the
/// default picker cap.
fn effective_max_rows(placement: PickerPlacement, ui: &crate::smelt_edit::Ui) -> u16 {
    let desired = match placement {
        PickerPlacement::PromptDocked { max_rows }
        | PickerPlacement::CmdlineDocked { max_rows } => max_rows,
        _ => 32,
    };
    match placement {
        PickerPlacement::PromptDocked { .. } => {
            let headroom = crate::content::layout::available_rows_above_prompt_chrome(ui);
            // Always keep at least one row so the picker remains usable even when
            // the prompt block fills the entire terminal.
            desired.max(1).min(headroom.max(1))
        }
        PickerPlacement::CmdlineDocked { .. } => {
            let (_, term_h) = ui.terminal_size();
            let headroom = term_h.saturating_sub(1);
            desired.max(1).min(headroom.max(1))
        }
        _ => desired.max(1),
    }
}

fn picker_height(item_count: usize, placement: PickerPlacement, ui: &crate::smelt_edit::Ui) -> u16 {
    let n = item_count.max(1) as u16;
    n.min(effective_max_rows(placement, ui))
}

fn logical_from_visual(visual: RowIndex, n: usize, reversed: bool) -> usize {
    if n == 0 {
        return 0;
    }
    let visual = visual.min((n - 1) as RowIndex) as usize;
    if reversed {
        n - 1 - visual
    } else {
        visual
    }
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

fn anchor_for(ui: &crate::smelt_edit::Ui, placement: PickerPlacement, height: u16) -> Anchor {
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
        PickerPlacement::CmdlineDocked { .. } => Anchor::ScreenBottom { above_rows: 1 },
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

/// Write a materialized visual-row range into the backing buffer.
fn write_buffer_range(
    app: &mut TuiApp,
    buf: BufId,
    items: &[PickerItem],
    range: std::ops::Range<RowIndex>,
    reversed: bool,
    max_label: usize,
) {
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
        let total = items.len() as RowIndex;
        for visual_row in range.start..range.end.min(total) {
            let src_idx = if reversed {
                (total - 1 - visual_row) as usize
            } else {
                visual_row as usize
            };
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
        let ui = fresh_ui();
        assert_eq!(picker_height(3, PickerPlacement::ScreenCenter, &ui), 3);
    }

    #[test]
    fn picker_height_clamps_to_max_rows_when_items_exceed_cap() {
        let ui = fresh_ui();
        assert_eq!(picker_height(50, PickerPlacement::ScreenCenter, &ui), 32);
    }

    #[test]
    fn picker_height_is_at_least_one_for_empty_lists() {
        // An empty picker still needs a row to show "(no matches)".
        let ui = fresh_ui();
        assert_eq!(picker_height(0, PickerPlacement::ScreenCenter, &ui), 1);
    }

    #[test]
    fn picker_height_treats_a_zero_cap_as_one() {
        // `max_rows = 0` is meaningless; fall back to a one-row picker.
        let ui = fresh_ui();
        assert_eq!(
            picker_height(5, PickerPlacement::PromptDocked { max_rows: 0 }, &ui),
            1
        );
    }

    #[test]
    fn picker_height_prompt_docked_clamps_to_headroom() {
        let mut ui = ui_with_prompt_layout(80, 24);
        // With an 80x24 terminal and a one-row prompt at the bottom, the
        // transcript occupies rows 0..21 and headroom above the prompt is 22.
        let placement = PickerPlacement::PromptDocked { max_rows: 8 };
        assert_eq!(picker_height(50, placement, &ui), 8);

        // Shrink terminal so only three rows sit above the one-row prompt
        // (seed layout: transcript + gap + prompt).
        ui.set_terminal_size(80, 4);
        ui.set_layout(crate::content::layout::seed_layout_tree(1));
        assert_eq!(picker_height(50, placement, &ui), 3);
    }

    fn ui_with_prompt_layout(term_w: u16, term_h: u16) -> crate::smelt_edit::Ui {
        let mut ui = crate::smelt_edit::Ui::new();
        ui.set_terminal_size(term_w, term_h);
        let tbuf = ui.buf_create(crate::smelt_edit::BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            crate::app::TRANSCRIPT_WIN,
            tbuf,
            crate::smelt_edit::SplitConfig {
                region: "transcript".into(),
                gutters: crate::smelt_edit::Gutters::default(),
            },
        ));
        let pbuf = ui.buf_create(crate::smelt_edit::BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            crate::app::PROMPT_WIN,
            pbuf,
            crate::smelt_edit::SplitConfig {
                region: "prompt".into(),
                gutters: crate::smelt_edit::Gutters {
                    pad_left: 1,
                    pad_right: 1,
                    ..Default::default()
                },
            },
        ));
        ui.set_layout(crate::content::layout::seed_layout_tree(1));
        ui
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

    fn cursor_and_scroll(
        selected: usize,
        item_count: usize,
        height: u16,
        reversed: bool,
        prev_scroll: RowIndex,
    ) -> (RowIndex, RowIndex) {
        let cursor = visual_cursor(selected, item_count, reversed);
        let max_scroll = (item_count as RowIndex).saturating_sub(height as RowIndex);
        let scroll = crate::smelt_edit::scroll_to_show(prev_scroll, cursor, height).min(max_scroll);
        (cursor, scroll)
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
        // Selection at row 7, viewport [0, 5) - needs to scroll so row 7 is visible.
        let (cursor, scroll) = cursor_and_scroll(7, 10, 5, false, 0);
        assert_eq!(cursor, 7);
        // Window slides so cursor is at the bottom: scroll = 7 + 1 - 5 = 3.
        assert_eq!(scroll, 3);
    }

    #[test]
    fn cursor_and_scroll_scrolls_up_when_selection_falls_above_viewport() {
        // Selection at row 1, viewport [4, 9) - needs to scroll up.
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

    fn fresh_ui() -> crate::smelt_edit::Ui {
        crate::smelt_edit::Ui::new()
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
        // The host has no `statusline` concept anymore - the Lua layer
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

    #[test]
    fn anchor_for_cmdline_docked_reserves_cmdline_row() {
        let ui = fresh_ui();
        match anchor_for(&ui, PickerPlacement::CmdlineDocked { max_rows: 8 }, 4) {
            Anchor::ScreenBottom { above_rows } => assert_eq!(above_rows, 1),
            other => panic!("expected Anchor::ScreenBottom, got {other:?}"),
        }
    }

    #[test]
    fn picker_height_cmdline_docked_clamps_above_cmdline() {
        let mut ui = fresh_ui();
        ui.set_terminal_size(80, 5);
        assert_eq!(
            picker_height(50, PickerPlacement::CmdlineDocked { max_rows: 8 }, &ui),
            4
        );
    }
}
