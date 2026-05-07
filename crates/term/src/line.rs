//! Styled text runs. `Span` is one attribute-uniform run; `Line` is a
//! sequence of spans painted on a single visual row. Both are
//! data-only — they describe *what* to paint, not *where*; rendering
//! happens via [`crate::grid::GridSlice::put_line`] and friends.
//!
//! Designed as the second-tier paint primitive alongside `Grid`-level
//! `set` / `put_str`. Where `put_str` writes a single styled run,
//! `put_line` lays out a heterogeneous row in one call so callers stop
//! threading manual columns through `put_str_clip`-style chains.

use crate::grid::Style;
use std::borrow::Cow;

/// A styled run of text. Cheap to clone — the text is `Cow`, so
/// borrowed literals stay zero-copy and owned strings move through.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Span<'a> {
    pub text: Cow<'a, str>,
    pub style: Style,
}

impl<'a> Span<'a> {
    pub fn raw(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    /// Display width of the span's text in terminal cells.
    pub fn width(&self) -> u16 {
        use unicode_width::UnicodeWidthStr;
        UnicodeWidthStr::width(self.text.as_ref()) as u16
    }
}

impl<'a> From<&'a str> for Span<'a> {
    fn from(s: &'a str) -> Self {
        Self::raw(s)
    }
}

impl From<String> for Span<'_> {
    fn from(s: String) -> Self {
        Self::raw(Cow::Owned(s))
    }
}

/// One row of styled text. Spans paint left-to-right with no implicit
/// gaps; callers add padding spans where they want them.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
}

impl<'a> Line<'a> {
    pub fn new() -> Self {
        Self { spans: Vec::new() }
    }

    pub fn from_spans<I: IntoIterator<Item = Span<'a>>>(spans: I) -> Self {
        Self {
            spans: spans.into_iter().collect(),
        }
    }

    pub fn raw(text: impl Into<Cow<'a, str>>) -> Self {
        Self::from_spans([Span::raw(text)])
    }

    /// Append a span and return self for chaining: `Line::new().push("hi").push(styled(", world", red))`.
    pub fn push<S: Into<Span<'a>>>(mut self, span: S) -> Self {
        self.spans.push(span.into());
        self
    }

    /// Total display width across all spans.
    pub fn width(&self) -> u16 {
        self.spans.iter().map(|s| s.width()).sum()
    }
}

impl<'a> From<&'a str> for Line<'a> {
    fn from(s: &'a str) -> Self {
        Self::raw(s)
    }
}

impl From<String> for Line<'_> {
    fn from(s: String) -> Self {
        Self::raw(Cow::Owned(s))
    }
}

impl<'a> From<Span<'a>> for Line<'a> {
    fn from(span: Span<'a>) -> Self {
        Self::from_spans([span])
    }
}

/// Construct a [`Line`] from a comma-separated list of `Into<Span>`
/// values. Mixes raw strings and explicit spans:
/// `line!["foo", " ", Span::styled("bar", red)]`.
#[macro_export]
macro_rules! line {
    () => { $crate::line::Line::new() };
    ($($span:expr),+ $(,)?) => {{
        $crate::line::Line::from_spans([$(::core::convert::Into::<$crate::line::Span<'_>>::into($span)),+])
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Color;

    #[test]
    fn span_from_str_is_default_style() {
        let s: Span = "hi".into();
        assert_eq!(s.text, "hi");
        assert_eq!(s.style, Style::default());
        assert_eq!(s.width(), 2);
    }

    #[test]
    fn line_width_sums_spans() {
        let l = Line::from_spans([
            Span::raw("ab"),
            Span::styled("cde", Style::new().fg(Color::Red)),
        ]);
        assert_eq!(l.width(), 5);
    }

    #[test]
    fn line_macro_mixes_strs_and_spans() {
        let l = line!["foo", " ", Span::styled("bar", Style::new().fg(Color::Red))];
        assert_eq!(l.spans.len(), 3);
        assert_eq!(l.spans[0].text, "foo");
        assert_eq!(l.spans[1].text, " ");
        assert_eq!(l.spans[2].text, "bar");
        assert_eq!(l.spans[2].style.fg, Some(Color::Red));
    }
}
