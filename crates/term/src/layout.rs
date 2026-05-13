pub use super::geometry::Rect;
use std::collections::HashMap;

/// Opaque leaf identifier. Hosts mint and dispatch on these; the renderer
/// treats them as opaque. Wide enough for common host-side id types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaintId(pub u64);

impl PaintId {
    pub fn raw(self) -> u64 {
        self.0
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
/// like `Fill`; gains true content awareness once leaves can be
/// queried for natural size.
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
    /// until leaves expose a natural-size hook.
    Fit,
}

/// A sizing `Constraint` paired with its subtree; used as `Vbox`/`Hbox` items.
pub type Item = (Constraint, LayoutTree);

/// Container chrome (gap, border, title) shared by `Vbox` and `Hbox`.
#[derive(Clone, Debug, Default)]
pub struct Chrome {
    /// Cells between adjacent children; `0` packs flush.
    pub gap: u16,
    /// Frame around the container; each enabled side reserves one row/col. `None` = no inset.
    pub border: Option<Border>,
    /// Title in the top border row. Requires `border = Some(_)`; renders as a styled [`Line`].
    pub title: Option<crate::line::Line<'static>>,
}

#[derive(Clone, Debug)]
pub enum LayoutTree {
    /// Terminal node; the host matches on `PaintId` in its paint dispatcher.
    Leaf(PaintId),
    /// Vertical container; children stack top-to-bottom.
    Vbox { items: Vec<Item>, chrome: Chrome },
    /// Horizontal container; children pack left-to-right.
    Hbox { items: Vec<Item>, chrome: Chrome },
}

impl LayoutTree {
    /// Vertical container. Use `.with_gap` / `.with_border` / `.with_title` to add chrome.
    pub fn vbox(items: Vec<Item>) -> Self {
        Self::Vbox {
            items,
            chrome: Chrome::default(),
        }
    }

    /// Horizontal container. Use `.with_gap` / `.with_border` / `.with_title` to add chrome.
    pub fn hbox(items: Vec<Item>) -> Self {
        Self::Hbox {
            items,
            chrome: Chrome::default(),
        }
    }

    /// Terminal leaf. Accepts anything `Into<PaintId>`.
    pub fn leaf(id: impl Into<PaintId>) -> Self {
        Self::Leaf(id.into())
    }

    fn chrome_mut(&mut self) -> Option<&mut Chrome> {
        match self {
            Self::Vbox { chrome, .. } | Self::Hbox { chrome, .. } => Some(chrome),
            Self::Leaf(_) => None,
        }
    }

    /// If `self` is a `Leaf`, wraps it in a single-item `Vbox` so chrome
    /// methods work uniformly. No-op for `Vbox`/`Hbox`.
    fn ensure_chrome_capable(self) -> Self {
        match self {
            Self::Leaf(_) => Self::Vbox {
                items: vec![(Constraint::Fill, self)],
                chrome: Chrome::default(),
            },
            other => other,
        }
    }

    pub fn with_gap(self, g: u16) -> Self {
        let mut tree = self.ensure_chrome_capable();
        if let Some(c) = tree.chrome_mut() {
            c.gap = g;
        }
        tree
    }

    pub fn with_border(self, b: Border) -> Self {
        let mut tree = self.ensure_chrome_capable();
        if let Some(c) = tree.chrome_mut() {
            c.border = Some(b);
        }
        tree
    }

    pub fn with_title(self, t: impl Into<crate::line::Line<'static>>) -> Self {
        let mut tree = self.ensure_chrome_capable();
        if let Some(c) = tree.chrome_mut() {
            c.title = Some(t.into());
        }
        tree
    }

    /// Whether `id` appears as a leaf in this tree (depth-first structural check).
    pub fn contains_leaf(&self, id: impl Into<PaintId>) -> bool {
        let id = id.into();
        self.contains_leaf_id(id)
    }

    fn contains_leaf_id(&self, id: PaintId) -> bool {
        match self {
            LayoutTree::Leaf(p) => *p == id,
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                items.iter().any(|(_, child)| child.contains_leaf_id(id))
            }
        }
    }

