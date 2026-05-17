use super::selection::{
    build_char_kinds, build_display_spans, map_cursor, spans_to_string, wrap_line,
    wrap_with_offsets, SpanKind,
};
use super::status::BarSpan;
use crate::input::PromptState;
use crate::smelt_term::grid::Style;
use crate::smelt_term::{Buffer, ExtmarkOpts, ExtmarkPayload};

use smelt_core::style::Color;

/// Extmark namespace for ghost-text (completer prediction).
pub(crate) const COMPLETER_NS: &str = "completer";

struct StyledSegment {
    text: String,
    style: Style,
}

struct WindowRow {
    segments: Vec<StyledSegment>,
}

impl WindowRow {
    fn styled(segments: Vec<StyledSegment>) -> Self {
        Self { segments }
    }
}

pub(crate) struct PromptAboveInput<'a> {
    pub(crate) queued: &'a [String],
    pub(crate) stash: &'a Option<crate::input::InputSnapshot>,
    pub(crate) bar_info: BarInfo,
    pub(crate) width: u16,
}

pub(crate) struct InputLeafInput<'a> {
    pub(crate) input: &'a PromptState,
    pub(crate) win: &'a crate::smelt_term::Window,
    pub(crate) clipboard: &'a crate::smelt_term::Clipboard,
    /// Inner width after gutters; `Window::render` shifts content past the left gutter.
    pub(crate) content_width: u16,
    pub(crate) height: u16,
    /// Host clock at render time; used to compute the yank-flash window.
    pub(crate) now: std::time::Instant,
}

pub(crate) struct BarInfo {
    pub(crate) model_label: Option<String>,
    pub(crate) reasoning_effort: protocol::ReasoningEffort,
    pub(crate) show_tokens: bool,
    pub(crate) context_tokens: Option<u32>,
    pub(crate) context_window: Option<u32>,
    pub(crate) show_cost: bool,
    pub(crate) session_cost_usd: f64,
}

fn theme_color(theme: &crate::smelt_term::Theme, group: &str) -> Color {
    let style = theme.get(group);
    style.fg.or(style.bg).unwrap_or(Color::Reset)
}

pub(crate) fn compute_prompt_above(
    pa: &PromptAboveInput<'_>,
    buf: &mut Buffer,
    theme: &crate::smelt_term::Theme,
) {
    let usable = pa.width as usize;

    let mut rows: Vec<WindowRow> = Vec::new();
    rows.extend(queued_message_rows(pa.queued, usable, theme));
    if pa.stash.is_some() {
        rows.push(stash_row(usable, theme));
    }
    let top_bar_right = build_top_bar_right(&pa.bar_info, theme);
    rows.push(bar_row(
        usable,
        None,
        if top_bar_right.is_empty() {
            None
        } else {
            Some(&top_bar_right)
        },
        theme,
    ));

    let mut all_lines: Vec<String> = Vec::with_capacity(rows.len());
    let mut all_highlights: Vec<(usize, u16, u16, Style)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let (text, hls) = window_row_to_buffer_line(row);
        all_lines.push(text);
        for (s, e, style) in hls {
            all_highlights.push((i, s, e, style));
        }
    }
    let total = all_lines.len();
    buf.set_all_lines(all_lines);
    buf.clear_highlights(0, total);
    for (line, s, e, style) in all_highlights {
        buf.add_highlight(line, s, e, style);
    }
    buf.set_selection(Vec::new());
}

pub(crate) fn compute_prompt_below(width: u16, buf: &mut Buffer, theme: &crate::smelt_term::Theme) {
    let bottom = bar_row(width as usize, None, None, theme);
    let (text, hls) = window_row_to_buffer_line(&bottom);
    buf.set_all_lines(vec![text]);
    buf.clear_highlights(0, 1);
    for (s, e, style) in hls {
        buf.add_highlight(0, s, e, style);
    }
    buf.set_selection(Vec::new());
}

