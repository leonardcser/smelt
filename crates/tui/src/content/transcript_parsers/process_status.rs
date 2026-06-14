//! `Block::ProcessStatus` renderer.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::inline_line::InlineLine;

pub(super) fn measure(text: &str, width: usize) -> u16 {
    let line = InlineLine::plain(text, ());
    line.wrap_plain_ranges(width).len() as u16
}

pub(super) fn render(out: &mut LineBuilder, text: &str) -> u16 {
    let line = InlineLine::plain(text, ());
    let chunks = line.wrap_plain_ranges(out.layout_width() as usize);
    if chunks.len() > 1 {
        out.mark_wrapped();
    }
    let mut style = out.theme().get("SmeltProcess");
    style.bg = None;
    style.italic = true;
    let mut rows = 0u16;
    for (start, end) in chunks {
        let chunk = smelt_buffer::text::slice(text, start..end);
        out.push(None, style);
        out.print(chunk);
        out.pop_style();
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}
