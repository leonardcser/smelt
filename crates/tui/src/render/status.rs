//! Responsive status line + top/bottom bar painters.
//!
//! `StatusSpan` drives the bottom status line (slug pill, mode,
//! throbber, permission/proc/agent indicators). `BarSpan` drives the
//! top-of-prompt and bottom-of-prompt horizontal rules drawn by
//! `draw_bar`. Both collapse on narrow terminals by dropping the
//! highest-priority (least-important) spans first.

use super::{selection::truncate_str, RenderOut, StyleState};
use crossterm::{style::Color, terminal, QueueableCommand};

pub(crate) fn vim_mode_label(mode: Option<crate::vim::ViMode>) -> Option<&'static str> {
    match mode {
        Some(crate::vim::ViMode::Insert) => Some("INSERT"),
        Some(crate::vim::ViMode::Visual) => Some("VISUAL"),
        Some(crate::vim::ViMode::VisualLine) => Some("VISUAL LINE"),
        _ => None,
    }
}

pub(crate) struct BarSpan {
    pub(crate) text: String,
    pub(crate) color: Color,
    pub(crate) bg: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    pub(crate) priority: u8,
}

/// A span in the status line with responsive priority support.
pub(crate) struct StatusSpan {
    pub(crate) text: String,
    pub(crate) style: StyleState,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    pub(crate) priority: u8,
    /// If true, a " · " separator is inserted before this span during rendering.
    pub(crate) group: bool,
    /// If true, the text can be truncated with "…" before being fully dropped.
    pub(crate) truncatable: bool,
}

/// Separator inserted between groups in the status line.
const STATUS_SEP: &str = " \u{00b7} "; // " · "
const STATUS_SEP_LEN: usize = 3;

/// Render status spans with responsive dropping and truncation.
///
/// Algorithm:
/// 1. Calculate total width of all visible spans (including group separators).
/// 2. While total > available width, find the highest-priority span and either
///    truncate it (if truncatable) or remove it entirely.
/// 3. Render the surviving spans left-to-right with group separators.
pub(crate) fn render_status_spans(
    out: &mut RenderOut,
    spans: &mut Vec<StatusSpan>,
    width: usize,
    fill_bg: Color,
) {
    // Calculate total char width including separators.
    let total_width = |spans: &[StatusSpan]| -> usize {
        let mut w = 0;
        for span in spans {
            if span.group && w > 0 {
                w += STATUS_SEP_LEN;
            }
            w += span.text.chars().count();
        }
        w
    };

    // Iteratively drop or truncate until it fits.
    while total_width(spans) > width && !spans.is_empty() {
        // Find the span with the highest priority (drop first).
        let max_pri = spans.iter().map(|s| s.priority).max().unwrap_or(0);
        if max_pri == 0 {
            break; // only priority-0 spans left, nothing more to drop
        }
        // Try truncating first: find the last truncatable span at this priority.
        let trunc_idx = spans
            .iter()
            .rposition(|s| s.priority == max_pri && s.truncatable);
        if let Some(idx) = trunc_idx {
            let available =
                width.saturating_sub(total_width(spans) - spans[idx].text.chars().count());
            if available >= 2 {
                spans[idx].text = truncate_str(&spans[idx].text, available);
                continue;
            }
        }
        // Drop ALL spans at this priority level at once (avoids orphaned separators).
        spans.retain(|s| s.priority != max_pri);
    }

    // Render.
    let sep_style = StyleState {
        fg: Some(crate::theme::muted()),
        bg: Some(fill_bg),
        dim: true,
        ..StyleState::default()
    };
    let mut col = 0;
    for span in spans.iter() {
        if span.group && col > 0 {
            out.push_style(sep_style.clone());
            out.print(STATUS_SEP);
            out.pop_style();
            col += STATUS_SEP_LEN;
        }
        out.push_style(span.style.clone());
        out.print(&span.text);
        out.pop_style();
        col += span.text.chars().count();
    }

    // Fill the rest of the line with the dark bg.
    if col < width {
        out.push_style(StyleState {
            bg: Some(fill_bg),
            ..StyleState::default()
        });
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        out.pop_style();
    }
    out.reset_style();
}

pub(crate) fn draw_bar(
    out: &mut RenderOut,
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<&[BarSpan]>,
    bar_color: Color,
) {
    let _perf = crate::perf::begin("render:bar");
    let dash = "\u{2500}";
    let min_dashes = 4;

    // Find the max priority we need to drop to fit.
    let max_priority = {
        let all_priorities: Vec<u8> = left
            .into_iter()
            .chain(right)
            .flat_map(|spans| spans.iter().map(|s| s.priority))
            .collect();
        *all_priorities.iter().max().unwrap_or(&0)
    };

    let mut drop_above = max_priority + 1; // start by showing everything
    loop {
        let left_chars: usize = left
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| s.text.chars().count())
                    .sum();
                if inner > 0 {
                    inner + 1
                } else {
                    0
                } // spans + trailing space
            })
            .unwrap_or(0);
        let right_chars: usize = right
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| s.text.chars().count())
                    .sum();
                if inner > 0 {
                    inner + 2
                } else {
                    0
                } // spans + space + trailing dash
            })
            .unwrap_or(0);
        let total = left_chars + min_dashes + right_chars;
        if total <= width || drop_above == 1 {
            break;
        }
        drop_above -= 1;
    }

    let left_filtered: Vec<&BarSpan> = left
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();
    let right_filtered: Vec<&BarSpan> = right
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();

    let left_len: usize = if left_filtered.is_empty() {
        0
    } else {
        left_filtered
            .iter()
            .map(|s| s.text.chars().count())
            .sum::<usize>()
            + 1 // trailing space
    };
    let right_len: usize = if right_filtered.is_empty() {
        0
    } else {
        right_filtered
            .iter()
            .map(|s| s.text.chars().count())
            .sum::<usize>()
            + 2
    };
    let bar_len = width.saturating_sub(left_len + right_len);

    if !left_filtered.is_empty() {
        for span in &left_filtered {
            out.push_style(StyleState {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..StyleState::default()
            });
            out.print(&span.text);
            out.pop_style();
        }
        out.print(" ");
    }

    out.push_fg(bar_color);
    out.print(&dash.repeat(bar_len));
    out.pop_style();

    if !right_filtered.is_empty() {
        for span in &right_filtered {
            out.push_style(StyleState {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..StyleState::default()
            });
            out.print(&span.text);
            out.pop_style();
        }
        out.print(" ");
        out.push_fg(bar_color);
        out.print(dash);
        out.pop_style();
    }
}
