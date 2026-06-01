use crate::text;
use crate::Theme;
use smelt_buffer::buffer::{Buffer, CopyOutput, LineDecoration, Span, VirtualText};
use smelt_buffer::wrap_layout::WrappedLayout;
use std::ops::Range;

pub type RowIndex = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocPos {
    BufferByte(usize),
    RowCol {
        row: RowIndex,
        col: usize,
    },
    Transcript {
        block_id: u64,
        local_row: RowIndex,
        col: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewAnchor {
    Buffer {
        revision: u64,
        row: RowIndex,
        byte: usize,
    },
    Transcript {
        block_id: u64,
        local_row: RowIndex,
        fallback_fraction: f64,
    },
}

#[derive(Clone, Debug, Default)]
pub struct DisplayRow {
    pub text: String,
    pub highlights: Vec<Span>,
    pub decoration: LineDecoration,
    pub virtual_text: Vec<VirtualText>,
}

pub trait Document {
    fn revision(&self) -> u64;
    fn total_rows(&mut self, width: u16, theme: &Theme) -> RowIndex;
    fn rows(&mut self, range: Range<RowIndex>, width: u16, theme: &Theme) -> Vec<DisplayRow>;

    fn row_to_pos(&mut self, row: RowIndex, col: usize, width: u16, theme: &Theme) -> DocPos;
    fn pos_to_row_col(&mut self, pos: &DocPos, width: u16, theme: &Theme) -> (RowIndex, usize);

    fn copy_range(&mut self, start: &DocPos, end: &DocPos) -> CopyOutput;
    fn word_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
    fn line_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
    fn block_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)>;
}

pub struct BufferDocument<'a> {
    buf: &'a Buffer,
    layout: WrappedLayout,
    layout_key: Option<(u64, u16, bool)>,
    wrap: bool,
}

impl<'a> BufferDocument<'a> {
    pub fn new(buf: &'a Buffer, wrap: bool) -> Self {
        Self {
            buf,
            layout: WrappedLayout::default(),
            layout_key: None,
            wrap,
        }
    }

    fn ensure_layout(&mut self, width: u16) {
        let key = (self.buf.changedtick(), width, self.wrap);
        if self.layout_key == Some(key) {
            return;
        }
        self.layout = WrappedLayout::from_buffer(self.buf, width, self.wrap);
        self.layout_key = Some(key);
    }

    fn cpos_at_visual(&mut self, row: RowIndex, col: usize, width: u16) -> usize {
        self.ensure_layout(width);
        if self.buf.lines().is_empty() {
            return 0;
        }
        let vrow = row_to_usize(row);
        let last_logical = self.buf.lines().len() - 1;
        let Some((logical_row, chunk_idx)) = self.layout.logical_at_visual(vrow) else {
            return self.buf.byte_at_display_pos(last_logical, 0);
        };
        let chunk_start = self
            .layout
            .chunks_of(logical_row)
            .get(chunk_idx)
            .map(|&(start, _)| start)
            .unwrap_or(0);
        let line = self.buf.get_line(logical_row).unwrap_or("");
        let chunk_cell = smelt_buffer::text::byte_to_cell(line, chunk_start);
        self.buf.byte_at_display_pos(logical_row, chunk_cell + col)
    }

    fn visual_for_cpos(&mut self, cpos: usize, width: u16) -> (RowIndex, usize) {
        self.ensure_layout(width);
        let (logical_row, byte_col) = self.buf.display_byte_pos(cpos);
        let (visual_row, byte_in_chunk) = self.layout.visual_for_logical(logical_row, byte_col);
        let line = self
            .layout
            .visual_line(self.buf.lines(), visual_row)
            .unwrap_or("");
        (
            visual_row as RowIndex,
            smelt_buffer::text::byte_to_cell(line, byte_in_chunk),
        )
    }

    fn buffer_range(&mut self, start: &DocPos, end: &DocPos) -> Option<Range<usize>> {
        let s = match start {
            DocPos::BufferByte(byte) => *byte,
            _ => return None,
        };
        let e = match end {
            DocPos::BufferByte(byte) => *byte,
            _ => return None,
        };
        (s != e).then_some(s.min(e)..s.max(e))
    }
}

