//! `Block::Thinking` renderer + the summary helpers shared with the
//! transcript projection (used to build a one-line fold marker when
//! `show_thinking` is off).

use smelt_core::content::builder::LineBuilder;
use smelt_core::content::wrap::wrap_line;

use super::tools::pluralize;

pub(super) fn render(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    show_thinking: bool,
) -> u16 {
    if !show_thinking {
        let (label, line_count) = thinking_summary(content);
        return render_thinking_summary(out, width, &label, line_count, false);
    }
    let max_cols = width.saturating_sub(3).max(1); // "│ " prefix + 1 margin
    let mut rows = 0u16;
    for line in content.lines() {
        let segments = wrap_line(line, max_cols);
        if segments.len() > 1 {
            out.mark_wrapped();
        }
        for seg in &segments {
            out.set_dim_italic();
            out.print_gutter("│ ");
            out.print(seg);
            out.reset_style();
            out.newline();
            rows += 1;
        }
    }
    rows
}

/// Animated trailing dots for streaming indicators.
pub(super) fn animated_dots() -> &'static str {
    let n = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis()
        / 333) as usize
        % 3
        + 1;
    &"..."[..n]
}

/// Extract a title and non-empty line count from thinking content.
/// If the first non-empty line is a markdown bold title (`**...**`), use it as the label.
pub fn thinking_summary(content: &str) -> (String, usize) {
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

/// Render a single hidden-thinking summary row with optional animated dots.
pub fn render_thinking_summary(
    out: &mut LineBuilder,
    width: usize,
    label: &str,
    line_count: usize,
    animated: bool,
) -> u16 {
    let dots = if animated { animated_dots() } else { "" };
    let summary = format!("{label} ({}){dots}", pluralize(line_count, "line", "lines"));
    let max_cols = width.saturating_sub(3).max(1);
    let segs = wrap_line(&summary, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        out.set_dim_italic();
        out.print_gutter("\u{2502} ");
        out.print(seg);
        out.reset_style();
        out.newline();
        rows += 1;
    }
    rows
}
