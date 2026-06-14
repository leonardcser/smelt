//! `Block::Text` renderer - thin wrapper over the markdown layout.

use smelt_core::content::builder::LineBuilder;

use super::markdown::{measure_markdown_inner, render_markdown_inner};

pub(in crate::content) fn render(out: &mut LineBuilder, content: &str, width: usize) -> u16 {
    render_markdown_inner(out, content, width, "", false, None)
}

pub(in crate::content) fn measure(content: &str, width: usize) -> u16 {
    measure_markdown_inner(content, width, "", false, None)
}
