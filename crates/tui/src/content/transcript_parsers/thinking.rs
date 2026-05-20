//! `Block::Thinking` renderer. `thinking_summary`/`render_thinking_summary` build
//! the fold marker when `show_thinking` is off.

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::wrap::wrap_line;

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
        let segments = wrap_line(line, max_cols);
        if segments.len() > 1 {
            out.mark_wrapped();
        }
        for seg in &segments {
            out.set_dim_italic();
            out.print_gutter(THINKING_GUTTER);
            out.print(seg);
            out.reset_style();
            out.newline();
            rows += 1;
        }
    }
    rows
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

fn render_thinking_summary(
    out: &mut LineBuilder,
    width: usize,
    label: &str,
    line_count: usize,
) -> u16 {
    let summary = format!("{label} ({})", pluralize(line_count, "line", "lines"));
    let max_cols = block_inner_width(width);
    let segs = wrap_line(&summary, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        out.set_dim_italic();
        out.print_gutter(THINKING_GUTTER);
        out.print(seg);
        out.reset_style();
        out.newline();
        rows += 1;
    }
    rows
}
