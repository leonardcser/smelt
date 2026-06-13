//! `Block::CodeLine` renderer - one streamed line of a fenced code block.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::code_block::parse_code_block;
use smelt_core::content::highlight::render_code_block;

pub(super) fn render(out: &mut LineBuilder, content: &str, lang: &str, width: usize) -> u16 {
    let block = parse_code_block(&[content], lang);
    render_code_block(out, &block, width, false, None, false)
}
