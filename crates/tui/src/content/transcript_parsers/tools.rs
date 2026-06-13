use super::metrics::{block_inner_width, BLOCK_GUTTER_SPACE, BLOCK_GUTTER_W};
use super::MAX_TOOL_BLOCK_ROWS;
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use protocol::{StyledLines, StyledSpan};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{RenderedLayout, RenderedLeaf};
use smelt_core::content::builder::{replay_buffer_row_into, LineBuilder};
use smelt_core::content::highlight::InlineSyntax;
use smelt_core::content::wrap::wrap_line_ranges;
use smelt_core::theme::{intern, HlGroup};
use smelt_core::transcript_model::{ToolOutput, ToolStatus};
use std::collections::HashMap;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut LineBuilder,
    name: &str,
    summary: &StyledLines,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    rendered: Option<&RenderedLayout>,
    width: usize,
) -> u16 {
    let color: HlGroup = match status {
        ToolStatus::Ok => intern("SmeltSuccess"),
        ToolStatus::Err | ToolStatus::Denied => intern("ErrorMsg"),
        ToolStatus::Confirm => intern("SmeltAccent"),
        ToolStatus::Pending => intern("SmeltToolPending"),
    };
    let time = if status == ToolStatus::Confirm {
        None
    } else {
        elapsed
    };
    let mut rows = print_tool_line(out, name, summary, color, status, time, width);
    if let Some(msg) = user_message {
        print_dim(out, &format!("{BLOCK_GUTTER_SPACE}{msg}"));
        out.newline();
        rows += 1;
    }
    if status != ToolStatus::Denied {
        if let Some(layout) = rendered {
            let inner_width = block_inner_width(width) as u16;
            rows += replay_rendered(out, layout, inner_width);
        } else if let Some(out_data) = output {
            if !out_data.content.trim().is_empty() {
                rows += print_tool_output(out, name, out_data, args, width);
            }
        }
    }
    rows
}

struct ToolLineLayout {
    prefix_len: usize,
    max_summary: usize,
}

fn tool_line_layout(
    name: &str,
    suffix_len: usize,
    has_title_tail: bool,
    width: usize,
) -> ToolLineLayout {
    let prefix_len = 2 + name.len() + usize::from(has_title_tail); // "⏺ name" + optional separator
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    ToolLineLayout {
        prefix_len,
        max_summary,
    }
}

fn tool_title_suffix(elapsed: Option<Duration>) -> String {
    elapsed.and_then(format_tool_duration).unwrap_or_default()
}

fn format_tool_duration(duration: Duration) -> Option<String> {
    let secs = duration.as_secs();
    if secs < 1 {
        None
    } else if secs < 60 {
        Some(format!("{secs}s"))
    } else if secs < 60 * 60 {
        Some(format!("{}m{}s", secs / 60, secs % 60))
    } else {
        Some(format!("{}h{}m", secs / 3600, (secs % 3600) / 60))
    }
}

