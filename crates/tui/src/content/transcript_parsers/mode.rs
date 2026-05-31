//! `Block::Mode` renderer.

use smelt_core::content::builder::LineBuilder;

pub(super) fn render(out: &mut LineBuilder, text: &str, icon: &str, hl_group: &str) -> u16 {
    let mut style = out.theme().get(hl_group);
    style.bg = None;
    style.italic = false;
    out.push(None, style);
    out.print(icon);
    style.italic = true;
    out.push(None, style);
    out.print(text);
    out.pop_style();
    out.pop_style();
    out.newline();
    1
}
