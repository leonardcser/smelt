//! ANSI SGR parser for transcript-style terminal output.
//!
//! Only SGR (Select Graphic Rendition) sequences are interpreted. Cursor
//! movement, screen clearing, OSC title strings, and other control sequences
//! are dropped because they are meaningless in a captured transcript. This is
//! not a terminal emulator; it maps ANSI style escapes onto Smelt's native
//! [`Style`] values.

use smelt_style::style::{Color, Style};

/// One contiguous run of plain text with a resolved Smelt style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: Style,
}

/// Parse `text` into styled spans, stripping all non-SGR ANSI escapes and
/// control characters except tab.
pub fn parse_ansi(text: &str) -> Vec<AnsiSpan> {
    let mut parser = Parser::default();
    parser.parse(text, false, |_| {});
    parser.finish()
}

/// Parse newline-delimited ANSI transcript text into styled lines.
///
/// SGR state is preserved across line breaks, matching terminal transcripts
/// where a style can start on one row and continue on the next. Explicit
/// trailing line breaks produce trailing empty lines; callers that want trimmed
/// display output should trim input before parsing.
pub fn parse_ansi_lines(text: &str) -> Vec<Vec<AnsiSpan>> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut parser = Parser::default();
    let mut lines = Vec::new();
    parser.parse(text, true, |parser| lines.push(parser.take_line()));
    lines.push(parser.finish());
    lines
}

#[derive(Default)]
struct Parser {
    spans: Vec<AnsiSpan>,
    current: String,
    style: Style,
}

impl Parser {
    fn parse(&mut self, text: &str, crlf_is_newline: bool, mut newline: impl FnMut(&mut Self)) {
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                self.flush_current();
                match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        while let Some(&c) = chars.peek() {
                            if ('@'..='~').contains(&c) {
                                let cmd = chars.next().unwrap();
                                if cmd == 'm' {
                                    self.style = apply_sgr(&params, self.style);
                                }
                                break;
                            }
                            params.push(chars.next().unwrap());
                        }
                    }
                    Some(']') => {
                        // OSC sequence - skip until BEL or ST (ESC \\).
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
            if ch == '\n' {
                newline(self);
                continue;
            }
            if ch == '\r' {
                if crlf_is_newline && chars.peek() == Some(&'\n') {
                    chars.next();
                    newline(self);
                } else {
                    self.current.clear();
                    self.spans.clear();
                }
                continue;
            }
            if ch == '\x08' {
                self.backspace();
                continue;
            }
            if ch == '\t' || !ch.is_control() {
                self.current.push(ch);
            }
        }
    }

    fn finish(mut self) -> Vec<AnsiSpan> {
        self.flush_current();
        self.spans
    }

    fn take_line(&mut self) -> Vec<AnsiSpan> {
        self.flush_current();
        std::mem::take(&mut self.spans)
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            if last.style == self.style {
                last.text.push_str(&self.current);
                self.current.clear();
                return;
            }
        }
        self.spans.push(AnsiSpan {
            text: std::mem::take(&mut self.current),
            style: self.style,
        });
    }

    fn backspace(&mut self) {
        if self.current.pop().is_some() {
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            last.text.pop();
            if last.text.is_empty() {
                self.spans.pop();
            }
        }
    }
}

