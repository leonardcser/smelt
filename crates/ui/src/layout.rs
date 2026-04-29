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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Vertical,
    Horizontal,
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

/// Upper bound on `Placement::FitContent`. Keeps pathologically tall
/// content (long permission lists, agent logs) from eating the whole
/// screen while still giving short content a snug fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitMax {
    /// Cap at half the terminal height.
    HalfScreen,
    /// Cap at (nearly) the full terminal height — keeps `above_rows`
    /// reserved so the status bar / prompt remains visible.
    FullScreen,
}

/// High-level dialog / float placement inside the terminal. Chosen over
/// raw anchor+row+col to keep per-dialog call sites free of rect math.
#[derive(Clone, Copy, Debug)]
pub enum Placement {
    /// Docked at the terminal bottom. Reserves `above_rows` for surfaces
    /// that must stay visible (status bar). `full_width = true` spans
    /// the full terminal width and ignores `max_width`.
    DockBottom {
        above_rows: u16,
        full_width: bool,
        max_width: Constraint,
        max_height: Constraint,
    },
    /// Like `DockBottom` but height tracks the dialog's content: the
    /// renderer asks the dialog for its natural height (sum of panel
    /// `line_count`s + chrome) and clamps to `max`. Short dialogs
    /// shrink, long dialogs cap and scroll internally. Always
    /// full-width; reserves 1 row above for the status bar.
    FitContent { max: FitMax },
    /// Centered in the terminal.
    Centered {
        width: Constraint,
        height: Constraint,
    },
    /// Positioned relative to the caret (completer / hover).
    /// `row_offset` is added to the anchor row, `col_offset` to the
    /// anchor column. If the dialog would overflow, the framework
    /// flips to render above/left of the anchor.
    AnchorCursor {
        row_offset: i32,
        col_offset: i32,
        width: Constraint,
        height: Constraint,
    },
    /// Escape hatch for absolute positioning. Prefer one of the above
    /// when possible.
    Manual {
        anchor: Anchor,
        row: i32,
        col: i32,
        width: Constraint,
        height: Constraint,
    },
    /// Docked directly above another window. Width matches the target's
    /// rect; height tracks the float's natural height (picker item count,
    /// etc.) clamped by `max_height`. The float grows upward from the
    /// target's top edge — the canonical placement for prompt-anchored
    /// pickers (completers, `/theme`, history search).
    DockedAbove {
        target: crate::WinId,
        max_height: Constraint,
    },
}

impl Placement {
    pub fn dock_bottom_full_width(max_height: Constraint) -> Self {
        Placement::DockBottom {
            above_rows: 1,
            full_width: true,
            max_width: Constraint::Fill,
            max_height,
        }
    }

    pub fn centered(width: Constraint, height: Constraint) -> Self {
        Placement::Centered { width, height }
    }

    pub fn fit_content(max: FitMax) -> Self {
        Placement::FitContent { max }
    }

    pub fn docked_above(target: crate::WinId, max_height: Constraint) -> Self {
        Placement::DockedAbove { target, max_height }
    }
}

