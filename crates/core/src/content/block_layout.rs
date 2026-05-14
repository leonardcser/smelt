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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(constraint: Constraint) -> HboxItem {
        HboxItem {
            constraint,
            layout: BlockLayout::Leaf(BufId(0)),
        }
    }

    #[test]
    fn leaves_returns_single_buf_id_for_leaf() {
        let layout = BlockLayout::Leaf(BufId(7));
        assert_eq!(layout.leaves(), vec![BufId(7)]);
    }

    #[test]
    fn leaves_visits_vbox_children_in_order() {
        let layout = BlockLayout::Vbox(vec![
            BlockLayout::Leaf(BufId(1)),
            BlockLayout::Leaf(BufId(2)),
            BlockLayout::Leaf(BufId(3)),
        ]);
        assert_eq!(layout.leaves(), vec![BufId(1), BufId(2), BufId(3)]);
    }

    #[test]
    fn leaves_visits_hbox_children_in_order() {
        let layout = BlockLayout::Hbox(vec![
            HboxItem {
                constraint: Constraint::Length(10),
                layout: BlockLayout::Leaf(BufId(5)),
            },
            HboxItem {
                constraint: Constraint::Fill(1),
                layout: BlockLayout::Leaf(BufId(6)),
            },
        ]);
        assert_eq!(layout.leaves(), vec![BufId(5), BufId(6)]);
    }

    #[test]
    fn leaves_walks_nested_layout_depth_first() {
        let layout = BlockLayout::Vbox(vec![
            BlockLayout::Leaf(BufId(1)),
            BlockLayout::Hbox(vec![
                HboxItem {
                    constraint: Constraint::Length(5),
                    layout: BlockLayout::Leaf(BufId(2)),
                },
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: BlockLayout::Vbox(vec![
                        BlockLayout::Leaf(BufId(3)),
                        BlockLayout::Leaf(BufId(4)),
                    ]),
                },
            ]),
            BlockLayout::Leaf(BufId(5)),
        ]);
        assert_eq!(
            layout.leaves(),
            vec![BufId(1), BufId(2), BufId(3), BufId(4), BufId(5)]
        );
    }

    #[test]
    fn solve_hbox_widths_lengths_consume_fixed_columns() {
        let items = vec![item(Constraint::Length(10)), item(Constraint::Length(5))];
        let widths = solve_hbox_widths(&items, 30);
        assert_eq!(widths, vec![10, 5]);
    }

    #[test]
    fn solve_hbox_widths_clamps_length_to_remaining_total() {
        // First column is requested 20 but total is only 15.
        let items = vec![item(Constraint::Length(20))];
        let widths = solve_hbox_widths(&items, 15);
        assert_eq!(widths, vec![15]);
    }

    #[test]
    fn solve_hbox_widths_distributes_fills_by_weight() {
        let items = vec![item(Constraint::Fill(1)), item(Constraint::Fill(3))];
        let widths = solve_hbox_widths(&items, 40);
        // 40 * 1/4 = 10, 40 * 3/4 = 30
        assert_eq!(widths, vec![10, 30]);
    }

    #[test]
    fn solve_hbox_widths_routes_rounding_excess_to_last_fill() {
        // 100 * 1/3 = 33 each; total 99, leftover 1 -> last col gets +1.
        let items = vec![
            item(Constraint::Fill(1)),
            item(Constraint::Fill(1)),
            item(Constraint::Fill(1)),
        ];
        let widths = solve_hbox_widths(&items, 100);
        assert_eq!(widths.iter().sum::<u16>(), 100);
        assert_eq!(widths[0], 33);
        assert_eq!(widths[1], 33);
        assert_eq!(widths[2], 34);
    }

    #[test]
    fn solve_hbox_widths_mix_length_and_fill_uses_remaining_for_fill() {
        let items = vec![
            item(Constraint::Length(10)),
            item(Constraint::Fill(1)),
            item(Constraint::Fill(1)),
        ];
        let widths = solve_hbox_widths(&items, 50);
        assert_eq!(widths[0], 10);
        assert_eq!(widths[1] + widths[2], 40);
    }

    #[test]
    fn solve_hbox_widths_returns_zero_for_fill_columns_when_total_consumed() {
        let items = vec![item(Constraint::Length(20)), item(Constraint::Fill(1))];
        let widths = solve_hbox_widths(&items, 20);
        assert_eq!(widths, vec![20, 0]);
    }

    #[test]
    fn solve_hbox_widths_empty_input_returns_empty_vec() {
        assert!(solve_hbox_widths(&[], 80).is_empty());
    }
}
