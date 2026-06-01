//! `Block::ProcessStatus` renderer.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::wrap::wrap_line;

pub(super) fn render(out: &mut LineBuilder, text: &str) -> u16 {
    let chunks = wrap_line(text, out.layout_width() as usize);
    if chunks.len() > 1 {
        out.mark_wrapped();
    }
    let mut style = out.theme().get("SmeltProcess");
    style.bg = None;
    style.italic = true;
    let mut rows = 0u16;
    for chunk in chunks {
        out.push(None, style);
        out.print(&chunk);
        out.pop_style();
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}
