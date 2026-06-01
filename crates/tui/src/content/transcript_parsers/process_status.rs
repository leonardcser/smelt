//! `Block::ProcessStatus` renderer.

use smelt_core::content::builder::LineBuilder;

pub(super) fn render(out: &mut LineBuilder, text: &str) -> u16 {
    let mut style = out.theme().get("SmeltProcess");
    style.bg = None;
    style.italic = true;
    out.push(None, style);
    out.print(text);
    out.pop_style();
    out.newline();
    1
}
