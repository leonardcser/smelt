//! ANSI SGR parser: converts terminal escape sequences into smelt [`Style`] spans.
//!
//! Only SGR (Select Graphic Rendition) sequences are interpreted. Cursor
//! movement, screen clearing, OSC title strings, and other control sequences
//! are dropped because they are meaningless in a scrollback transcript buffer.
//!
//! This is the canonical conversion point between the external ANSI wire
//! format and smelt's native style primitives. Callers that receive raw
//! terminal output (tool `render` callbacks, `!exec` blocks, etc.) should
//! route text through here instead of stripping escapes blindly.

use crate::style::{Color, Style};
use smelt_buffer::wrap::wrap_line_ranges;

/// One contiguous run of plain text with a resolved smelt style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: Style,
}

/// Parse `text` into styled spans, stripping all non-SGR ANSI escapes and
/// control characters (except tab, which is preserved for callers to expand).
pub fn parse_ansi(text: &str) -> Vec<AnsiSpan> {
    let mut spans: Vec<AnsiSpan> = Vec::new();
    let mut current = String::new();
    let mut style = Style::default();

    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if !current.is_empty() {
                spans.push(AnsiSpan {
                    text: std::mem::take(&mut current),
                    style,
                });
            }
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    while let Some(&c) = chars.peek() {
                        if ('@'..='~').contains(&c) {
                            let cmd = chars.next().unwrap();
                            if cmd == 'm' {
                                style = apply_sgr(&params, style);
                            }
                            break;
                        }
                        params.push(chars.next().unwrap());
                    }
                }
                Some(']') => {
                    // OSC sequence - skip until BEL or ST (ESC \).
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
            continue;
        }
        if ch == '\r' {
            // Carriage return: simulate terminal overwrite by discarding the
            // accumulated text for the current line.
            current.clear();
            continue;
        }
        if ch == '\x08' {
            // Backspace: remove the last accumulated character.
            current.pop();
            continue;
        }
        if ch == '\t' || !ch.is_control() {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        spans.push(AnsiSpan {
            text: current,
            style,
        });
    }

    // Coalesce adjacent spans with identical styles to reduce push/pop churn.
    let mut coalesced: Vec<AnsiSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        if let Some(last) = coalesced.last_mut() {
            if last.style == span.style {
                last.text.push_str(&span.text);
                continue;
            }
        }
        coalesced.push(span);
    }
    coalesced
}

/// Cumulative byte boundaries for a list of spans.
fn span_boundaries(spans: &[AnsiSpan]) -> Vec<usize> {
    let mut out = vec![0usize];
    let mut acc = 0;
    for span in spans {
        acc += span.text.len();
        out.push(acc);
    }
    out
}

/// Parse ANSI escapes, wrap the resulting plain text to `width`, and return
/// everything needed for emission.
///
/// Returns `(spans, wrap_ranges, boundaries)` where `wrap_ranges` are byte
/// ranges into the concatenated plain text, and `boundaries` maps span
/// indices to their cumulative byte offsets.
pub fn wrap_ansi(text: &str, width: usize) -> (Vec<AnsiSpan>, Vec<(usize, usize)>, Vec<usize>) {
    let spans = parse_ansi(text);
    let plain: String = spans.iter().map(|s| s.text.as_str()).collect();
    let ranges = wrap_line_ranges(&plain, width);
    let boundaries = span_boundaries(&spans);
    (spans, ranges, boundaries)
}

/// Emit one wrapped row of ANSI spans through `out`.
///
/// `boundaries` is the cumulative byte boundary array produced by [`wrap_ansi`].
pub fn emit_ansi_row(
    out: &mut crate::content::builder::LineBuilder,
    spans: &[AnsiSpan],
    boundaries: &[usize],
    wrap_start: usize,
    wrap_end: usize,
) {
    let mut pos = wrap_start;
    while pos < wrap_end {
        let span_idx = boundaries.partition_point(|&b| b <= pos).saturating_sub(1);
        if span_idx >= spans.len() {
            break;
        }
        let span_start = boundaries[span_idx];
        let span_end = boundaries[span_idx + 1];
        let seg_start = pos - span_start;
        let seg_end = (wrap_end - span_start).min(span_end - span_start);
        if seg_start >= seg_end {
            pos = span_end;
            continue;
        }
        let seg = &spans[span_idx].text[seg_start..seg_end];
        let pushed = apply_style(out, &spans[span_idx].style);
        out.print(seg);
        if pushed {
            out.pop_style();
        }
        pos = span_start + seg_end;
    }
}

