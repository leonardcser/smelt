//! Composable layout shape returned by a tool's `render` callback.
//!
//! Tools paint into one or more `Buffer`s and assemble them into a
//! `BlockLayout` tree. The transcript composer walks the tree and
//! replays leaves into the surrounding `LineBuilder`. Tools that don't
//! register a `render` callback fall back to the default body
//! formatting.
//!
//! The shape is intentionally minimal — a tool block is a sequence of
//! buffer leaves stacked vertically (`Vbox`) or laid out side-by-side
//! (`Hbox`), sometimes nested.

use crate::buffer::BufId;

/// Per-child sizing for [`BlockLayout::Hbox`] columns.
#[derive(Clone, Copy, Debug)]
pub enum Constraint {
    /// Fixed width in display columns.
    Length(u16),
    /// Fill the remaining width proportionally to the weight.
    Fill(u16),
}

/// One column inside an [`BlockLayout::Hbox`].
#[derive(Clone, Debug)]
pub struct HboxItem {
    pub constraint: Constraint,
    pub layout: BlockLayout,
}

/// One node in a tool's block layout tree.
#[derive(Clone, Debug)]
pub enum BlockLayout {
    /// Replay the contents of `buf` into the parent `LineBuilder`.
    Leaf(BufId),
    /// Stack children top-to-bottom.
    Vbox(Vec<BlockLayout>),
    /// Lay children out side-by-side.
    Hbox(Vec<HboxItem>),
}

impl BlockLayout {
    /// Buffer ids referenced by this tree, in depth-first declaration
    /// order. The composer walks leaves in this order.
    pub fn leaves(&self) -> Vec<BufId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<BufId>) {
        match self {
            BlockLayout::Leaf(id) => out.push(*id),
            BlockLayout::Vbox(items) => {
                for child in items {
                    child.collect_leaves(out);
                }
            }
            BlockLayout::Hbox(items) => {
                for item in items {
                    item.layout.collect_leaves(out);
                }
            }
        }
    }
}

/// Allocate column widths for an `Hbox` given total available width.
/// `Length` constraints satisfy first (clamped to remaining); the rest
/// is split among `Fill` weights proportionally. Excess from rounding
/// goes to the last fill column.
pub fn solve_hbox_widths(items: &[HboxItem], total: u16) -> Vec<u16> {
    let mut widths = vec![0u16; items.len()];
    let mut used: u16 = 0;
    let mut total_fill: u32 = 0;
    let mut last_fill: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        match item.constraint {
            Constraint::Length(n) => {
                let take = n.min(total.saturating_sub(used));
                widths[i] = take;
                used = used.saturating_add(take);
            }
            Constraint::Fill(w) => {
                total_fill += w as u32;
                last_fill = Some(i);
            }
        }
    }
    let remaining = total.saturating_sub(used) as u32;
    if total_fill > 0 && remaining > 0 {
        let mut allocated: u32 = 0;
        for (i, item) in items.iter().enumerate() {
            if let Constraint::Fill(w) = item.constraint {
                let share = remaining * (w as u32) / total_fill;
                widths[i] = share as u16;
                allocated += share;
            }
        }
        if let Some(last) = last_fill {
            let leftover = remaining.saturating_sub(allocated) as u16;
            widths[last] = widths[last].saturating_add(leftover);
        }
    }
    widths
}
