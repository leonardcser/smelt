use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(top: u16, left: u16, width: u16, height: u16) -> Self {
        Self {
            top,
            left,
            width,
            height,
        }
    }

    pub fn bottom(&self) -> u16 {
        self.top + self.height
    }

    pub fn right(&self) -> u16 {
        self.left + self.width
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.top && row < self.bottom() && col >= self.left && col < self.right()
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

/// Sizing constraint for a layout child along the parent's primary
/// axis. Resolved by `resolve_constraints` against the parent's total
/// size, in declaration order:
///
/// 1. Hard sizes first (`Length`, `Percentage`, `Ratio`, `Max`)
///    consume their exact share of the available space.
/// 2. `Min(n)` reserves at least `n` cells, then competes with
///    `Fill` for the remainder.
/// 3. `Fill` (and any unsatisfied `Min`) splits whatever remains
///    evenly.
///
/// `Fit` is reserved for content-natural sizing — currently behaves
/// like `Fill`; gains true content awareness in P1.b.3 when leaves
/// carry `WinId` and can be queried for natural size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// Exactly `n` cells along the axis.
    Length(u16),
    /// `p` percent of the parent's total size, clamped to remaining.
    Percentage(u16),
    /// Proportional share `num / denom` of the parent. Multiple
    /// `Ratio` siblings split proportionally to one another.
    Ratio(u16, u16),
    /// At least `n` cells; competes with `Fill` for the remainder
    /// once the minimum is satisfied.
    Min(u16),
    /// At most `n` cells. Acts like `Length(n)` when the parent has
    /// at least `n` available; smaller parents shrink it.
    Max(u16),
    /// Fill the remaining space; siblings split evenly.
    Fill,
    /// Size to the leaf's natural content. Falls back to `Fill`
    /// until leaves carry `WinId` (P1.b.3).
    Fit,
}

/// One child of a container: a sizing `Constraint` paired with the
/// subtree it applies to. Used by `LayoutTree::Vbox` and
/// `LayoutTree::Hbox` items.
pub type Item = (Constraint, LayoutTree);

/// Style of the line drawn on the middle row (or column) of a
/// container's `gap`. Requires `gap >= 1` to render; `with_separator`
/// auto-inflates `gap = 0 → 1` when a non-`None` style is set so a
/// caller can opt into a separator without manually budgeting space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SeparatorStyle {
    /// No separator drawn — siblings just have empty cells between
    /// them (or no cells, when `gap = 0`).
    #[default]
    None,
    /// Solid `─` (Vbox) / `│` (Hbox).
    Solid,
    /// Dashed `┄` (Vbox) / `┆` (Hbox).
    Dashed,
}

/// Container chrome shared by `Vbox` and `Hbox`.
#[derive(Clone, Debug, Default)]
pub struct Chrome {
    /// Cells inserted between adjacent children along the primary
    /// axis. `0` packs children flush.
    pub gap: u16,
    /// Optional frame drawn around the container; subtracts 2 from
    /// the inner area on each axis so children render inside the
    /// border. `None` = no frame, no inset.
    pub border: Option<Border>,
    /// Optional title displayed in the top border row. Doesn't
    /// consume layout space (lives in the border row); requires
    /// `border = Some(_)` to render.
    pub title: Option<String>,
    /// Line drawn on the middle row of the gap between adjacent
    /// children. Renders only when `gap >= 1`. `with_separator` keeps
    /// the field and `gap` consistent; setting it directly without
    /// raising `gap` leaves the separator invisible.
    pub separator: SeparatorStyle,
}

#[derive(Clone, Debug)]
pub enum LayoutTree {
    /// Terminal node identifying a single window. The constraint
    /// governing its size lives in its parent's `items`.
    Leaf(super::WinId),
    /// Vertical container; children stack top-to-bottom.
    Vbox { items: Vec<Item>, chrome: Chrome },
    /// Horizontal container; children pack left-to-right.
    Hbox { items: Vec<Item>, chrome: Chrome },
}

