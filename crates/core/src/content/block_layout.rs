//! Composable layout tree returned by a tool's `render` callback.
//! Leaves are `Buffer` ids replayed by the transcript composer into the surrounding `LineBuilder`.

use crate::buffer::BufId;

#[derive(Clone, Copy, Debug)]
pub enum Constraint {
    /// Fixed width in display columns.
    Length(u16),
    /// Fill the remaining width proportionally to the weight.
    Fill(u16),
}

#[derive(Clone, Debug)]
pub struct HboxItem {
    pub constraint: Constraint,
    pub layout: BlockLayout,
}

#[derive(Clone, Debug)]
pub enum BlockLayout {
    Leaf(BufId),
    Vbox(Vec<BlockLayout>),
    Hbox(Vec<HboxItem>),
}

impl BlockLayout {
    /// Buffer ids in depth-first order; the composer walks leaves in this order.
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

/// Allocate column widths: `Length` constraints first, remaining width split among `Fill` weights.
/// Rounding excess goes to the last fill column.
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
