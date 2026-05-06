//! Composable layout shape returned by a tool's `render` callback.
//!
//! Tools paint into one or more `Buffer`s and assemble them into a
//! `BlockLayout` tree. The transcript composer walks the tree and
//! replays leaves into the surrounding `LineBuilder`. Tools that don't
//! register a `render` callback fall back to the default body
//! formatting.
//!
//! The shape is intentionally minimal — a tool block is a sequence of
//! buffer leaves stacked vertically, sometimes nested.

use crate::buffer::BufId;

/// One node in a tool's block layout tree.
#[derive(Clone, Debug)]
pub enum BlockLayout {
    /// Replay the contents of `buf` into the parent `LineBuilder`.
    Leaf(BufId),
    /// Stack children top-to-bottom.
    Vbox(Vec<BlockLayout>),
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
        }
    }
}
