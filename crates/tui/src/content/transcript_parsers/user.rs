//! `Block::User` renderer.

use smelt_core::content::builder::LineBuilder;
use smelt_core::theme::intern;

use super::metrics::chrome_text_width;

/// Preprocessed user message layout: tab-expanded, blank-trimmed lines.
pub(super) struct UserBlockGeometry {
    pub lines: Vec<String>,
}

impl UserBlockGeometry {
    pub(super) fn new(text: &str) -> Self {
        let all_lines: Vec<String> = text
            .lines()
            .map(|l| crate::content::display_safe_text(&l.replace('\t', "    ")))
            .collect();
        let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
        let end = all_lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        let lines: Vec<String> = all_lines[start..end]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        Self { lines }
    }
}

pub(super) fn render(
    out: &mut LineBuilder,
    text: &str,
    image_labels: &[String],
    width: usize,
) -> u16 {
    let text_w = chrome_text_width(width);
    let geom = UserBlockGeometry::new(text);
    let command_token_chars = geom
        .lines
        .first()
        .and_then(|line| smelt_core::commands::registered_command_token(line))
        .map(|token| token.chars().count())
        .unwrap_or(0);
    super::chrome::render(out, &geom.lines, text_w, |out, chunk, pos| {
        let command_prefix_chars = if pos.logical_line == 0 {
            command_token_chars
                .saturating_sub(pos.char_start)
                .min(chunk.chars().count())
        } else {
            0
        };
        print_highlights(out, chunk, image_labels, command_prefix_chars);
    })
}

fn print_highlights(
    out: &mut LineBuilder,
    text: &str,
    image_labels: &[String],
    command_prefix_chars: usize,
) {
    // Push only the accent foreground so the active user-message background
    // keeps painting underneath. `push_hl` would swap the whole highlight
    // group and lose the bg.
    let accent_fg = out
        .theme()
        .resolve(intern("SmeltAccent"))
        .fg
        .unwrap_or(smelt_core::style::Color::Reset);

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

    let take = command_prefix_chars.min(len);
    if take > 0 {
        let token: String = chars[..take].iter().collect();
        accent(out, token);
        i = take;
    }

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
    use smelt_core::content::builder::test_util::{read_buffer, TestLine};
    use smelt_core::style::{Color, Style};
    use smelt_core::theme::Theme;

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

    fn render_content_rows(text: &str, width: usize) -> Vec<TestLine> {
        let theme = themed();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let rows = {
            let mut out = LineBuilder::new(&mut buf, &theme, width as u16);
            let r = render(&mut out, text, &[], width);
            out.finish();
            r as usize
        };
        let lines = read_buffer(&buf, &theme, rows);
        // Content rows sit between the leading and trailing blank padding rows.
        lines
            .into_iter()
            .skip(1)
            .take(rows.saturating_sub(2))
            .collect()
    }

    fn render_content_row_styles(text: &str) -> Vec<Style> {
        let mut rows = render_content_rows(text, 40);
        rows.remove(0).spans.into_iter().map(|s| s.style).collect()
    }

    #[test]
    fn blank_chrome_rows_fill_layout_width() {
        let theme = themed();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let width = 40;
        let rows = {
            let mut out = LineBuilder::new(&mut buf, &theme, width);
            let r = render(&mut out, "hello\n\nworld", &[], width as usize);
            out.finish();
            r as usize
        };
        let lines = read_buffer(&buf, &theme, rows);
        assert_eq!(lines[0].text.len(), width as usize);
        assert_eq!(lines[2].text.len(), width as usize);
        assert_eq!(lines[rows - 1].text.len(), width as usize);
    }

    #[test]
    fn user_geometry_sanitizes_display_controls() {
        let geom = UserBlockGeometry::new("a\0\tb\nc\r");
        assert_eq!(
            geom.lines,
            vec!["a\u{FFFD}    b".to_string(), "c\u{FFFD}".to_string()]
        );
    }

    #[test]
    fn registered_slash_command_paints_accent_fg() {
        let _g = crate::COMMAND_RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|name| name == "commit");
        let styles = render_content_row_styles("/commit");
        assert!(
            styles.iter().any(|s| s.fg == Some(ACCENT)),
            "expected an accent-fg span for /commit, got {styles:?}"
        );
    }

    #[test]
    fn registered_slash_command_paints_only_command_token() {
        let _g = crate::COMMAND_RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|name| name == "commit");
        let rows = render_content_rows("/commit message", 80);
        let spans = &rows[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.text == "/commit" && s.style.fg == Some(ACCENT)),
            "expected only /commit to be accented, got {:?}",
            spans.iter().map(|s| (&s.text, s.style)).collect::<Vec<_>>()
        );
        assert!(
            spans
                .iter()
                .any(|s| s.text.contains("message") && s.style.fg != Some(ACCENT)),
            "expected arguments to use the normal user-message style, got {:?}",
            spans.iter().map(|s| (&s.text, s.style)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn multiline_registered_slash_command_paints_only_command_token() {
        let _g = crate::COMMAND_RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|name| name == "simplify");
        let rows = render_content_rows("/simplify first line\nsecond line", 80);
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.text == "/simplify" && s.style.fg == Some(ACCENT)),
            "expected /simplify to be accented, got {:?}",
            rows[0]
                .spans
                .iter()
                .map(|s| (&s.text, s.style))
                .collect::<Vec<_>>()
        );
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.text.contains("first line") && s.style.fg != Some(ACCENT)),
            "expected first-line arguments to stay unaccented, got {:?}",
            rows[0]
                .spans
                .iter()
                .map(|s| (&s.text, s.style))
                .collect::<Vec<_>>()
        );
        assert!(
            rows[1].spans.iter().all(|s| s.style.fg != Some(ACCENT)),
            "expected following lines to stay unaccented, got {:?}",
            rows[1]
                .spans
                .iter()
                .map(|s| (&s.text, s.style))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn unregistered_slash_text_stays_un_accented() {
        let _g = crate::COMMAND_RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|_| false);
        let styles = render_content_row_styles("/notreal");
        assert!(
            styles.iter().all(|s| s.fg != Some(ACCENT)),
            "non-command should not paint accent fg, got {styles:?}"
        );
    }
}
