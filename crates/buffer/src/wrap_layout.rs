//! Wrap layout: a derived view that maps a buffer's logical lines onto visual rows.
//!
//! `WrappedLayout` is a pure function of (logical lines, width, wrap-enabled).
//! It records the byte ranges each logical row breaks into and the first visual
//! row index for each logical row, so renderers can iterate visual rows without
//! mutating the buffer and so coordinate translations (logical ↔ visual) are
//! single-source.

use crate::buffer::Buffer;
use crate::wrap::wrap_line_ranges;

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
    /// Total visual row count.
    visual_count: usize,
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
        Self::from_lines_with(buf.lines(), width, |row| {
            wrap && !buf.decoration_at(row).pre_formatted
        })
    }

    fn from_lines_with<F: Fn(usize) -> bool>(lines: &[String], width: u16, row_wraps: F) -> Self {
        let mut chunks_per_row: Vec<Vec<(usize, usize)>> = Vec::with_capacity(lines.len());
        let mut row_starts: Vec<usize> = Vec::with_capacity(lines.len());
        let mut visual_count = 0usize;
        for (idx, line) in lines.iter().enumerate() {
            row_starts.push(visual_count);
            let chunks = if !row_wraps(idx) || line.is_empty() {
                vec![(0, line.len())]
            } else {
                wrap_line_ranges(line, width as usize)
            };
            visual_count += chunks.len();
            chunks_per_row.push(chunks);
        }
        if lines.is_empty() {
            chunks_per_row.push(vec![(0, 0)]);
            row_starts.push(0);
            visual_count = 1;
        }
        Self {
            chunks_per_row,
            row_starts,
            visual_count,
        }
    }

    pub fn visual_count(&self) -> usize {
        self.visual_count
    }

    pub fn logical_count(&self) -> usize {
        self.chunks_per_row.len()
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
}
