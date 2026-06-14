pub use smelt_core::content::builder;
pub use smelt_core::content::highlight;
pub(crate) mod prompt_spans;

pub(crate) mod display_block;
pub(crate) mod display_cache;
pub mod display_renderers;
pub(crate) mod layout;
pub(crate) mod prompt_buf;
pub(crate) mod prompt_parser;
pub(crate) mod source_view;
pub(crate) mod to_buffer;
pub(crate) mod transcript_buf;

pub(crate) fn display_safe_char(ch: char) -> char {
    if ch != '\n' && ch.is_control() {
        '\u{FFFD}'
    } else {
        ch
    }
}

pub(crate) fn display_safe_text(text: &str) -> String {
    text.chars().map(display_safe_char).collect()
}

use crossterm::terminal;

pub(crate) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub(crate) fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}
