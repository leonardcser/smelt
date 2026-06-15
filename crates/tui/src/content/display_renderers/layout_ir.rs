use super::metrics::pluralize;
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{
    solve_hbox_widths, BlockLayout, IrLeaf, LayoutIr, RunsSpec, TextSpec,
};
use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::InlineSyntax;
use smelt_core::content::inline_line::{BreakPolicy, InlineLine, InlineRun, WrappedRun};
use smelt_core::theme::intern;

pub(crate) fn render_layout_ir_into(out: &mut LineBuilder, layout: &LayoutIr, width: u16) -> u16 {
    render_layout_ir_range(out, layout, width, 0, u16::MAX, None)
}

pub(crate) fn measure_layout_ir(layout: &LayoutIr, width: u16) -> u16 {
    measure_layout_ir_full(layout, width)
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
        IrLeaf::Runs(spec) => render_runs_spec(out, spec, width, row_start, row_count, gutter),
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
        IrLeaf::Runs(spec) => measure_runs_spec(spec, width),
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

fn wrap_styled_runs(spans: &[protocol::StyledSpan], width: u16) -> Vec<Vec<WrappedRun<usize>>> {
    if spans.is_empty() {
        return vec![Vec::new()];
    }
    let line = InlineLine::new(
        spans
            .iter()
            .enumerate()
            .map(|(idx, span)| InlineRun::new(span.text.clone(), idx, BreakPolicy::BreakOnSpaces))
            .collect(),
    );
    line.wrap_fragments(width.max(1) as usize)
}

fn render_runs_spec(
    out: &mut LineBuilder,
    spec: &RunsSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&str>,
) -> u16 {
    let default_hl = spec.hl_group.as_deref();
    let mut seen = 0u16;
    let mut rows = 0u16;
    'outer: for spans in &spec.lines.0 {
        let wrapped = wrap_styled_runs(spans, width);
        if wrapped.len() > 1 {
            out.mark_wrapped();
        }
        for (seg_idx, row_fragments) in wrapped.iter().enumerate() {
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
            if seg_idx > 0 {
                out.mark_soft_wrap_continuation();
            }
            for fragment in row_fragments {
                let Some(span) = spans.get(fragment.run_index) else {
                    continue;
                };
                print_styled_span_range(out, span, default_hl, fragment.range.clone());
            }
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_runs_spec(spec: &RunsSpec, width: u16) -> u16 {
    spec.lines
        .0
        .iter()
        .map(|spans| wrap_styled_runs(spans, width).len() as u16)
        .sum()
}

fn print_styled_span_range(
    out: &mut LineBuilder,
    span: &protocol::StyledSpan,
    default_hl: Option<&str>,
    range: std::ops::Range<usize>,
) {
    let piece = smelt_buffer::text::slice(&span.text, range.clone());
    if piece.is_empty() {
        return;
    }
    let fg_color = span.fg.as_deref().and_then(|name| out.theme().get(name).fg);
    let bg_color = span.bg.as_deref().and_then(|name| out.theme().get(name).bg);
    out.save_style();
    if let Some(group) = span.hl.as_deref().or(default_hl) {
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
        Some(lang) if span.selectable => {
            let mut h = InlineSyntax::new(lang);
            h.print_line_range(out, &span.text, range);
        }
        _ if span.selectable => out.print(piece),
        _ => out.print_with_meta(
            piece,
            SpanMeta {
                selectable: false,
                copy_as: None,
            },
        ),
    }
    out.pop_style();
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
