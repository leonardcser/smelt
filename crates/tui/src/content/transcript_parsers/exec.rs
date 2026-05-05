//! `Block::Exec` renderer — one shell-escape command + (optional)
//! captured output.

use smelt_core::content::display::{ColorRole, ColorValue, NamedColor, SpanStyle};
use smelt_core::content::layout_out::SpanCollector;

use super::tools::render_wrapped_output;

pub(super) fn render(out: &mut SpanCollector, command: &str, output: &str, width: usize) -> u16 {
    let char_len = command.chars().count() + 1;
    let pad_width = (char_len + 2).min(width);
    let trailing = pad_width.saturating_sub(char_len + 1);
    out.push_style(SpanStyle {
        bg: Some(ColorValue::Role(ColorRole::UserBg)),
        fg: Some(ColorValue::Role(ColorRole::Exec)),
        bold: true,
        ..Default::default()
    });
    out.print("!");
    out.set_fg(ColorValue::Named(NamedColor::Reset));
    out.print_string(format!("{}{}", command, " ".repeat(trailing)));
    out.pop_style();
    out.newline();
    let mut rows = 1u16;
    if !output.is_empty() {
        rows += render_wrapped_output(out, output, false, width);
    }
    rows
}