/// Write editable input rows, selection, and ghost-text extmark into `buf`.
/// Cursor/selection positions are content-local; `Window::render` applies the gutter shift.
pub(crate) fn compute_input(
    inp: &InputLeafInput<'_>,
    buf: &mut Buffer,
    theme: &crate::smelt_term::Theme,
) {
    let usable = inp.content_width as usize;

    let completer_ns = buf.create_namespace(COMPLETER_NS);
    let prediction: Option<String> = buf.extmarks(completer_ns).into_iter().find_map(|(_, m)| {
        if let ExtmarkPayload::VirtText { text, .. } = &m.payload {
            Some(text.clone())
        } else {
            None
        }
    });

    if buf.has_parser() {
        // Parser already built lines and highlights. Just map selection and ghost text.
        let total_lines = buf.line_count();

        // Map source selection to display selection via the shared helper —
        // same code path the transcript will eventually use.
        let pctx_ref = crate::input::PromptCtxRef { buf, win: inp.win };
        if let Some((start, end)) =
            inp.input
                .display_selection_range(pctx_ref, inp.clipboard, inp.now)
        {
            let ranges = smelt_buffer::coords::selection_to_row_ranges(buf, start, end);
            buf.set_selection(ranges);
        } else {
            buf.set_selection(Vec::new());
        }

        buf.clear_namespace(completer_ns, 0, usize::MAX);
        if let Some(text) = prediction {
            let row = if buf.source().is_empty() {
                0
            } else {
                total_lines
            };
            buf.set_extmark(
                completer_ns,
                row,
                0,
                ExtmarkOpts::virt_text(text, Some("GhostText".into())),
            );
        }
        return;
    }

    let input_area = compute_input_area(inp, buf, usable, theme);

    let lines = input_area.lines.clone();
    let total_lines = lines.len();
    buf.set_all_lines(lines);
    buf.clear_highlights(0, total_lines);
    for &(li, s, e, ref style) in &input_area.highlights {
        buf.add_highlight(li, s, e, *style);
    }
    let selection: Vec<crate::smelt_term::SelectionRange> = input_area
        .selection_ranges
        .iter()
        .map(|(li, s, e)| crate::smelt_term::SelectionRange {
            line: *li,
            col_start: *s,
            col_end: *e,
        })
        .collect();
    buf.set_selection(selection);

    buf.clear_namespace(completer_ns, 0, usize::MAX);
    if let Some(text) = prediction {
        // Row 0 when input is empty (dim suggestion visible); past last row otherwise
        // (keeps storage alive without rendering; Window::render only walks 0..line_count).
        let row = if buf.source().is_empty() {
            0
        } else {
            total_lines
        };
        buf.set_extmark(
            completer_ns,
            row,
            0,
            ExtmarkOpts::virt_text(text, Some("GhostText".into())),
        );
    }
}

fn window_row_to_buffer_line(row: &WindowRow) -> (String, Vec<(u16, u16, Style)>) {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut col: u16 = 0;
    for seg in &row.segments {
        let start = col;
        for ch in seg.text.chars() {
            text.push(ch);
            col = col.saturating_add(1);
        }
        if col > start && seg.style != Style::default() {
            highlights.push((start, col, seg.style));
        }
    }
    (text, highlights)
}

fn queued_message_rows(
    queued: &[String],
    usable: usize,
    theme: &crate::smelt_term::Theme,
) -> Vec<WindowRow> {
    let indent = 1usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = Vec::new();
    let user_bg = theme_color(theme, "SmeltUserBg");

    let comment_fg = theme_color(theme, "Comment");
    for msg in queued {
        let is_command = smelt_core::commands::is_command(msg.trim());
        let geom = crate::content::transcript_parsers::UserBlockGeometry::new(msg, text_w);
        let mut first_chunk = true;
        for line in &geom.lines {
            if line.is_empty() {
                let fill_w = if geom.block_w > 0 {
                    geom.block_w + 1
                } else {
                    2
                };
                let mut segs = vec![StyledSegment {
                    text: " ".repeat(indent),
                    style: Style::default(),
                }];
                segs.push(StyledSegment {
                    text: " ".repeat(fill_w),
                    style: Style::new().bg(user_bg),
                });
                rows.push(WindowRow::styled(segs));
                continue;
            }
            let chunks = wrap_line(line, text_w);
            for chunk in &chunks {
                let chunk_w = super::builder::display_width(chunk);
                let trailing = if geom.block_w > 0 {
                    geom.block_w.saturating_sub(chunk_w)
                } else {
                    1
                };
                let bg_style = Style {
                    bg: Some(user_bg),
                    fg: Some(comment_fg),
                    bold: true,
                    ..Style::default()
                };

                let mut segs = vec![StyledSegment {
                    text: " ".repeat(indent),
                    style: Style::default(),
                }];
                let prefix = if first_chunk { "↪ " } else { "  " };
                segs.push(StyledSegment {
                    text: prefix.into(),
                    style: bg_style,
                });

                let chunk_segs = user_highlight_segments(chunk, is_command, bg_style, theme);
                segs.extend(chunk_segs);

                segs.push(StyledSegment {
                    text: " ".repeat(trailing),
                    style: bg_style,
                });
                rows.push(WindowRow::styled(segs));
                first_chunk = false;
            }
        }
    }
    rows
}

