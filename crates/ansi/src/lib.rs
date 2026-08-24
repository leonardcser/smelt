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

/// ANSI style state at a resumable logical-line boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnsiState {
    style: Style,
}

/// Parse `text` into styled spans, stripping all non-SGR ANSI escapes and
/// control characters except tab.
pub fn parse_ansi(text: &str) -> Vec<AnsiSpan> {
    let mut state = AnsiState::default();
    parse_ansi_with_state(text, &mut state)
}

/// Parse one complete logical line, preserving SGR state for the next line.
pub fn parse_ansi_with_state(text: &str, state: &mut AnsiState) -> Vec<AnsiSpan> {
    let mut parser = Parser {
        style: state.style,
        ..Parser::default()
    };
    parser.parse(text, false, |_| {});
    state.style = parser.style;
    parser.finish()
}

/// Advance only the resumable SGR state without constructing rendered spans.
///
/// This is the indexing path for completed transcript lines. It deliberately
/// ignores text and non-SGR control sequences, matching [`parse_ansi_with_state`]
/// while performing no allocation.
pub fn advance_ansi_state(text: &str, state: &mut AnsiState) {
    let bytes = text.as_bytes();
    let mut offset = 0usize;
    while let Some(relative) = bytes[offset..].iter().position(|byte| *byte == b'\x1b') {
        let escape = offset + relative;
        let Some(kind) = bytes.get(escape + 1).copied() else {
            break;
        };
        match kind {
            b'[' => {
                let params_start = escape + 2;
                let mut end = params_start;
                while let Some(byte) = bytes.get(end).copied() {
                    if (b'@'..=b'~').contains(&byte) {
                        if byte == b'm' {
                            state.style = apply_sgr(&text[params_start..end], state.style);
                        }
                        end += 1;
                        break;
                    }
                    end += 1;
                }
                offset = end;
            }
            b']' => {
                let mut end = escape + 2;
                while let Some(byte) = bytes.get(end).copied() {
                    end += 1;
                    if byte == b'\x07' {
                        break;
                    }
                    if byte == b'\x1b' && bytes.get(end) == Some(&b'\\') {
                        end += 1;
                        break;
                    }
                }
                offset = end;
            }
            _ => offset = escape + 2,
        }
    }
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
    let mut codes = SgrParams::new(params);
    while let Some(code) = codes.next() {
        match sgr_code(code) {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 if colon_params => {
                if let Some(mode) = codes.next() {
                    style.underline = sgr_code(mode) != 0;
                } else {
                    style.underline = true;
                }
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
            38 => apply_extended_color(&mut codes, colon_params, |color| style.fg = Some(color)),
            39 => style.fg = None,
            40 => style.bg = Some(Color::Black),
            41 => style.bg = Some(Color::DarkRed),
            42 => style.bg = Some(Color::DarkGreen),
            43 => style.bg = Some(Color::DarkYellow),
            44 => style.bg = Some(Color::DarkBlue),
            45 => style.bg = Some(Color::DarkMagenta),
            46 => style.bg = Some(Color::DarkCyan),
            47 => style.bg = Some(Color::Grey),
            48 => apply_extended_color(&mut codes, colon_params, |color| style.bg = Some(color)),
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
    }
    style
}

#[derive(Clone, Copy)]
struct SgrParams<'a> {
    params: &'a str,
    offset: usize,
    finished: bool,
}

impl<'a> SgrParams<'a> {
    fn new(params: &'a str) -> Self {
        Self {
            params,
            offset: 0,
            finished: false,
        }
    }
}

impl Iterator for SgrParams<'_> {
    type Item = Option<u16>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let remaining = &self.params[self.offset..];
        let separator = remaining.find([';', ':']);
        let end = separator.map_or(self.params.len(), |index| self.offset + index);
        let token = &self.params[self.offset..end];
        self.finished = separator.is_none();
        self.offset = end.saturating_add(usize::from(separator.is_some()));
        Some(if token.is_empty() {
            None
        } else {
            token.parse().ok()
        })
    }
}

fn sgr_code(code: Option<u16>) -> u16 {
    code.unwrap_or(0)
}

fn sgr_value(code: Option<u16>) -> Option<u8> {
    code.map(|value| value as u8)
}

fn apply_extended_color(
    codes: &mut SgrParams<'_>,
    colon_params: bool,
    mut apply: impl FnMut(Color),
) {
    let mut candidate = *codes;
    match sgr_code(candidate.next().flatten()) {
        5 => {
            let Some(value) = sgr_value(candidate.next().flatten()) else {
                return;
            };
            apply(Color::AnsiValue(value));
            *codes = candidate;
        }
        2 => {
            let direct = candidate;
            if colon_params {
                let mut colon = candidate;
                colon.next();
                if let Some((r, g, b)) = take_rgb(&mut colon) {
                    apply(Color::Rgb { r, g, b });
                    *codes = colon;
                    return;
                }
            }
            let mut direct = direct;
            if let Some((r, g, b)) = take_rgb(&mut direct) {
                apply(Color::Rgb { r, g, b });
                *codes = direct;
            }
        }
        _ => {}
    }
}

fn take_rgb(codes: &mut SgrParams<'_>) -> Option<(u8, u8, u8)> {
    Some((
        sgr_value(codes.next().flatten())?,
        sgr_value(codes.next().flatten())?,
        sgr_value(codes.next().flatten())?,
    ))
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
    fn resumable_state_preserves_sgr_across_lines() {
        let mut state = AnsiState::default();
        let first = parse_ansi_with_state("plain\x1b[31mred", &mut state);
        let second = parse_ansi_with_state("still red\x1b[0mplain", &mut state);

        assert_eq!(first.last().unwrap().style.fg, Some(Color::DarkRed));
        assert_eq!(second[0].style.fg, Some(Color::DarkRed));
        assert_eq!(second[1].style, Style::default());
    }

    #[test]
    fn state_only_scan_matches_render_parser_across_lines() {
        let lines = [
            "plain\x1b[1;31mbold red",
            "\x1b]0;ignored title\x07still red\x1b[38;5;208mindexed",
            "\x1b[48:2::10:20:30mtruecolor background",
            "\x1b[4:2munderline\x1b[0mplain",
        ];
        let mut rendered = AnsiState::default();
        let mut indexed = AnsiState::default();

        for line in lines {
            parse_ansi_with_state(line, &mut rendered);
            advance_ansi_state(line, &mut indexed);
            assert_eq!(indexed, rendered, "state mismatch after {line:?}");
        }
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