    /// All leaf `PaintId`s in depth-first declaration order.
    pub fn leaves_in_order(&self) -> Vec<PaintId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaintId>) {
        match self {
            LayoutTree::Leaf(p) => out.push(*p),
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                for (_, child) in items {
                    child.collect_leaves(out);
                }
            }
        }
    }

    /// Natural `(width, height)` bounded by `cap`. `Fill`/`Fit` contribute `0`;
    /// chrome (border, gap) is added on top. Result is always `<= cap`.
    pub fn natural_size(&self, cap: (u16, u16)) -> (u16, u16) {
        match self {
            // Leaves have no intrinsic size.
            LayoutTree::Leaf(_) => (0, 0),
            LayoutTree::Vbox { items, chrome } => natural_box(items, chrome, cap, true),
            LayoutTree::Hbox { items, chrome } => natural_box(items, chrome, cap, false),
        }
    }
}

fn natural_box(items: &[Item], chrome: &Chrome, cap: (u16, u16), vertical: bool) -> (u16, u16) {
    let (cap_w, cap_h) = cap;
    let (border_w, border_h) = match chrome.border {
        Some(b) => {
            let bw = u16::from(b.left.is_some()) + u16::from(b.right.is_some());
            let bh = u16::from(b.top.is_some()) + u16::from(b.bottom.is_some());
            (bw, bh)
        }
        None => (0, 0),
    };
    let gaps = chrome
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);

    // Inner cap subtracts border and gap from the primary axis.
    let (primary_cap, secondary_cap) = if vertical {
        (
            cap_h.saturating_sub(border_h).saturating_sub(gaps),
            cap_w.saturating_sub(border_w),
        )
    } else {
        (
            cap_w.saturating_sub(border_w).saturating_sub(gaps),
            cap_h.saturating_sub(border_h),
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
    let (primary_border, secondary_border) = if vertical {
        (border_h, border_w)
    } else {
        (border_w, border_h)
    };
    primary = primary.saturating_add(gaps).saturating_add(primary_border);
    secondary = secondary.saturating_add(secondary_border);

    let (w, h) = if vertical {
        (secondary, primary)
    } else {
        (primary, secondary)
    };
    (w.min(cap_w), h.min(cap_h))
}

/// Which corner of a rectangle serves as its anchor point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    NW,
    NE,
    SW,
    SE,
}

/// Screen position for an anchored overlay. Carries position only;
/// sizing lives on the container's layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Centered on screen.
    ScreenCenter,
    /// Absolute screen position; `corner` is placed at `(row, col)`.
    ScreenAt { row: i32, col: i32, corner: Corner },
    /// Anchored to the text cursor; flips to the opposite corner on screen overflow.
    Cursor {
        corner: Corner,
        row_offset: i32,
        col_offset: i32,
    },
    /// Anchored to another window; `attach` corner aligns to the target's edge.
    Win {
        target: PaintId,
        attach: Corner,
        row_offset: i32,
        col_offset: i32,
    },
    /// Docked to the bottom of the screen; height clamps to `term_h - above_rows`.
    ScreenBottom { above_rows: u16 },
}

/// Glyph family painted along a border edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    Single,
    Double,
    Rounded,
}

/// Styling for one edge of a `Border`. Currently only `color` (a theme highlight
/// group resolved at paint time). `EdgeStyle::default()` paints with the
/// terminal's default fg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeStyle {
    pub color: Option<smelt_style::theme::HlGroup>,
}

impl EdgeStyle {
    pub const fn new() -> Self {
        Self { color: None }
    }
    pub const fn with_color(hl: smelt_style::theme::HlGroup) -> Self {
        Self { color: Some(hl) }
    }
}

impl From<()> for EdgeStyle {
    fn from(_: ()) -> Self {
        Self::new()
    }
}

impl From<smelt_style::theme::HlGroup> for EdgeStyle {
    fn from(hl: smelt_style::theme::HlGroup) -> Self {
        Self::with_color(hl)
    }
}

/// A frame around a container: glyph family plus per-side `Option<EdgeStyle>`.
/// A side that is `None` is not drawn and reserves no row/column. A side that is
/// `Some(_)` reserves one row/column and is painted in the resolved fg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Border {
    pub style: BorderStyle,
    pub top: Option<EdgeStyle>,
    pub right: Option<EdgeStyle>,
    pub bottom: Option<EdgeStyle>,
    pub left: Option<EdgeStyle>,
}

