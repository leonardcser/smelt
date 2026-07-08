pub use smelt_core::content::builder;
pub use smelt_core::content::highlight;
pub(crate) mod prompt_spans;

pub(crate) mod display_layout;
pub(crate) mod display_renderers;
pub(crate) mod layout;
pub(crate) mod prompt_buf;
pub(crate) mod prompt_parser;
pub(crate) mod render_plan;
pub(crate) mod source_view;
pub(crate) mod to_buffer;
pub(crate) mod transcript_buf;

pub(crate) use smelt_core::content::{display_safe_char, display_safe_text};

pub(crate) fn estimate_text_rows(text: &str, width: u16) -> crate::smelt_edit::RowIndex {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| {
            let cells = if line.is_ascii() {
                line.len()
            } else {
                smelt_buffer::text::byte_to_cell(line, line.len())
            };
            cells.max(1).div_ceil(width) as crate::smelt_edit::RowIndex
        })
        .sum::<crate::smelt_edit::RowIndex>()
        .max(1)
}

use crossterm::terminal;

pub(crate) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub(crate) fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}