fn print_tool_line(
    out: &mut LineBuilder,
    name: &str,
    summary: &StyledLines,
    pill_color: HlGroup,
    status: ToolStatus,
    elapsed: Option<Duration>,
    width: usize,
) -> u16 {
    out.push_hl(pill_color);
    out.print("*");
    out.pop_style();
    let timer = tool_title_suffix(elapsed);
    let (summary, suffix_spans) = split_title_summary(summary, &timer, status);
    let has_summary = !summary.as_plain_text().is_empty();
    let suffix_text_len: usize = suffix_spans.iter().map(|s| s.text.len()).sum();
    let suffix_len = suffix_text_len
        + suffix_spans.len().saturating_sub(1)
        + 2 * usize::from(has_summary && !suffix_spans.is_empty());
    let has_title_tail = has_summary || !suffix_spans.is_empty();
    let ly = tool_line_layout(name, suffix_len, has_title_tail, width);

    print_dim(out, &format!(" {name}"));
    if has_title_tail {
        out.print(" ");
    }

    let max_seg = ly.max_summary.max(1);

    struct Wrapped {
        // Cumulative byte offsets: span i covers the concatenated plain at offs[i]..offs[i+1].
        offs: Vec<usize>,
        // Wrap segments as byte ranges into the concatenated plain text.
        ranges: Vec<(usize, usize)>,
    }

    let mut wlines: Vec<Wrapped> = summary
        .0
        .iter()
        .map(|spans| {
            let mut plain = String::new();
            let mut offs = Vec::with_capacity(spans.len() + 1);
            offs.push(0);
            for s in spans {
                plain.push_str(&s.text);
                offs.push(plain.len());
            }
            let ranges = wrap_line_ranges(&plain, max_seg);
            Wrapped { offs, ranges }
        })
        .collect();
    if wlines.is_empty() {
        wlines.push(Wrapped {
            offs: vec![0],
            ranges: vec![(0, 0)],
        });
    }

    if wlines.iter().any(|w| w.ranges.len() > 1) {
        out.mark_wrapped();
    }

    let total: usize = wlines.iter().map(|w| w.ranges.len()).sum();
    let show = total.min(MAX_TOOL_BLOCK_ROWS);
    let mut rows = 0u16;
    let mut emitted = 0usize;

    'outer: for (li, w) in wlines.iter().enumerate() {
        let spans: &[StyledSpan] = summary
            .0
            .get(li)
            .map(Vec::as_slice)
            .unwrap_or(&[] as &[StyledSpan]);
        let line_source = selectable_line_text(spans);
        for (seg_idx, &(rs, re)) in w.ranges.iter().enumerate() {
            if emitted >= show {
                break 'outer;
            }
            let is_first = emitted == 0;
            if !is_first {
                out.print_gutter(&" ".repeat(ly.prefix_len));
                if seg_idx > 0 {
                    out.mark_soft_wrap_continuation();
                }
            }
            if seg_idx == 0 {
                out.set_source_text(&tool_title_source_text(name, &line_source, is_first));
            }

            for (sp_idx, span) in spans.iter().enumerate() {
                let sp_start = w.offs[sp_idx];
                let sp_end = w.offs[sp_idx + 1];
                let lo = sp_start.max(rs);
                let hi = sp_end.min(re);
                if lo >= hi {
                    continue;
                }
                let start = lo - sp_start;
                let end = hi - sp_start;
                let piece = smelt_buffer::text::slice(&span.text, start..end);

                let fg_color = span.fg.as_deref().and_then(|name| out.theme().get(name).fg);
                let bg_color = span.bg.as_deref().and_then(|name| out.theme().get(name).bg);
                out.save_style();
                if let Some(group) = span.hl.as_deref() {
                    out.set_hl(intern(group));
                }
                if let Some(c) = fg_color {
                    out.set_fg(c);
                }
                if let Some(c) = bg_color {
                    out.set_bg(c);
                }
                if span.dim {
                    out.set_dim();
                }
                if span.bold {
                    out.set_bold();
                }
                if span.italic {
                    out.set_italic();
                }
                match span.syntax.as_deref() {
                    Some(lang) => {
                        let mut h = InlineSyntax::new(lang);
                        h.print_line_range(out, &span.text, start..end);
                    }
                    None if span.selectable => out.print(piece),
                    None => out.print_with_meta(
                        piece,
                        SpanMeta {
                            selectable: false,
                            copy_as: None,
                        },
                    ),
                }
                out.pop_style();
            }

            if is_first && !suffix_spans.is_empty() {
                if has_summary {
                    print_dim_non_selectable(out, "  ");
                }
                for (idx, span) in suffix_spans.iter().enumerate() {
                    if idx > 0 {
                        print_dim_non_selectable(out, " ");
                    }
                    print_nonselectable_styled_span(out, span);
                }
            }
            out.newline();
            rows += 1;
            emitted += 1;
        }
    }

    if total > MAX_TOOL_BLOCK_ROWS {
        let skipped = total - MAX_TOOL_BLOCK_ROWS;
        out.print_gutter(&" ".repeat(ly.prefix_len));
        print_dim(
            out,
            &format!("... {} below", pluralize(skipped, "line", "lines")),
        );
        out.newline();
        rows += 1;
    }

    rows
}

