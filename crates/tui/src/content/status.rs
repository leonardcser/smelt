//! Status line and bar span types with priority-based responsive collapsing.

use super::{builder::display_width, selection::truncate_str};
use smelt_core::style::{Color, Style};

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

#[derive(Clone, Debug)]
pub(crate) struct StatusItem {
    pub(crate) text: String,
    pub(crate) style: Style,
    pub(crate) priority: u8,
    pub(crate) align_right: bool,
    pub(crate) truncatable: bool,
    pub(crate) separated: bool,
}

impl StatusItem {
    pub(crate) fn to_span(&self, fill_bg: Color) -> StatusSpan {
        StatusSpan {
            text: self.text.clone(),
            style: StyleState {
                fg: self.style.fg,
                bg: Some(self.style.bg.unwrap_or(fill_bg)),
                bold: self.style.bold,
                dim: self.style.dim,
                italic: self.style.italic,
                underline: self.style.underline,
                crossedout: self.style.crossedout,
            },
            priority: self.priority,
            align_right: self.align_right,
            truncatable: self.truncatable,
            separated: self.separated,
        }
    }
}

pub(crate) fn vim_mode_label(mode: Option<crate::smelt_term::VimMode>) -> Option<&'static str> {
    match mode {
        Some(crate::smelt_term::VimMode::Insert) => Some("INSERT"),
        Some(crate::smelt_term::VimMode::Visual) => Some("VISUAL"),
        Some(crate::smelt_term::VimMode::VisualLine) => Some("VISUAL LINE"),
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

#[derive(Default)]
pub(crate) struct StatusSpan {
    pub(crate) text: String,
    pub(crate) style: StyleState,
    /// 0 = always show, higher = drop first.
    pub(crate) priority: u8,
    /// Insert " · " separator before this span.
    pub(crate) separated: bool,
    /// Can truncate with "…" before being fully dropped.
    pub(crate) truncatable: bool,
    /// Pull to right edge with a one-cell gap; no separator.
    pub(crate) align_right: bool,
}

const STATUS_SEP: &str = " \u{00b7} "; // " · "
const STATUS_SEP_LEN: usize = 3;

#[derive(Clone, Debug)]
pub(crate) struct StatusSpanOut {
    pub(crate) col_start: u16,
    pub(crate) col_end: u16,
    pub(crate) style: crate::smelt_term::grid::Style,
}

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
            if s.separated && !first {
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

    let sep_style = crate::smelt_term::grid::Style {
        fg: sep_fg,
        bg: Some(fill_bg),
        dim: true,
        ..crate::smelt_term::grid::Style::default()
    };
    let fill_style = crate::smelt_term::grid::Style {
        bg: Some(fill_bg),
        ..crate::smelt_term::grid::Style::default()
    };
    let style_to_grid = |ss: &StyleState| -> crate::smelt_term::grid::Style {
        crate::smelt_term::grid::Style {
            fg: ss.fg,
            bg: ss.bg.or(Some(fill_bg)),
            bold: ss.bold,
            dim: ss.dim,
            italic: ss.italic,
            underline: ss.underline,
            crossedout: ss.crossedout,
        }
    };

    let mut left_runs: Vec<(String, crate::smelt_term::grid::Style)> = Vec::new();
    let mut right_runs: Vec<(String, crate::smelt_term::grid::Style)> = Vec::new();

    let mut first_left = true;
    for s in spans.iter().filter(|s| !s.align_right) {
        if s.separated && !first_left {
            left_runs.push((STATUS_SEP.to_string(), sep_style));
        }
        left_runs.push((s.text.clone(), style_to_grid(&s.style)));
        first_left = false;
    }
    let mut first_right = true;
    for s in spans.iter().filter(|s| s.align_right) {
        if s.separated && !first_right {
            right_runs.push((STATUS_SEP.to_string(), sep_style));
        }
        right_runs.push((s.text.clone(), style_to_grid(&s.style)));
        first_right = false;
    }

    let right_w: usize = right_runs.iter().map(|(t, _)| display_width(t)).sum();
    let right_start = width.saturating_sub(right_w);

    let mut text = String::with_capacity(width);
    let mut out_spans: Vec<StatusSpanOut> = Vec::new();

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
    while col < width {
        text.push(' ');
        col += 1;
    }
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
