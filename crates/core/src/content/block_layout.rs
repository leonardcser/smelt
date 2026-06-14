//! Composable width-independent layout trees returned by Lua display callbacks.
//!
//! Lua builds declarative `BlockLayout` trees. The TUI compiles transient leaves
//! such as source diffs into serializable `LayoutIr` before caching/rendering.
//! Width and theme are intentionally absent from these types.

use crate::buffer::BufId;
use crate::content::highlight::DiffIr;
use serde::{Deserialize, Serialize};

pub const DEFAULT_TOOL_BLOCK_ROWS: u16 = 20;

/// Inline-diff render directive. The worker calls `print_inline_diff` directly,
/// so width / indent / bg-fill / wrap math all live in one render path with no
/// scratch-buffer seam.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffSpec {
    pub old: String,
    pub new: String,
    pub path: String,
    pub anchor: String,
    pub lang: Option<String>,
}

/// File-view render directive (all-Context diff IR). Used by `write_file` and
/// notebook insert mode - same renderer as `Diff`, single line-number column.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileViewSpec {
    pub content: String,
    pub path: String,
    pub lang: Option<String>,
}

/// Plain text layout leaf. Wrapping is computed at measurement/render time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSpec {
    pub content: String,
    pub hl_group: Option<String>,
    pub ansi: bool,
}

/// Serializable source-view render directive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SourceViewIr {
    Diff(DiffIr),
}

/// Width-independent leaf stored in display IR.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LayoutLeaf {
    Text(TextSpec),
    SourceView(SourceViewIr),
}

/// A leaf parameterised on the buffer payload `B`. With `B = BufId` this is the
/// raw Lua-returned shape; compiled layout IR uses `LayoutLeaf` instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Leaf<B> {
    Buf(B),
    Text(TextSpec),
    Diff(DiffSpec),
    FileView(FileViewSpec),
    SourceView(SourceViewIr),
}