impl LayoutTree {
    /// Vertical container with no chrome. Children stack top-to-bottom.
    /// Use `.with_gap` / `.with_border` / `.with_title` to add chrome.
    pub fn vbox(items: Vec<Item>) -> Self {
        Self::Vbox {
            items,
            chrome: Chrome::default(),
        }
    }

    /// Horizontal container with no chrome. Children pack left-to-right.
    pub fn hbox(items: Vec<Item>) -> Self {
        Self::Hbox {
            items,
            chrome: Chrome::default(),
        }
    }

    /// Terminal leaf for a single window.
    pub fn leaf(win: super::WinId) -> Self {
        Self::Leaf(win)
    }

    fn chrome_mut(&mut self) -> Option<&mut Chrome> {
        match self {
            Self::Vbox { chrome, .. } | Self::Hbox { chrome, .. } => Some(chrome),
            Self::Leaf(_) => None,
        }
    }

    pub fn with_gap(mut self, g: u16) -> Self {
        if let Some(c) = self.chrome_mut() {
            c.gap = g;
        }
        self
    }

    pub fn with_border(mut self, b: Border) -> Self {
        if let Some(c) = self.chrome_mut() {
            c.border = Some(b);
        }
        self
    }

    pub fn with_title(mut self, t: impl Into<String>) -> Self {
        if let Some(c) = self.chrome_mut() {
            c.title = Some(t.into());
        }
        self
    }

    /// Whether this tree contains `win` as one of its leaves
    /// (depth-first). Pure structural check — no rect math, no
    /// dependency on terminal size.
    pub fn contains_leaf(&self, win: super::WinId) -> bool {
        match self {
            LayoutTree::Leaf(w) => *w == win,
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                items.iter().any(|(_, child)| child.contains_leaf(win))
            }
        }
    }

    /// Leaf `WinId`s in document (depth-first, declaration) order.
    /// Used by Tab cycling to walk focusable windows in a stable
    /// sequence the user can predict.
    pub fn leaves_in_order(&self) -> Vec<super::WinId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<super::WinId>) {
        match self {
            LayoutTree::Leaf(w) => out.push(*w),
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                for (_, child) in items {
                    child.collect_leaves(out);
                }
            }
        }
    }

    /// Natural `(width, height)` of this tree given an outer cap. Used
    /// by overlay sizing: `Length` / `Percentage` / `Ratio` / `Min` /
    /// `Max` contribute their resolved sizes along the parent axis;
    /// `Fill` / `Fit` contribute `0` (no content awareness yet — that
    /// arrives when leaves expose their Buffer extent in P1.d). The
    /// secondary axis takes the max across siblings. Chrome (border,
    /// gap) is added on top.
    ///
    /// `cap` bounds `Percentage` / `Ratio` resolution and clamps the
    /// final result. The return value is always `<= cap`.
    pub fn natural_size(&self, cap: (u16, u16)) -> (u16, u16) {
        match self {
            // Leaves have no intrinsic size yet — content-natural
            // sizing waits for windows to expose Buffer dimensions.
            // Callers wrap them in containers with explicit sizing.
            LayoutTree::Leaf(_) => (0, 0),
            LayoutTree::Vbox { items, chrome } => natural_box(items, chrome, cap, true),
            LayoutTree::Hbox { items, chrome } => natural_box(items, chrome, cap, false),
        }
    }
}

