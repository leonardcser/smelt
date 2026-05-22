//! Wrap layout: a derived view that maps a buffer's logical lines onto visual rows.
//!
//! `WrappedLayout` is a pure function of (logical lines, width, wrap-enabled).
//! It records the byte ranges each logical row breaks into and the first visual
//! row index for each logical row, so renderers can iterate visual rows without
//! mutating the buffer and so coordinate translations (logical ↔ visual) are
//! single-source.

use crate::buffer::Buffer;
use crate::wrap::wrap_line_ranges;
use unicode_width::UnicodeWidthStr;

/// Mapping from logical lines to visual rows at a specific width.
#[derive(Clone, Debug, Default)]
pub struct WrappedLayout {
    /// Byte ranges within each logical line. `chunks_per_row[crow]` is the list
    /// of `(start, end)` byte offsets in `lines[crow]` that each become one
    /// visual row.
    chunks_per_row: Vec<Vec<(usize, usize)>>,
    /// `row_starts[crow]` is the absolute visual-row index of the first chunk
    /// of logical row `crow`.
    row_starts: Vec<usize>,
    /// Widest chunk for each logical row, in terminal cells.
    row_max_widths: Vec<u16>,
    /// Total visual row count.
    visual_count: usize,
    /// Widest visual row, in terminal cells. Drives horizontal scroll math:
    /// callers clamp `scroll_left` to `max_row_width - viewport_cols` so the
    /// content never pans past its rightmost cell. Pre-formatted (non-wrapped)
    /// rows can exceed the viewport — that's exactly what `scroll_left` is for.
    max_row_width: u16,
}

impl WrappedLayout {
    /// Build a layout for `lines` at `width`. When `wrap` is false every logical
    /// row contributes exactly one identity chunk regardless of width.
    pub fn from_lines(lines: &[String], width: u16, wrap: bool) -> Self {
        Self::from_lines_with(lines, width, |_| wrap)
    }

    /// Build a layout against `buf`'s lines, consulting each row's
    /// `decoration_at(row).pre_formatted` flag. Pre-formatted rows always
    /// contribute an identity chunk so the producer's layout (parser output,
    /// markdown tables, diff hunks) is preserved verbatim.
    pub fn from_buffer(buf: &Buffer, width: u16, wrap: bool) -> Self {
        let _perf = smelt_perf::perf::begin("wrap_layout:from_buffer");
        let line_count = buf.line_count();
        let mut chunks_per_row: Vec<Vec<(usize, usize)>> = Vec::with_capacity(line_count);
        let mut row_starts: Vec<usize> = Vec::with_capacity(line_count);
        let mut row_max_widths: Vec<u16> = Vec::with_capacity(line_count);
        let mut visual_count = 0usize;
        let mut max_row_width: usize = 0;

        // Fast path: when wrap is disabled, every row is a single identity chunk.
        // We only need line lengths, not the actual text.
        if !wrap {
            for idx in 0..line_count {
                row_starts.push(visual_count);
                let line = buf.get_line(idx).unwrap_or_default();
                let line_len = line.len();
                chunks_per_row.push(vec![(0, line_len)]);
                let w = UnicodeWidthStr::width(line);
                row_max_widths.push(w.min(u16::MAX as usize) as u16);
                if w > max_row_width {
                    max_row_width = w;
                }
                visual_count += 1;
            }
            if line_count == 0 {
                chunks_per_row.push(vec![(0, 0)]);
                row_starts.push(0);
                row_max_widths.push(0);
                visual_count = 1;
            }
            return Self {
                chunks_per_row,
                row_starts,
                row_max_widths,
                visual_count,
                max_row_width: max_row_width.min(u16::MAX as usize) as u16,
            };
        }

        for idx in 0..line_count {
            row_starts.push(visual_count);
            let line = buf.get_line(idx).unwrap_or_default();
            let chunks = if buf.decoration_at(idx).pre_formatted || line.is_empty() {
                vec![(0, line.len())]
            } else {
                wrap_line_ranges(line, width as usize)
            };
            let mut row_max = 0usize;
            for &(s, e) in &chunks {
                let w = UnicodeWidthStr::width(&line[s..e]);
                if w > row_max {
                    row_max = w;
                }
                if w > max_row_width {
                    max_row_width = w;
                }
            }
            visual_count += chunks.len();
            chunks_per_row.push(chunks);
            row_max_widths.push(row_max.min(u16::MAX as usize) as u16);
        }
        if line_count == 0 {
            chunks_per_row.push(vec![(0, 0)]);
            row_starts.push(0);
            row_max_widths.push(0);
            visual_count = 1;
        }
        Self {
            chunks_per_row,
            row_starts,
            row_max_widths,
            visual_count,
            max_row_width: max_row_width.min(u16::MAX as usize) as u16,
        }
    }

