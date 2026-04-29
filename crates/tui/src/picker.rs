//! Picker — Buffer-backed list overlay for selectable items.
//!
//! The picker is a Vbox-shaped Overlay containing one Buffer-backed
//! list leaf. Each item lives as one buffer line shaped
//! `{indent}{prefix}{label}{padding}{description}`; per-item accents
//! and the dim description column are rendered as highlight extmarks.
//! Selection is the leaf's `cursor_line` flagged with
//! `cursor_line_highlight = true`, so the selected row picks up the
//! `CursorLine` theme background.
//!
//! Reversed mode (used by prompt-docked completer pickers) places the
//! "best match" at the bottom row by writing items in reverse order
//! into the buffer; selection is mapped logical → visual by
//! `n - 1 - logical`.
//!
//! All callers go through `open` to allocate the overlay and through
//! `set_items` / `set_selected` to mutate an existing one. Closing the
//! leaf via `close_float` (or `Ui::win_close`) cascades through
//! `overlay_close` to remove the overlay.

use crate::app::App;
use crossterm::style::Color;
use ui::buffer::{BufCreateOpts, SpanStyle};
use ui::layout::Anchor;
use ui::{
    BufId, Constraint, Corner, LayoutTree, Overlay, OverlayId, SplitConfig, WinId, PROMPT_WIN,
};

/// One row in a picker. Prefix sits left of the label; description (if
/// any) prints in a column-aligned dim block to the right of the
/// longest label across the whole item set.
#[derive(Clone, Default, Debug)]
pub struct PickerItem {
    pub label: String,
    pub description: Option<String>,
    pub prefix: String,
    /// Optional per-item accent color. When set, the prefix paints in
    /// this color regardless of selection.
    pub accent: Option<Color>,
}

impl PickerItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Default::default()
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn with_accent(mut self, color: Color) -> Self {
        self.accent = Some(color);
        self
    }
}

/// Where the picker overlay anchors on screen. Mirrors the placement
/// vocabulary the legacy `Placement` enum carried, mapped onto the
/// overlay anchor system.
#[derive(Clone, Copy, Debug)]
pub enum PickerPlacement {
    /// Centered on screen — used by focusable selector overlays.
    ScreenCenter,
    /// Docked above the prompt window. Reversed: best match sits at
    /// the bottom row, closest to the prompt.
    PromptDocked { max_rows: u16 },
    /// Anchored to the cursor (fallback for `placement = "cursor"`).
    Cursor,
    /// Docked to the bottom of the screen, full width, reserving one
    /// row for the status bar.
    ScreenBottom,
}

/// Per-leaf picker bookkeeping. Lives on `App::picker_state` keyed by
/// the leaf `WinId` so `set_items` / `set_selected` can resize the
/// overlay's outer height constraint and reverse logical → visual
/// indices without re-deriving placement on every call.
#[derive(Clone, Copy, Debug)]
pub struct PickerState {
    pub overlay: OverlayId,
    pub placement: PickerPlacement,
    pub reversed: bool,
    pub max_rows: u16,
}

const INDENT: usize = 1;
const DESC_GAP: usize = 2;