fn natural_box(items: &[Item], chrome: &Chrome, cap: (u16, u16), vertical: bool) -> (u16, u16) {
    let (cap_w, cap_h) = cap;
    let border_inset: u16 = if chrome.border.is_some() { 2 } else { 0 };
    let gaps = chrome
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);

    // Inner cap subtracts border (both axes) and gap (primary only).
    let (primary_cap, secondary_cap) = if vertical {
        (
            cap_h.saturating_sub(border_inset).saturating_sub(gaps),
            cap_w.saturating_sub(border_inset),
        )
    } else {
        (
            cap_w.saturating_sub(border_inset).saturating_sub(gaps),
            cap_h.saturating_sub(border_inset),
        )
    };

    let inner_cap = if vertical {
        (secondary_cap, primary_cap)
    } else {
        (primary_cap, secondary_cap)
    };

    let mut primary = 0u16;
    let mut secondary = 0u16;
    for (constraint, child) in items {
        let (child_w, child_h) = child.natural_size(inner_cap);
        let primary_size = match constraint {
            Constraint::Length(n) | Constraint::Max(n) | Constraint::Min(n) => *n,
            Constraint::Percentage(p) => {
                ((primary_cap as u32 * *p as u32) / 100).min(primary_cap as u32) as u16
            }
            Constraint::Ratio(num, denom) => {
                if *denom == 0 {
                    0
                } else {
                    ((primary_cap as u32 * *num as u32) / *denom as u32).min(primary_cap as u32)
                        as u16
                }
            }
            Constraint::Fill | Constraint::Fit => {
                if vertical {
                    child_h
                } else {
                    child_w
                }
            }
        };
        let cross_size = if vertical { child_w } else { child_h };
        primary = primary.saturating_add(primary_size);
        secondary = secondary.max(cross_size);
    }
    primary = primary.saturating_add(gaps).saturating_add(border_inset);
    secondary = secondary.saturating_add(border_inset);

    let (w, h) = if vertical {
        (secondary, primary)
    } else {
        (primary, secondary)
    };
    (w.min(cap_w), h.min(cap_h))
}

/// Which corner of a rectangle is its anchor point. Used by
/// `Anchor::Win { target, attach, .. }` to specify which corner of
/// the target window the overlay attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    NW,
    NE,
    SW,
    SE,
}

/// Where an `Overlay` is positioned on screen. Drag = mutate the
/// anchor; the renderer recomputes the overlay's rect each frame
/// from the anchor + the overlay's natural / configured size.
/// Sizing lives on the overlay's `layout: LayoutTree`; this enum
/// only carries position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Centered on screen along both axes.
    ScreenCenter,
    /// Absolute screen position. The overlay's `corner` is placed at
    /// `(row, col)` (terminal cell coordinates, top-left = 0,0).
    ScreenAt { row: i32, col: i32, corner: Corner },
    /// Anchored to the text cursor. The overlay's `corner` touches
    /// the cursor cell, offset by `(row_offset, col_offset)`. If
    /// the overlay would overflow the screen, the renderer flips to
    /// the opposite corner (canonical completer popup behavior).
    Cursor {
        corner: Corner,
        row_offset: i32,
        col_offset: i32,
    },
    /// Anchored to another window. The overlay's `attach` corner
    /// sits on the corresponding edge of the target window's rect,
    /// shifted by `(row_offset, col_offset)`. Negative offsets pull
    /// the overlay above / left of the target — e.g. a one-line
    /// notification one row above the prompt is `attach: NW,
    /// row_offset: -1, col_offset: 0`.
    Win {
        target: super::WinId,
        attach: Corner,
        row_offset: i32,
        col_offset: i32,
    },
    /// Docked to the bottom of the screen. The overlay's natural
    /// height clamps to `term_h - above_rows` (reserve room for the
    /// statusline); width and horizontal centering follow the
    /// overlay's natural width — typically the layout's outer
    /// container is sized full-width via `Constraint::Percentage(100)`,
    /// so the rect spans the whole terminal. Used by tool-approval
    /// dialogs that want a sticky bottom-edge presence.
    ScreenBottom { above_rows: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    None,
    Single,
    Double,
    Rounded,
}

#[derive(Clone, Debug, Default)]
pub struct Gutters {
    pub pad_left: u16,
    pub pad_right: u16,
    pub scrollbar: bool,
}