    fn from_lines_with<F: Fn(usize) -> bool>(lines: &[String], width: u16, row_wraps: F) -> Self {
        let mut chunks_per_row: Vec<Vec<(usize, usize)>> = Vec::with_capacity(lines.len());
        let mut row_starts: Vec<usize> = Vec::with_capacity(lines.len());
        let mut row_max_widths: Vec<u16> = Vec::with_capacity(lines.len());
        let mut visual_count = 0usize;
        let mut max_row_width: usize = 0;
        for (idx, line) in lines.iter().enumerate() {
            row_starts.push(visual_count);
            let chunks = if !row_wraps(idx) || line.is_empty() {
                vec![(0, line.len())]
            } else {
                wrap_line_ranges(line, width as usize)
            };
            let mut row_max = 0usize;
            for &(s, e) in &chunks {
                let w = UnicodeWidthStr::width(&line[s..e]);
                if w > row_max {
                    row_max = w;
                }
                if w > max_row_width {
                    max_row_width = w;
                }
            }
            visual_count += chunks.len();
            chunks_per_row.push(chunks);
            row_max_widths.push(row_max.min(u16::MAX as usize) as u16);
        }
        if lines.is_empty() {
            chunks_per_row.push(vec![(0, 0)]);
            row_starts.push(0);
            row_max_widths.push(0);
            visual_count = 1;
        }
        Self {
            chunks_per_row,
            row_starts,
            row_max_widths,
            visual_count,
            max_row_width: max_row_width.min(u16::MAX as usize) as u16,
        }
    }

    /// Rebuild only logical rows from `start` to the end of `buf`, preserving
    /// the already-computed prefix. This is useful for append/replace-suffix
    /// edits such as transcript streaming, where rescanning a long immutable
    /// prefix dominates frame time.
    pub fn replace_suffix_from_buffer(
        &mut self,
        buf: &Buffer,
        start: usize,
        width: u16,
        wrap: bool,
    ) {
        let line_count = buf.line_count();
        let start = start.min(self.chunks_per_row.len()).min(line_count);
        let visual_count = self
            .row_starts
            .get(start)
            .copied()
            .unwrap_or(self.visual_count);
        self.chunks_per_row.truncate(start);
        self.row_starts.truncate(start);
        self.row_max_widths.truncate(start);
        self.visual_count = visual_count;
        let mut max_row_width = self.row_max_widths.iter().copied().max().unwrap_or(0);

        for idx in start..line_count {
            self.row_starts.push(self.visual_count);
            let line = buf.get_line(idx).unwrap_or_default();
            let chunks = if !wrap || buf.decoration_at(idx).pre_formatted || line.is_empty() {
                vec![(0, line.len())]
            } else {
                wrap_line_ranges(line, width as usize)
            };
            let row_max = chunks
                .iter()
                .map(|&(s, e)| UnicodeWidthStr::width(&line[s..e]).min(u16::MAX as usize) as u16)
                .max()
                .unwrap_or(0);
            self.visual_count += chunks.len();
            self.chunks_per_row.push(chunks);
            self.row_max_widths.push(row_max);
            max_row_width = max_row_width.max(row_max);
        }

        if self.chunks_per_row.is_empty() {
            self.chunks_per_row.push(vec![(0, 0)]);
            self.row_starts.push(0);
            self.row_max_widths.push(0);
            self.visual_count = 1;
            max_row_width = 0;
        }
        self.max_row_width = max_row_width;
    }

