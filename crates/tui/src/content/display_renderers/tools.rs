use super::metrics::{block_inner_width, BLOCK_GUTTER_SPACE};
use super::MAX_TOOL_BLOCK_ROWS;
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use protocol::{StyledLines, StyledSpan};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{
    solve_hbox_widths, BlockLayout, IrLeaf, LayoutIr, TextSpec, ToolBody,
};
use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::InlineSyntax;
use smelt_core::content::inline_line::{BreakPolicy, InlineLine, InlineRun, WrappedRun};
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
    body: Option<&ToolBody>,
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
        if let Some(body) = body {
            let inner_width = block_inner_width(width) as u16;
            rows += render_tool_body(out, body, inner_width);
        } else if let Some(out_data) = output {
            if !out_data.content.trim().is_empty() {
                rows += print_tool_output(out, name, out_data, args, width);
            }
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn measure_tool_height(
    name: &str,
    summary: &StyledLines,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    body: Option<&ToolBody>,
    width: usize,
) -> u16 {
    let time = if status == ToolStatus::Confirm {
        None
    } else {
        elapsed
    };
    let mut rows = measure_tool_line(name, summary, status, time, width);
    if user_message.is_some() {
        rows = rows.saturating_add(1);
    }
    if status != ToolStatus::Denied {
        if let Some(body) = body {
            rows = rows.saturating_add(measure_tool_body(body, block_inner_width(width) as u16));
        } else if let Some(out_data) = output {
            if !out_data.content.trim().is_empty() {
                rows = rows.saturating_add(measure_tool_output(&out_data.content, width));
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
        rows: Vec<Vec<WrappedRun<()>>>,
    }

    let mut wlines: Vec<Wrapped> = summary
        .0
        .iter()
        .map(|spans| {
            let line = InlineLine::new(
                spans
                    .iter()
                    .map(|s| InlineRun::new(s.text.clone(), (), BreakPolicy::BreakOnSpaces))
                    .collect(),
            );
            Wrapped {
                rows: line.wrap_fragments(max_seg),
            }
        })
        .collect();
    if wlines.is_empty() {
        wlines.push(Wrapped {
            rows: vec![Vec::new()],
        });
    }

    if wlines.iter().any(|w| w.rows.len() > 1) {
        out.mark_wrapped();
    }

    let total: usize = wlines.iter().map(|w| w.rows.len()).sum();
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
        for (seg_idx, row_fragments) in w.rows.iter().enumerate() {
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

            for fragment in row_fragments {
                let Some(span) = spans.get(fragment.run_index) else {
                    continue;
                };
                let start = fragment.range.start;
                let end = fragment.range.end;
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

fn measure_tool_line(
    name: &str,
    summary: &StyledLines,
    status: ToolStatus,
    elapsed: Option<Duration>,
    width: usize,
) -> u16 {
    let timer = tool_title_suffix(elapsed);
    let (summary, suffix_spans) = split_title_summary(summary, &timer, status);
    let has_summary = !summary.as_plain_text().is_empty();
    let suffix_text_len: usize = suffix_spans.iter().map(|s| s.text.len()).sum();
    let suffix_len = suffix_text_len
        + suffix_spans.len().saturating_sub(1)
        + 2 * usize::from(has_summary && !suffix_spans.is_empty());
    let has_title_tail = has_summary || !suffix_spans.is_empty();
    let ly = tool_line_layout(name, suffix_len, has_title_tail, width);
    let max_seg = ly.max_summary.max(1);
    let total: usize = summary
        .0
        .iter()
        .map(|spans| {
            let line = InlineLine::new(
                spans
                    .iter()
                    .map(|s| InlineRun::new(s.text.clone(), (), BreakPolicy::BreakOnSpaces))
                    .collect(),
            );
            line.wrap_fragments(max_seg).len()
        })
        .sum::<usize>()
        .max(1);
    let shown = total.min(MAX_TOOL_BLOCK_ROWS);
    (shown + usize::from(total > MAX_TOOL_BLOCK_ROWS)) as u16
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

fn render_tool_body(out: &mut LineBuilder, body: &ToolBody, inner_width: u16) -> u16 {
    match body {
        ToolBody::Layout(layout) => render_layout_ir_node(
            out,
            layout,
            MAX_TOOL_BLOCK_ROWS as u16,
            inner_width,
            Some(BLOCK_GUTTER_SPACE),
        ),
    }
}

pub(crate) fn measure_tool_body(body: &ToolBody, inner_width: u16) -> u16 {
    match body {
        ToolBody::Layout(layout) => {
            measure_layout_ir_node(layout, MAX_TOOL_BLOCK_ROWS as u16, inner_width)
        }
    }
}

fn render_layout_ir_node(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    rows_cap: u16,
    width: u16,
    gutter: Option<&str>,
) -> u16 {
    render_layout_ir_range(out, layout, width, 0, rows_cap, gutter)
}

fn render_layout_ir_range(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => render_ir_leaf(out, leaf, width, row_start, row_count, gutter),
        BlockLayout::Vbox(items) => render_ir_vbox(out, items, width, row_start, row_count, gutter),
        BlockLayout::Hbox(items) => render_ir_hbox(out, items, width, row_start, row_count, gutter),
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            render_layout_ir_range(
                out,
                child,
                child_width,
                row_start,
                row_count,
                Some(&spec.text),
            )
        }
        BlockLayout::Cap { child, spec } => {
            render_ir_cap(out, child, spec, width, row_start, row_count, gutter)
        }
    }
}

fn render_ir_vbox(
    out: &mut LineBuilder,
    items: &[LayoutIr],
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    let mut written = 0u16;
    let mut skipped = row_start;
    for child in items {
        if written >= row_count {
            break;
        }
        let child_rows = measure_layout_ir_full_with_gutter(child, width, gutter_width(gutter));
        if skipped >= child_rows {
            skipped = skipped.saturating_sub(child_rows);
            continue;
        }
        let child_count = row_count.saturating_sub(written).min(child_rows - skipped);
        written = written.saturating_add(render_layout_ir_range(
            out,
            child,
            width,
            skipped,
            child_count,
            gutter,
        ));
        skipped = 0;
    }
    written
}

fn measure_layout_ir_node(layout: &LayoutIr, rows_cap: u16, width: u16) -> u16 {
    measure_layout_ir_full(layout, width).min(rows_cap)
}

fn measure_layout_ir_full(layout: &LayoutIr, width: u16) -> u16 {
    measure_layout_ir_full_with_gutter(layout, width, 0)
}

fn measure_layout_ir_full_with_gutter(layout: &LayoutIr, width: u16, gutter_cells: u16) -> u16 {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => measure_ir_leaf(leaf, width, gutter_cells),
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|child| measure_layout_ir_full_with_gutter(child, width, gutter_cells))
            .sum(),
        BlockLayout::Hbox(items) => {
            let widths = solve_hbox_widths(items, width);
            items
                .iter()
                .zip(widths)
                .map(|(item, w)| measure_layout_ir_full(&item.layout, w))
                .max()
                .unwrap_or(0)
        }
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            measure_layout_ir_full_with_gutter(child, child_width, spec_width)
        }
        BlockLayout::Cap { child, spec } => {
            let child_rows = measure_layout_ir_full_with_gutter(child, width, gutter_cells);
            let kept = child_rows.min(spec.rows);
            let marker = u16::from(spec.marker.is_some() && child_rows > spec.rows);
            kept.saturating_add(marker)
        }
    }
}

fn render_ir_leaf(
    out: &mut LineBuilder,
    leaf: &IrLeaf,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => render_text_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::SourceView(smelt_core::content::block_layout::SourceViewIr::Diff(cache)) => {
            let indent = gutter_width(gutter);
            let target =
                SourceViewTarget::new(indent, width.saturating_add(indent), row_start, row_count);
            render_source_view(out, SourceView::DiffIr(cache), target)
        }
    }
}

fn measure_ir_leaf(leaf: &IrLeaf, width: u16, gutter_cells: u16) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => measure_text_spec(spec, width),
        IrLeaf::SourceView(smelt_core::content::block_layout::SourceViewIr::Diff(cache)) => {
            smelt_core::content::highlight::measure_diff_ir(
                cache,
                width.saturating_add(gutter_cells),
                smelt_core::content::highlight::GutterStyle::InlineLineNumbers,
                gutter_cells,
            )
        }
    }
}

