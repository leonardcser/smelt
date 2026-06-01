//! `Block::Exec` renderer - one shell-escape command + (optional)
//! captured output. Shares the `SmeltUserBg` panel chrome with
//! `Block::User`; the leading `!` is painted in `SmeltExecPrefix`.

use smelt_core::content::builder::LineBuilder;
use smelt_core::style::Color;
use smelt_core::theme::intern;

use super::metrics::chrome_text_width;
use super::tools::render_wrapped_output;

pub(super) fn render(out: &mut LineBuilder, command: &str, output: &str, width: usize) -> u16 {
    let text_w = chrome_text_width(width);
    let exec_fg = out
        .theme()
        .resolve(intern("SmeltExecPrefix"))
        .fg
        .unwrap_or(Color::Reset);
    let lines = [format!("!{command}")];

    let mut rows = super::chrome::render(out, &lines, text_w, |out, chunk, idx| {
        // `!` is ASCII, so byte 0 is a safe char boundary on chunk 0.
        if idx == 0 && chunk.starts_with('!') {
            out.push_fg(exec_fg);
            out.print("!");
            out.pop_style();
            out.print(&chunk[1..]);
        } else {
            out.print(chunk);
        }
    });

    if !output.is_empty() {
        rows += render_wrapped_output(out, output, false, width);
    }
    rows
}