/// Open a Buffer-backed picker overlay. `selected` is a logical
/// 0-based index into `items`; `reversed` is implicit from
/// `placement`. Returns the leaf `WinId` (caller stores it for
/// subsequent `set_items` / `set_selected` calls).
pub fn open(
    app: &mut App,
    items: Vec<PickerItem>,
    selected: usize,
    placement: PickerPlacement,
    focusable: bool,
    blocks_agent: bool,
    z: u16,
) -> Option<WinId> {
    let max_rows = match placement {
        PickerPlacement::PromptDocked { max_rows } => max_rows,
        _ => 32, // generous cap for screen-center / cursor / dock-bottom
    };
    let reversed = matches!(placement, PickerPlacement::PromptDocked { .. });

    let buf = app.ui.buf_create(BufCreateOpts {
        modifiable: false,
        ..Default::default()
    });
    write_buffer(app, buf, &items, reversed);

    let leaf = app.ui.win_open_split(
        buf,
        SplitConfig {
            region: "picker_overlay".into(),
            gutters: Default::default(),
        },
    )?;
    if let Some(w) = app.ui.win_mut(leaf) {
        w.cursor_line_highlight = true;
        w.focusable = focusable;
        w.cursor_line = visual_cursor(selected, items.len(), reversed);
        w.scroll_top = 0;
    }

    let height = picker_height(items.len(), max_rows);
    let layout = layout_for(leaf, height);
    let anchor = anchor_for(placement, height);
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

/// Replace the picker's items in place. Resizes the overlay's outer
/// height constraint when the count changes; preserves selection at
/// `selected` (clamped). No-op when `leaf` isn't a known picker.
pub fn set_items(app: &mut App, leaf: WinId, items: Vec<PickerItem>, selected: usize) {
    let Some(state) = app.picker_state.get(&leaf).copied() else {
        return;
    };
    let buf_id = app.ui.win(leaf).map(|w| w.buf);
    if let Some(buf_id) = buf_id {
        write_buffer(app, buf_id, &items, state.reversed);
    }
    if let Some(w) = app.ui.win_mut(leaf) {
        w.cursor_line = visual_cursor(selected, items.len(), state.reversed);
        w.scroll_top = 0;
    }
    let height = picker_height(items.len(), state.max_rows);
    if let Some(ov) = app.ui.overlay_mut(state.overlay) {
        ov.layout = layout_for(leaf, height);
        ov.anchor = anchor_for(state.placement, height);
    }
}

/// Update the picker's logical selection. Clamps to `n - 1`.
pub fn set_selected(app: &mut App, leaf: WinId, selected: usize) {
    let Some(state) = app.picker_state.get(&leaf).copied() else {
        return;
    };
    let buf_id = match app.ui.win(leaf).map(|w| w.buf) {
        Some(id) => id,
        None => return,
    };
    let n = app.ui.buf(buf_id).map(|b| b.line_count()).unwrap_or(0);
    if let Some(w) = app.ui.win_mut(leaf) {
        w.cursor_line = visual_cursor(selected, n, state.reversed);
    }
}

/// Remove the picker's bookkeeping when its leaf closes. The overlay
/// itself is removed by `Ui::win_close → overlay_close` cascade.
pub fn forget(app: &mut App, leaf: WinId) {
    app.picker_state.remove(&leaf);
}

fn picker_height(item_count: usize, max_rows: u16) -> u16 {
    let n = item_count.max(1) as u16;
    n.min(max_rows.max(1))
}

fn visual_cursor(logical: usize, n: usize, reversed: bool) -> u16 {
    if n == 0 {
        return 0;
    }
    let clamped = logical.min(n - 1);
    if reversed {
        (n - 1 - clamped) as u16
    } else {
        clamped as u16
    }
}

fn layout_for(leaf: WinId, height: u16) -> LayoutTree {
    LayoutTree::vbox(vec![(
        Constraint::Length(height),
        LayoutTree::hbox(vec![(Constraint::Percentage(100), LayoutTree::leaf(leaf))]),
    )])
}

fn anchor_for(placement: PickerPlacement, height: u16) -> Anchor {
    match placement {
        PickerPlacement::PromptDocked { .. } => Anchor::Win {
            target: PROMPT_WIN,
            attach: Corner::NW,
            row_offset: -(height as i32),
            col_offset: 0,
        },
        PickerPlacement::ScreenCenter => Anchor::ScreenCenter,
        PickerPlacement::Cursor => Anchor::Cursor {
            corner: Corner::NW,
            row_offset: 1,
            col_offset: 0,
        },
        PickerPlacement::ScreenBottom => Anchor::ScreenBottom { above_rows: 1 },
    }
}

/// Compute the longest label width (`prefix + label`) across the item
/// set so descriptions can align in a column.
fn max_label_chars(items: &[PickerItem]) -> usize {
    items
        .iter()
        .map(|i| i.prefix.chars().count() + i.label.chars().count())
        .max()
        .unwrap_or(0)
}

/// Render `items` into `buf` as one row each, populating highlight
/// extmarks for per-item accent (prefix) and the dim description
/// column. Reversed mode flips item order so logical 0 lands on the
/// last visual row.
fn write_buffer(app: &mut App, buf: BufId, items: &[PickerItem], reversed: bool) {
    let max_label = max_label_chars(items);
    let order: Vec<usize> = if reversed {
        (0..items.len()).rev().collect()
    } else {
        (0..items.len()).collect()
    };

    let mut lines: Vec<String> = Vec::with_capacity(items.len().max(1));
    for &src_idx in &order {
        let item = &items[src_idx];
        let label_chars = item.prefix.chars().count() + item.label.chars().count();
        let mut line = String::new();
        line.push_str(&" ".repeat(INDENT));
        line.push_str(&item.prefix);
        line.push_str(&item.label);
        if let Some(desc) = item.description.as_deref() {
            let pad = max_label.saturating_sub(label_chars) + DESC_GAP;
            line.push_str(&" ".repeat(pad));
            line.push_str(desc);
        }
        lines.push(line);
    }
    if lines.is_empty() {
        // A picker with zero items renders as one blank dim line. Keeps
        // the overlay non-empty even when filtering returns no matches.
        lines.push(" (no matches)".into());
    }

    let Some(b) = app.ui.buf_mut(buf) else {
        return;
    };
    b.set_all_lines(lines);

    if items.is_empty() {
        b.add_highlight(0, 0, 14, SpanStyle::dim());
        return;
    }

    for (visual_row, &src_idx) in order.iter().enumerate() {
        let item = &items[src_idx];
        let prefix_start = INDENT as u16;
        let prefix_end = prefix_start + item.prefix.chars().count() as u16;
        if prefix_end > prefix_start {
            if let Some(color) = item.accent {
                b.add_highlight(visual_row, prefix_start, prefix_end, SpanStyle::fg(color));
            }
        }
        if let Some(desc) = item.description.as_deref() {
            let label_chars = item.prefix.chars().count() + item.label.chars().count();
            let pad = max_label.saturating_sub(label_chars) + DESC_GAP;
            let start = INDENT as u16 + label_chars as u16 + pad as u16;
            let end = start + desc.chars().count() as u16;
            if end > start {
                b.add_highlight(visual_row, start, end, SpanStyle::dim());
            }
        }
    }
}
