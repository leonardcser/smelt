//! `Block::CodeLine` renderer — one streamed line of a fenced code block.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::render_code_block;

pub(super) fn render(out: &mut LineBuilder, content: &str, lang: &str, width: usize) -> u16 {
    render_code_block(out, &[content], lang, width, false, None, false)
}