fn split_title_summary(
    summary: &StyledLines,
    timer: &str,
    status: ToolStatus,
) -> (StyledLines, Vec<StyledSpan>) {
    let mut body = summary.clone();
    let mut suffix = Vec::new();
    if !timer.is_empty() {
        suffix.push(StyledSpan {
            text: timer.to_string(),
            dim: true,
            selectable: false,
            ..Default::default()
        });
    }

    let Some(first_line) = body.0.first_mut() else {
        return (body, suffix);
    };
    let mut trailing = Vec::new();
    while first_line.last().is_some_and(|span| span.title_suffix) {
        let mut span = first_line.pop().unwrap();
        span.text = span.text.trim().to_string();
        if status == ToolStatus::Pending && !span.text.is_empty() {
            trailing.push(span);
        }
    }
    trailing.reverse();
    suffix.extend(trailing);
    (body, suffix)
}

fn print_nonselectable_styled_span(out: &mut LineBuilder, span: &StyledSpan) {
    let fg_color = span.fg.as_deref().and_then(|name| out.theme().get(name).fg);
    let bg_color = span.bg.as_deref().and_then(|name| out.theme().get(name).bg);
    out.save_style();
    if let Some(group) = span.hl.as_deref() {
        out.set_hl(intern(group));
    }
    if let Some(c) = fg_color {
        out.set_fg(c);
    }
    if let Some(c) = bg_color {
        out.set_bg(c);
    }
    if span.dim {
        out.set_dim();
    }
    if span.bold {
        out.set_bold();
    }
    if span.italic {
        out.set_italic();
    }
    out.print_with_meta(
        &span.text,
        SpanMeta {
            selectable: false,
            copy_as: None,
        },
    );
    out.pop_style();
}

fn selectable_line_text(spans: &[StyledSpan]) -> String {
    let mut out = String::new();
    for span in spans {
        if span.selectable {
            out.push_str(&span.text);
        }
    }
    out
}

fn tool_title_source_text(name: &str, line_source: &str, include_title: bool) -> String {
    if include_title {
        if line_source.is_empty() {
            format!("* {name}")
        } else {
            format!("* {name} {line_source}")
        }
    } else {
        line_source.to_string()
    }
}

fn replay_rendered(out: &mut LineBuilder, layout: &RenderedLayout, inner_width: u16) -> u16 {
    let cap = MAX_TOOL_BLOCK_ROWS as u16;
    replay_node(out, layout, cap, inner_width, true)
}

/// Render a `RenderedLayout` directly into `out` without a tool-block gutter
/// or row cap. Used by the confirm dialog's preview pipeline, which renders
/// into a fresh dialog-owned buffer instead of stamping into a transcript row.
pub fn render_layout_into(out: &mut LineBuilder, layout: &RenderedLayout, width: u16) -> u16 {
    replay_node(out, layout, u16::MAX, width, false)
}

fn replay_node(
    out: &mut LineBuilder,
    layout: &RenderedLayout,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    if rows_cap == 0 {
        return 0;
    }
    match layout {
        RenderedLayout::Leaf(leaf) => render_leaf(out, leaf, rows_cap, width, with_gutter),
        RenderedLayout::Vbox(items) => {
            let mut written = 0u16;
            for child in items {
                let remaining = rows_cap.saturating_sub(written);
                if remaining == 0 {
                    break;
                }
                written =
                    written.saturating_add(replay_node(out, child, remaining, width, with_gutter));
            }
            written
        }
        RenderedLayout::Hbox(items) => replay_hbox(out, items, rows_cap, width, with_gutter),
    }
}

fn render_leaf(
    out: &mut LineBuilder,
    leaf: &RenderedLeaf,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    let source_view_target = SourceViewTarget::new(
        if with_gutter {
            BLOCK_GUTTER_W as u16
        } else {
            0
        },
        u16::MAX,
    );
    match leaf {
        RenderedLeaf::Buf(buf) => replay_leaf(out, buf, rows_cap, width, with_gutter),
        RenderedLeaf::Diff(spec) => {
            render_source_view(out, SourceView::Diff(spec), source_view_target)
        }
        RenderedLeaf::DiffCache(cache) => {
            render_source_view(out, SourceView::DiffCache(cache), source_view_target)
        }
        RenderedLeaf::FileView(spec) => {
            render_source_view(out, SourceView::FileView(spec), source_view_target)
        }
    }
}

