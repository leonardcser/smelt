//! Source ↔ display coordinate mapping.
//!
//! [`ProjectionMaps`] holds a parser-populated source-char ↔ display-char map
//! used when source bytes don't equal display bytes (e.g. the prompt's
//! attachment markers expanding to `[label]`). Buffers without maps fall
//! through to identity walks of `Buffer::lines()` inside
//! [`Buffer::display_cursor_pos`] / [`Buffer::byte_at_display_pos`]. The maps
//! are indexed by display characters internally; `Buffer` converts public
//! columns to and from terminal cells.

use crate::buffer::{Buffer, SelectionRange, Span};
use crate::text;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
pub fn byte_range_to_row_ranges(
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
pub fn range_contains_selectable(
    buf: &Buffer,
    row: usize,
    col_start: usize,
    col_end: usize,
) -> bool {
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
pub fn last_non_selectable_end(buf: &Buffer, row: usize, line_width: usize) -> usize {
    let highlights = buf.highlights_at(row);
    let mut max_end = 0;
    for span in highlights {
        if !span.meta.selectable {
            max_end = max_end.max(span.col_end as usize);
        }
    }
    max_end.min(line_width)
}

/// Render a byte range as user-facing display text.
///
/// This drops non-selectable cells, applies `copy_as`, prefers `source_text`
/// when a row's selectable cells are fully covered, and coalesces soft-wrapped
/// or copy-continuation rows.
pub fn copy_byte_range(buf: &Buffer, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    let lines = buf.lines();
    let (sr, sc) = byte_to_row_col(lines, start);
    let (er, ec) = byte_to_row_col(lines, end);
    let er = er.min(lines.len().saturating_sub(1));

    let mut out = String::new();
    let mut source_text_emitted = false;
    for (r, line) in lines.iter().enumerate().take(er + 1).skip(sr) {
        let line_width = text::byte_to_cell(line, line.len());
        let dec = buf.decoration_at(r);
        let is_soft = dec.soft_wrapped;
        let is_copy_cont = dec.copy_continuation;
        if r > sr && !is_soft && !is_copy_cont {
            out.push('\n');
            source_text_emitted = false;
        }

        let is_first = r == sr;
        let is_last = r == er;
        let c_start = if is_first { sc } else { 0 };
        let c_end = if is_last {
            ec.min(line_width)
        } else {
            line_width
        };

        let highlights = buf.highlights_at(r);
        let unselectable_intervals = collect_unselectable(&highlights, line_width);
        let all_selectable_covered =
            all_selectable_in_range(&unselectable_intervals, line_width, c_start, c_end);

        if all_selectable_covered && is_copy_cont && source_text_emitted {
            continue;
        }

        if all_selectable_covered {
            let src =
                external_source_text_for_selection(buf, r, sr, er).or(dec.source_text.as_deref());
            if let Some(src) = src {
                out.push_str(src);
                source_text_emitted = true;
                continue;
            }
        }

        emit_row_cells(line, &highlights, c_start, c_end, &mut out);
    }
    out
}

fn external_source_text_for_selection(
    buf: &Buffer,
    row: usize,
    selection_start_row: usize,
    selection_end_row: usize,
) -> Option<&str> {
    let dec = buf.decoration_at(row);
    let src = dec.external_source_text.as_deref()?;
    let (group_start, group_end) = external_source_group(buf, row);
    if selection_start_row < group_start || selection_end_row > group_end {
        Some(src)
    } else {
        None
    }
}

fn external_source_group(buf: &Buffer, row: usize) -> (usize, usize) {
    let mut start = row;
    while start > 0 && external_source_group_row(buf, start - 1) {
        start -= 1;
    }

    let mut end = row;
    while end + 1 < buf.line_count() && external_source_group_row(buf, end + 1) {
        end += 1;
    }

    (start, end)
}

fn external_source_group_row(buf: &Buffer, row: usize) -> bool {
    let dec = buf.decoration_at(row);
    dec.external_source_text.is_some() || dec.copy_continuation
}

fn byte_to_row_col(lines: &[String], byte: usize) -> (usize, usize) {
    let mut acc = 0usize;
    for (r, row) in lines.iter().enumerate() {
        let row_end = acc + row.len();
        if byte <= row_end {
            let col_byte = byte.saturating_sub(acc).min(row.len());
            let col = text::byte_to_cell(row, col_byte);
            return (r, col);
        }
        acc = row_end + 1;
    }
    let last_row = lines.len().saturating_sub(1);
    let last_col = lines
        .last()
        .map(|r| text::byte_to_cell(r, r.len()))
        .unwrap_or(0);
    (last_row, last_col)
}

fn collect_unselectable(highlights: &[Span], line_width: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for h in highlights {
        if h.meta.selectable {
            continue;
        }
        let s = (h.col_start as usize).min(line_width);
        let e = (h.col_end as usize).min(line_width);
        if e > s {
            out.push((s, e));
        }
    }
    out
}

fn all_selectable_in_range(
    unselectable: &[(usize, usize)],
    line_width: usize,
    c_start: usize,
    c_end: usize,
) -> bool {
    'outer: for i in 0..line_width {
        for (s, e) in unselectable {
            if i >= *s && i < *e {
                continue 'outer;
            }
        }
        if i < c_start || i >= c_end {
            return false;
        }
    }
    true
}

fn emit_row_cells(line: &str, highlights: &[Span], c_start: usize, c_end: usize, out: &mut String) {
    let mut emitted_copy_as: Vec<usize> = Vec::new();
    let mut col = 0usize;
    for ch in line.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        let ch_end = col.saturating_add(w);
        if ch_end <= c_start || col >= c_end {
            col = ch_end;
            continue;
        }
        let mut selectable = true;
        let mut copy_as_hit: Option<(usize, &str)> = None;
        for (idx, span) in highlights.iter().enumerate() {
            let s = span.col_start as usize;
            let e = span.col_end as usize;
            if ch_end <= s || col >= e {
                continue;
            }
            if !span.meta.selectable {
                selectable = false;
                break;
            }
            if let Some(s_str) = span.meta.copy_as.as_deref() {
                copy_as_hit = Some((idx, s_str));
            }
        }
        if !selectable {
            col = ch_end;
            continue;
        }
        if let Some((idx, s)) = copy_as_hit {
            if !emitted_copy_as.contains(&idx) {
                out.push_str(s);
                emitted_copy_as.push(idx);
            }
        } else {
            out.push(ch);
        }
        col = ch_end;
    }
}

