use super::selection::{
    build_char_kinds, build_display_spans, map_cursor, spans_to_string, wrap_with_offsets, SpanKind,
};
use crate::input::PromptState;
use crate::smelt_term::grid::Style;
use crate::smelt_term::{Buffer, ExtmarkOpts, ExtmarkPayload};

use smelt_core::style::Color;

/// Extmark namespace for `Win:placeholder` text rendered as a dim
/// suggestion when the buffer is empty.
pub(crate) const PLACEHOLDER_NS: &str = "placeholder";

struct StyledSegment {
    text: String,
    style: Style,
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

fn theme_color(theme: &crate::smelt_term::Theme, group: &str) -> Color {
    let style = theme.get(group);
    style.fg.or(style.bg).unwrap_or(Color::Reset)
}

/// Write editable input rows, selection, and ghost-text extmark into `buf`.
/// Cursor/selection positions are content-local; `Window::render` applies the gutter shift.
pub(crate) fn compute_input(
    inp: &InputLeafInput<'_>,
    buf: &mut Buffer,
    theme: &crate::smelt_term::Theme,
) {
    let usable = inp.content_width as usize;

    let placeholder_ns = buf.create_namespace(PLACEHOLDER_NS);
    let prediction: Option<String> = buf.extmarks(placeholder_ns).into_iter().find_map(|(_, m)| {
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

        buf.clear_namespace(placeholder_ns, 0, usize::MAX);
        if let Some(text) = prediction {
            let row = if buf.source().is_empty() {
                0
            } else {
                total_lines
            };
            buf.set_extmark(
                placeholder_ns,
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

    buf.clear_namespace(placeholder_ns, 0, usize::MAX);
    if let Some(text) = prediction {
        // Row 0 when input is empty (dim suggestion visible); past last row otherwise
        // (keeps storage alive without rendering; Window::render only walks 0..line_count).
        let row = if buf.source().is_empty() {
            0
        } else {
            total_lines
        };
        buf.set_extmark(
            placeholder_ns,
            row,
            0,
            ExtmarkOpts::virt_text(text, Some("GhostText".into())),
        );
    }
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
        crate::theme::default_baked().as_ref().clone()
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
    fn window_render_honors_prompt_gutters() {
        use crate::app::PROMPT_WIN;
        use crate::smelt_term::{
            grid::Grid, BufCreateOpts, CursorShape, DrawContext, Gutters, SplitConfig, Theme,
            Window,
        };

        let theme: std::sync::Arc<Theme> = crate::theme::default_baked().clone();

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