pub fn resolve_layout(tree: &LayoutTree, area: Rect) -> HashMap<super::WinId, Rect> {
    let mut result = HashMap::new();
    // Top-level tree gets the full area; its constraint is implicit.
    resolve_node(tree, area, &mut result);
    result
}

/// Paint a container's chrome (border + title) into `grid` at `area`.
/// `Border::None` is a no-op — pads `gap` and `separator` are
/// layout-side and consume cells that the content fills, so they're
/// rendered by the children, not chrome paint. Title sits in the top
/// border row, after the top-left corner glyph; truncates at the
/// pre-corner column.
pub fn paint_chrome(
    grid: &mut crate::ui::grid::Grid,
    area: Rect,
    chrome: &Chrome,
    _theme: &crate::ui::Theme,
) {
    let Some(border) = chrome.border else {
        return;
    };
    if area.width < 2 || area.height < 2 {
        return;
    }
    let (h, v, tl, tr, bl, br) = match border {
        Border::None => return,
        Border::Single => ('─', '│', '┌', '┐', '└', '┘'),
        Border::Double => ('═', '║', '╔', '╗', '╚', '╝'),
        Border::Rounded => ('─', '│', '╭', '╮', '╰', '╯'),
    };
    let style = super::grid::Style::default();
    let right = area.left + area.width - 1;
    let bottom = area.top + area.height - 1;

    grid.set(area.left, area.top, tl, style);
    for col in (area.left + 1)..right {
        grid.set(col, area.top, h, style);
    }
    grid.set(right, area.top, tr, style);

    for row in (area.top + 1)..bottom {
        grid.set(area.left, row, v, style);
        grid.set(right, row, v, style);
    }

    grid.set(area.left, bottom, bl, style);
    for col in (area.left + 1)..right {
        grid.set(col, bottom, h, style);
    }
    grid.set(right, bottom, br, style);

    if let Some(title) = chrome.title.as_deref() {
        let max_title_cols = area.width.saturating_sub(2);
        if max_title_cols > 0 {
            let truncated: String = title.chars().take(max_title_cols as usize).collect();
            grid.put_str(area.left + 1, area.top, &truncated, style);
        }
    }
}

fn resolve_node(node: &LayoutTree, area: Rect, out: &mut HashMap<super::WinId, Rect>) {
    match node {
        LayoutTree::Leaf(win) => {
            out.insert(*win, area);
        }
        LayoutTree::Vbox { items, chrome } => {
            resolve_box(items, chrome, area, true, out);
        }
        LayoutTree::Hbox { items, chrome } => {
            resolve_box(items, chrome, area, false, out);
        }
    }
}

fn resolve_box(
    items: &[Item],
    chrome: &Chrome,
    area: Rect,
    vertical: bool,
    out: &mut HashMap<super::WinId, Rect>,
) {
    let inner = match chrome.border {
        Some(_) => Rect::new(
            area.top + 1,
            area.left + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        ),
        None => area,
    };
    let primary_total = if vertical { inner.height } else { inner.width };
    let total_gap = chrome
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);
    let available = primary_total.saturating_sub(total_gap);
    let sizes = resolve_constraints(items, available);
    let mut offset = 0u16;
    for (i, ((_, child), &size)) in items.iter().zip(sizes.iter()).enumerate() {
        let child_area = if vertical {
            Rect::new(inner.top + offset, inner.left, inner.width, size)
        } else {
            Rect::new(inner.top, inner.left + offset, size, inner.height)
        };
        resolve_node(child, child_area, out);
        offset += size;
        if i + 1 < items.len() {
            offset += chrome.gap;
        }
    }
}

