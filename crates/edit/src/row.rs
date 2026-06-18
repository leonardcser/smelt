use std::ops::Range;

use crate::{text, BufId, Buffer, WinId};
use smelt_buffer::buffer::{CopyOutput, SpanAction};
use smelt_term::Rect;

pub type RowIndex = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocumentHandle(pub u64);

impl DocumentHandle {
    pub fn raw(self) -> u64 {
        self.0
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayAction {
    pub cell_start: usize,
    pub cell_end: usize,
    pub action: SpanAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRow {
    pub text: String,
    /// Separator between the previous row and this row in joined display text.
    /// `None` means this is the first row in the returned slice.
    pub break_before: Option<RowBreak>,
    /// Byte ranges in `text` that are selectable/searchable display text.
    pub selectable_ranges: Vec<Range<usize>>,
    /// Cell ranges in `text` that trigger actions such as opening links/files.
    pub actions: Vec<DisplayAction>,
}

impl DisplayRow {
    pub fn new(text: String, selectable_ranges: Vec<Range<usize>>) -> Self {
        Self {
            text,
            break_before: None,
            selectable_ranges,
            actions: Vec::new(),
        }
    }

    pub fn with_actions(mut self, actions: Vec<DisplayAction>) -> Self {
        self.actions = actions;
        self
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

    fn search_matches(
        &mut self,
        query: &str,
        _origin: DocPosition,
        _forward: bool,
        chunk_rows: RowIndex,
    ) -> Vec<DocRange> {
        let total_rows = self.snapshot().total_rows;
        scan_document_rows(self, query, 0, total_rows, chunk_rows)
    }

    fn action_at(&mut self, pos: DocPosition) -> Option<SpanAction> {
        let row = self
            .materialize(pos.row..pos.row.saturating_add(1))
            .rows
            .into_iter()
            .next()?;
        let byte_col = text::snap(&row.text, pos.byte_col.min(row.text.len()));
        let cell = text::byte_to_cell(&row.text, byte_col);
        row.actions
            .into_iter()
            .rev()
            .find(|action| cell >= action.cell_start && cell < action.cell_end)
            .map(|action| action.action)
    }
}

pub(crate) fn scan_document_rows<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    start: RowIndex,
    total_rows: RowIndex,
    chunk_rows: RowIndex,
) -> Vec<DocRange> {
    if query.is_empty() || start >= total_rows {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let chunk_rows = chunk_rows.max(1);
    let mut row = start;
    while row < total_rows {
        let count = chunk_rows.min(total_rows - row);
        let display = document.materialize(row..row.saturating_add(count));
        collect_display_matches(&display.rows, row, query, &mut matches);
        row = row.saturating_add(count);
    }
    matches
}

pub(crate) fn scan_document_row_window<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    origin: DocPosition,
    total_rows: RowIndex,
    chunk_rows: RowIndex,
) -> Vec<DocRange> {
    if total_rows == 0 {
        return Vec::new();
    }
    let chunk_rows = chunk_rows.max(1);
    let mut matches = Vec::new();
    let start = origin.row.min(total_rows.saturating_sub(1));
    let count = chunk_rows.min(total_rows - start);
    let display = document.materialize(start..start.saturating_add(count));
    collect_display_matches(&display.rows, start, query, &mut matches);
    if matches.is_empty() && start > 0 {
        let count = chunk_rows.min(start);
        let display = document.materialize(0..count);
        collect_display_matches(&display.rows, 0, query, &mut matches);
    }
    matches
}

fn collect_display_matches(
    rows: &[DisplayRow],
    start: RowIndex,
    query: &str,
    matches: &mut Vec<DocRange>,
) {
    for (offset, row) in rows.iter().enumerate() {
        let row_index = start.saturating_add(offset as RowIndex);
        for (byte_col, _) in row.text.match_indices(query) {
            let end_col = byte_col + query.len();
            if row
                .selectable_ranges
                .iter()
                .any(|range| range.start <= byte_col && end_col <= range.end)
            {
                matches.push(DocRange {
                    start: DocPosition {
                        row: row_index,
                        byte_col,
                    },
                    end: DocPosition {
                        row: row_index,
                        byte_col: end_col,
                    },
                });
            }
        }
    }
}

pub struct StaticRowsDocument {
    rows: Vec<DisplayRow>,
    generation: u64,
}

impl StaticRowsDocument {
    pub fn new(rows: Vec<DisplayRow>) -> Self {
        Self {
            rows,
            generation: 0,
        }
    }

    pub fn from_text_rows(rows: Vec<String>) -> Self {
        Self::new(
            rows.into_iter()
                .map(|text| {
                    let range = 0..text.len();
                    DisplayRow::new(text, vec![range])
                })
                .collect(),
        )
    }

    pub fn set_rows(&mut self, rows: Vec<DisplayRow>) {
        self.rows = rows;
        self.generation = self.generation.wrapping_add(1);
    }
}

impl DisplayDocument for StaticRowsDocument {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.generation,
            total_rows: self.rows.len() as RowIndex,
        }
    }

    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
        let start = row_to_usize(range.start).min(self.rows.len());
        let end = row_to_usize(range.end).min(self.rows.len());
        DisplayRows {
            rows: self.rows[start..end].to_vec(),
        }
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        let range = range.rows()?;
        let start_row = range.start.row.min(range.end.row);
        let end_row = range.start.row.max(range.end.row);
        let mut out = String::new();
        for row_idx in start_row..=end_row {
            let row = self.rows.get(row_to_usize(row_idx))?;
            if !out.is_empty() {
                out.push('\n');
            }
            if start_row == end_row {
                let start = range.start.byte_col.min(range.end.byte_col);
                let end = range.start.byte_col.max(range.end.byte_col);
                out.push_str(smelt_buffer::text::slice(&row.text, start..end));
            } else if row_idx == range.start.row {
                out.push_str(smelt_buffer::text::slice(
                    &row.text,
                    range.start.byte_col..row.text.len(),
                ));
            } else if row_idx == range.end.row {
                out.push_str(smelt_buffer::text::slice(&row.text, 0..range.end.byte_col));
            } else {
                out.push_str(&row.text);
            }
        }
        Some(CopyOutput::same(out))
    }
}

