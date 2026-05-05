//! Projection: turn `DisplayBlock` output (produced by `SpanCollector`)
//! into a `crate::ui::Buffer` so content rendered by `print_inline_diff`,
//! `print_syntax_file`, `render_markdown_inner`, etc. flows through the
//! normal buffer → view → grid path and inherits scrollbar, selection,
//! and vim motions.

use super::layout_out::SpanCollector;
use crate::ui::buffer::{Buffer, LineDecoration, SpanMeta, SpanStyle};
use crate::ui::Theme;
use smelt_core::content::display::{
    ColorRole, ColorValue, DisplayLine, NamedColor, SpanStyle as DisplaySpanStyle,
};
use smelt_core::style::Color;

fn named_color_to_crossterm(n: smelt_core::content::display::NamedColor) -> Color {
    use smelt_core::content::display::NamedColor;
    match n {
        NamedColor::Reset => Color::Reset,
        NamedColor::Black => Color::Black,
        NamedColor::DarkGrey => Color::DarkGrey,
        NamedColor::Red => Color::Red,
        NamedColor::DarkRed => Color::DarkRed,
        NamedColor::Green => Color::Green,
        NamedColor::DarkGreen => Color::DarkGreen,
        NamedColor::Yellow => Color::Yellow,
        NamedColor::DarkYellow => Color::DarkYellow,
        NamedColor::Blue => Color::Blue,
        NamedColor::DarkBlue => Color::DarkBlue,
        NamedColor::Magenta => Color::Magenta,
        NamedColor::DarkMagenta => Color::DarkMagenta,
        NamedColor::Cyan => Color::Cyan,
        NamedColor::DarkCyan => Color::DarkCyan,
        NamedColor::White => Color::White,
        NamedColor::Grey => Color::Grey,
    }
}

/// Resolve a `ColorValue` against a theme registry. The theme is
/// borrowed for the render so a single redraw is theme-consistent.
#[inline]
fn resolve(c: ColorValue, theme: &Theme) -> Color {
    match c {
        ColorValue::Rgb(r, g, b) => Color::Rgb { r, g, b },
        ColorValue::Ansi(v) => Color::AnsiValue(v),
        ColorValue::Named(n) => named_color_to_crossterm(n),
        ColorValue::Role(role) => {
            let group = match role {
                ColorRole::Accent => "SmeltAccent",
                ColorRole::Slug => "SmeltSlug",
                ColorRole::UserBg => "SmeltUserBg",
                ColorRole::CodeBlockBg => "SmeltCodeBlockBg",
                ColorRole::Bar => "SmeltBar",
                ColorRole::ToolPending => "SmeltToolPending",
                ColorRole::ReasonOff => "SmeltReasonOff",
                ColorRole::Muted => "Comment",
                ColorRole::Success => "SmeltSuccess",
                ColorRole::ErrorMsg => "ErrorMsg",
                ColorRole::Apply => "SmeltModeApply",
                ColorRole::Plan => "SmeltModePlan",
                ColorRole::Exec => "SmeltModeExec",
                ColorRole::Heading => "SmeltHeading",
                ColorRole::ReasonLow => "SmeltReasonLow",
                ColorRole::ReasonMed => "SmeltReasonMed",
                ColorRole::ReasonHigh => "SmeltReasonHigh",
                ColorRole::ReasonMax => "SmeltReasonMax",
            };
            let style = theme.get(group);
            // Roles whose conventional slot is bg (Slug, UserBg,
            // CodeBlockBg, Bar) populate `Style::bg`; others populate
            // `fg`. Try fg first, fall back to bg.
            style.fg.or(style.bg).unwrap_or(Color::Reset)
        }
    }
}

/// Run any span-emitting renderer (inline diff, syntax highlighter,
/// markdown, etc.) against a fresh `SpanCollector` and project the
/// captured `DisplayBlock` into `buf`. Renderers write into
/// `&mut SpanCollector`; their styled output lands as `SpanStyle`
/// highlights on `buf`, gaining scrollbar / selection / vim motions
/// for free.
pub(crate) fn render_into_buffer(
    buf: &mut Buffer,
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut SpanCollector),
) {
    let mut collector = SpanCollector::new(width);
    fill(&mut collector);
    let block = collector.finish();
    let lines: Vec<ProjectedLine> = block
        .lines
        .iter()
        .map(|l| project_display_line(l, theme))
        .collect();
    apply_to_buffer(buf, &lines);
}

#[derive(Default)]
pub(crate) struct ProjectedLine {
    pub(crate) text: String,
    pub(crate) highlights: Vec<(u16, u16, SpanStyle, SpanMeta)>,
    pub(crate) decoration: LineDecoration,
}

