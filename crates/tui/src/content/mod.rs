pub use smelt_core::content::builder;
pub use smelt_core::content::highlight;
pub(crate) mod selection;

pub(crate) mod block_buffers;
pub(crate) mod layout;
pub(crate) mod prompt_buf;
pub(crate) mod prompt_parser;
pub(crate) mod to_buffer;
pub(crate) mod transcript_buf;
pub mod transcript_parsers;

use crossterm::terminal;

pub(crate) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub(crate) fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}
