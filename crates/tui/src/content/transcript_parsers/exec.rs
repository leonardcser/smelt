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
    let command = crate::content::display_safe_text(&format!("!{command}"));
    let lines = [command];

    let mut rows = super::chrome::render(out, &lines, text_w, |out, chunk, pos| {
        // `!` is ASCII, so byte 0 is a safe char boundary on chunk 0.
        if pos.chunk_idx == 0 && chunk.starts_with('!') {
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

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::buffer::{BufCreateOpts, BufId, Buffer};
    use smelt_core::theme::Theme;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn command_controls_are_sanitized_before_wrapping() {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let command = "\0\0\x00677S7\0\0\0\0\0\0*\x001k77 @crates/term/tests/storybook/snapshots/layout::vbox_mixed_length_fill_and_min.snap @FI";
        let rows = {
            let mut out = LineBuilder::new(&mut buf, &theme, 107);
            let rows = render(&mut out, command, "", 107);
            out.finish();
            rows as usize
        };

        assert!(rows > 3, "long command should wrap inside chrome");
        for row in 0..rows {
            let line = buf.get_line(row).unwrap_or("");
            assert!(
                !line.contains('\0'),
                "control byte leaked into row {row}: {line:?}"
            );
            assert!(
                UnicodeWidthStr::width(line) <= 107,
                "row {row} overflowed transcript width: {line:?}"
            );
        }
    }
}