fn apply_style(out: &mut crate::content::builder::LineBuilder, style: &Style) -> bool {
    if *style == Style::default() {
        // Inherit the caller's base style (dim / hl_group / etc.).
        return false;
    }
    out.save_style();
    if let Some(fg) = style.fg {
        out.set_fg(fg);
    }
    if let Some(bg) = style.bg {
        out.set_bg(bg);
    }
    if style.bold {
        out.set_bold();
    }
    if style.dim {
        out.set_dim();
    }
    if style.italic {
        out.set_italic();
    }
    if style.underline {
        out.set_underline();
    }
    if style.crossedout {
        out.set_crossedout();
    }
    true
}

fn apply_sgr(params: &str, mut style: Style) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    let codes: Vec<u16> = params.split(';').filter_map(|s| s.parse().ok()).collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            9 => style.crossedout = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            29 => style.crossedout = false,
            30 => style.fg = Some(Color::Black),
            31 => style.fg = Some(Color::DarkRed),
            32 => style.fg = Some(Color::DarkGreen),
            33 => style.fg = Some(Color::DarkYellow),
            34 => style.fg = Some(Color::DarkBlue),
            35 => style.fg = Some(Color::DarkMagenta),
            36 => style.fg = Some(Color::DarkCyan),
            37 => style.fg = Some(Color::Grey),
            38 if i + 2 < codes.len() && codes[i + 1] == 5 => {
                style.fg = Some(Color::AnsiValue(codes[i + 2] as u8));
                i += 2;
            }
            38 if i + 4 < codes.len() && codes[i + 1] == 2 => {
                style.fg = Some(Color::Rgb {
                    r: codes[i + 2] as u8,
                    g: codes[i + 3] as u8,
                    b: codes[i + 4] as u8,
                });
                i += 4;
            }
            39 => style.fg = None,
            40 => style.bg = Some(Color::Black),
            41 => style.bg = Some(Color::DarkRed),
            42 => style.bg = Some(Color::DarkGreen),
            43 => style.bg = Some(Color::DarkYellow),
            44 => style.bg = Some(Color::DarkBlue),
            45 => style.bg = Some(Color::DarkMagenta),
            46 => style.bg = Some(Color::DarkCyan),
            47 => style.bg = Some(Color::Grey),
            48 if i + 2 < codes.len() && codes[i + 1] == 5 => {
                style.bg = Some(Color::AnsiValue(codes[i + 2] as u8));
                i += 2;
            }
            48 if i + 4 < codes.len() && codes[i + 1] == 2 => {
                style.bg = Some(Color::Rgb {
                    r: codes[i + 2] as u8,
                    g: codes[i + 3] as u8,
                    b: codes[i + 4] as u8,
                });
                i += 4;
            }
            49 => style.bg = None,
            90 => style.fg = Some(Color::DarkGrey),
            91 => style.fg = Some(Color::Red),
            92 => style.fg = Some(Color::Green),
            93 => style.fg = Some(Color::Yellow),
            94 => style.fg = Some(Color::Blue),
            95 => style.fg = Some(Color::Magenta),
            96 => style.fg = Some(Color::Cyan),
            97 => style.fg = Some(Color::White),
            100 => style.bg = Some(Color::DarkGrey),
            101 => style.bg = Some(Color::Red),
            102 => style.bg = Some(Color::Green),
            103 => style.bg = Some(Color::Yellow),
            104 => style.bg = Some(Color::Blue),
            105 => style.bg = Some(Color::Magenta),
            106 => style.bg = Some(Color::Cyan),
            107 => style.bg = Some(Color::White),
            _ => {}
        }
        i += 1;
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty() {
        assert!(parse_ansi("").is_empty());
    }

    #[test]
    fn parse_plain_text() {
        let spans = parse_ansi("hello");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello");
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn parse_red_fg() {
        let spans = parse_ansi("a\x1b[31mb\x1b[0mc");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "a");
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[1].text, "b");
        assert_eq!(spans[1].style, Style::new().fg(Color::DarkRed));
        assert_eq!(spans[2].text, "c");
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn parse_256_color() {
        let spans = parse_ansi("\x1b[38;5;208mhello");
        assert_eq!(spans[0].style.fg, Some(Color::AnsiValue(208)));
    }

    #[test]
    fn parse_truecolor() {
        let spans = parse_ansi("\x1b[38;2;255;128;0mhello");
        assert_eq!(
            spans[0].style.fg,
            Some(Color::Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
    }

    #[test]
    fn parse_bold_and_dim() {
        let spans = parse_ansi("\x1b[1mbold\x1b[2mdim");
        assert!(spans[0].style.bold);
        assert!(spans[1].style.bold); // bold persists
        assert!(spans[1].style.dim);
    }

    #[test]
    fn parse_resets_bold() {
        let spans = parse_ansi("\x1b[1ma\x1b[22mb");
        assert!(spans[0].style.bold);
        assert!(!spans[1].style.bold);
    }

    #[test]
    fn parse_drops_cursor_sequences() {
        let spans = parse_ansi("a\x1b[1;1Hb");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "ab");
    }

    #[test]
    fn parse_drops_osc() {
        let spans = parse_ansi("pre\x1b]0;title\x07post");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "prepost");
    }

    #[test]
    fn wrap_ansi_basic() {
        let (spans, ranges, boundaries) = wrap_ansi("hello world", 5);
        assert_eq!(ranges, vec![(0, 5), (6, 11)]);
        assert_eq!(boundaries, vec![0, 11]);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn wrap_ansi_with_spans() {
        let (spans, ranges, _boundaries) = wrap_ansi("abc\x1b[31mdefghi", 5);
        assert_eq!(ranges, vec![(0, 5), (5, 9)]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "abc");
        assert_eq!(spans[1].text, "defghi");
    }

    #[test]
    fn parse_preserves_tab() {
        let spans = parse_ansi("a\tb");
        assert_eq!(spans[0].text, "a\tb");
    }

    #[test]
    fn parse_drops_control_chars() {
        let spans = parse_ansi("a\x00b\x01c");
        assert_eq!(spans[0].text, "abc");
    }

    #[test]
    fn parse_coalesces_same_style() {
        let spans = parse_ansi("a\x1b[31mb\x1b[31mc");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "a");
        assert_eq!(spans[1].text, "bc");
        assert_eq!(spans[1].style.fg, Some(Color::DarkRed));
    }

    #[test]
    fn wrap_ansi_empty() {
        let (spans, ranges, boundaries) = wrap_ansi("", 10);
        assert_eq!(ranges, vec![(0, 0)]);
        assert_eq!(boundaries, vec![0]);
        assert!(spans.is_empty());
    }

    #[test]
    fn render_ansi_multiline_into_buffer() {
        use crate::buffer::{BufCreateOpts, BufId, Buffer};
        use crate::content::builder::render_into;
        use crate::theme::Theme;

        let text = "line1\n\x1b[31mline2\x1b[0m\nline3";
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let theme = Theme::default();
        let width = 80u16;

        render_into(&mut buf, width, &theme, |sink| {
            let max_cols = width as usize;
            sink.push_dim();
            for line in text.lines() {
                let expanded = line.replace('\t', "    ");
                let (spans, ranges, boundaries) = wrap_ansi(&expanded, max_cols);
                for &(ws, we) in &ranges {
                    emit_ansi_row(sink, &spans, &boundaries, ws, we);
                    sink.newline();
                }
            }
            sink.pop_style();
        });

        assert_eq!(
            buf.line_count(),
            3,
            "expected 3 lines, got {:?}",
            (0..buf.line_count())
                .map(|i| buf.get_line(i))
                .collect::<Vec<_>>()
        );
        assert_eq!(buf.get_line(0), Some("line1"));
        assert_eq!(buf.get_line(1), Some("line2"));
        assert_eq!(buf.get_line(2), Some("line3"));
    }

    #[test]
    fn parse_carriage_return_overwrites() {
        let spans = parse_ansi("abc\rdef");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "def");
    }

    #[test]
    fn parse_multiple_carriage_returns() {
        let spans = parse_ansi("0%\r25%\r50%\r100%");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "100%");
    }

    #[test]
    fn parse_carriage_return_with_ansi_colors() {
        // Carriage return discards preceding text (terminal overwrite).
        let spans = parse_ansi("\x1b[31mabc\r\x1b[32mdef");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "def");
        assert_eq!(spans[0].style.fg, Some(Color::DarkGreen));
    }

    #[test]
    fn parse_backspace_removes_char() {
        let spans = parse_ansi("abc\x08d");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "abd");
    }

    #[test]
    fn parse_backspace_at_start_is_noop() {
        let spans = parse_ansi("\x08abc");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "abc");
    }
}
