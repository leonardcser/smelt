//! Responsive status line + top/bottom bar painters.
//!
//! `StatusSpan` drives the bottom status line (slug pill, mode,
//! throbber, permission/proc/agent indicators). `BarSpan` drives the
//! top-of-prompt and bottom-of-prompt horizontal rules drawn by
//! `draw_bar`. Both collapse on narrow terminals by dropping the
//! highest-priority (least-important) spans first.

use super::{layout_out::display_width, selection::truncate_str};
use crossterm::style::Color;

/// Foreground / background / attribute snapshot for a status-line
/// span. The status bar is the only consumer; it converts to
/// `crate::ui::grid::Style` at paint time via `style_to_grid` below.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct StyleState {
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) crossedout: bool,
    pub(crate) underline: bool,
}

/// A structured status item that Lua (or internal code) provides.
/// Rust owns width fitting, priority dropping, and truncation.
#[derive(Clone, Debug)]
pub(crate) struct StatusItem {
    pub(crate) text: String,
    pub(crate) fg: Option<Color>,
    pub(crate) bg: Option<Color>,
    pub(crate) bold: bool,
    pub(crate) priority: u8,
    pub(crate) align_right: bool,
    pub(crate) truncatable: bool,
    pub(crate) group: bool,
}

impl StatusItem {
    pub(crate) fn to_span(&self, fill_bg: Color) -> StatusSpan {
        StatusSpan {
            text: self.text.clone(),
            style: StyleState {
                fg: self.fg,
                bg: Some(self.bg.unwrap_or(fill_bg)),
                bold: self.bold,
                ..StyleState::default()
            },
            priority: self.priority,
            align_right: self.align_right,
            truncatable: self.truncatable,
            group: self.group,
        }
    }
}

