//! ANSI helpers for turning SGR-styled transcript text into terminal lines.
//!
//! The canonical SGR parser lives in `smelt-ansi` so non-terminal crates can
//! use it without depending on the renderer. This module re-exports that parser
//! and adds adapters for [`Line`] / [`Span`].

pub use smelt_ansi::{parse_ansi, AnsiSpan};

use crate::{Line, Span};

/// Parse one row of ANSI transcript text into a Smelt terminal line.
///
/// Non-SGR terminal controls are dropped by [`parse_ansi`]. Newline characters
/// are treated as controls, so pass one visual row at a time or use
/// [`parse_ansi_lines`].
pub fn parse_ansi_line(text: &str) -> Line<'static> {
    Line::from_spans(
        parse_ansi(text)
            .into_iter()
            .map(|span| Span::styled(span.text, span.style)),
    )
}

/// Parse newline-delimited ANSI transcript text into Smelt terminal lines.
///
/// SGR state is preserved across line breaks, matching terminal transcripts
/// where a style can start on one row and continue on the next. Explicit
/// trailing line breaks produce trailing empty lines; callers that want trimmed
/// display output should trim input before parsing.
pub fn parse_ansi_lines(text: &str) -> Vec<Line<'static>> {
    smelt_ansi::parse_ansi_lines(text)
        .into_iter()
        .map(|spans| {
            Line::from_spans(
                spans
                    .into_iter()
                    .map(|span| Span::styled(span.text, span.style)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;

    #[test]
    fn ansi_line_uses_terminal_spans() {
        let line = parse_ansi_line("a\x1b[31mb");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].text, "a");
        assert_eq!(line.spans[1].text, "b");
        assert_eq!(line.spans[1].style.fg, Some(Color::DarkRed));
    }

    #[test]
    fn ansi_lines_split_rows() {
        let lines = parse_ansi_lines("one\n\x1b[32mtwo");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].text, "one");
        assert_eq!(lines[1].spans[0].text, "two");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::DarkGreen));
    }

    #[test]
    fn ansi_lines_preserve_sgr_across_rows() {
        let lines = parse_ansi_lines("\x1b[31mred\nstill red");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkRed));
        assert_eq!(lines[1].spans[0].text, "still red");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::DarkRed));
    }

    #[test]
    fn ansi_lines_preserve_trailing_empty_row() {
        let lines = parse_ansi_lines("one\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].text, "one");
        assert!(lines[1].spans.is_empty());
    }
}
