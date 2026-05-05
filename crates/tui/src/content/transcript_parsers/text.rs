//! `Block::Text` renderer — thin wrapper over the markdown layout.

use smelt_core::content::layout_out::SpanCollector;

use super::markdown::render_markdown_inner;

pub(super) fn render(out: &mut SpanCollector, content: &str, width: usize) -> u16 {
    render_markdown_inner(out, content, width, "", false, None)
}