pub(crate) fn vim_mode_label(mode: Option<crate::ui::VimMode>) -> Option<&'static str> {
    match mode {
        Some(crate::ui::VimMode::Insert) => Some("INSERT"),
        Some(crate::ui::VimMode::Visual) => Some("VISUAL"),
        Some(crate::ui::VimMode::VisualLine) => Some("VISUAL LINE"),
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
#[derive(Default)]
pub(crate) struct StatusSpan {
    pub(crate) text: String,
    pub(crate) style: StyleState,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    pub(crate) priority: u8,
    /// If true, a " · " separator is inserted before this span during rendering.
    pub(crate) group: bool,
    /// If true, the text can be truncated with "…" before being fully dropped.
    pub(crate) truncatable: bool,
    /// If true, the span is pulled to the right edge of the status bar
    /// with a one-cell gap before the terminal edge. Right-aligned
    /// spans render after every left-aligned span and don't accept
    /// group separators.
    pub(crate) align_right: bool,
}

/// Separator inserted between groups in the status line.
const STATUS_SEP: &str = " \u{00b7} "; // " · "
const STATUS_SEP_LEN: usize = 3;

/// One styled run inside a baked status line. `col_start` /  `col_end`
/// are display-cell offsets into [`StatusLine::text`]; `style` is the
/// fully-merged `crate::ui::grid::Style` (already includes `fill_bg` for
/// segments that didn't override it). Non-overlapping; together with
/// gap fills they cover the whole line width so every painted cell
/// gets exactly one span.
#[derive(Clone, Debug)]
pub(crate) struct StatusSpanOut {
    pub(crate) col_start: u16,
    pub(crate) col_end: u16,
    pub(crate) style: crate::ui::grid::Style,
}

/// Materialised status line: a flat string padded to terminal width
/// plus the spans `Buffer::add_highlight` should attach. `text` is
/// always a single line of length `width` cells; left-aligned spans
/// pack from column 0, right-aligned spans align to `width - 1` with
/// one cell of right-edge gap.
#[derive(Clone, Debug, Default)]
pub(crate) struct StatusLine {
    pub(crate) text: String,
    pub(crate) spans: Vec<StatusSpanOut>,
}

pub(crate) fn spans_to_buffer_line(
    spans: &mut Vec<StatusSpan>,
    width: usize,
    fill_bg: Color,
    sep_fg: Option<Color>,
) -> StatusLine {
    const RIGHT_EDGE_GAP: usize = 1;

    let span_cols = |spans: &[StatusSpan], right: bool| -> usize {
        let mut w = 0;
        let mut first = true;
        for s in spans.iter().filter(|s| s.align_right == right) {
            if s.group && !first {
                w += STATUS_SEP_LEN;
            }
            w += display_width(&s.text);
            first = false;
        }
        w
    };
    let total_width = |spans: &[StatusSpan]| -> usize {
        let left = span_cols(spans, false);
        let right = span_cols(spans, true);
        let gap = if right > 0 { RIGHT_EDGE_GAP } else { 0 };
        left + right + gap
    };

    while total_width(spans) > width && !spans.is_empty() {
        let max_pri = spans.iter().map(|s| s.priority).max().unwrap_or(0);
        if max_pri == 0 {
            break;
        }
        let trunc_idx = spans
            .iter()
            .rposition(|s| s.priority == max_pri && s.truncatable);
        if let Some(idx) = trunc_idx {
            let available =
                width.saturating_sub(total_width(spans) - display_width(&spans[idx].text));
            if available >= 2 {
                spans[idx].text = truncate_str(&spans[idx].text, available);
                continue;
            }
        }
        spans.retain(|s| s.priority != max_pri);
    }

    let sep_style = crate::ui::grid::Style {
        fg: sep_fg,
        bg: Some(fill_bg),
        dim: true,
        ..crate::ui::grid::Style::default()
    };
    let fill_style = crate::ui::grid::Style {
        bg: Some(fill_bg),
        ..crate::ui::grid::Style::default()
    };
    let style_to_grid = |ss: &StyleState| -> crate::ui::grid::Style {
        crate::ui::grid::Style {
            fg: ss.fg,
            bg: ss.bg.or(Some(fill_bg)),
            bold: ss.bold,
            dim: ss.dim,
            italic: ss.italic,
            underline: ss.underline,
            crossedout: ss.crossedout,
        }
    };

    // Emit (text, style) pairs for the left half (col 0 → ...) and
    // the right half (... → width). Right segments are concatenated
    // forward; we offset them to the right edge below.
    let mut left_runs: Vec<(String, crate::ui::grid::Style)> = Vec::new();
    let mut right_runs: Vec<(String, crate::ui::grid::Style)> = Vec::new();

    let mut first_left = true;
    for s in spans.iter().filter(|s| !s.align_right) {
        if s.group && !first_left {
            left_runs.push((STATUS_SEP.to_string(), sep_style));
        }
        left_runs.push((s.text.clone(), style_to_grid(&s.style)));
        first_left = false;
    }
    let mut first_right = true;
    for s in spans.iter().filter(|s| s.align_right) {
        if s.group && !first_right {
            right_runs.push((STATUS_SEP.to_string(), sep_style));
        }
        right_runs.push((s.text.clone(), style_to_grid(&s.style)));
        first_right = false;
    }

    let right_w: usize = right_runs.iter().map(|(t, _)| display_width(t)).sum();
    let right_start = width.saturating_sub(right_w);

    // Build the line text: left runs at col 0, spaces in the middle,
    // right runs ending at col `width`. Pad with spaces so the text is
    // exactly `width` cells wide.
    let mut text = String::with_capacity(width);
    let mut out_spans: Vec<StatusSpanOut> = Vec::new();

    // Left runs.
    let mut col: usize = 0;
    for (t, style) in &left_runs {
        let cells = display_width(t);
        let start = col;
        let end = (col + cells).min(width);
        if start < end {
            text.push_str(t);
            out_spans.push(StatusSpanOut {
                col_start: start as u16,
                col_end: end as u16,
                style: *style,
            });
        }
        col = end;
    }
    // Gap between left and right (filled with status_bg).
    if col < right_start {
        let pad = right_start - col;
        for _ in 0..pad {
            text.push(' ');
        }
        out_spans.push(StatusSpanOut {
            col_start: col as u16,
            col_end: right_start as u16,
            style: fill_style,
        });
        col = right_start;
    }
    // Right runs.
    for (t, style) in &right_runs {
        let cells = display_width(t);
        let start = col;
        let end = (col + cells).min(width);
        if start < end {
            text.push_str(t);
            out_spans.push(StatusSpanOut {
                col_start: start as u16,
                col_end: end as u16,
                style: *style,
            });
        }
        col = end;
    }
    // Pad to full width if right was truncated below `width` cells.
    while col < width {
        text.push(' ');
        col += 1;
    }
    // Make sure the final span covers any tail short of `width` (only
    // happens when `right_w == 0` and the last left run was truncated
    // — the gap span above already covered the middle in normal cases).
    let tail_start = out_spans.last().map(|s| s.col_end as usize).unwrap_or(0);
    if tail_start < width {
        if let Some(last) = out_spans.last_mut() {
            if last.style == fill_style {
                last.col_end = width as u16;
            } else {
                out_spans.push(StatusSpanOut {
                    col_start: tail_start as u16,
                    col_end: width as u16,
                    style: fill_style,
                });
            }
        } else {
            out_spans.push(StatusSpanOut {
                col_start: 0,
                col_end: width as u16,
                style: fill_style,
            });
        }
    }

    StatusLine {
        text,
        spans: out_spans,
    }
}
