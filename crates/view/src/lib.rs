//! Transcript and prompt presentation: parses prompt text into spans, lays
//! transcript blocks out into display rows, and renders them into buffers.

/// Route the view test binary through the counting global allocator so
/// allocation-accounting tests see real numbers.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: smelt_core::allocator::Allocator = smelt_core::allocator::ALLOCATOR;

pub mod display_layout;
pub mod display_renderers;
pub mod prompt_parser;
pub mod prompt_spans;
pub mod source_view;
pub mod transcript_buf;
pub mod transcript_scene;

pub use smelt_transcript::content::{display_safe_char, display_safe_text};

pub fn estimate_text_rows(text: &str, width: u16) -> smelt_edit::RowIndex {
    let width = usize::from(width.max(1));
    text.lines()
        .map(|line| {
            let cells = if line.is_ascii() {
                line.len()
            } else {
                smelt_buffer::text::byte_to_cell(line, line.len())
            };
            cells.max(1).div_ceil(width) as smelt_edit::RowIndex
        })
        .sum::<smelt_edit::RowIndex>()
        .max(1)
}

pub fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

pub fn term_height() -> usize {
    crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(24)
}