pub(crate) fn project_display_line(dline: &DisplayLine, theme: &Theme) -> ProjectedLine {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut char_offset: u16 = 0;

    for span in &dline.spans {
        let col_start = char_offset;
        text.push_str(&span.text);
        let span_chars = span.text.chars().count() as u16;
        char_offset += span_chars;
        let col_end = char_offset;

        let style = resolve_span_style(&span.style, theme);
        let has_style = !style_is_default(&style);
        let has_meta = !span.meta.selectable || span.meta.copy_as.is_some();
        if has_style || has_meta {
            highlights.push((
                col_start,
                col_end,
                style,
                SpanMeta {
                    selectable: span.meta.selectable,
                    copy_as: span.meta.copy_as.clone(),
                },
            ));
        }
    }

    let decoration = LineDecoration {
        gutter_bg: dline.gutter_bg.map(|c| resolve(c, theme)),
        fill_bg: dline.fill_bg.map(|c| resolve(c, theme)),
        fill_right_margin: dline.fill_right_margin,
        soft_wrapped: dline.soft_wrapped,
        source_text: dline.source_text.clone(),
    };

    ProjectedLine {
        text,
        highlights,
        decoration,
    }
}

fn resolve_span_style(span: &DisplaySpanStyle, theme: &Theme) -> SpanStyle {
    SpanStyle {
        fg: span.fg.map(|c| resolve(c, theme)),
        bg: span.bg.map(|c| resolve(c, theme)),
        bold: span.bold,
        dim: span.dim,
        italic: span.italic,
        underline: span.underline,
        crossedout: span.crossedout,
    }
}

fn style_is_default(s: &SpanStyle) -> bool {
    s.fg.is_none()
        && s.bg.is_none()
        && !s.bold
        && !s.dim
        && !s.italic
        && !s.underline
        && !s.crossedout
}

/// Inverse of `render_into_buffer`: walk a `Buffer` and emit its lines
/// (with their highlights) into an existing `SpanCollector`. Lossy on
/// theme roles (Buffer highlights store resolved `Color` rather than
/// `ColorValue`), but tool render output is user content (file body,
/// command stdout) rather than theme-reactive chrome — so the
/// round-trip preserves the observable look.
///
/// Transient bridge for the P9.b tool-render path: Lua hooks now write
/// into a Buffer (the architectural target), and this projection feeds
/// the still-`SpanCollector`-based transcript pipeline. Retires when
/// per-block parsers also write into Buffer.
pub fn buffer_into_collector(buf: &Buffer, out: &mut SpanCollector) {
    let n = buf.line_count();
    for i in 0..n {
        let text = buf.get_line(i).unwrap_or("");
        let mut highlights = buf.highlights_at(i);
        highlights.sort_by_key(|h| h.col_start);

        let chars: Vec<char> = text.chars().collect();
        let mut col: u16 = 0;
        for h in &highlights {
            if h.col_end <= col {
                continue;
            }
            if h.col_start > col {
                let plain: String = chars[col as usize..h.col_start as usize].iter().collect();
                out.print(&plain);
                col = h.col_start;
            }
            let end = h.col_end.min(chars.len() as u16);
            if end <= col {
                continue;
            }
            let segment: String = chars[col as usize..end as usize].iter().collect();
            let style = buffer_style_to_display(&h.style);
            out.push_style(style);
            out.print(&segment);
            out.pop_style();
            col = end;
        }
        if (col as usize) < chars.len() {
            let tail: String = chars[col as usize..].iter().collect();
            out.print(&tail);
        }
        out.newline();
    }
}

fn buffer_style_to_display(style: &SpanStyle) -> DisplaySpanStyle {
    DisplaySpanStyle {
        fg: style.fg.map(color_to_value),
        bg: style.bg.map(color_to_value),
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        underline: style.underline,
        crossedout: style.crossedout,
    }
}