pub type LuaLeaf = Leaf<BufId>;
pub type IrLeaf = LayoutLeaf;
pub type LayoutIr = BlockLayout<LayoutLeaf>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolBody {
    Layout(LayoutIr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapKeep {
    Head,
    Tail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapMarker {
    Above,
    Below,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapSpec {
    pub rows: u16,
    pub keep: CapKeep,
    pub marker: Option<CapMarker>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GutterSpec {
    pub text: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Constraint {
    /// Fixed width in display columns.
    Length(u16),
    /// Fill the remaining width proportionally to the weight.
    Fill(u16),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HboxItem<L = LuaLeaf> {
    pub constraint: Constraint,
    pub layout: BlockLayout<L>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BlockLayout<L = LuaLeaf> {
    Empty,
    Leaf(L),
    Vbox(Vec<BlockLayout<L>>),
    Hbox(Vec<HboxItem<L>>),
    Gutter {
        child: Box<BlockLayout<L>>,
        spec: GutterSpec,
    },
    Cap {
        child: Box<BlockLayout<L>>,
        spec: CapSpec,
    },
}

pub type LayoutNode<L = LuaLeaf> = BlockLayout<L>;

impl<L> BlockLayout<L> {
    /// Leaf payloads in depth-first order; the composer walks leaves in this order.
    pub fn leaves(&self) -> Vec<&L> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a L>) {
        match self {
            BlockLayout::Empty => {}
            BlockLayout::Leaf(l) => out.push(l),
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
            BlockLayout::Gutter { child, .. } | BlockLayout::Cap { child, .. } => {
                child.collect_leaves(out);
            }
        }
    }
}

/// Allocate column widths from raw constraints (the leaf type is irrelevant). Returns
/// one width per constraint, summing to at most `total`. `Length` is consumed first,
/// remaining width is split among `Fill` columns by weight; rounding excess lands on
/// the last fill column.
pub fn solve_hbox_widths_from_constraints(constraints: &[Constraint], total: u16) -> Vec<u16> {
    let mut widths = vec![0u16; constraints.len()];
    let mut used: u16 = 0;
    let mut total_fill: u32 = 0;
    let mut last_fill: Option<usize> = None;
    for (i, c) in constraints.iter().enumerate() {
        match *c {
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
        for (i, c) in constraints.iter().enumerate() {
            if let Constraint::Fill(w) = *c {
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

/// Convenience wrapper: extract constraints and dispatch to `_from_constraints`.
pub fn solve_hbox_widths<L>(items: &[HboxItem<L>], total: u16) -> Vec<u16> {
    let constraints: Vec<Constraint> = items.iter().map(|i| i.constraint).collect();
    solve_hbox_widths_from_constraints(&constraints, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(constraint: Constraint) -> HboxItem {
        HboxItem {
            constraint,
            layout: BlockLayout::Leaf(LuaLeaf::Buf(BufId(0))),
        }
    }

    fn leaf(n: u64) -> BlockLayout {
        BlockLayout::Leaf(LuaLeaf::Buf(BufId(n)))
    }

    fn extract_buf(l: &LuaLeaf) -> BufId {
        match l {
            LuaLeaf::Buf(id) => *id,
            _ => panic!("expected Buf"),
        }
    }

    #[test]
    fn leaves_returns_single_buf_id_for_leaf() {
        let layout = leaf(7);
        let leaves = layout.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(extract_buf(leaves[0]), BufId(7));
    }

    #[test]
    fn leaves_visits_vbox_children_in_order() {
        let layout = BlockLayout::Vbox(vec![leaf(1), leaf(2), leaf(3)]);
        let ids: Vec<BufId> = layout.leaves().iter().map(|l| extract_buf(l)).collect();
        assert_eq!(ids, vec![BufId(1), BufId(2), BufId(3)]);
    }

    #[test]
    fn leaves_visits_hbox_children_in_order() {
        let layout = BlockLayout::Hbox(vec![
            HboxItem {
                constraint: Constraint::Length(10),
                layout: leaf(5),
            },
            HboxItem {
                constraint: Constraint::Fill(1),
                layout: leaf(6),
            },
        ]);
        let ids: Vec<BufId> = layout.leaves().iter().map(|l| extract_buf(l)).collect();
        assert_eq!(ids, vec![BufId(5), BufId(6)]);
    }

    #[test]
    fn leaves_walks_nested_layout_depth_first() {
        let layout = BlockLayout::Vbox(vec![
            leaf(1),
            BlockLayout::Hbox(vec![
                HboxItem {
                    constraint: Constraint::Length(5),
                    layout: leaf(2),
                },
                HboxItem {
                    constraint: Constraint::Fill(1),
                    layout: BlockLayout::Vbox(vec![leaf(3), leaf(4)]),
                },
            ]),
            leaf(5),
        ]);
        let ids: Vec<BufId> = layout.leaves().iter().map(|l| extract_buf(l)).collect();
        assert_eq!(ids, vec![BufId(1), BufId(2), BufId(3), BufId(4), BufId(5)]);
    }

    #[test]
    fn solve_hbox_widths_lengths_consume_fixed_columns() {
        let items = vec![item(Constraint::Length(10)), item(Constraint::Length(5))];
        let widths = solve_hbox_widths(&items, 30);
        assert_eq!(widths, vec![10, 5]);
    }

    #[test]
    fn solve_hbox_widths_clamps_length_to_remaining_total() {
        let items = vec![item(Constraint::Length(20))];
        let widths = solve_hbox_widths(&items, 15);
        assert_eq!(widths, vec![15]);
    }

    #[test]
    fn solve_hbox_widths_distributes_fills_by_weight() {
        let items = vec![item(Constraint::Fill(1)), item(Constraint::Fill(3))];
        let widths = solve_hbox_widths(&items, 40);
        assert_eq!(widths, vec![10, 30]);
    }

    #[test]
    fn solve_hbox_widths_routes_rounding_excess_to_last_fill() {
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
        assert!(solve_hbox_widths::<LuaLeaf>(&[], 80).is_empty());
    }
}