/// Snap `col` (display cell on `row`) to the nearest selectable cell.
pub fn snap_col_to_selectable(buf: &Buffer, row: usize, col: usize) -> usize {
    let Some(line) = buf.get_line(row) else {
        return col;
    };
    let line_width = text::byte_to_cell(line, line.len());
    if line_width == 0 {
        return col;
    }
    let highlights = buf.highlights_at(row);
    let unselectable = collect_unselectable(&highlights, line_width);
    let is_selectable =
        |c: usize| c < line_width && !unselectable.iter().any(|(s, e)| c >= *s && c < *e);
    if is_selectable(col) {
        return col;
    }
    for c in (col + 1)..line_width {
        if is_selectable(c) {
            return c;
        }
    }
    if col > 0 {
        for c in (0..col.min(line_width)).rev() {
            if is_selectable(c) {
                return c;
            }
        }
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::buffer::{BufCreateOpts, BufId, Buffer, LineDecoration, SpanMeta};

    #[test]
    fn selection_paints_one_cell_on_empty_middle_row() {
        // Source "aaa\n\nccc": row 1 is empty. Selecting bytes 0..8 spans all
        // three rows. The empty middle row must still appear in the highlight
        // as a 1-cell virtual span at col 0; otherwise the gap looks like a
        // selection break.
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_source("aaa\n\nccc".into());
        buf.set_all_lines(vec!["aaa".into(), "".into(), "ccc".into()]);
        let ranges = byte_range_to_row_ranges(&buf, 0, 8);
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
        let ranges = byte_range_to_row_ranges(&buf, 0, 3);
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
            crate::buffer::SpanMeta::unselectable(),
        );
        // "│ hello" = 9 bytes + newline + "│ " = 4 bytes + newline + "│ world" = 9 bytes
        // Total bytes = 9 + 1 + 4 + 1 + 9 = 24. Select all: 0..24.
        let ranges = byte_range_to_row_ranges(&buf, 0, 24);
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
            crate::buffer::SpanMeta::unselectable(),
        );
        // " hello " = 7 bytes + newline + " " = 1 byte + newline + " world " = 7 bytes
        // Total = 7 + 1 + 1 + 1 + 7 = 17.
        let ranges = byte_range_to_row_ranges(&buf, 0, 17);
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
            crate::buffer::SpanMeta::unselectable(),
        );
        // "│ " = 4 bytes + newline + "│ content" = 11 bytes. Total = 16.
        let ranges = byte_range_to_row_ranges(&buf, 0, 16);
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

        let ranges = byte_range_to_row_ranges(&buf, 0, "a\n你b".len());
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
            crate::buffer::SpanMeta::unselectable(),
        );

        let ranges = byte_range_to_row_ranges(&buf, 0, "你 \ndone".len());
        assert_eq!(ranges.len(), 2);
        assert_eq!((ranges[0].col_start, ranges[0].col_end), (3, 4));
        assert_eq!((ranges[1].col_start, ranges[1].col_end), (0, 4));
    }

    fn unselectable_meta() -> SpanMeta {
        SpanMeta::unselectable()
    }

    fn copy_as_meta(s: &str) -> SpanMeta {
        SpanMeta::copy_as(s)
    }

    fn hl_for_test() -> crate::theme::HlGroup {
        crate::theme::intern("Normal")
    }

    #[test]
    fn copy_byte_range_basic_text() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(copy_byte_range(&buf, 0, 5), "hello");
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello\nworld");
        assert_eq!(copy_byte_range(&buf, 6, 11), "world");
    }

    #[test]
    fn copy_skips_non_selectable_chrome() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["│ hi".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), unselectable_meta());
        let line_bytes = "│ hi".len();
        assert_eq!(copy_byte_range(&buf, 0, line_bytes), "hi");
    }

    #[test]
    fn copy_applies_copy_as_substitution_once_per_span() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["+ add".into()]);
        buf.add_highlight_group_with_meta(0, 0, 2, hl_for_test(), copy_as_meta(""));
        assert_eq!(copy_byte_range(&buf, 0, "+ add".len()), "add");
    }

    #[test]
    fn copy_uses_inner_source_text_when_selection_stays_inside_external_group() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["echo hi".into(), "echo bye".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                source_text: Some("echo hi".into()),
                external_source_text: Some("```sh\necho hi".into()),
                ..Default::default()
            },
        );
        buf.set_decoration(
            1,
            LineDecoration {
                source_text: Some("echo bye".into()),
                external_source_text: Some("echo bye\n```".into()),
                ..Default::default()
            },
        );

        assert_eq!(
            copy_byte_range(&buf, 0, "echo hi\necho bye".len()),
            "echo hi\necho bye"
        );
    }

    #[test]
    fn copy_uses_external_source_text_when_selection_spans_external_group() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["before".into(), "echo hi".into(), "echo bye".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                source_text: Some("echo hi".into()),
                external_source_text: Some("```sh\necho hi".into()),
                ..Default::default()
            },
        );
        buf.set_decoration(
            2,
            LineDecoration {
                source_text: Some("echo bye".into()),
                external_source_text: Some("echo bye\n```".into()),
                ..Default::default()
            },
        );

        assert_eq!(
            copy_byte_range(&buf, 0, "before\necho hi\necho bye".len()),
            "before\n```sh\necho hi\necho bye\n```"
        );
    }

    #[test]
    fn copy_uses_source_text_when_full_row_selected() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["Title".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                source_text: Some("# Title".into()),
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 5), "# Title");
        assert_eq!(copy_byte_range(&buf, 1, 4), "itl");
    }

    #[test]
    fn copy_coalesces_copy_continuation_rows_via_source_text() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["hello".into(), "world".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                source_text: Some("hello world".into()),
                ..Default::default()
            },
        );
        buf.set_decoration(
            1,
            LineDecoration {
                copy_continuation: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 11), "hello world");
    }

    #[test]
    fn copy_copy_continuation_without_source_text_emits_all_rows() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["abc".into(), "def".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                copy_continuation: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 7), "abcdef");
    }

    #[test]
    fn copy_soft_wrap_without_source_text_emits_all_rows() {
        let mut buf = Buffer::new(BufId(1), Default::default());
        buf.set_all_lines(vec!["abc".into(), "def".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                soft_wrapped: true,
                ..Default::default()
            },
        );
        assert_eq!(copy_byte_range(&buf, 0, 7), "abcdef");
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
