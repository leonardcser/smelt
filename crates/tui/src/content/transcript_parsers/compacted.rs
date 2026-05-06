//! `Block::Compacted` renderer — a hr-flanked label with the
//! summary text underneath, all dim.

use smelt_core::content::builder::LineBuilder;

use super::markdown::render_markdown_inner;

pub(super) fn render(out: &mut LineBuilder, summary: &str, width: usize) -> u16 {
    let label = " compacted ";
    let label_len = label.len();
    let remaining = width.saturating_sub(label_len);
    let left = remaining / 2;
    let right = remaining - left;
    out.push_dim();
    out.print_gutter(&"─".repeat(left));
    out.print_gutter(label);
    out.print_gutter(&"─".repeat(right));
    out.pop_style();
    out.newline();
    1 + render_markdown_inner(out, summary, width, "", true, None)
}
