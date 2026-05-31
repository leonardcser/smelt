//! `Block::Mode` renderer.

use smelt_core::content::builder::LineBuilder;
use smelt_core::theme::intern;

pub(super) fn render(out: &mut LineBuilder, text: &str, icon: &str, hl_group: &str) -> u16 {
    out.push_hl(intern(hl_group));
    out.print(icon);
    out.print(text);
    out.pop_style();
    out.newline();
    1
}