#[derive(Clone, Debug)]
pub enum LayoutTree {
    Leaf {
        name: String,
        constraint: Constraint,
    },
    Split {
        direction: Direction,
        children: Vec<LayoutTree>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    NW,
    NE,
    SW,
    SE,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatRelative {
    Editor,
    Cursor,
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

pub fn resolve_layout(tree: &LayoutTree, area: Rect) -> HashMap<String, Rect> {
    let mut result = HashMap::new();
    resolve_node(tree, area, &mut result);
    result
}

fn resolve_node(node: &LayoutTree, area: Rect, out: &mut HashMap<String, Rect>) {
    match node {
        LayoutTree::Leaf { name, .. } => {
            out.insert(name.clone(), area);
        }
        LayoutTree::Split {
            direction,
            children,
        } => {
            let total = match direction {
                Direction::Vertical => area.height,
                Direction::Horizontal => area.width,
            };
            let sizes = resolve_constraints(children, total);
            let mut offset = 0u16;
            for (child, &size) in children.iter().zip(sizes.iter()) {
                let child_area = match direction {
                    Direction::Vertical => {
                        Rect::new(area.top + offset, area.left, area.width, size)
                    }
                    Direction::Horizontal => {
                        Rect::new(area.top, area.left + offset, size, area.height)
                    }
                };
                resolve_node(child, child_area, out);
                offset += size;
            }
        }
    }
}

fn resolve_constraints(children: &[LayoutTree], total: u16) -> Vec<u16> {
    let mut sizes = vec![0u16; children.len()];
    let mut remaining = total;

    // Pass 1: hard-sized constraints consume their share.
    for (i, child) in children.iter().enumerate() {
        match constraint_of(child) {
            Constraint::Length(n) | Constraint::Max(n) => {
                let n = n.min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Percentage(pct) => {
                let n = ((total as u32 * pct as u32) / 100).min(remaining as u32) as u16;
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Min(n) => {
                let n = n.min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            _ => {}
        }
    }

    // Pass 2: `Ratio` splits its slice of the remaining proportionally
    // to its siblings' (num, denom) pairs.
    let ratio_total: u32 = children
        .iter()
        .filter_map(|c| match constraint_of(c) {
            Constraint::Ratio(num, _) => Some(num as u32),
            _ => None,
        })
        .sum();
    let ratio_pool = remaining;
    let mut consumed = 0u16;
    for (i, child) in children.iter().enumerate() {
        if let Constraint::Ratio(num, _) = constraint_of(child) {
            let n = (ratio_pool as u32 * num as u32)
                .checked_div(ratio_total)
                .unwrap_or(0) as u16;
            sizes[i] = n;
            consumed += n;
        }
    }
    remaining -= consumed.min(remaining);

    // Pass 3: `Fill` and `Fit` split the remainder evenly. (`Fit`
    // behaves like `Fill` until B.3 wires content size.)
    let fill_count = children
        .iter()
        .filter(|c| matches!(constraint_of(c), Constraint::Fill | Constraint::Fit))
        .count() as u16;
    if let Some(per_fill) = remaining.checked_div(fill_count) {
        let mut extra = remaining % fill_count;
        for (i, child) in children.iter().enumerate() {
            if matches!(
                constraint_of(child),
                Constraint::Fill | Constraint::Fit
            ) {
                sizes[i] = per_fill + u16::from(extra > 0);
                extra = extra.saturating_sub(1);
            }
        }
    }

    sizes
}

fn constraint_of(node: &LayoutTree) -> Constraint {
    match node {
        LayoutTree::Leaf { constraint, .. } => *constraint,
        LayoutTree::Split { .. } => Constraint::Fill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_leaf_fills_area() {
        let tree = LayoutTree::Leaf {
            name: "main".into(),
            constraint: Constraint::Fill,
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["main"], Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn vertical_split_fixed_and_fill() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "top".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "bottom".into(),
                    constraint: Constraint::Length(5),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["top"], Rect::new(0, 0, 80, 19));
        assert_eq!(result["bottom"], Rect::new(19, 0, 80, 5));
    }

    #[test]
    fn vertical_split_pct_and_fill() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "transcript".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "prompt".into(),
                    constraint: Constraint::Percentage(25),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["prompt"].height, 6); // 25% of 24
        assert_eq!(result["transcript"].height, 18);
    }

    #[test]
    fn horizontal_split() {
        let tree = LayoutTree::Split {
            direction: Direction::Horizontal,
            children: vec![
                LayoutTree::Leaf {
                    name: "left".into(),
                    constraint: Constraint::Length(20),
                },
                LayoutTree::Leaf {
                    name: "right".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["left"], Rect::new(0, 0, 20, 24));
        assert_eq!(result["right"], Rect::new(0, 20, 60, 24));
    }

    #[test]
    fn multiple_fills_distribute_evenly() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Fill,
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["a"].height, 12);
        assert_eq!(result["b"].height, 12);
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
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Split {
                    direction: Direction::Horizontal,
                    children: vec![
                        LayoutTree::Leaf {
                            name: "tl".into(),
                            constraint: Constraint::Fill,
                        },
                        LayoutTree::Leaf {
                            name: "tr".into(),
                            constraint: Constraint::Fill,
                        },
                    ],
                },
                LayoutTree::Leaf {
                    name: "bottom".into(),
                    constraint: Constraint::Length(4),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["bottom"], Rect::new(20, 0, 80, 4));
        assert_eq!(result["tl"], Rect::new(0, 0, 40, 20));
        assert_eq!(result["tr"], Rect::new(0, 40, 40, 20));
    }

    #[test]
    fn min_reserves_floor_then_competes_with_fill() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "header".into(),
                    constraint: Constraint::Min(3),
                },
                LayoutTree::Leaf {
                    name: "body".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["header"].height, 3);
        assert_eq!(result["body"].height, 21);
    }

    #[test]
    fn max_caps_at_ceiling_when_parent_has_room() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "capped".into(),
                    constraint: Constraint::Max(5),
                },
                LayoutTree::Leaf {
                    name: "rest".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["capped"].height, 5);
        assert_eq!(result["rest"].height, 19);
    }

    #[test]
    fn max_shrinks_when_parent_smaller_than_ceiling() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![LayoutTree::Leaf {
                name: "capped".into(),
                constraint: Constraint::Max(50),
            }],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["capped"].height, 24);
    }

    #[test]
    fn ratio_splits_remaining_proportionally() {
        let tree = LayoutTree::Split {
            direction: Direction::Horizontal,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Ratio(1, 3),
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Ratio(2, 3),
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 90, 24));
        assert_eq!(result["a"].width, 30);
        assert_eq!(result["b"].width, 60);
    }

    #[test]
    fn ratio_competes_with_length_for_remaining() {
        let tree = LayoutTree::Split {
            direction: Direction::Horizontal,
            children: vec![
                LayoutTree::Leaf {
                    name: "fixed".into(),
                    constraint: Constraint::Length(20),
                },
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Ratio(1, 2),
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Ratio(1, 2),
                },
            ],
        };
        // 80 total - 20 fixed = 60 to split: 30/30 by ratio.
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["fixed"].width, 20);
        assert_eq!(result["a"].width, 30);
        assert_eq!(result["b"].width, 30);
    }

    #[test]
    fn fit_falls_back_to_fill_for_now() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Fit,
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result["a"].height, 12);
        assert_eq!(result["b"].height, 12);
    }

    #[test]
    fn zero_height_produces_empty_rects() {
        let tree = LayoutTree::Split {
            direction: Direction::Vertical,
            children: vec![
                LayoutTree::Leaf {
                    name: "a".into(),
                    constraint: Constraint::Length(30),
                },
                LayoutTree::Leaf {
                    name: "b".into(),
                    constraint: Constraint::Fill,
                },
            ],
        };
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 10));
        assert_eq!(result["a"].height, 10);
        assert_eq!(result["b"].height, 0);
    }
}