fn user_highlight_segments(
    text: &str,
    is_command: bool,
    base_style: Style,
    theme: &crate::smelt_term::Theme,
) -> Vec<StyledSegment> {
    if is_command {
        return vec![StyledSegment {
            text: text.into(),
            style: Style {
                fg: Some(theme_color(theme, "SmeltAccent")),
                ..base_style
            },
        }];
    }

    vec![StyledSegment {
        text: text.into(),
        style: base_style,
    }]
}

fn stash_row(_usable: usize, theme: &crate::smelt_term::Theme) -> WindowRow {
    let text = "» Stashed (ctrl+s to unstash)";
    let display: String = text.chars().take(_usable).collect();
    WindowRow::styled(vec![
        StyledSegment {
            text: "  ".into(),
            style: Style::default(),
        },
        StyledSegment {
            text: display,
            style: Style {
                fg: Some(theme_color(theme, "Comment")),
                ..Style::default()
            },
        },
    ])
}

fn bar_row(
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<&[BarSpan]>,
    theme: &crate::smelt_term::Theme,
) -> WindowRow {
    let dash = "\u{2500}";
    let bar_color = theme_color(theme, "SmeltBar");
    let min_dashes = 4;

    let max_priority = left
        .into_iter()
        .chain(right)
        .flat_map(|spans| spans.iter().map(|s| s.priority))
        .max()
        .unwrap_or(0);

    let mut drop_above = max_priority + 1;
    loop {
        let left_chars: usize = left
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| super::builder::display_width(&s.text))
                    .sum();
                if inner > 0 {
                    inner + 1
                } else {
                    0
                }
            })
            .unwrap_or(0);
        let right_chars: usize = right
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| super::builder::display_width(&s.text))
                    .sum();
                if inner > 0 {
                    inner + 2
                } else {
                    0
                }
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
            .map(|s| super::builder::display_width(&s.text))
            .sum::<usize>()
            + 1
    };
    let right_len: usize = if right_filtered.is_empty() {
        0
    } else {
        right_filtered
            .iter()
            .map(|s| super::builder::display_width(&s.text))
            .sum::<usize>()
            + 2
    };
    let bar_len = width.saturating_sub(left_len + right_len);

    let mut segs: Vec<StyledSegment> = Vec::new();

    for span in &left_filtered {
        segs.push(StyledSegment {
            text: span.text.clone(),
            style: Style {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..Style::default()
            },
        });
    }
    if !left_filtered.is_empty() {
        segs.push(StyledSegment {
            text: " ".into(),
            style: Style::default(),
        });
    }

    segs.push(StyledSegment {
        text: dash.repeat(bar_len),
        style: Style::new().fg(bar_color),
    });

    if !right_filtered.is_empty() {
        for span in &right_filtered {
            segs.push(StyledSegment {
                text: span.text.clone(),
                style: Style {
                    fg: Some(span.color),
                    bg: span.bg,
                    bold: span.bold,
                    dim: span.dim,
                    ..Style::default()
                },
            });
        }
        segs.push(StyledSegment {
            text: " ".into(),
            style: Style::default(),
        });
        segs.push(StyledSegment {
            text: dash.into(),
            style: Style::new().fg(bar_color),
        });
    }

    WindowRow::styled(segs)
}

