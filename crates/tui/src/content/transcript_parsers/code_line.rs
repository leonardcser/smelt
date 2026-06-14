//! `Block::CodeLine` renderer - one streamed line of a fenced code block.

#[cfg(test)]
use smelt_core::content::builder::LineBuilder;
#[cfg(test)]
use smelt_core::content::code_block::parse_code_block;
#[cfg(test)]
use smelt_core::content::highlight::render_code_block;

#[cfg(test)]
pub(crate) fn render(out: &mut LineBuilder, content: &str, lang: &str, width: usize) -> u16 {
    let block = parse_code_block(&[content], lang);
    render_code_block(out, &block, width, false, None, false)
}
