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
pub(crate) mod transcript_search_text;

pub(crate) use smelt_core::content::{display_safe_char, display_safe_text};

pub(crate) fn display_cell_width(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        smelt_buffer::text::byte_to_cell(text, text.len())
    }
}

pub(crate) fn estimate_text_rows(text: &str, width: u16) -> crate::smelt_edit::RowIndex {
    estimate_text_rows_with_first_line_prefix("", text, width)
}

pub(crate) fn estimate_text_rows_with_first_line_prefix(
    prefix: &str,
    text: &str,
    width: u16,
) -> crate::smelt_edit::RowIndex {
    let width = usize::from(width.max(1));
    let prefix_cells = display_cell_width(prefix);
    let mut rows = 0;
    let mut first = true;
    for line in text.lines() {
        let mut cells = display_cell_width(line);
        if first {
            cells = cells.saturating_add(prefix_cells);
            first = false;
        }
        rows += cells.max(1).div_ceil(width) as crate::smelt_edit::RowIndex;
    }
    rows.max(1)
}

use crossterm::terminal;

pub(crate) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub(crate) fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}
