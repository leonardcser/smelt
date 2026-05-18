//! `Block::User` renderer.

use smelt_core::buffer::SpanMeta;
use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::wrap::wrap_line;
use smelt_core::style::Color;
use smelt_core::theme::intern;

/// Preprocessed user message layout: tab-expanded, blank-trimmed lines with a computed `block_w`.
pub struct UserBlockGeometry {
    pub lines: Vec<String>,
    pub block_w: usize,
}

impl UserBlockGeometry {
    pub fn new(text: &str, text_w: usize) -> Self {
        let all_lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
        let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
        let end = all_lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        let lines: Vec<String> = all_lines[start..end]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        let wraps = lines.iter().any(|l| display_width(l) > text_w);
        let multiline = lines.len() > 1 || wraps;
        let block_w = if multiline {
            if wraps {
                text_w + 1
            } else {
                lines.iter().map(|l| display_width(l)).max().unwrap_or(0) + 1
            }
        } else {
            0
        };
        Self { lines, block_w }
    }
}

pub(super) fn render(
    out: &mut LineBuilder,
    text: &str,
    image_labels: &[String],
    width: usize,
) -> u16 {
    let is_command = smelt_core::commands::is_command(text.trim());
    let text_w = width.saturating_sub(2).max(1);
    let geom = UserBlockGeometry::new(text, text_w);
    let user_bg = intern("SmeltUserBg");
    let user_bg_color = out.theme().resolve(user_bg).bg.unwrap_or(Color::Reset);
    let mut rows = 0u16;
    let pad_meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    // Blank padding rows mirror the content-row column layout — 1-col chrome
    // pad + 1-col selectable cell — so a multi-line selection lands its
    // 1-cell virtual span at col 1, lined up with where content starts on
    // neighbor rows. `copy_as = Some("")` keeps the placeholder cell out of
    // the yank output so a blank row stays blank in the clipboard. The bg
    // for the remaining cells comes from `fill_line_bg` (no inline pad text)
    // so the row's text content stays minimal.
    let blank_anchor_meta = SpanMeta {
        selectable: true,
        copy_as: Some(String::new()),
    };
    let blank_row = |out: &mut LineBuilder| {
        out.set_hl(user_bg);
        out.print_with_meta(" ", pad_meta.clone());
        out.print_with_meta(" ", blank_anchor_meta.clone());
        out.reset_style();
        out.fill_line_bg(user_bg_color);
        out.newline();
    };
    blank_row(out);
    rows += 1;
    for logical_line in &geom.lines {
        if logical_line.is_empty() {
            blank_row(out);
            rows += 1;
            continue;
        }
        let chunks = wrap_line(logical_line, text_w);
        if chunks.len() > 1 {
            out.mark_wrapped();
        }
        for chunk in &chunks {
            out.set_hl(user_bg);
            out.print_with_meta(" ", pad_meta.clone());
            out.set_bold();
            print_highlights(out, chunk, image_labels, is_command);
            out.set_hl(user_bg);
            out.pad_row_to_layout_width(pad_meta.clone());
            out.reset_style();
            out.newline();
            rows += 1;
        }
    }
    blank_row(out);
    rows += 1;
    rows
}

fn print_highlights(out: &mut LineBuilder, text: &str, image_labels: &[String], is_command: bool) {
    // Push only the accent foreground so the active user-message background
    // keeps painting underneath. `push_hl` would swap the whole highlight
    // group and lose the bg.
    let accent_fg = out
        .theme()
        .resolve(intern("SmeltAccent"))
        .fg
        .unwrap_or(smelt_core::style::Color::Reset);

    if is_command {
        out.push_fg(accent_fg);
        out.print(text);
        out.pop_style();
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    let flush = |out: &mut LineBuilder, plain: &mut String| {
        if !plain.is_empty() {
            let s = std::mem::take(plain);
            out.print(&s);
        }
    };

    let accent = |out: &mut LineBuilder, token: String| {
        out.push_fg(accent_fg);
        out.print(&token);
        out.pop_style();
    };

    while i < len {
        if chars[i] == '[' {
            let remaining: String = chars[i..].iter().collect();
            if let Some(label) = image_labels
                .iter()
                .find(|l| remaining.starts_with(l.as_str()))
            {
                flush(out, &mut plain);
                accent(out, label.clone());
                i += label.chars().count();
                continue;
            }
        }

        if let Some((token, end)) = smelt_core::content::selection::try_at_ref(&chars, i) {
            flush(out, &mut plain);
            accent(out, token);
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush(out, &mut plain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::buffer::{BufCreateOpts, BufId, Buffer};
    use smelt_core::content::builder::test_util::read_buffer;
    use smelt_core::style::{Color, Style};
    use smelt_core::theme::Theme;
    use std::sync::Mutex;

    static RESOLVER_GUARD: Mutex<()> = Mutex::new(());
    const ACCENT: Color = Color::AnsiValue(99);
    const USER_BG: Color = Color::AnsiValue(234);

    fn themed() -> Theme {
        let mut t = Theme::default();
        t.set(
            "SmeltAccent",
            Style {
                fg: Some(ACCENT),
                ..Style::default()
            },
        );
        t.set(
            "SmeltUserBg",
            Style {
                bg: Some(USER_BG),
                ..Style::default()
            },
        );
        t
    }

    fn render_content_row_styles(text: &str) -> Vec<Style> {
        let theme = themed();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let rows = {
            let mut out = LineBuilder::new(&mut buf, &theme, 40);
            let r = render(&mut out, text, &[], 40);
            out.finish();
            r as usize
        };
        let lines = read_buffer(&buf, &theme, rows);
        // Content rows sit between the leading and trailing blank padding rows.
        lines[1].spans.iter().map(|s| s.style).collect()
    }

    #[test]
    fn registered_slash_command_paints_accent_fg() {
        let _g = RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|name| name == "commit");
        let styles = render_content_row_styles("/commit");
        assert!(
            styles.iter().any(|s| s.fg == Some(ACCENT)),
            "expected an accent-fg span for /commit, got {styles:?}"
        );
    }

    #[test]
    fn unregistered_slash_text_stays_un_accented() {
        let _g = RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|_| false);
        let styles = render_content_row_styles("/notreal");
        assert!(
            styles.iter().all(|s| s.fg != Some(ACCENT)),
            "non-command should not paint accent fg, got {styles:?}"
        );
    }
}