fn replay_leaf(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    let n = buf.line_count();
    if n == 0 || rows_cap == 0 {
        return 0;
    }
    if is_unit_leaf(buf) && width > 0 {
        let glyph = buf.get_line(0).unwrap_or("");
        if with_gutter {
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        out.print(&glyph.repeat(width as usize));
        out.newline();
        return 1;
    }
    let limit = (n as u16).min(rows_cap);
    for i in 0..limit {
        if with_gutter {
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        replay_buffer_row_into(buf, i, out);
        out.newline();
    }
    limit
}

fn replay_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::RenderedHboxItem],
    rows_cap: u16,
    total_width: u16,
    with_gutter: bool,
) -> u16 {
    use smelt_core::content::block_layout::solve_hbox_widths;
    let widths = solve_hbox_widths(items, total_width);

    let mut columns: Vec<Vec<&smelt_core::buffer::Buffer>> = Vec::with_capacity(items.len());
    let mut col_height: u16 = 0;
    let mut any_unit_only = true;
    for item in items {
        // Hbox columns can only contain buffer leaves; spec leaves (diff/file_view)
        // would need width-dependent height computation that hbox layout can't do
        // ahead of time. The built-in tools that use specs (edit_file, write_file)
        // wrap them in a top-level `Leaf`, not an `Hbox`, so this restriction is
        // invisible to them.
        let bufs: Vec<&smelt_core::buffer::Buffer> = item
            .layout
            .leaves()
            .into_iter()
            .filter_map(|l| match l {
                RenderedLeaf::Buf(b) => Some(&**b),
                _ => None,
            })
            .collect();
        let height: u16 = bufs
            .iter()
            .map(|b| {
                if is_unit_leaf(b) {
                    0
                } else {
                    b.line_count() as u16
                }
            })
            .sum();
        if height > 0 {
            any_unit_only = false;
        }
        if height > col_height {
            col_height = height;
        }
        columns.push(bufs);
    }
    if any_unit_only {
        col_height = 1; // pure separator row
    }
    let row_total = col_height.min(rows_cap);
    if row_total == 0 {
        return 0;
    }

    for r in 0..row_total {
        if with_gutter {
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        for (col_idx, bufs) in columns.iter().enumerate() {
            let col_w = widths.get(col_idx).copied().unwrap_or(0);
            if col_w == 0 {
                continue;
            }
            let emitted = emit_column_row(out, bufs, r, col_w);
            if emitted < col_w {
                out.print(&" ".repeat((col_w - emitted) as usize));
            }
        }
        out.newline();
    }
    row_total
}

fn emit_column_row(
    out: &mut LineBuilder,
    bufs: &[&smelt_core::buffer::Buffer],
    r: u16,
    col_w: u16,
) -> u16 {
    // Unit leaves (1×1) repeat horizontally to fill the column.
    for buf in bufs {
        if is_unit_leaf(buf) {
            let glyph = buf.get_line(0).unwrap_or("");
            let repeat = col_w as usize;
            let s = glyph.repeat(repeat);
            out.print(&s);
            return col_w;
        }
    }
    let mut consumed: u16 = 0;
    for buf in bufs {
        let h = buf.line_count() as u16;
        if r < consumed + h {
            return emit_buffer_row_clipped(buf, r - consumed, col_w, out);
        }
        consumed = consumed.saturating_add(h);
    }
    0
}

fn is_unit_leaf(buf: &smelt_core::buffer::Buffer) -> bool {
    if buf.line_count() != 1 {
        return false;
    }
    let line = buf.get_line(0).unwrap_or("");
    smelt_core::content::builder::display_width(line) == 1
}

fn emit_buffer_row_clipped(
    buf: &smelt_core::buffer::Buffer,
    row: u16,
    max_cols: u16,
    out: &mut LineBuilder,
) -> u16 {
    use unicode_width::UnicodeWidthChar;

    let text = buf.get_line(row as usize).unwrap_or("");
    let mut highlights = buf.highlights_at(row as usize);
    highlights.sort_by_key(|h| h.col_start);

    let chars: Vec<char> = text.chars().collect();
    let mut emitted_cols: u16 = 0;
    let mut col_idx: u16 = 0;

    let theme_clone = out.theme().clone();

    for h in &highlights {
        if h.col_end <= col_idx {
            continue;
        }
        if h.col_start > col_idx {
            let plain: String = chars[col_idx as usize..h.col_start as usize]
                .iter()
                .collect();
            let used = emit_clipped(
                out,
                &plain,
                None,
                SpanMeta::default(),
                max_cols,
                emitted_cols,
            );
            emitted_cols = emitted_cols.saturating_add(used);
            col_idx = h.col_start;
            if emitted_cols >= max_cols {
                return emitted_cols;
            }
        }
        let end = h.col_end.min(chars.len() as u16);
        if end <= col_idx {
            continue;
        }
        let segment: String = chars[col_idx as usize..end as usize].iter().collect();
        let style = theme_clone.resolve(h.hl);
        let used = emit_clipped(
            out,
            &segment,
            Some(style),
            h.meta.clone(),
            max_cols,
            emitted_cols,
        );
        emitted_cols = emitted_cols.saturating_add(used);
        col_idx = end;
        if emitted_cols >= max_cols {
            return emitted_cols;
        }
    }
    if (col_idx as usize) < chars.len() && emitted_cols < max_cols {
        let tail: String = chars[col_idx as usize..].iter().collect();
        let used = emit_clipped(
            out,
            &tail,
            None,
            SpanMeta::default(),
            max_cols,
            emitted_cols,
        );
        emitted_cols = emitted_cols.saturating_add(used);
    }
    let _ = UnicodeWidthChar::width(' '); // satisfy import even if loop empty
    emitted_cols
}

fn emit_clipped(
    out: &mut LineBuilder,
    segment: &str,
    style: Option<smelt_core::style::Style>,
    meta: SpanMeta,
    max_cols: u16,
    already: u16,
) -> u16 {
    use unicode_width::UnicodeWidthChar;
    let budget = max_cols.saturating_sub(already);
    if budget == 0 {
        return 0;
    }
    let mut acc = String::new();
    let mut acc_w: u16 = 0;
    for ch in segment.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if acc_w.saturating_add(cw) > budget {
            break;
        }
        acc.push(ch);
        acc_w = acc_w.saturating_add(cw);
    }
    if acc.is_empty() {
        return 0;
    }
    if let Some(s) = style {
        out.append_resolved_span(&acc, s, meta);
    } else {
        out.print(&acc);
    }
    acc_w
}

