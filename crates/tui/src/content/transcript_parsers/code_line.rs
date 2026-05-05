//! `Block::CodeLine` renderer — one streamed line of a fenced code block.

use smelt_core::content::highlight::render_code_block;
use smelt_core::content::layout_out::SpanCollector;

pub(super) fn render(out: &mut SpanCollector, content: &str, lang: &str, width: usize) -> u16 {
    render_code_block(out, &[content], lang, width, false, None, false)
}