impl Border {
    /// All sides disabled; glyph family `Single`. Use as a base for builders.
    pub const OFF: Self = Self {
        style: BorderStyle::Single,
        top: None,
        right: None,
        bottom: None,
        left: None,
    };

    pub const fn single() -> Self {
        Self {
            style: BorderStyle::Single,
            ..Self::OFF
        }
    }
    pub const fn rounded() -> Self {
        Self {
            style: BorderStyle::Rounded,
            ..Self::OFF
        }
    }
    pub const fn double() -> Self {
        Self {
            style: BorderStyle::Double,
            ..Self::OFF
        }
    }

    pub fn top(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.top = Some(e.into());
        self
    }
    pub fn right(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.right = Some(e.into());
        self
    }
    pub fn bottom(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.bottom = Some(e.into());
        self
    }
    pub fn left(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.left = Some(e.into());
        self
    }
    /// Enable every side with `e`. Copy bound lets callers pass `()` or a `HlGroup`.
    pub fn all<E: Into<EdgeStyle> + Copy>(self, e: E) -> Self {
        self.top(e).right(e).bottom(e).left(e)
    }

    pub fn any_side(&self) -> bool {
        self.top.is_some() || self.right.is_some() || self.bottom.is_some() || self.left.is_some()
    }

    /// `Border::single().all(())` — single glyphs on all four sides, default color.
    pub fn single_all() -> Self {
        Self::single().all(())
    }
    pub fn rounded_all() -> Self {
        Self::rounded().all(())
    }
    pub fn double_all() -> Self {
        Self::double().all(())
    }