fn build_top_bar_right(info: &BarInfo, theme: &crate::smelt_term::Theme) -> Vec<BarSpan> {
    let muted = theme_color(theme, "Comment");
    let bar = theme_color(theme, "SmeltBar");
    let mut spans = Vec::new();
    if let Some(ref model) = info.model_label {
        spans.push(BarSpan {
            text: format!(" {}", model),
            color: muted,
            bg: None,
            bold: false,
            dim: false,
            priority: 2,
        });
        if info.reasoning_effort != protocol::ReasoningEffort::Off {
            let effort = info.reasoning_effort;
            spans.push(BarSpan {
                text: format!(" {}", effort.label()),
                color: super::reasoning_color(effort, theme),
                bg: None,
                bold: false,
                dim: false,
                priority: 2,
            });
        }
    }
    if info.show_tokens {
        if let Some(tokens) = info.context_tokens {
            if !spans.is_empty() {
                spans.push(BarSpan {
                    text: " ·".into(),
                    color: bar,
                    bg: None,
                    bold: false,
                    dim: false,
                    priority: 2,
                });
            }
            let token_text = if let Some(window) = info.context_window {
                if window > 0 {
                    let pct = (tokens as f64 / window as f64 * 100.0) as u32;
                    format!(" {} ({}%)", super::format_tokens(tokens), pct)
                } else {
                    format!(" {}", super::format_tokens(tokens))
                }
            } else {
                format!(" {}", super::format_tokens(tokens))
            };
            spans.push(BarSpan {
                text: token_text,
                color: muted,
                bg: None,
                bold: false,
                dim: false,
                priority: 1,
            });
        }
    }
    if info.show_cost && info.session_cost_usd > 0.0 {
        if !spans.is_empty() {
            spans.push(BarSpan {
                text: " ·".into(),
                color: bar,
                bg: None,
                bold: false,
                dim: false,
                priority: 2,
            });
        }
        spans.push(BarSpan {
            text: format!(" {}", crate::metrics::format_cost(info.session_cost_usd)),
            color: muted,
            bg: None,
            bold: false,
            dim: false,
            priority: 1,
        });
    }
    spans
}

struct InputArea {
    lines: Vec<String>,
    highlights: Vec<(usize, u16, u16, Style)>,
    selection_ranges: Vec<(usize, u16, u16)>,
}

fn compute_input_area(
    input: &InputLeafInput<'_>,
    edit_buf: &crate::smelt_term::Buffer,
    usable: usize,
    theme: &crate::smelt_term::Theme,
) -> InputArea {
    let height = input.height as usize;
    let state = input.input;

    let spans = build_display_spans(
        edit_buf.source(),
        &edit_buf.attachment_ids,
        &state.store.lock().unwrap(),
    );
    let display_buf = spans_to_string(&spans);
    let char_kinds = build_char_kinds(&spans);
    let pctx_ref = crate::input::PromptCtxRef {
        buf: edit_buf,
        win: input.win,
    };
    let display_selection = state
        .display_selection_range(pctx_ref, input.clipboard, input.now)
        .map(|(start, end)| {
            let raw_start_char = crate::input::char_pos(edit_buf.source(), start);
            let raw_end_char = crate::input::char_pos(edit_buf.source(), end);
            let ds = map_cursor(raw_start_char, edit_buf.source(), &spans);
            let de = map_cursor(raw_end_char, edit_buf.source(), &spans);
            (ds, de)
        });
    let wrap_out = wrap_with_offsets(&display_buf, &char_kinds, usable);
    let visual_lines = wrap_out.visual_lines;
    let line_char_offsets = wrap_out.row_offsets;
    let single_line = !edit_buf.source().contains('\n');
    let plain_only = !single_line;
    let is_command = !plain_only && smelt_core::commands::is_command(edit_buf.source().trim());
    let is_exec = !plain_only
        && matches!(edit_buf.source().as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
    let is_exec_invalid = !plain_only && edit_buf.source() == "!";
    let total_content_rows = visual_lines.len();

    let max_content_rows = height.max(1);
    let content_rows = total_content_rows.min(max_content_rows);
    let scroll_offset = input.win.scroll_top as usize;

    let mut highlights: Vec<(usize, u16, u16, Style)> = Vec::new();
    let mut selection_ranges: Vec<(usize, u16, u16)> = Vec::new();

    let mut lines: Vec<String> = visual_lines
        .iter()
        .skip(scroll_offset)
        .take(content_rows)
        .map(|(line, _)| line.clone())
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }

    for (li, (line, kinds)) in visual_lines
        .iter()
        .skip(scroll_offset)
        .take(content_rows)
        .enumerate()
    {
        let abs_idx = scroll_offset + li;
        let line_chars = line.chars().count();

        let line_sel = display_selection.and_then(|(sel_start, sel_end)| {
            let line_start = line_char_offsets[abs_idx];
            let line_end = line_start + line_chars;
            if line_chars == 0 && sel_start <= line_start && sel_end > line_start {
                Some((0usize, 1usize))
            } else if sel_end <= line_start || sel_start >= line_end {
                None
            } else {
                let s = sel_start.saturating_sub(line_start);
                let e = sel_end.min(line_end) - line_start;
                Some((s, e))
            }
        });

        // Virtual trailing cell paints when selection extends past line end
        // so line-end participation in `v`/`V` is visible.
        if let Some((s, e)) = line_sel {
            let s_col = s as u16;
            let raw_e = e.min(line_chars);
            let visible_end = raw_e as u16;
            let virtual_tail = if e > line_chars && s <= line_chars {
                1
            } else {
                0
            };
            let total_end = visible_end + virtual_tail;
            if line_chars == 0 {
                selection_ranges.push((li, 0, 1));
            } else if total_end > s_col {
                selection_ranges.push((li, s_col, total_end));
            }
        }

        let segments = if is_command {
            let prefix_chars = if abs_idx == 0 {
                line.char_indices()
                    .find(|(_, c)| c.is_whitespace())
                    .map(|(i, _)| line[..i].chars().count())
                    .unwrap_or(line_chars)
            } else {
                0
            };
            let mut cmd_kinds = vec![SpanKind::AtRef; prefix_chars];
            cmd_kinds.resize(line_chars, SpanKind::Plain);
            styled_char_segments(line, &cmd_kinds, theme)
        } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
            exec_bang_segments(line, kinds, theme)
        } else {
            styled_char_segments(line, kinds, theme)
        };
        push_segment_highlights(&mut highlights, li, &segments);
    }

    InputArea {
        lines,
        highlights,
        selection_ranges,
    }
}