fn color_to_value(c: Color) -> ColorValue {
    match c {
        Color::Reset => ColorValue::Named(NamedColor::Reset),
        Color::Black => ColorValue::Named(NamedColor::Black),
        Color::DarkGrey => ColorValue::Named(NamedColor::DarkGrey),
        Color::Red => ColorValue::Named(NamedColor::Red),
        Color::DarkRed => ColorValue::Named(NamedColor::DarkRed),
        Color::Green => ColorValue::Named(NamedColor::Green),
        Color::DarkGreen => ColorValue::Named(NamedColor::DarkGreen),
        Color::Yellow => ColorValue::Named(NamedColor::Yellow),
        Color::DarkYellow => ColorValue::Named(NamedColor::DarkYellow),
        Color::Blue => ColorValue::Named(NamedColor::Blue),
        Color::DarkBlue => ColorValue::Named(NamedColor::DarkBlue),
        Color::Magenta => ColorValue::Named(NamedColor::Magenta),
        Color::DarkMagenta => ColorValue::Named(NamedColor::DarkMagenta),
        Color::Cyan => ColorValue::Named(NamedColor::Cyan),
        Color::DarkCyan => ColorValue::Named(NamedColor::DarkCyan),
        Color::White => ColorValue::Named(NamedColor::White),
        Color::Grey => ColorValue::Named(NamedColor::Grey),
        Color::Rgb { r, g, b } => ColorValue::Rgb(r, g, b),
        Color::AnsiValue(v) => ColorValue::Ansi(v),
    }
}

pub(crate) fn apply_to_buffer(buf: &mut Buffer, lines: &[ProjectedLine]) {
    let text_lines: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
    buf.set_all_lines(text_lines);

    for (i, pline) in lines.iter().enumerate() {
        for (col_start, col_end, style, meta) in &pline.highlights {
            buf.add_highlight_with_meta(i, *col_start, *col_end, *style, meta.clone());
        }

        let dec = &pline.decoration;
        if dec.gutter_bg.is_some()
            || dec.fill_bg.is_some()
            || dec.fill_right_margin != 0
            || dec.soft_wrapped
            || dec.source_text.is_some()
        {
            buf.set_decoration(i, dec.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::buffer::BufCreateOpts;
    use crate::ui::BufId;
    use smelt_core::content::display::{ColorValue, DisplaySpan, SpanStyle as DSpanStyle};
    use smelt_core::style::Color;

    fn test_theme() -> Theme {
        let mut t = Theme::new();
        crate::theme::populate_ui_theme(&mut t);
        t
    }

    fn make_buf() -> Buffer {
        Buffer::new(BufId(99), BufCreateOpts::default())
    }

    #[test]
    fn projects_styled_spans() {
        let theme = test_theme();
        let dline = DisplayLine {
            spans: vec![
                DisplaySpan {
                    text: "red".into(),
                    style: DSpanStyle {
                        fg: Some(ColorValue::Rgb(255, 0, 0)),
                        ..Default::default()
                    },
                    meta: Default::default(),
                },
                DisplaySpan {
                    text: " normal".into(),
                    style: DSpanStyle::default(),
                    meta: Default::default(),
                },
            ],
            ..Default::default()
        };
        let projected = project_display_line(&dline, &theme);
        assert_eq!(projected.text, "red normal");
        assert_eq!(projected.highlights.len(), 1);
        assert_eq!(projected.highlights[0].0, 0);
        assert_eq!(projected.highlights[0].1, 3);
        assert_eq!(
            projected.highlights[0].2.fg,
            Some(Color::Rgb { r: 255, g: 0, b: 0 })
        );
    }

    #[test]
    fn render_into_buffer_captures_inline_diff_output() {
        use super::super::highlight::print_inline_diff;
        let mut buf = make_buf();
        let theme = test_theme();
        render_into_buffer(&mut buf, 40, &theme, |sink| {
            print_inline_diff(
                sink,
                "old\nline\n",
                "new\nline\n",
                "/tmp/x.txt",
                "old\nline\n",
                0,
                10,
            );
        });
        // The diff renderer emits at least one styled line per changed
        // line; projecting must produce non-empty buffer content.
        assert!(buf.line_count() > 0);
        // At least one line should have a highlight (diff bg / fg).
        let any_highlight = (0..buf.line_count()).any(|i| !buf.highlights_at(i).is_empty());
        assert!(any_highlight, "expected at least one styled span");
    }

    #[test]
    fn apply_writes_to_buffer() {
        let mut buf = make_buf();
        let lines = vec![
            ProjectedLine {
                text: "line one".into(),
                highlights: vec![(0, 4, SpanStyle::bold(), SpanMeta::default())],
                decoration: LineDecoration::default(),
            },
            ProjectedLine {
                text: "line two".into(),
                highlights: vec![],
                decoration: LineDecoration {
                    fill_bg: Some(Color::Blue),
                    ..LineDecoration::default()
                },
            },
        ];
        apply_to_buffer(&mut buf, &lines);

        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.get_line(0), Some("line one"));
        assert_eq!(buf.get_line(1), Some("line two"));
        assert_eq!(buf.highlights_at(0).len(), 1);
        assert!(buf.highlights_at(0)[0].style.bold);
        assert_eq!(buf.decoration_at(1).fill_bg, Some(Color::Blue));
    }
}
