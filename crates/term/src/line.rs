//! Styled text primitives. `Span` is a single attribute-uniform run;
//! `Line` is a sequence of spans for one visual row. Both are data-only;
//! rendering happens via [`crate::grid::GridSlice::put_line`].

use crate::grid::Style;
use std::borrow::Cow;

/// A styled run of text. `Cow` text keeps borrowed literals zero-copy.
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

    /// Append a span; returns `self` for chaining.
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

/// Construct a [`Line`] from a list of `Into<Span>` values.
/// `line!["foo", " ", Span::styled("bar", red)]`
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

    #[test]
    fn span_width_counts_two_columns_per_wide_char() {
        // CJK chars and most emoji render 2 cells wide.
        assert_eq!(Span::raw("漢字").width(), 4);
        assert_eq!(Span::raw("a漢b").width(), 4);
    }

    #[test]
    fn span_width_is_zero_for_empty_text() {
        assert_eq!(Span::raw("").width(), 0);
    }

    #[test]
    fn line_width_zero_for_empty_and_for_all_empty_spans() {
        assert_eq!(Line::new().width(), 0);
        let l = Line::from_spans([Span::raw(""), Span::raw("")]);
        assert_eq!(l.width(), 0);
    }

    #[test]
    fn line_push_appends_a_span() {
        let l = Line::new()
            .push("a")
            .push(Span::styled("b", Style::new().fg(Color::Red)));
        assert_eq!(l.spans.len(), 2);
        assert_eq!(l.spans[0].text, "a");
        assert_eq!(l.spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn line_raw_wraps_text_in_a_single_default_span() {
        let l = Line::raw("hello");
        assert_eq!(l.spans.len(), 1);
        assert_eq!(l.spans[0].text, "hello");
        assert_eq!(l.spans[0].style, Style::default());
    }
}
