//! Source ↔ display coordinate mapping.
//!
//! [`ProjectionMaps`] holds a parser-populated source-char ↔ display-char map
//! used when source bytes don't equal display bytes (e.g. the prompt's
//! attachment markers expanding to `[label]`). Buffers without maps fall
//! through to identity walks of `Buffer::lines()` inside
//! [`Buffer::display_cursor_pos`] / [`Buffer::byte_at_display_pos`].

use crate::buffer::{Buffer, SelectionRange};
use crate::text;

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
    /// display chars (matches the prompt's wrapped-row indexing).
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
/// When `end_byte` falls past the last char on `end_row`, paints one virtual
/// cell so an EOL cursor is visible. Only the end row gets that extension —
/// middle rows always run to `line_chars`.
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
        let line = buf.get_line(row).unwrap_or("");
        let line_chars = line.chars().count();
        let cs = if row == start_row { start_col } else { 0 };
        let ce = if row == end_row {
            if end_col > line_chars {
                line_chars + 1
            } else {
                end_col
            }
        } else {
            line_chars
        };
        ranges.push(SelectionRange {
            line: row,
            col_start: cs as u16,
            col_end: ce as u16,
        });
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

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