fn push_segment_highlights(
    out: &mut Vec<(usize, u16, u16, Style)>,
    line_idx: usize,
    segments: &[StyledSegment],
) {
    let mut col = 0u16;
    for seg in segments {
        let len = seg.text.chars().count() as u16;
        if len > 0 {
            let end = col + len;
            if seg.style != Style::default() {
                out.push((line_idx, col, end, seg.style));
            }
            col = end;
        }
    }
}

fn styled_char_segments(
    line: &str,
    kinds: &[SpanKind],
    theme: &crate::smelt_term::Theme,
) -> Vec<StyledSegment> {
    let mut segments: Vec<StyledSegment> = Vec::new();
    let mut current_text = String::new();
    let mut current_style = Style::default();
    let accent = theme_color(theme, "SmeltAccent");

    for (i, ch) in line.chars().enumerate() {
        let kind = kinds.get(i).copied().unwrap_or(SpanKind::Plain);
        let fg = match kind {
            SpanKind::AtRef | SpanKind::Attachment => Some(accent),
            SpanKind::Plain => None,
        };
        let style = Style {
            fg,
            ..Style::default()
        };

        if style != current_style && !current_text.is_empty() {
            segments.push(StyledSegment {
                text: std::mem::take(&mut current_text),
                style: current_style,
            });
        }
        current_style = style;
        current_text.push(ch);
    }

    if !current_text.is_empty() {
        segments.push(StyledSegment {
            text: current_text,
            style: current_style,
        });
    }

    segments
}

