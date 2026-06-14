//! `Block::Thinking` renderer. `thinking_summary`/`render_thinking_summary` build
//! the fold marker when `show_thinking` is off.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{emit_inline_spans, parse_inline_spans, wrap_inline_spans};
use smelt_core::content::inline_line::InlineLine;

use super::metrics::{block_inner_width, THINKING_GUTTER};
use super::tools::pluralize;

pub(super) fn render(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    show_thinking: bool,
) -> u16 {
    if !show_thinking {
        let (label, line_count) = thinking_summary(content);
        return render_thinking_summary(out, width, &label, line_count);
    }
    let max_cols = block_inner_width(width);
    let mut rows = 0u16;
    for line in content.lines() {
        let mut spans = parse_inline_spans(line, true);
        // Preserve the dim-italic thinking aesthetic across every span,
        // so bold/code/strike runs stay italic instead of resetting to upright.
        for span in &mut spans {
            span.style.italic = true;
        }
        let wrapped = wrap_inline_spans(&spans, max_cols);
        if wrapped.len() > 1 {
            out.mark_wrapped();
        }
        for row_spans in &wrapped {
            out.set_dim_italic();
            out.print_gutter(THINKING_GUTTER);
            out.reset_style();
            emit_inline_spans(out, row_spans);
            out.newline();
            rows += 1;
        }
    }
    rows
}

pub(super) fn measure(content: &str, width: usize, show_thinking: bool) -> u16 {
    if !show_thinking {
        let (label, line_count) = thinking_summary(content);
        return measure_thinking_summary(width, &label, line_count);
    }
    let max_cols = block_inner_width(width);
    content
        .lines()
        .map(|line| {
            let mut spans = parse_inline_spans(line, true);
            for span in &mut spans {
                span.style.italic = true;
            }
            wrap_inline_spans(&spans, max_cols).len() as u16
        })
        .sum()
}

/// Returns `(label, non_empty_line_count)`. Uses the first `**bold**` line as label if present.
pub(super) fn thinking_summary(content: &str) -> (String, usize) {
    let mut label = None;
    let mut lines = 0usize;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = Some(trimmed[2..trimmed.len() - 2].trim().to_string());
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

fn measure_thinking_summary(width: usize, label: &str, line_count: usize) -> u16 {
    let summary = format!("{label} ({})", pluralize(line_count, "line", "lines"));
    let max_cols = block_inner_width(width);
    let line = InlineLine::plain(summary.as_str(), ());
    line.wrap_plain_ranges(max_cols).len() as u16
}

fn render_thinking_summary(
    out: &mut LineBuilder,
    width: usize,
    label: &str,
    line_count: usize,
) -> u16 {
    let summary = format!("{label} ({})", pluralize(line_count, "line", "lines"));
    let max_cols = block_inner_width(width);
    let line = InlineLine::plain(summary.as_str(), ());
    let segs = line.wrap_plain_ranges(max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for (start, end) in segs {
        let seg = smelt_buffer::text::slice(&summary, start..end);
        out.set_dim_italic();
        out.print_gutter(THINKING_GUTTER);
        out.print(seg);
        out.reset_style();
        out.newline();
        rows += 1;
    }
    rows
}