fn render_text_spec(
    out: &mut LineBuilder,
    spec: &TextSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    let hl = spec.hl_group.as_deref().map(intern);
    let mut seen = 0u16;
    let mut rows = 0u16;
    'outer: for line in spec.content.lines() {
        let expanded = line.replace('\t', "    ");
        let ansi_wrapped = spec.ansi.then(|| wrap_ansi(&expanded, width as usize));
        let plain_ranges;
        let ranges: &[(usize, usize)] = if let Some((_, ranges, _)) = &ansi_wrapped {
            ranges
        } else {
            let line = InlineLine::new(vec![InlineRun::new(
                expanded.clone(),
                (),
                BreakPolicy::BreakOnSpaces,
            )]);
            plain_ranges = line.wrap_plain_ranges(width as usize);
            &plain_ranges
        };
        if ranges.len() > 1 {
            out.mark_wrapped();
        }
        for &(ws, we) in ranges {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if rows >= row_count {
                break 'outer;
            }
            if let Some(gutter) = gutter {
                out.print_gutter(gutter);
            }
            match hl {
                Some(group) => out.push_hl(group),
                None => out.push_dim(),
            }
            if let Some((spans, _, boundaries)) = &ansi_wrapped {
                emit_ansi_row(out, spans, boundaries, ws, we);
            } else {
                out.print(smelt_buffer::text::slice(&expanded, ws..we));
            }
            out.pop_style();
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_text_spec(spec: &TextSpec, width: u16) -> u16 {
    spec.content
        .lines()
        .map(|line| {
            let expanded = line.replace('\t', "    ");
            if spec.ansi {
                let (_, ranges, _) = wrap_ansi(&expanded, width as usize);
                ranges.len() as u16
            } else {
                let line = InlineLine::new(vec![InlineRun::new(
                    expanded,
                    (),
                    BreakPolicy::BreakOnSpaces,
                )]);
                line.wrap_plain_ranges(width as usize).len() as u16
            }
        })
        .sum()
}

fn render_ir_cap(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    let child_rows = measure_layout_ir_full_with_gutter(child, width, gutter_width(gutter));
    let kept = child_rows.min(spec.rows);
    let truncated = child_rows > spec.rows;
    let marker = spec.marker.filter(|_| truncated);
    let total = kept.saturating_add(u16::from(marker.is_some()));
    let end = row_start.saturating_add(row_count).min(total);
    let mut written = 0u16;
    for row in row_start..end {
        let marker_above = marker == Some(CapMarker::Above);
        let marker_below = marker == Some(CapMarker::Below);
        if marker_above && row == 0 {
            render_cap_marker(out, child_rows.saturating_sub(kept), "above", gutter);
            written = written.saturating_add(1);
            continue;
        }
        if marker_below && row == kept {
            render_cap_marker(out, child_rows.saturating_sub(kept), "below", gutter);
            written = written.saturating_add(1);
            continue;
        }
        let kept_row = row.saturating_sub(u16::from(marker_above));
        if kept_row >= kept {
            continue;
        }
        let child_start = match spec.keep {
            CapKeep::Head => kept_row,
            CapKeep::Tail => child_rows.saturating_sub(kept).saturating_add(kept_row),
        };
        written = written.saturating_add(render_layout_ir_range(
            out,
            child,
            width,
            child_start,
            1,
            gutter,
        ));
    }
    written
}

fn render_cap_marker(out: &mut LineBuilder, skipped: u16, direction: &str, gutter: Option<&str>) {
    if let Some(gutter) = gutter {
        out.print_gutter(gutter);
    }
    out.push_dim();
    out.print(&format!(
        "... {} {direction}",
        pluralize(skipped as usize, "line", "lines")
    ));
    out.pop_style();
    out.newline();
}

fn render_ir_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::HboxItem<IrLeaf>],
    total_width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    let widths = solve_hbox_widths(items, total_width);
    let total_rows = items
        .iter()
        .zip(widths.iter().copied())
        .map(|(item, w)| measure_layout_ir_full(&item.layout, w))
        .max()
        .unwrap_or(0);
    let end = row_start.saturating_add(row_count).min(total_rows);
    let mut written = 0u16;
    let theme = out.theme().clone();
    for row in row_start..end {
        if let Some(gutter) = gutter {
            out.print_gutter(gutter);
        }
        for (idx, item) in items.iter().enumerate() {
            let col_w = widths.get(idx).copied().unwrap_or(0);
            if col_w == 0 {
                continue;
            }
            let mut buf = smelt_core::buffer::Buffer::new(
                smelt_core::buffer::BufId(idx as u64 + 1),
                Default::default(),
            );
            {
                let mut col = LineBuilder::new(&mut buf, &theme, col_w);
                render_layout_ir_range(&mut col, &item.layout, col_w, row, 1, None);
                col.finish();
            }
            let emitted = emit_buffer_row_clipped(&buf, 0, col_w, out);
            if emitted < col_w {
                out.print(&" ".repeat((col_w - emitted) as usize));
            }
        }
        out.newline();
        written = written.saturating_add(1);
    }
    written
}

fn gutter_width(gutter: Option<&str>) -> u16 {
    gutter.map(display_width_u16).unwrap_or(0)
}

fn display_width_u16(text: &str) -> u16 {
    unicode_width::UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}

fn child_width_after_gutter(width: u16, gutter_width: u16) -> u16 {
    width.saturating_sub(gutter_width).max(1)
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

pub(super) fn measure_wrapped_output(content: &str, width: usize) -> u16 {
    let max_cols = super::metrics::block_inner_width(width);
    let total_rows: usize = content
        .lines()
        .map(|line| {
            let expanded = line.replace('\t', "    ");
            let (_, ranges, _) = wrap_ansi(&expanded, max_cols);
            ranges.len()
        })
        .sum();
    (total_rows.min(MAX_TOOL_BLOCK_ROWS) + usize::from(total_rows > MAX_TOOL_BLOCK_ROWS)) as u16
}

fn measure_tool_output(content: &str, width: usize) -> u16 {
    measure_wrapped_output(content, width)
}

pub fn render_tool_body_into(out: &mut LineBuilder, body: &ToolBody, width: u16) -> u16 {
    match body {
        ToolBody::Layout(layout) => render_layout_ir_node(out, layout, u16::MAX, width, None),
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
