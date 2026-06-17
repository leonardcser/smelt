use std::ops::Range;

use crate::{BufId, WinId};
use smelt_buffer::buffer::CopyOutput;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextRange {
    Bytes(Range<usize>),
    Rows(DocRange),
}

impl TextRange {
    pub fn rows(&self) -> Option<DocRange> {
        match self {
            Self::Rows(range) => Some(*range),
            Self::Bytes(_) => None,
        }
    }

    pub fn start_position(&self) -> Option<DocPosition> {
        self.rows().map(|range| range.start)
    }
}

impl From<DocRange> for TextRange {
    fn from(range: DocRange) -> Self {
        Self::Rows(range)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowBreak {
    Soft,
    Hard,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRow {
    pub text: String,
    /// Separator between the previous row and this row in joined display text.
    /// `None` means this is the first row in the returned slice.
    pub break_before: Option<RowBreak>,
    /// Byte ranges in `text` that are selectable/searchable display text.
    pub selectable_ranges: Vec<Range<usize>>,
}

impl DisplayRow {
    pub fn new(text: String, selectable_ranges: Vec<Range<usize>>) -> Self {
        Self {
            text,
            break_before: None,
            selectable_ranges,
        }
    }

    pub fn with_break_before(mut self, break_before: RowBreak) -> Self {
        self.break_before = Some(break_before);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplaySnapshot {
    pub generation: u64,
    pub total_rows: RowIndex,
}

pub trait DisplayDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRows {
    pub rows: Vec<DisplayRow>,
}

impl DisplayRows {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn into_text_rows(self) -> Vec<String> {
        self.rows.into_iter().map(|row| row.text).collect()
    }

    pub fn text_rows(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.text.clone()).collect()
    }

    pub fn selectable_text(&self) -> String {
        self.rows
            .iter()
            .flat_map(|row| {
                row.selectable_ranges.iter().filter_map(|range| {
                    let text = smelt_buffer::text::slice(&row.text, range.clone());
                    (!text.is_empty()).then(|| text.to_string())
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn soft_breaks(&self) -> Vec<usize> {
        self.breaks(RowBreak::Soft)
    }

    pub fn hard_breaks(&self) -> Vec<usize> {
        self.breaks(RowBreak::Hard)
    }

    fn breaks(&self, kind: RowBreak) -> Vec<usize> {
        let mut breaks = Vec::new();
        let mut pos = 0usize;
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                if row.break_before == Some(kind) {
                    breaks.push(pos);
                }
                pos = pos.saturating_add(1);
            }
            pos = pos.saturating_add(row.text.len());
        }
        breaks
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
