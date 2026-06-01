//! Source ↔ display coordinate mapping.
//!
//! [`ProjectionMaps`] holds a parser-populated source-char ↔ display-char map
//! used when source bytes don't equal display bytes (e.g. the prompt's
//! attachment markers expanding to `[label]`). Buffers without maps fall
//! through to identity walks of `Buffer::lines()` inside
//! [`Buffer::display_cursor_pos`] / [`Buffer::byte_at_display_pos`]. The maps
//! are indexed by display characters internally; `Buffer` converts public
//! columns to and from terminal cells.

use crate::buffer::{Buffer, SelectionRange};
use crate::text;
use unicode_width::UnicodeWidthStr;

/// Bidirectional source↔display char maps + per-row offsets.
/// Stored on [`Buffer`] after parse for buffers whose source-byte stream
/// doesn't match their display rows 1:1.
#[derive(Clone, Debug, Default)]
pub struct ProjectionMaps {
    /// `source_char_to_display_char[n] = display-char index of source char n`.
    /// Length = `source.chars().count() + 1`; final entry = total display chars.
    pub source_char_to_display_char: Vec<usize>,
    /// `display_char_to_source_char[n] = source-char index of display char n`.
    /// Length = total display chars + 1.
    pub display_char_to_source_char: Vec<usize>,
    /// Display-char offset at the start of each wrapped row.
    pub row_offsets: Vec<usize>,
}

impl ProjectionMaps {
    /// Map a source byte offset to a display `(row, col)` pair. `col` is in
    /// display chars; `Buffer::display_cursor_pos` converts it to cells for
    /// callers.
    pub fn cursor_pos(&self, source: &str, src_byte: usize) -> (usize, usize) {
        if self.row_offsets.is_empty() {
            return (0, 0);
        }
        let src_char = text::char_pos(source, src_byte);
        let display_char = self
            .source_char_to_display_char
            .get(src_char)
            .copied()
            .or_else(|| self.source_char_to_display_char.last().copied())
            .unwrap_or(0);
        let row = self
            .row_offsets
            .partition_point(|&o| o <= display_char)
            .saturating_sub(1);
        let col = display_char - self.row_offsets[row];
        (row, col)
    }

    /// Map a display `(row, col)` (display chars within the row) to a source
    /// byte offset.
    pub fn byte_at(&self, source: &str, row: usize, col: usize) -> usize {
        if self.row_offsets.is_empty() {
            return 0;
        }
        let row = row.min(self.row_offsets.len().saturating_sub(1));
        let display_char = self.row_offsets[row] + col;
        let src_char = self
            .display_char_to_source_char
            .get(display_char)
            .copied()
            .or_else(|| self.display_char_to_source_char.last().copied())
            .unwrap_or(0);
        text::byte_of_char(source, src_char)
    }
}

/// Per-row visual-mode selection ranges for a source byte range.
///
/// Routes through `buf.display_cursor_pos` so it works identically for buffers
/// with and without a `ProjectionMaps`. Caller passes already-snapped source
/// byte offsets; this helper does no further snapping.
///
/// Paints one virtual cell when a row is included in the selection but has
/// nothing to highlight on its own (empty middle row, empty start row whose
/// selection extends past it, end-of-line cursor past the last char). This
/// mirrors vim's visible-newline behavior so multi-line selections always show
/// the empty rows as part of the range.
///
/// For rows that contain only non-selectable chrome (e.g. a thinking-block
/// gutter `│ ` or a user-block padding space), the virtual cell is placed
/// *after* the chrome so the selection highlight doesn't paint over it.
pub fn selection_to_row_ranges(
    buf: &Buffer,
    start_byte: usize,
    end_byte: usize,
) -> Vec<SelectionRange> {
    if start_byte >= end_byte {
        return Vec::new();
    }
    let (start_row, start_col) = buf.display_cursor_pos(start_byte);
    let (end_row, end_col) = buf.display_cursor_pos(end_byte);
    let mut ranges = Vec::with_capacity(end_row - start_row + 1);
    for row in start_row..=end_row {
        let line = buf.get_line(row).unwrap_or_default();
        let line_width = UnicodeWidthStr::width(line);
        let mut cs = if row == start_row { start_col } else { 0 };
        let mut ce = if row == end_row {
            if end_col > line_width {
                line_width + 1
            } else {
                end_col
            }
        } else {
            line_width
        };
        // A row that's part of the selection but has no selectable text gets a
        // one-cell virtual span placed after any leading non-selectable chrome
        // (gutter, padding) so the highlight doesn't paint over chrome cells.
        if !range_contains_selectable(buf, row, 0, line_width) {
            let chrome_end = last_non_selectable_end(buf, row, line_width);
            cs = cs.max(chrome_end);
            ce = cs + 1;
        }
        ranges.push(SelectionRange {
            line: row,
            col_start: cs as u16,
            col_end: ce as u16,
        });
    }
    ranges
}