impl Document for BufferDocument<'_> {
    fn revision(&self) -> u64 {
        self.buf.changedtick()
    }

    fn total_rows(&mut self, width: u16, _theme: &Theme) -> RowIndex {
        self.ensure_layout(width);
        self.layout.visual_count() as RowIndex
    }

    fn rows(&mut self, range: Range<RowIndex>, width: u16, _theme: &Theme) -> Vec<DisplayRow> {
        self.ensure_layout(width);
        let mut rows = Vec::new();
        let start = row_to_usize(range.start);
        let end = row_to_usize(range.end).min(self.layout.visual_count());
        let mut highlights = Vec::new();
        let mut virtual_text = Vec::new();
        for visual_row in start..end {
            let Some((logical_row, chunk_idx)) = self.layout.logical_at_visual(visual_row) else {
                continue;
            };
            highlights.clear();
            self.buf.highlights_at_into(logical_row, &mut highlights);
            virtual_text.clear();
            if chunk_idx == 0 {
                self.buf
                    .virtual_text_at_into(logical_row, &mut virtual_text);
            }
            rows.push(DisplayRow {
                text: self
                    .layout
                    .visual_line(self.buf.lines(), visual_row)
                    .unwrap_or("")
                    .to_string(),
                highlights: highlights.clone(),
                decoration: self.buf.decoration_at(logical_row).clone(),
                virtual_text: virtual_text.clone(),
            });
        }
        rows
    }

    fn row_to_pos(&mut self, row: RowIndex, col: usize, width: u16, _theme: &Theme) -> DocPos {
        DocPos::BufferByte(self.cpos_at_visual(row, col, width))
    }

    fn pos_to_row_col(&mut self, pos: &DocPos, width: u16, _theme: &Theme) -> (RowIndex, usize) {
        match pos {
            DocPos::BufferByte(byte) => self.visual_for_cpos(*byte, width),
            DocPos::RowCol { row, col } => (*row, *col),
            DocPos::Transcript { local_row, col, .. } => (*local_row, *col),
        }
    }

    fn copy_range(&mut self, start: &DocPos, end: &DocPos) -> CopyOutput {
        self.buffer_range(start, end)
            .map(|range| self.buf.copy_range(range))
            .unwrap_or_default()
    }

    fn word_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)> {
        let DocPos::BufferByte(byte) = pos else {
            return None;
        };
        let text = self.buf.text();
        let (start, end) = text::big_word_range_at_transparent(&text, *byte, &[])?;
        Some((DocPos::BufferByte(start), DocPos::BufferByte(end)))
    }

    fn line_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)> {
        let DocPos::BufferByte(byte) = pos else {
            return None;
        };
        let text = self.buf.text();
        let hard_breaks = crate::hard_breaks_for_text(&text);
        let (start, end) = text::line_range_at(&text, *byte, &hard_breaks)?;
        Some((DocPos::BufferByte(start), DocPos::BufferByte(end)))
    }

    fn block_range_at(&mut self, pos: &DocPos) -> Option<(DocPos, DocPos)> {
        let DocPos::BufferByte(byte) = pos else {
            return None;
        };
        let (row, _) = self.buf.display_cursor_pos(*byte);
        if !self.buf.decoration_at(row).block_selectable {
            return None;
        }
        let mut first = row;
        while first > 0 && self.buf.decoration_at(first - 1).block_selectable {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < self.buf.line_count() && self.buf.decoration_at(last + 1).block_selectable
        {
            last += 1;
        }
        let start = self.buf.byte_at_display_pos(first, 0);
        let end = self.buf.byte_at_display_pos(last, 0) + self.buf.get_line(last)?.len();
        Some((DocPos::BufferByte(start), DocPos::BufferByte(end)))
    }
}

pub(crate) fn row_to_usize(row: RowIndex) -> usize {
    row.min(usize::MAX as RowIndex) as usize
}