pub struct BufferDocument<'a> {
    buf: &'a Buffer,
}

impl<'a> BufferDocument<'a> {
    pub fn new(buf: &'a Buffer) -> Self {
        Self { buf }
    }
}

impl DisplayDocument for BufferDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.buf.changedtick(),
            total_rows: self.buf.line_count() as RowIndex,
        }
    }

    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
        let start = row_to_usize(range.start).min(self.buf.line_count());
        let end = row_to_usize(range.end).min(self.buf.line_count());
        let rows = self
            .buf
            .get_lines(start, end)
            .iter()
            .map(|line| DisplayRow::new(line.clone(), std::iter::once(0..line.len()).collect()))
            .collect();
        DisplayRows { rows }
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        match range {
            TextRange::Bytes(range) => Some(self.buf.copy_range(range)),
            TextRange::Rows(range) => copy_buffer_doc_range(self.buf, range),
        }
    }
}

pub(crate) fn copy_buffer_doc_range(buf: &Buffer, range: DocRange) -> Option<CopyOutput> {
    let lines = buf.lines();
    if lines.is_empty()
        || (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col)
    {
        return None;
    }
    let start_row = range.start.row.min(range.end.row);
    let end_row = range.start.row.max(range.end.row);
    let mut out = String::new();
    for row_idx in start_row..=end_row {
        if row_idx == end_row && range.end.byte_col == 0 && end_row > start_row {
            break;
        }
        let row = lines.get(row_to_usize(row_idx))?;
        if !out.is_empty() {
            out.push('\n');
        }
        if start_row == end_row {
            let start = range.start.byte_col.min(range.end.byte_col);
            let end = range.start.byte_col.max(range.end.byte_col);
            out.push_str(smelt_buffer::text::slice(row, start..end));
        } else if row_idx == range.start.row {
            out.push_str(smelt_buffer::text::slice(
                row,
                range.start.byte_col..row.len(),
            ));
        } else if row_idx == range.end.row {
            out.push_str(smelt_buffer::text::slice(row, 0..range.end.byte_col));
        } else {
            out.push_str(row);
        }
    }
    Some(CopyOutput::same(out))
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
    pub document_handle: Option<DocumentHandle>,
    pub rect: Rect,
    pub gutter_width: u16,
    pub content_width: u16,
    pub scroll_top: RowIndex,
    pub follow_tail: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedWindowRequest {
    pub win: WinId,
    pub buf: BufId,
    pub document_handle: Option<DocumentHandle>,
    pub rect: Rect,
    pub gutter_width: u16,
    pub content_width: u16,
}

pub fn row_to_usize(row: RowIndex) -> usize {
    row.min(usize::MAX as RowIndex) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_rows_document_materializes_and_copies_ranges() {
        let mut doc = StaticRowsDocument::from_text_rows(vec!["alpha".into(), "beta".into()]);
        assert_eq!(doc.snapshot().total_rows, 2);
        assert_eq!(doc.materialize(1..2).text_rows(), vec!["beta"]);

        let copied = doc
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 1,
                },
                end: DocPosition {
                    row: 1,
                    byte_col: 2,
                },
            }))
            .expect("copy range");
        assert_eq!(copied.kill_ring, "lpha\nbe");
    }

    #[test]
    fn buffer_document_copies_row_ranges() {
        let mut buf = Buffer::new(BufId(1), crate::BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "beta".into(), "gamma".into()]);
        let mut doc = BufferDocument::new(&buf);

        let copied = doc
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 1,
                },
                end: DocPosition {
                    row: 2,
                    byte_col: 2,
                },
            }))
            .expect("copy range");
        assert_eq!(copied.kill_ring, "lpha\nbeta\nga");
    }

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

    #[test]
    fn display_document_action_at_uses_row_actions() {
        struct Doc {
            row: DisplayRow,
        }

        impl DisplayDocument for Doc {
            fn snapshot(&mut self) -> DisplaySnapshot {
                DisplaySnapshot {
                    generation: 0,
                    total_rows: 1,
                }
            }

            fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
                if range.contains(&0) {
                    DisplayRows {
                        rows: vec![self.row.clone()],
                    }
                } else {
                    DisplayRows::empty()
                }
            }

            fn copy_range(&mut self, _range: TextRange) -> Option<CopyOutput> {
                None
            }
        }

        let action = SpanAction::OpenUrl("https://example.test".into());
        let mut doc = Doc {
            row: DisplayRow::new("open link".into(), std::iter::once(0..9).collect()).with_actions(
                vec![DisplayAction {
                    cell_start: 5,
                    cell_end: 9,
                    action: action.clone(),
                }],
            ),
        };

        assert_eq!(
            doc.action_at(DocPosition {
                row: 0,
                byte_col: 6
            }),
            Some(action)
        );
        assert_eq!(
            doc.action_at(DocPosition {
                row: 0,
                byte_col: 4
            }),
            None
        );
    }
}
