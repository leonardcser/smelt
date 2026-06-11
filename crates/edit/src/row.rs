use std::ops::Range;

use crate::{BufId, WinId};
use smelt_term::Rect;

pub type RowIndex = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocPosition {
    pub row: RowIndex,
    pub byte_col: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocRange {
    pub start: DocPosition,
    pub end: DocPosition,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRows {
    pub rows: Vec<String>,
    pub soft_breaks: Vec<usize>,
    pub hard_breaks: Vec<usize>,
    /// Byte ranges in each row that are selectable/searchable display text.
    /// `None` means callers should treat every row's text as selectable.
    pub selectable_ranges: Option<Vec<Vec<Range<usize>>>>,
}

impl DisplayRows {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterializedRows {
    pub clamped_scroll: RowIndex,
    pub row_base: RowIndex,
    pub total_rows: RowIndex,
    pub materialized_rows: RowIndex,
}

impl MaterializedRows {
    pub fn materialized_range(self) -> Range<RowIndex> {
        self.row_base
            ..self
                .row_base
                .saturating_add(self.materialized_rows)
                .min(self.total_rows)
    }

    pub fn contains_abs_row(self, row: RowIndex) -> bool {
        self.materialized_range().contains(&row)
    }

    pub fn local_row(self, abs: RowIndex) -> RowIndex {
        abs.saturating_sub(self.row_base)
    }

    pub fn absolute_row(self, local: RowIndex) -> RowIndex {
        self.row_base.saturating_add(local)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterializeRequest {
    pub win: WinId,
    pub buf: BufId,
    pub rect: Rect,
    pub gutter_width: u16,
    pub content_width: u16,
    pub scroll_top: RowIndex,
    pub follow_tail: bool,
}

pub fn row_to_usize(row: RowIndex) -> usize {
    row.min(usize::MAX as RowIndex) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialized_rows_translate_between_absolute_and_local_rows() {
        let rows = MaterializedRows {
            clamped_scroll: 105,
            row_base: 100,
            total_rows: 1_000,
            materialized_rows: 25,
        };

        assert_eq!(rows.materialized_range(), 100..125);
        assert!(rows.contains_abs_row(100));
        assert!(rows.contains_abs_row(124));
        assert!(!rows.contains_abs_row(125));
        assert_eq!(rows.local_row(117), 17);
        assert_eq!(rows.local_row(1), 0);
        assert_eq!(rows.absolute_row(7), 107);
    }

    #[test]
    fn materialized_rows_range_is_clamped_to_total_rows() {
        let rows = MaterializedRows {
            clamped_scroll: 0,
            row_base: 8,
            total_rows: 10,
            materialized_rows: 10,
        };

        assert_eq!(rows.materialized_range(), 8..10);
        assert!(rows.contains_abs_row(9));
        assert!(!rows.contains_abs_row(10));
    }
}