/// True if any column in `[col_start, col_end)` on `row` is selectable.
/// Unstyled cells (not covered by any highlight span) are implicitly
/// selectable. Empty ranges always return `false`.
fn range_contains_selectable(buf: &Buffer, row: usize, col_start: usize, col_end: usize) -> bool {
    let line = buf.get_line(row).unwrap_or_default();
    let line_width = UnicodeWidthStr::width(line);
    if col_start >= col_end || line_width == 0 {
        return false;
    }
    let highlights = buf.highlights_at(row);
    let mut unselectable = vec![false; line_width];
    for span in highlights {
        if !span.meta.selectable {
            for i in span.col_start as usize..span.col_end as usize {
                if i < unselectable.len() {
                    unselectable[i] = true;
                }
            }
        }
    }
    for cell in unselectable
        .iter()
        .take(col_end.min(line_width))
        .skip(col_start)
    {
        if !cell {
            return true;
        }
    }
    false
}

/// The largest `col_end` among non-selectable spans on `row`, clamped to the
/// row display width. Returns `0` when there is no non-selectable span.
fn last_non_selectable_end(buf: &Buffer, row: usize, line_width: usize) -> usize {
    let highlights = buf.highlights_at(row);
    let mut max_end = 0;
    for span in highlights {
        if !span.meta.selectable {
            max_end = max_end.max(span.col_end as usize);
        }
    }
    max_end.min(line_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::buffer::{BufCreateOpts, BufId, Buffer};

    #[test]
    fn selection_paints_one_cell_on_empty_middle_row() {
        // Source "aaa\n\nccc": row 1 is empty. Selecting bytes 0..8 spans all
        // three rows. The empty middle row must still appear in the highlight
        // as a 1-cell virtual span at col 0; otherwise the gap looks like a
        // selection break.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_source("aaa\n\nccc".into());
        buf.set_all_lines(vec!["aaa".into(), "".into(), "ccc".into()]);
        let ranges = selection_to_row_ranges(&buf, 0, 8);
        assert_eq!(ranges.len(), 3);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 3));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 1));
        assert_eq!((ranges[2].col_start, ranges[2].col_end), (0, 3));
    }

    #[test]
    fn selection_paints_one_cell_on_empty_start_row() {
        // Selection begins on an empty row and extends into the next row.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_source("\nbbb".into());
        buf.set_all_lines(vec!["".into(), "bbb".into()]);
        let ranges = selection_to_row_ranges(&buf, 0, 3);
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 1));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 2));
    }

    #[test]
    fn selection_paints_virtual_cell_after_chrome_on_empty_middle_row() {
        // Thinking-block empty line: gutter "│ " is non-selectable chrome.
        // Selecting across the block should place the virtual cell after the
        // gutter so the bar itself doesn't receive the visual bg.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec!["│ hello".into(), "│ ".into(), "│ world".into()]);
        buf.add_highlight_group_with_meta(
            1,
            0,
            2,
            crate::theme::intern("Normal"),
            crate::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        // "│ hello" = 9 bytes + newline + "│ " = 4 bytes + newline + "│ world" = 9 bytes
        // Total bytes = 9 + 1 + 4 + 1 + 9 = 24. Select all: 0..24.
        let ranges = selection_to_row_ranges(&buf, 0, 24);
        assert_eq!(ranges.len(), 3);
        // Row 0: content row, normal range.
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 7));
        // Row 1: empty chrome-only row; virtual span placed after the 2-char gutter.
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (2, 3));
        // Row 2: end row.
        assert_eq!((ranges[2].col_start, ranges[2].col_end), (0, 7));
    }

    #[test]
    fn selection_paints_virtual_cell_after_padding_on_user_empty_row() {
        // User-block blank row: one padding space is non-selectable chrome.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec![" hello ".into(), " ".into(), " world ".into()]);
        buf.add_highlight_group_with_meta(
            1,
            0,
            1,
            crate::theme::intern("Normal"),
            crate::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        // " hello " = 7 bytes + newline + " " = 1 byte + newline + " world " = 7 bytes
        // Total = 7 + 1 + 1 + 1 + 7 = 17.
        let ranges = selection_to_row_ranges(&buf, 0, 17);
        assert_eq!(ranges.len(), 3);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 7));
        // Virtual span after the 1-char padding.
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (1, 2));
        assert_eq!((ranges[2].col_start, ranges[2].col_end), (0, 7));
    }

    #[test]
    fn selection_does_not_override_chrome_on_start_row_with_no_selectable_cells() {
        // Selection starts on an empty chrome-only row and extends down.
        // The virtual span should be after the chrome, not at col 0.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec!["│ ".into(), "│ content".into()]);
        buf.add_highlight_group_with_meta(
            0,
            0,
            3,
            crate::theme::intern("Normal"),
            crate::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );
        // "│ " = 4 bytes + newline + "│ content" = 11 bytes. Total = 16.
        let ranges = selection_to_row_ranges(&buf, 0, 16);
        assert_eq!(ranges.len(), 2);
        // Start row (also middle row): virtual span after gutter.
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (2, 3));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 9));
    }

    #[test]
    fn selection_ranges_use_display_cells_for_wide_chars() {
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_source("a\n你b".into());
        buf.set_all_lines(vec!["a".into(), "你b".into()]);

        let ranges = selection_to_row_ranges(&buf, 0, "a\n你b".len());
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (0, 1));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 3));
    }

    #[test]
    fn virtual_span_after_chrome_uses_display_cells() {
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec!["你 ".into(), "done".into()]);
        buf.add_highlight_group_with_meta(
            0,
            0,
            3,
            crate::theme::intern("Normal"),
            crate::buffer::SpanMeta {
                selectable: false,
                copy_as: None,
            },
        );

        let ranges = selection_to_row_ranges(&buf, 0, "你 \ndone".len());
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (3, 4));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 4));
    }

    #[test]
    fn projection_maps_round_trip_through_expansion() {
        // Source: "a<M>b" (3 chars). Marker expands to "[X]" (3 display chars).
        // Display: "a[X]b" (5 display chars).
        let maps = ProjectionMaps {
            source_char_to_display_char: vec![0, 1, 4, 5],
            display_char_to_source_char: vec![0, 1, 1, 1, 2, 3],
            row_offsets: vec![0],
        };
        let source = "a\u{FFFC}b";
        // Cursor at start of "b" (source byte after the marker).
        let marker_len = '\u{FFFC}'.len_utf8();
        let b_byte = 1 + marker_len;
        let (row, col) = maps.cursor_pos(source, b_byte);
        assert_eq!((row, col), (0, 4));
        // Reverse: display col 4 → source byte at start of "b".
        let back = maps.byte_at(source, 0, 4);
        assert_eq!(back, b_byte);
    }
}