    /// Compatibility shortcuts for the three most common presets.
    pub const SINGLE: Border = Border {
        style: BorderStyle::Single,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
    pub const DOUBLE: Border = Border {
        style: BorderStyle::Double,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
    pub const ROUNDED: Border = Border {
        style: BorderStyle::Rounded,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Gutters {
    pub pad_left: u16,
    pub pad_right: u16,
    pub scrollbar: bool,
}

impl Gutters {
    pub fn scrollbar_width(&self) -> u16 {
        if self.scrollbar {
            1
        } else {
            0
        }
    }

    /// Width inside the left gutter (still includes the scrollbar column if any).
    pub fn layer_width(&self, total: u16) -> u16 {
        total.saturating_sub(self.pad_left)
    }

    /// Inner content width once `pad_left`, `pad_right`, and the scrollbar column are subtracted.
    pub fn content_width(&self, total: u16) -> u16 {
        self.layer_width(total)
            .saturating_sub(self.pad_right)
            .saturating_sub(self.scrollbar_width())
    }
}

/// Resolve the tree against `area` and return the rect of every leaf.
pub fn resolve_layout(tree: &LayoutTree, area: Rect) -> HashMap<PaintId, Rect> {
    let mut result = HashMap::new();
    resolve_node(tree, area, &mut result);
    result
}

/// Inner area after subtracting the border's per-side reservations.
/// Returns `area` unchanged when `border` is `None`.
pub fn inset_for_border(area: Rect, border: Option<Border>) -> Rect {
    let Some(b) = border else {
        return area;
    };
    let top_pad = if b.top.is_some() { 1 } else { 0 };
    let bot_pad = if b.bottom.is_some() { 1 } else { 0 };
    let left_pad = if b.left.is_some() { 1 } else { 0 };
    let right_pad = if b.right.is_some() { 1 } else { 0 };
    let h = area.height.saturating_sub(top_pad).saturating_sub(bot_pad);
    let w = area
        .width
        .saturating_sub(left_pad)
        .saturating_sub(right_pad);
    Rect::new(area.top + top_pad, area.left + left_pad, w, h)
}

/// Paint a container's border and title into `grid` at `area`.
/// Corners are drawn only when both adjacent edges are enabled. When two
/// adjacent edges disagree on color, the top/bottom edge wins.
/// Title requires `border.top.is_some()`.
pub fn paint_chrome(
    grid: &mut crate::grid::Grid,
    area: Rect,
    chrome: &Chrome,
    theme: &crate::Theme,
) {
    let Some(border) = chrome.border else {
        return;
    };
    if !border.any_side() {
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (h, v, tl, tr, bl, br) = match border.style {
        BorderStyle::Single => ('─', '│', '┌', '┐', '└', '┘'),
        BorderStyle::Double => ('═', '║', '╔', '╗', '╚', '╝'),
        BorderStyle::Rounded => ('─', '│', '╭', '╮', '╰', '╯'),
    };
    let edge_style = |e: Option<EdgeStyle>| -> super::grid::Style {
        match e.and_then(|s| s.color) {
            Some(hl) => theme.resolve(hl),
            None => super::grid::Style::default(),
        }
    };
    let top_style = edge_style(border.top);
    let bot_style = edge_style(border.bottom);
    let left_style = edge_style(border.left);
    let right_style = edge_style(border.right);
    let right = area.left + area.width - 1;
    let bottom = area.top + area.height - 1;

    if border.top.is_some() {
        for col in area.left..=right {
            grid.set(col, area.top, h, top_style);
        }
    }
    if border.bottom.is_some() && bottom != area.top {
        for col in area.left..=right {
            grid.set(col, bottom, h, bot_style);
        }
    }
    if border.left.is_some() {
        for row in area.top..=bottom {
            grid.set(area.left, row, v, left_style);
        }
    }
    if border.right.is_some() && right != area.left {
        for row in area.top..=bottom {
            grid.set(right, row, v, right_style);
        }
    }
    // Corners only when both adjacent edges are present. Top/bottom wins on color.
    if border.top.is_some() && border.left.is_some() {
        grid.set(area.left, area.top, tl, top_style);
    }
    if border.top.is_some() && border.right.is_some() && right != area.left {
        grid.set(right, area.top, tr, top_style);
    }
    if border.bottom.is_some() && border.left.is_some() && bottom != area.top {
        grid.set(area.left, bottom, bl, bot_style);
    }
    if border.bottom.is_some() && border.right.is_some() && bottom != area.top && right != area.left
    {
        grid.set(right, bottom, br, bot_style);
    }

    if border.top.is_some() {
        if let Some(title) = chrome.title.as_ref() {
            // Inset title by one cell from each end so it reads as `─title──`
            // regardless of whether the left/right sides are enabled.
            let title_left = area.left + 1;
            let title_right_excl = right;
            if title_right_excl > title_left {
                let limit = title_right_excl;
                let mut col = title_left;
                for span in &title.spans {
                    if col >= limit {
                        break;
                    }
                    let span_style = merge_title_span_style(top_style, span.style);
                    let mut written = false;
                    for ch in span.text.chars() {
                        use unicode_width::UnicodeWidthChar;
                        let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
                        if col + cw > limit {
                            break;
                        }
                        grid.set(col, area.top, ch, span_style);
                        col += cw;
                        written = true;
                    }
                    if !written {
                        break;
                    }
                }
            }
        }
    }
}

/// Merge a title span's style over the chrome style: span fg/bg override when set; attrs OR.
fn merge_title_span_style(
    base: crate::grid::Style,
    span: crate::grid::Style,
) -> crate::grid::Style {
    crate::grid::Style {
        fg: span.fg.or(base.fg),
        bg: span.bg.or(base.bg),
        bold: base.bold || span.bold,
        dim: base.dim || span.dim,
        italic: base.italic || span.italic,
        underline: base.underline || span.underline,
        crossedout: base.crossedout || span.crossedout,
    }
}

fn resolve_node(node: &LayoutTree, area: Rect, out: &mut HashMap<PaintId, Rect>) {
    match node {
        LayoutTree::Leaf(id) => {
            out.insert(*id, area);
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
    out: &mut HashMap<PaintId, Rect>,
) {
    let inner = inset_for_border(area, chrome.border);
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

pub fn resolve_constraints(items: &[Item], total: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; items.len()];
    let mut remaining = total;

    // Pass 1: hard-sized constraints (`Length`, `Max`, `Percentage`) consume their share first.
    // `Min(n)` is not hard-sized — it competes with `Fill` in pass 3.
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
            _ => {}
        }
    }

    // Pass 2: `Ratio` siblings split the remaining pool proportionally.
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

    // Pass 3: `Fill`, `Fit`, and `Min` share the remainder equally.
    // `Min(n)` clamps its share up to `n`; if floors push the total over
    // budget, surplus is taken from non-Min children first, then Min
    // children proportionally.
    let flex_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (c, _))| {
            matches!(c, Constraint::Fill | Constraint::Fit | Constraint::Min(_)).then_some(i)
        })
        .collect();
    let flex_count = flex_indices.len() as u16;
    if flex_count == 0 || remaining == 0 {
        return sizes;
    }

    let per = remaining / flex_count;
    let mut leftover = remaining % flex_count;
    let mut shares = vec![0u16; flex_indices.len()];
    for share in shares.iter_mut() {
        *share = per + u16::from(leftover > 0);
        leftover = leftover.saturating_sub(1);
    }
    for (k, &i) in flex_indices.iter().enumerate() {
        if let Constraint::Min(n) = items[i].0 {
            if shares[k] < n {
                shares[k] = n;
            }
        }
    }

    let total_shares: u32 = shares.iter().map(|&v| v as u32).sum();
    if total_shares > remaining as u32 {
        let mut surplus = (total_shares - remaining as u32) as u16;
        for (k, &i) in flex_indices.iter().enumerate() {
            if surplus == 0 {
                break;
            }
            if !matches!(items[i].0, Constraint::Min(_)) {
                let take = shares[k].min(surplus);
                shares[k] -= take;
                surplus -= take;
            }
        }
        if surplus > 0 {
            let min_total: u32 = flex_indices
                .iter()
                .enumerate()
                .filter(|(_, &i)| matches!(items[i].0, Constraint::Min(_)))
                .map(|(k, _)| shares[k] as u32)
                .sum();
            if let Some(divisor) = (min_total > 0).then_some(min_total) {
                for (k, &i) in flex_indices.iter().enumerate() {
                    if matches!(items[i].0, Constraint::Min(_)) {
                        let take = ((shares[k] as u32 * surplus as u32) / divisor) as u16;
                        shares[k] = shares[k].saturating_sub(take);
                    }
                }
                let new_total: u32 = shares.iter().map(|&v| v as u32).sum();
                let mut residual = new_total.saturating_sub(remaining as u32) as u16;
                for (k, &i) in flex_indices.iter().enumerate() {
                    if residual == 0 {
                        break;
                    }
                    if matches!(items[i].0, Constraint::Min(_)) {
                        let take = shares[k].min(residual);
                        shares[k] -= take;
                        residual -= take;
                    }
                }
            }
        }
    }

    for (k, &i) in flex_indices.iter().enumerate() {
        sizes[i] = shares[k];
    }

    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaintId = PaintId(100);
    const B: PaintId = PaintId(101);
    const C: PaintId = PaintId(102);

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
    fn min_competes_with_fill_for_equal_share_when_floor_satisfied() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 12);
        assert_eq!(result[&B].height, 12);
    }

    #[test]
    fn min_clamps_up_to_floor_when_equal_share_too_small() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(20), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 20);
        assert_eq!(result[&B].height, 4);
    }

    #[test]
    fn min_zero_alone_consumes_all_remaining() {
        let tree = LayoutTree::vbox(vec![(Constraint::Min(0), LayoutTree::leaf(A))]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 24);
    }

    #[test]
    fn min_with_length_sibling_takes_remainder() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(10), LayoutTree::leaf(A)),
            (Constraint::Min(0), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 10);
        assert_eq!(result[&B].height, 14);
    }

    #[test]
    fn two_mins_split_evenly_when_total_overruns_floors() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(20), LayoutTree::leaf(A)),
            (Constraint::Min(20), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height + result[&B].height, 24);
        assert!((result[&A].height as i32 - result[&B].height as i32).abs() <= 1);
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
            .with_border(Border::SINGLE);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(1, 1, 78, 22));
    }

    #[test]
    fn border_and_gap_compose() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ])
        .with_border(Border::SINGLE)
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
        assert_eq!(tree.natural_size((80, 24)), (0, 16));
    }

    #[test]
    fn natural_size_border_adds_two_each_axis() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(10), LayoutTree::leaf(A))])
            .with_border(Border::SINGLE);
        assert_eq!(tree.natural_size((80, 24)), (2, 12));
    }

    #[test]
    fn natural_size_percentage_resolves_against_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Percentage(50), LayoutTree::leaf(A))]);
        assert_eq!(tree.natural_size((80, 24)), (0, 12));
    }

    #[test]
    fn natural_size_ratio_resolves_against_cap() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Ratio(1, 4), LayoutTree::leaf(A)),
            (Constraint::Ratio(1, 4), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (40, 0));
    }

    #[test]
    fn natural_size_fill_contributes_zero() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (0, 3));
    }

    #[test]
    fn natural_size_clamps_to_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(100), LayoutTree::leaf(A))]);
        assert_eq!(tree.natural_size((80, 24)), (0, 24));
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
    fn leaf_with_border_auto_wraps_and_keeps_id_resolvable() {
        let tree = LayoutTree::leaf(A)
            .with_border(Border::SINGLE)
            .with_title("hi");
        assert_eq!(tree.leaves_in_order(), vec![A]);
        assert!(tree.contains_leaf(A));
        match &tree {
            LayoutTree::Vbox { chrome, .. } => {
                assert!(chrome.border.is_some());
                assert!(chrome.title.is_some());
            }
            _ => panic!("expected Vbox wrapper"),
        }
    }

    #[test]
    fn leaf_with_chrome_resolves_inside_inset_rect() {
        let tree = LayoutTree::leaf(A).with_border(Border::SINGLE);
        let area = Rect::new(0, 0, 10, 6);
        let rects = resolve_layout(&tree, area);
        let inner = rects.get(&A).copied().expect("leaf rect resolved");
        assert_eq!(inner, Rect::new(1, 1, 8, 4));
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
        let tree = LayoutTree::vbox(vec![(
            Constraint::Length(5),
            LayoutTree::hbox(vec![
                (Constraint::Length(20), LayoutTree::leaf(A)),
                (Constraint::Length(10), LayoutTree::leaf(B)),
            ]),
        )])
        .with_border(Border::SINGLE);
        assert_eq!(tree.natural_size((80, 24)), (32, 7));
    }

    #[test]
    fn paint_chrome_no_border_is_noop() {
        let mut grid = crate::grid::Grid::new(10, 5);
        let chrome = Chrome::default();
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }

    #[test]
    fn paint_chrome_single_border_draws_corners_and_edges() {
        let mut grid = crate::grid::Grid::new(10, 5);
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(9, 0).symbol, '┐');
        assert_eq!(grid.cell(0, 4).symbol, '└');
        assert_eq!(grid.cell(9, 4).symbol, '┘');
        assert_eq!(grid.cell(5, 0).symbol, '─');
        assert_eq!(grid.cell(0, 2).symbol, '│');
    }

    #[test]
    fn paint_chrome_title_paints_styled_spans() {
        use crate::grid::Color;
        use crate::line::{Line, Span};
        let mut grid = crate::grid::Grid::new(20, 3);
        let red = crate::grid::Style::new().fg(Color::Red);
        let chrome = Chrome {
            border: Some(Border::ROUNDED),
            title: Some(Line::from_spans([
                Span::raw("ok "),
                Span::styled("FAIL", red),
            ])),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 20, 3),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(1, 0).symbol, 'o');
        assert_eq!(grid.cell(1, 0).style.fg, None);
        assert_eq!(grid.cell(4, 0).symbol, 'F');
        assert_eq!(grid.cell(4, 0).style.fg, Some(Color::Red));
    }

    #[test]
    fn paint_chrome_title_lands_on_top_border() {
        let mut grid = crate::grid::Grid::new(20, 5);
        let chrome = Chrome {
            border: Some(Border::ROUNDED),
            title: Some("hello".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 20, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '╭');
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(5, 0).symbol, 'o');
        assert_eq!(grid.cell(6, 0).symbol, '─');
    }

    #[test]
    fn paint_chrome_truncates_title_to_inner_width() {
        let mut grid = crate::grid::Grid::new(8, 3);
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            title: Some("muchtoolong".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 8, 3),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(1, 0).symbol, 'm');
        assert_eq!(grid.cell(6, 0).symbol, 'o');
        assert_eq!(grid.cell(7, 0).symbol, '┐');
    }
}
