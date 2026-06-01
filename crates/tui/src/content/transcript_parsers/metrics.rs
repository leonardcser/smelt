//! Single source of truth for the cell widths and gutter glyphs used by
//! per-block renderers. Every block keeps its leftmost column inside a
//! shared budget so a `Text → CodeLine → Tool` transition doesn't step.
//!
//! The numbers are intentionally small and untyped - they describe terminal
//! cell counts, not byte lengths.

/// Cells reserved on the left of every non-chrome block (thinking, tool
/// output, replayed leaves, diff/file-view leaves, truncation hints).
pub(super) const BLOCK_GUTTER_W: usize = 2;

/// Plain two-space block gutter. Used by tool output, tool user_message,
/// replayed buffer leaves, and the "... N above" truncation hint.
pub(super) const BLOCK_GUTTER_SPACE: &str = "  ";

/// Thinking block gutter. `│` is one cell, the trailing space brings the
/// gutter up to `BLOCK_GUTTER_W` cells.
pub(super) const THINKING_GUTTER: &str = "\u{2502} ";

/// Cells reserved inside the User/Exec chrome bg on each side before content.
/// The chrome paints `SmeltUserBg` across the full row, so this is the
/// content offset from the painted edges, not from the window edge.
pub(super) const CHROME_INNER_PAD: usize = 1;

/// Extra cell kept free at the right edge so wrap math doesn't have to
/// reason about the cursor sitting flush against the scrollbar column.
pub(super) const RIGHT_SAFETY: usize = 1;

/// Inner content width for a block that owns a `BLOCK_GUTTER_W`-wide left
/// gutter. Clamped to at least 1 so degenerate widths still wrap something.
#[inline]
pub(super) fn block_inner_width(width: usize) -> usize {
    width.saturating_sub(BLOCK_GUTTER_W + RIGHT_SAFETY).max(1)
}

/// Content width inside the User/Exec chrome panel.
#[inline]
pub(super) fn chrome_text_width(width: usize) -> usize {
    width.saturating_sub(2 * CHROME_INNER_PAD).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_gutter_matches_block_gutter_width() {
        assert_eq!(
            smelt_core::content::builder::display_width(THINKING_GUTTER),
            BLOCK_GUTTER_W
        );
    }

    #[test]
    fn block_gutter_space_matches_block_gutter_width() {
        assert_eq!(BLOCK_GUTTER_SPACE.len(), BLOCK_GUTTER_W);
        assert_eq!(
            smelt_core::content::builder::display_width(BLOCK_GUTTER_SPACE),
            BLOCK_GUTTER_W
        );
    }

    #[test]
    fn block_inner_width_subtracts_gutter_and_safety() {
        assert_eq!(block_inner_width(80), 80 - BLOCK_GUTTER_W - RIGHT_SAFETY);
        assert_eq!(block_inner_width(0), 1);
        assert_eq!(block_inner_width(1), 1);
    }

    #[test]
    fn chrome_text_width_subtracts_both_pads() {
        assert_eq!(chrome_text_width(80), 80 - 2 * CHROME_INNER_PAD);
        assert_eq!(chrome_text_width(0), 1);
    }
}