pub(super) fn print_tool_output(
    out: &mut LineBuilder,
    _name: &str,
    output: &ToolOutput,
    _args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    render_wrapped_output(out, &output.content, output.is_error, width)
}

pub(super) fn print_dim(out: &mut LineBuilder, text: &str) {
    out.push_dim();
    out.print(text);
    out.pop_style();
}

fn print_dim_non_selectable(out: &mut LineBuilder, time_str: &str) {
    let meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    if !time_str.is_empty() {
        out.push_dim();
        out.print_with_meta(time_str, meta);
        out.pop_style();
    }
}

pub fn render_wrapped_output(
    out: &mut LineBuilder,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:wrapped_output");
    let max_cols = super::metrics::block_inner_width(width);

    let mut all_lines = Vec::new();
    let mut total_rows = 0usize;

    for line in content.lines() {
        let expanded = line.replace('\t', "    ");
        let (spans, ranges, boundaries) = wrap_ansi(&expanded, max_cols);
        if ranges.len() > 1 {
            out.mark_wrapped();
        }
        total_rows += ranges.len();
        all_lines.push((spans, ranges, boundaries));
    }

    let mut rows = 0u16;
    if total_rows > MAX_TOOL_BLOCK_ROWS {
        let skipped = total_rows - MAX_TOOL_BLOCK_ROWS;
        print_dim(
            out,
            &format!(
                "{BLOCK_GUTTER_SPACE}... {} above",
                pluralize(skipped, "line", "lines")
            ),
        );
        out.newline();
        rows += 1;
    }

    let mut skip = total_rows.saturating_sub(MAX_TOOL_BLOCK_ROWS);
    for (spans, ranges, boundaries) in &all_lines {
        let count = ranges.len();
        if skip >= count {
            skip -= count;
            continue;
        }
        let start = skip;
        skip = 0;
        for &(ws, we) in &ranges[start..] {
            if is_error {
                out.push_hl(intern("ErrorMsg"));
            } else {
                out.push_dim();
            }
            out.print(BLOCK_GUTTER_SPACE);
            emit_ansi_row(out, spans, boundaries, ws, we);
            out.pop_style();
            out.newline();
            rows += 1;
        }
    }
    rows
}

pub(super) fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}
