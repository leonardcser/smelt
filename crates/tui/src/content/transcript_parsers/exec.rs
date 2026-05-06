//! `Block::Exec` renderer — one shell-escape command + (optional)
//! captured output.

use smelt_core::content::builder::LineBuilder;
use smelt_core::style::{Color, Style};
use smelt_core::theme::role_hl;

use super::tools::render_wrapped_output;

pub(super) fn render(out: &mut LineBuilder, command: &str, output: &str, width: usize) -> u16 {
    let char_len = command.chars().count() + 1;
    let pad_width = (char_len + 2).min(width);
    let trailing = pad_width.saturating_sub(char_len + 1);
    let user_bg = out
        .theme()
        .resolve(role_hl("UserBg"))
        .bg
        .unwrap_or(Color::Reset);
    let exec_fg = out
        .theme()
        .resolve(role_hl("Exec"))
        .fg
        .unwrap_or(Color::Reset);
    out.push(
        None,
        Style {
            bg: Some(user_bg),
            fg: Some(exec_fg),
            bold: true,
            ..Default::default()
        },
    );
    out.print("!");
    out.set_fg(Color::Reset);
    out.print_string(format!("{}{}", command, " ".repeat(trailing)));
    out.pop_style();
    out.newline();
    let mut rows = 1u16;
    if !output.is_empty() {
        rows += render_wrapped_output(out, output, false, width);
    }
    rows
}