fn apply_sgr(params: &str, mut style: Style) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    let colon_params = params.contains(':');
    let codes = parse_sgr_params(params);
    let mut i = 0;
    while i < codes.len() {
        match sgr_code(&codes, i) {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 if colon_params && i + 1 < codes.len() => {
                style.underline = sgr_code(&codes, i + 1) != 0;
                i += 1;
            }
            4 => style.underline = true,
            7 => style.reverse = true,
            9 => style.crossedout = true,
            21 => style.underline = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            27 => style.reverse = false,
            29 => style.crossedout = false,
            30 => style.fg = Some(Color::Black),
            31 => style.fg = Some(Color::DarkRed),
            32 => style.fg = Some(Color::DarkGreen),
            33 => style.fg = Some(Color::DarkYellow),
            34 => style.fg = Some(Color::DarkBlue),
            35 => style.fg = Some(Color::DarkMagenta),
            36 => style.fg = Some(Color::DarkCyan),
            37 => style.fg = Some(Color::Grey),
            38 if sgr_code(&codes, i + 1) == 5 => {
                if let Some(n) = sgr_value(&codes, i + 2) {
                    style.fg = Some(Color::AnsiValue(n));
                    i += 2;
                }
            }
            38 if sgr_code(&codes, i + 1) == 2 => {
                if let Some((r, g, b, advance)) = sgr_rgb(&codes, i, colon_params) {
                    style.fg = Some(Color::Rgb { r, g, b });
                    i += advance;
                }
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
            48 if sgr_code(&codes, i + 1) == 5 => {
                if let Some(n) = sgr_value(&codes, i + 2) {
                    style.bg = Some(Color::AnsiValue(n));
                    i += 2;
                }
            }
            48 if sgr_code(&codes, i + 1) == 2 => {
                if let Some((r, g, b, advance)) = sgr_rgb(&codes, i, colon_params) {
                    style.bg = Some(Color::Rgb { r, g, b });
                    i += advance;
                }
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

fn parse_sgr_params(params: &str) -> Vec<Option<u16>> {
    params
        .split([';', ':'])
        .map(|s| if s.is_empty() { None } else { s.parse().ok() })
        .collect()
}

fn sgr_code(codes: &[Option<u16>], idx: usize) -> u16 {
    codes.get(idx).and_then(|c| *c).unwrap_or(0)
}

fn sgr_value(codes: &[Option<u16>], idx: usize) -> Option<u8> {
    codes.get(idx).and_then(|c| *c).map(|n| n as u8)
}

fn sgr_rgb(codes: &[Option<u16>], idx: usize, colon_params: bool) -> Option<(u8, u8, u8, usize)> {
    let direct = || {
        Some((
            sgr_value(codes, idx + 2)?,
            sgr_value(codes, idx + 3)?,
            sgr_value(codes, idx + 4)?,
            4,
        ))
    };

    if colon_params {
        if let Some(rgb) = (|| {
            Some((
                sgr_value(codes, idx + 3)?,
                sgr_value(codes, idx + 4)?,
                sgr_value(codes, idx + 5)?,
                5,
            ))
        })() {
            return Some(rgb);
        }
    }

    direct()
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
    fn parse_colon_truecolor() {
        let spans = parse_ansi("\x1b[38:2::255:128:0mhello");
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
    fn parse_colon_256_color() {
        let spans = parse_ansi("\x1b[38:5:208mhello");
        assert_eq!(spans[0].style.fg, Some(Color::AnsiValue(208)));
    }

    #[test]
    fn parse_empty_sgr_params_as_reset() {
        let spans = parse_ansi("\x1b[31mred\x1b[;32mgreen");
        assert_eq!(spans[0].style.fg, Some(Color::DarkRed));
        assert_eq!(spans[1].style.fg, Some(Color::DarkGreen));
        assert!(!spans[1].style.bold);
    }

    #[test]
    fn parse_bold_and_dim() {
        let spans = parse_ansi("\x1b[1mbold\x1b[2mdim");
        assert!(spans[0].style.bold);
        assert!(spans[1].style.bold);
        assert!(spans[1].style.dim);
    }

    #[test]
    fn parse_resets_bold() {
        let spans = parse_ansi("\x1b[1ma\x1b[22mb");
        assert!(spans[0].style.bold);
        assert!(!spans[1].style.bold);
    }

    #[test]
    fn parse_reverse_and_reverse_off() {
        let spans = parse_ansi("\x1b[7ma\x1b[27mb");
        assert!(spans[0].style.reverse);
        assert!(!spans[1].style.reverse);
    }

    #[test]
    fn parse_colon_underline_style_without_dim() {
        let spans = parse_ansi("\x1b[4:2munder");
        assert!(spans[0].style.underline);
        assert!(!spans[0].style.dim);
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

    #[test]
    fn parse_lines_preserves_sgr_across_newlines() {
        let lines = parse_ansi_lines("\x1b[31mred\nstill red\x1b[0m");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "red");
        assert_eq!(lines[0][0].style.fg, Some(Color::DarkRed));
        assert_eq!(lines[1][0].text, "still red");
        assert_eq!(lines[1][0].style.fg, Some(Color::DarkRed));
    }

    #[test]
    fn parse_lines_preserves_trailing_empty_line() {
        let lines = parse_ansi_lines("one\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "one");
        assert!(lines[1].is_empty());
    }

    #[test]
    fn parse_lines_treats_crlf_as_newline() {
        let lines = parse_ansi_lines("one\r\ntwo");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].text, "one");
        assert_eq!(lines[1][0].text, "two");
    }
}