fn exec_bang_segments(
    line: &str,
    kinds: &[SpanKind],
    theme: &crate::smelt_term::Theme,
) -> Vec<StyledSegment> {
    let mut segs = Vec::new();

    segs.push(StyledSegment {
        text: "!".into(),
        style: Style {
            fg: Some(Color::Red),
            bold: true,
            ..Style::default()
        },
    });

    if line.len() > 1 {
        segs.extend(styled_char_segments(
            &line[1..],
            if kinds.len() > 1 { &kinds[1..] } else { &[] },
            theme,
        ));
    }

    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> crate::smelt_term::Theme {
        let mut t = crate::smelt_term::Theme::new();
        crate::theme::populate_ui_theme(&mut t);
        t
    }

    #[test]
    fn stash_row_has_muted_style() {
        let theme = test_theme();
        let row = stash_row(40, &theme);
        assert_eq!(row.segments[1].style.fg, Some(theme_color(&theme, "Comment")));
    }

    #[test]
    fn bar_row_fills_with_dashes() {
        let row = bar_row(20, None, None, &test_theme());
        let text: String = row.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("────"));
    }

    #[test]
    fn bar_row_with_right_spans() {
        let right = vec![BarSpan {
            text: " model".into(),
            color: smelt_core::style::Color::White,
            bg: None,
            bold: false,
            dim: false,
            priority: 0,
        }];
        let row = bar_row(30, None, Some(&right), &test_theme());
        let text: String = row.segments.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains(" model"));
        assert!(text.contains("────"));
    }

    #[test]
    fn styled_char_segments_plain() {
        let segs = styled_char_segments("hello", &[SpanKind::Plain; 5], &test_theme());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "hello");
    }

    #[test]
    fn exec_bang_segments_highlights_bang() {
        let kinds = vec![SpanKind::Plain; 4];
        let segs = exec_bang_segments("!ls", &kinds, &test_theme());
        assert_eq!(segs[0].text, "!");
        assert_eq!(segs[0].style.fg, Some(smelt_core::style::Color::Red));
        assert!(segs[0].style.bold);
    }

    #[test]
    fn compute_input_writes_input_only() {
        let input_state = PromptState::default();
        let test_clipboard = crate::smelt_term::Clipboard::null();
        let test_win = crate::smelt_term::Window::new(
            crate::app::PROMPT_WIN,
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_term::SplitConfig {
                region: "prompt".into(),
                gutters: crate::smelt_term::Gutters::default(),
            },
        );
        let inp = InputLeafInput {
            input: &input_state,
            win: &test_win,
            clipboard: &test_clipboard,
            content_width: 78,
            height: 4,
            now: std::time::Instant::now(),
        };
        let mut input_buf = Buffer::new(
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_term::BufCreateOpts::default(),
        );
        compute_input(&inp, &mut input_buf, &test_theme());
        assert_eq!(input_buf.line_count(), 1);
    }

    #[test]
    fn compute_prompt_above_writes_top_bar() {
        let pa = PromptAboveInput {
            queued: &[],
            stash: &None,
            width: 80,
            bar_info: BarInfo {
                model_label: None,
                reasoning_effort: protocol::ReasoningEffort::Off,
                show_tokens: false,
                context_tokens: None,
                context_window: None,
                show_cost: false,
                session_cost_usd: 0.0,
            },
        };
        let mut buf = Buffer::new(
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_term::BufCreateOpts::default(),
        );
        compute_prompt_above(&pa, &mut buf, &test_theme());
        assert_eq!(buf.line_count(), 1);
        let line = buf.get_line(0).unwrap_or("");
        let chars = line.chars().count();
        assert!(
            chars >= 78,
            "bar should fill nearly the full width, got {chars} chars"
        );
    }

    #[test]
    fn compute_prompt_below_writes_one_bar_row() {
        let mut buf = Buffer::new(
            crate::app::PROMPT_EDIT_BUF,
            crate::smelt_term::BufCreateOpts::default(),
        );
        compute_prompt_below(80, &mut buf, &test_theme());
        assert_eq!(buf.line_count(), 1);
    }

    #[test]
    fn window_render_honors_prompt_gutters() {
        use crate::app::PROMPT_WIN;
        use crate::smelt_term::{
            grid::Grid, BufCreateOpts, CursorShape, DrawContext, Gutters, SplitConfig, Theme,
            Window,
        };

        let theme = std::sync::Arc::new({
            let mut t = Theme::new();
            crate::theme::populate_ui_theme(&mut t);
            t
        });

        let mut buf = Buffer::new(crate::app::PROMPT_EDIT_BUF, BufCreateOpts::default());
        buf.set_all_lines(vec!["hello".into()]);

        let win = Window::new(
            PROMPT_WIN,
            buf.id(),
            SplitConfig {
                region: "prompt".into(),
                gutters: Gutters {
                    pad_left: 1,
                    pad_right: 1,
                    ..Default::default()
                },
            },
        );

        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(crate::smelt_term::Rect::new(0, 0, 10, 1));
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: true,
            cursor_shape: CursorShape::Block {
                glyph: '█',
                style: crate::smelt_term::grid::Style::default(),
                pos: None,
            },
            theme,
            vim_mode: crate::smelt_term::VimMode::Insert,
        };
        win.render(&buf, &mut slice, &ctx);

        let cells: Vec<char> = (0..10).map(|c| grid.cell(c, 0).symbol).collect();
        assert_eq!(cells[0], ' ', "left gutter blank");
        assert_eq!(cells[9], ' ', "right gutter blank");
        // Block cursor inverts the underlying glyph rather than stamping its own,
        // so the buffer char at the cursor stays visible and only its style flips.
        assert_eq!(cells[1], 'h', "block cursor preserves underlying glyph");
        assert_eq!(
            &cells[2..6],
            ['e', 'l', 'l', 'o'],
            "content paints inside inner zone"
        );
    }
}