    pub fn visual_count(&self) -> usize {
        self.visual_count
    }

    pub fn logical_count(&self) -> usize {
        self.chunks_per_row.len()
    }

    /// Widest visual row in terminal cells. Used to clamp `scroll_left` so
    /// horizontal pan stops at the last content column.
    pub fn max_row_width(&self) -> u16 {
        self.max_row_width
    }

    /// Visual-row slices of `lines`. One iteration step per visual row, in order.
    pub fn visual_lines<'a>(&'a self, lines: &'a [String]) -> impl Iterator<Item = &'a str> + 'a {
        self.chunks_per_row
            .iter()
            .enumerate()
            .flat_map(move |(crow, chunks)| {
                let line = lines.get(crow).map(String::as_str).unwrap_or("");
                chunks.iter().map(move |&(s, e)| &line[s..e])
            })
    }

    /// Random-access visual-row lookup. `None` when `vrow >= visual_count`.
    pub fn visual_line<'a>(&self, lines: &'a [String], vrow: usize) -> Option<&'a str> {
        let (crow, chunk_idx) = self.logical_at_visual(vrow)?;
        let (s, e) = *self.chunks_per_row.get(crow)?.get(chunk_idx)?;
        lines.get(crow).map(|l| &l.as_str()[s..e])
    }

    /// Byte ranges for the chunks of logical row `crow`.
    pub fn chunks_of(&self, crow: usize) -> &[(usize, usize)] {
        self.chunks_per_row
            .get(crow)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// First visual-row index of logical row `crow`.
    pub fn row_start(&self, crow: usize) -> Option<usize> {
        self.row_starts.get(crow).copied()
    }

    /// Reverse-map a visual row to `(logical_row, chunk_idx)`.
    pub fn logical_at_visual(&self, vrow: usize) -> Option<(usize, usize)> {
        if vrow >= self.visual_count {
            return None;
        }
        let crow = self
            .row_starts
            .partition_point(|&s| s <= vrow)
            .saturating_sub(1);
        let chunk_idx = vrow - self.row_starts[crow];
        Some((crow, chunk_idx))
    }

    /// Project a logical `(row, byte_col)` to its visual `(row, byte_col)`. The
    /// returned column is the byte offset inside the visual row's content (still
    /// pre-`byte_to_cell`). Out-of-range inputs clamp to the last chunk.
    pub fn visual_for_logical(&self, crow: usize, byte_col: usize) -> (usize, usize) {
        let chunks = match self.chunks_per_row.get(crow) {
            Some(c) if !c.is_empty() => c,
            _ => return (self.row_starts.get(crow).copied().unwrap_or(0), 0),
        };
        for (i, &(s, e)) in chunks.iter().enumerate() {
            if byte_col < e || (s == e && byte_col == s) {
                let new_col = byte_col.saturating_sub(s);
                return (self.row_starts[crow] + i, new_col);
            }
        }
        let last = chunks.len() - 1;
        let (ls, le) = chunks[last];
        (self.row_starts[crow] + last, le - ls)
    }

    /// Project a visual `(row, byte_col_in_visual)` back to a logical
    /// `(row, byte_col)`. Out-of-range visual row falls back to the buffer's
    /// last logical row.
    pub fn logical_for_visual(&self, vrow: usize, byte_col: usize) -> (usize, usize) {
        let Some((crow, chunk_idx)) = self.logical_at_visual(vrow) else {
            let last = self.chunks_per_row.len().saturating_sub(1);
            return (last, 0);
        };
        let chunks = &self.chunks_per_row[crow];
        let (s, _e) = chunks[chunk_idx];
        (crow, s + byte_col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_disabled_is_identity() {
        let ls = lines(&["hello world", "abc"]);
        let layout = WrappedLayout::from_lines(&ls, 5, false);
        assert_eq!(layout.visual_count(), 2);
        let visuals: Vec<&str> = layout.visual_lines(&ls).collect();
        assert_eq!(visuals, vec!["hello world", "abc"]);
    }

    #[test]
    fn wrap_splits_long_lines() {
        let ls = lines(&["hello world"]);
        let layout = WrappedLayout::from_lines(&ls, 5, true);
        assert_eq!(layout.visual_count(), 2);
        let visuals: Vec<&str> = layout.visual_lines(&ls).collect();
        assert_eq!(visuals, vec!["hello", "world"]);
    }

    #[test]
    fn logical_at_visual_round_trips_row_start() {
        let ls = lines(&["aaa", "bbbbb bbbbb", "ccc"]);
        let layout = WrappedLayout::from_lines(&ls, 5, true);
        assert_eq!(layout.logical_at_visual(0), Some((0, 0)));
        assert_eq!(layout.logical_at_visual(1), Some((1, 0)));
        assert_eq!(layout.logical_at_visual(2), Some((1, 1)));
        assert_eq!(layout.logical_at_visual(3), Some((2, 0)));
        assert_eq!(layout.logical_at_visual(4), None);
    }

    #[test]
    fn empty_lines_keep_one_empty_chunk() {
        let ls = lines(&[""]);
        let layout = WrappedLayout::from_lines(&ls, 5, true);
        assert_eq!(layout.visual_count(), 1);
        assert_eq!(layout.chunks_of(0), &[(0, 0)]);
    }

    #[test]
    fn max_row_width_tracks_widest_visual_row_when_wrap_disabled() {
        let ls = lines(&["short", "much longer line"]);
        let layout = WrappedLayout::from_lines(&ls, 80, false);
        assert_eq!(layout.max_row_width(), 16);
    }

    #[test]
    fn max_row_width_clamps_to_wrap_width_when_wrap_enabled() {
        let ls = lines(&["aaaa aaaa aaaa"]);
        let layout = WrappedLayout::from_lines(&ls, 5, true);
        // Each wrapped chunk is ≤ 5 cells.
        assert!(layout.max_row_width() <= 5);
    }

    #[test]
    fn max_row_width_counts_cells_not_bytes_for_wide_chars() {
        // CJK glyphs are width-2 in terminal cells.
        let ls = lines(&["漢字"]);
        let layout = WrappedLayout::from_lines(&ls, 80, false);
        assert_eq!(layout.max_row_width(), 4);
    }

    #[test]
    fn empty_input_yields_single_empty_visual_row() {
        let ls: Vec<String> = Vec::new();
        let layout = WrappedLayout::from_lines(&ls, 5, true);
        assert_eq!(layout.visual_count(), 1);
        assert_eq!(layout.logical_count(), 1);
    }

    #[test]
    fn pre_formatted_row_skips_wrap_even_when_window_wraps() {
        use crate::buffer::{BufCreateOpts, BufId, Buffer, LineDecoration};
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec![
            "this row is far too long for the narrow width".to_string(),
            "wrappable row that should split".to_string(),
        ]);
        let pre = LineDecoration {
            pre_formatted: true,
            ..LineDecoration::default()
        };
        buf.set_decoration(0, pre);
        let layout = WrappedLayout::from_buffer(&buf, 5, true);
        assert_eq!(
            layout.chunks_of(0).len(),
            1,
            "pre_formatted row must not wrap"
        );
        assert!(
            layout.chunks_of(1).len() > 1,
            "neighbouring non-pre_formatted row should still wrap"
        );
    }

    #[test]
    fn replace_suffix_from_buffer_matches_full_rebuild() {
        use crate::buffer::{BufCreateOpts, BufId, Buffer};
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec![
            "stable prefix row".to_string(),
            "another stable prefix row".to_string(),
            "old tail".to_string(),
        ]);
        let mut incremental = WrappedLayout::from_buffer(&buf, 8, true);

        buf.set_lines(
            2,
            3,
            vec![
                "new tail wraps differently".to_string(),
                "short".to_string(),
            ],
        );
        incremental.replace_suffix_from_buffer(&buf, 2, 8, true);
        let full = WrappedLayout::from_buffer(&buf, 8, true);

        assert_eq!(incremental.visual_count(), full.visual_count());
        assert_eq!(incremental.max_row_width(), full.max_row_width());
        for row in 0..buf.line_count() {
            assert_eq!(incremental.chunks_of(row), full.chunks_of(row));
            assert_eq!(incremental.row_start(row), full.row_start(row));
        }
    }
}