pub(crate) fn resolve_constraints(items: &[Item], total: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; items.len()];
    let mut remaining = total;

    // Pass 1: hard-sized constraints consume their share.
    for (i, (c, _)) in items.iter().enumerate() {
        match c {
            Constraint::Length(n) | Constraint::Max(n) => {
                let n = (*n).min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Percentage(pct) => {
                let n = ((total as u32 * *pct as u32) / 100).min(remaining as u32) as u16;
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Min(n) => {
                let n = (*n).min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            _ => {}
        }
    }

    // Pass 2: `Ratio` splits its slice of the remaining proportionally
    // to its siblings' (num, denom) pairs.
    let ratio_total: u32 = items
        .iter()
        .filter_map(|(c, _)| match c {
            Constraint::Ratio(num, _) => Some(*num as u32),
            _ => None,
        })
        .sum();
    let ratio_pool = remaining;
    let mut consumed = 0u16;
    for (i, (c, _)) in items.iter().enumerate() {
        if let Constraint::Ratio(num, _) = c {
            let n = (ratio_pool as u32 * *num as u32)
                .checked_div(ratio_total)
                .unwrap_or(0) as u16;
            sizes[i] = n;
            consumed += n;
        }
    }
    remaining -= consumed.min(remaining);

    // Pass 3: `Fill` and `Fit` split the remainder evenly. (`Fit`
    // behaves like `Fill` until leaves expose natural size.)
    let fill_count = items
        .iter()
        .filter(|(c, _)| matches!(c, Constraint::Fill | Constraint::Fit))
        .count() as u16;
    if let Some(per_fill) = remaining.checked_div(fill_count) {
        let mut extra = remaining % fill_count;
        for (i, (c, _)) in items.iter().enumerate() {
            if matches!(c, Constraint::Fill | Constraint::Fit) {
                sizes[i] = per_fill + u16::from(extra > 0);
                extra = extra.saturating_sub(1);
            }
        }
    }

    sizes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::WinId;

    const A: WinId = WinId(100);
    const B: WinId = WinId(101);
    const C: WinId = WinId(102);

    #[test]
    fn single_leaf_fills_area() {
        let tree = LayoutTree::leaf(A);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn vertical_split_fixed_and_fill() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Length(5), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 19));
        assert_eq!(result[&B], Rect::new(19, 0, 80, 5));
    }

    #[test]
    fn vertical_split_pct_and_fill() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Percentage(25), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&B].height, 6);
        assert_eq!(result[&A].height, 18);
    }

    #[test]
    fn horizontal_split() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 20, 24));
        assert_eq!(result[&B], Rect::new(0, 20, 60, 24));
    }

    #[test]
    fn multiple_fills_distribute_evenly() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 12);
        assert_eq!(result[&B].height, 12);
    }

    #[test]
    fn rect_contains() {
        let r = Rect::new(5, 10, 20, 10);
        assert!(r.contains(5, 10));
        assert!(r.contains(14, 29));
        assert!(!r.contains(15, 10));
        assert!(!r.contains(5, 30));
    }

    #[test]
    fn nested_split() {
        let tree = LayoutTree::vbox(vec![
            (
                Constraint::Fill,
                LayoutTree::hbox(vec![
                    (Constraint::Fill, LayoutTree::leaf(A)),
                    (Constraint::Fill, LayoutTree::leaf(B)),
                ]),
            ),
            (Constraint::Length(4), LayoutTree::leaf(C)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&C], Rect::new(20, 0, 80, 4));
        assert_eq!(result[&A], Rect::new(0, 0, 40, 20));
        assert_eq!(result[&B], Rect::new(0, 40, 40, 20));
    }

    #[test]
    fn min_reserves_floor_then_competes_with_fill() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 3);
        assert_eq!(result[&B].height, 21);
    }

    #[test]
    fn max_caps_at_ceiling_when_parent_has_room() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Max(5), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 5);
        assert_eq!(result[&B].height, 19);
    }

    #[test]
    fn max_shrinks_when_parent_smaller_than_ceiling() {
        let tree = LayoutTree::vbox(vec![(Constraint::Max(50), LayoutTree::leaf(A))]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 24);
    }

    #[test]
    fn ratio_splits_remaining_proportionally() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Ratio(1, 3), LayoutTree::leaf(A)),
            (Constraint::Ratio(2, 3), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 90, 24));
        assert_eq!(result[&A].width, 30);
        assert_eq!(result[&B].width, 60);
    }

    #[test]
    fn ratio_competes_with_length_for_remaining() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Ratio(1, 2), LayoutTree::leaf(B)),
            (Constraint::Ratio(1, 2), LayoutTree::leaf(C)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].width, 20);
        assert_eq!(result[&B].width, 30);
        assert_eq!(result[&C].width, 30);
    }

    #[test]
    fn fit_falls_back_to_fill_for_now() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 12);
        assert_eq!(result[&B].height, 12);
    }

    #[test]
    fn zero_height_produces_empty_rects() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(30), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 10));
        assert_eq!(result[&A].height, 10);
        assert_eq!(result[&B].height, 0);
    }

    #[test]
    fn gap_inserts_spacing_between_children() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
            (Constraint::Fill, LayoutTree::leaf(C)),
        ])
        .with_gap(2);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 7));
        assert_eq!(result[&B].top, 9);
        assert_eq!(result[&C].top, 18);
    }

    #[test]
    fn border_insets_children_by_one_each_side() {
        let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(A))])
            .with_border(Border::Single);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(1, 1, 78, 22));
    }

    #[test]
    fn border_and_gap_compose() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ])
        .with_border(Border::Single)
        .with_gap(1)
        .with_title("dialog");
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].top, 1);
        assert_eq!(result[&A].height + result[&B].height, 21);
        assert_eq!(result[&B].top, result[&A].top + result[&A].height + 1);
    }

    #[test]
    fn natural_size_leaf_is_zero() {
        let tree = LayoutTree::leaf(A);
        assert_eq!(tree.natural_size((80, 24)), (0, 0));
    }

    #[test]
    fn natural_size_vbox_lengths_sum_along_primary() {
        // Two Length(5) children stacked vertically → height 10,
        // width 0 (leaves have no width).
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(5), LayoutTree::leaf(A)),
            (Constraint::Length(5), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (0, 10));
    }

    #[test]
    fn natural_size_hbox_lengths_sum_along_primary() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Length(10), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (30, 0));
    }

    #[test]
    fn natural_size_vbox_gap_adds_to_primary() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (Constraint::Length(4), LayoutTree::leaf(B)),
            (Constraint::Length(5), LayoutTree::leaf(C)),
        ])
        .with_gap(2);
        // 3 + 4 + 5 + 2*(3-1) = 16
        assert_eq!(tree.natural_size((80, 24)), (0, 16));
    }

    #[test]
    fn natural_size_border_adds_two_each_axis() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(10), LayoutTree::leaf(A))])
            .with_border(Border::Single);
        // height: 10 + 2 (border); width: 0 + 2 (border).
        assert_eq!(tree.natural_size((80, 24)), (2, 12));
    }

    #[test]
    fn natural_size_percentage_resolves_against_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Percentage(50), LayoutTree::leaf(A))]);
        // 50% of cap_h=24 = 12.
        assert_eq!(tree.natural_size((80, 24)), (0, 12));
    }

    #[test]
    fn natural_size_ratio_resolves_against_cap() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Ratio(1, 4), LayoutTree::leaf(A)),
            (Constraint::Ratio(1, 4), LayoutTree::leaf(B)),
        ]);
        // 1/4 of cap_w=80 = 20, twice → 40.
        assert_eq!(tree.natural_size((80, 24)), (40, 0));
    }

    #[test]
    fn natural_size_fill_contributes_zero() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        // Fill has no natural size yet, so total = 3.
        assert_eq!(tree.natural_size((80, 24)), (0, 3));
    }

    #[test]
    fn natural_size_clamps_to_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(100), LayoutTree::leaf(A))]);
        assert_eq!(tree.natural_size((80, 24)), (0, 24));
    }

    #[test]
    fn separator_default_is_none() {
        let chrome = Chrome::default();
        assert_eq!(chrome.separator, SeparatorStyle::None);
    }

    #[test]
    fn leaves_in_order_walks_depth_first() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (
                Constraint::Length(5),
                LayoutTree::hbox(vec![
                    (Constraint::Fill, LayoutTree::leaf(B)),
                    (Constraint::Fill, LayoutTree::leaf(C)),
                ]),
            ),
        ]);
        assert_eq!(tree.leaves_in_order(), vec![A, B, C]);
    }

    #[test]
    fn leaves_in_order_single_leaf() {
        let tree = LayoutTree::leaf(A);
        assert_eq!(tree.leaves_in_order(), vec![A]);
    }

    #[test]
    fn contains_leaf_finds_direct_leaf() {
        let tree = LayoutTree::leaf(A);
        assert!(tree.contains_leaf(A));
        assert!(!tree.contains_leaf(B));
    }

    #[test]
    fn contains_leaf_walks_nested_containers() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (
                Constraint::Length(5),
                LayoutTree::hbox(vec![(Constraint::Fill, LayoutTree::leaf(B))]),
            ),
        ]);
        assert!(tree.contains_leaf(A));
        assert!(tree.contains_leaf(B));
        assert!(!tree.contains_leaf(C));
    }

    #[test]
    fn natural_size_nested_chrome_composes() {
        // Outer Vbox border + inner Hbox of two Length children.
        let tree = LayoutTree::vbox(vec![(
            Constraint::Length(5),
            LayoutTree::hbox(vec![
                (Constraint::Length(20), LayoutTree::leaf(A)),
                (Constraint::Length(10), LayoutTree::leaf(B)),
            ]),
        )])
        .with_border(Border::Single);
        // Outer: 5 (length) + 2 (border) = 7 height; inner Hbox width
        // 30 + 2 (outer border) = 32.
        assert_eq!(tree.natural_size((80, 24)), (32, 7));
    }

    #[test]
    fn paint_chrome_no_border_is_noop() {
        let mut grid = crate::ui::grid::Grid::new(10, 5);
        let chrome = Chrome::default();
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::ui::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }

    #[test]
    fn paint_chrome_single_border_draws_corners_and_edges() {
        let mut grid = crate::ui::grid::Grid::new(10, 5);
        let chrome = Chrome {
            border: Some(Border::Single),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::ui::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(9, 0).symbol, '┐');
        assert_eq!(grid.cell(0, 4).symbol, '└');
        assert_eq!(grid.cell(9, 4).symbol, '┘');
        assert_eq!(grid.cell(5, 0).symbol, '─');
        assert_eq!(grid.cell(0, 2).symbol, '│');
    }

    #[test]
    fn paint_chrome_title_lands_on_top_border() {
        let mut grid = crate::ui::grid::Grid::new(20, 5);
        let chrome = Chrome {
            border: Some(Border::Rounded),
            title: Some("hello".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 20, 5),
            &chrome,
            &crate::ui::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '╭');
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(5, 0).symbol, 'o');
        // Beyond the title, the top edge resumes the border glyph.
        assert_eq!(grid.cell(6, 0).symbol, '─');
    }

    #[test]
    fn paint_chrome_truncates_title_to_inner_width() {
        let mut grid = crate::ui::grid::Grid::new(8, 3);
        let chrome = Chrome {
            border: Some(Border::Single),
            title: Some("muchtoolong".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 8, 3),
            &chrome,
            &crate::ui::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(1, 0).symbol, 'm');
        assert_eq!(grid.cell(6, 0).symbol, 'o');
        // Last cell before the corner — overwritten by the title's 6th
        // char (max_title_cols = 6 = area.width − 2).
        assert_eq!(grid.cell(7, 0).symbol, '┐');
    }
}
