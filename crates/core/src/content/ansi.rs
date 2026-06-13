//! ANSI content adapters for Smelt transcript buffers.
//!
//! The low-level SGR parser lives in `smelt-ansi` so it can be shared by core
//! content rendering and terminal apps without coupling either layer to the
//! other. This module keeps the buffer-specific wrapping and `LineBuilder`
//! emission helpers used by transcript rendering.

use crate::content::inline_line::{BreakPolicy, InlineLine, InlineRun};
use crate::style::Style;
pub use smelt_ansi::{parse_ansi, AnsiSpan};

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
    let line = InlineLine::new(
        spans
            .iter()
            .map(|span| InlineRun::new(span.text.clone(), (), BreakPolicy::BreakOnSpaces))
            .collect(),
    );
    let ranges = line.wrap_plain_ranges(width);
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
    if style.reverse {
        out.set_reverse();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
