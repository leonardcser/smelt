use super::markdown::{measure_markdown_inner_with_options, render_markdown_inner_with_options};
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{
    solve_hbox_widths_with_fit, BlockLayout, CodeSpec, ElapsedSpec, GutterSpec, IrLeaf, LayoutIr,
    LineSpec, MarkdownSpec, PanelSpec, RunsSpec, SeparatorSpec, StyleSpec, TextSpec,
};
use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, parse_inline_spans_with_options, render_code_block, wrap_inline_spans,
    InlineOptions, InlineSpan, InlineSyntax,
};
use smelt_core::content::inline_line::{BreakPolicy, InlineLine, InlineRun, WrappedRun};
use smelt_core::theme::intern;
use smelt_core::transcript_model::{BlockHistory, ToolStatus};

pub(crate) fn render_layout_ir_into(out: &mut LineBuilder, layout: &LayoutIr, width: u16) -> u16 {
    render_layout_ir_range(
        out,
        layout,
        width,
        0,
        u16::MAX,
        None,
        None,
        &InlineOptions::default(),
    )
}

pub(crate) fn render_layout_ir_into_with_history(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    history: &BlockHistory,
    inline_options: &InlineOptions,
) -> u16 {
    render_layout_ir_range(
        out,
        layout,
        width,
        0,
        u16::MAX,
        None,
        Some(history),
        inline_options,
    )
}

#[cfg(test)]
pub(crate) fn measure_layout_ir(layout: &LayoutIr, width: u16) -> u16 {
    measure_layout_ir_with_options(layout, width, &InlineOptions::default())
}

pub(crate) fn measure_layout_ir_with_options(
    layout: &LayoutIr,
    width: u16,
    inline_options: &InlineOptions,
) -> u16 {
    measure_layout_ir_full(layout, width, inline_options)
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[allow(clippy::too_many_arguments)]
fn render_layout_ir_range(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => render_ir_leaf(
            out,
            leaf,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Vbox(items) => render_ir_vbox(
            out,
            items,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Hbox(items) => render_ir_hbox(
            out,
            items,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            render_layout_ir_range(
                out,
                child,
                child_width,
                row_start,
                row_count,
                Some(spec),
                history,
                inline_options,
            )
        }
        BlockLayout::Panel { child, spec } => render_ir_panel(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Style { child, spec } => render_ir_style(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
        BlockLayout::Cap { child, spec } => render_ir_cap(
            out,
            child,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            history,
            inline_options,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_vbox(
    out: &mut LineBuilder,
    items: &[LayoutIr],
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let mut written = 0u16;
    let mut skipped = row_start;
    for child in items {
        if written >= row_count {
            break;
        }
        let child_rows =
            measure_layout_ir_full_with_gutter(child, width, gutter_width(gutter), inline_options);
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
            history,
            inline_options,
        ));
        skipped = 0;
    }
    written
}

fn measure_layout_ir_full(layout: &LayoutIr, width: u16, inline_options: &InlineOptions) -> u16 {
    measure_layout_ir_full_with_gutter(layout, width, 0, inline_options)
}

fn measure_layout_ir_full_with_gutter(
    layout: &LayoutIr,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> u16 {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => measure_ir_leaf(leaf, width, gutter_cells, inline_options),
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|child| {
                measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
            })
            .sum(),
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            items
                .iter()
                .zip(widths)
                .map(|(item, w)| measure_layout_ir_full(&item.layout, w, inline_options))
                .max()
                .unwrap_or(0)
        }
        BlockLayout::Gutter { child, spec } => {
            let spec_width = display_width_u16(&spec.text);
            let child_width = child_width_after_gutter(width, spec_width);
            measure_layout_ir_full_with_gutter(child, child_width, spec_width, inline_options)
        }
        BlockLayout::Panel { child, spec } => measure_ir_panel(child, spec, width, inline_options),
        BlockLayout::Style { child, .. } => {
            measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
        }
        BlockLayout::Cap { child, spec } => {
            let child_rows =
                measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options);
            cap_rows(child_rows, spec).len().min(u16::MAX as usize) as u16
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_leaf(
    out: &mut LineBuilder,
    leaf: &IrLeaf,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => render_text_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Runs(spec) => render_runs_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Line(spec) => render_line_spec(out, spec, row_start, row_count, gutter),
        IrLeaf::Markdown(spec) => render_markdown_spec(
            out,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        ),
        IrLeaf::Code(spec) => render_code_spec(out, spec, width, row_start, row_count, gutter),
        IrLeaf::Elapsed(spec) => {
            render_elapsed_spec(out, spec, row_start, row_count, gutter, history)
        }
        IrLeaf::Separator(spec) => {
            render_separator_spec(out, spec, width, row_start, row_count, gutter)
        }
        IrLeaf::SourceView(smelt_core::content::block_layout::SourceViewIr::Diff(cache)) => {
            let indent = gutter_width(gutter);
            let target =
                SourceViewTarget::new(indent, width.saturating_add(indent), row_start, row_count);
            render_source_view(out, SourceView::DiffIr(cache), target)
        }
    }
}

fn measure_ir_leaf(
    leaf: &IrLeaf,
    width: u16,
    gutter_cells: u16,
    inline_options: &InlineOptions,
) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => measure_text_spec(spec, width),
        IrLeaf::Runs(spec) => measure_runs_spec(spec, width),
        IrLeaf::Line(spec) => measure_line_spec(spec),
        IrLeaf::Markdown(spec) => measure_markdown_spec(spec, width, inline_options),
        IrLeaf::Code(spec) => measure_code_spec(spec, width),
        IrLeaf::Elapsed(_) => 1,
        IrLeaf::Separator(spec) => measure_separator_spec(spec),
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
    gutter: Option<&GutterSpec>,
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
            if let Some(gutter) = gutter.filter(|g| !g.styled) {
                out.print_gutter(&gutter.text);
            }
            match hl {
                Some(group) => out.push_hl(group),
                None => out.push_dim(),
            }
            if let Some(gutter) = gutter.filter(|g| g.styled) {
                out.print_gutter(&gutter.text);
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

fn wrap_styled_runs(
    spans: &[protocol::StyledSpan],
    width: u16,
    continuation_indent: u16,
) -> Vec<Vec<WrappedRun<usize>>> {
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
    let width = width.max(1) as usize;
    let indent = continuation_indent as usize;
    let continuation_width = width
        .saturating_sub(indent.min(width.saturating_sub(1)))
        .max(1);
    line.wrap_fragments_with_widths(width, continuation_width)
}

fn runs_continuation_indent(spec: &RunsSpec, width: u16) -> u16 {
    spec.continuation_indent.min(width.saturating_sub(1))
}

fn render_runs_spec(
    out: &mut LineBuilder,
    spec: &RunsSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let default_hl = spec.hl_group.as_deref();
    let continuation_indent = runs_continuation_indent(spec, width);
    let mut seen = 0u16;
    let mut rows = 0u16;
    'outer: for spans in &spec.lines.0 {
        let wrapped = wrap_styled_runs(spans, width, continuation_indent);
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
                out.print_gutter(&gutter.text);
            }
            if seg_idx > 0 {
                out.mark_soft_wrap_continuation();
                if continuation_indent > 0 {
                    out.print_with_meta(
                        &" ".repeat(continuation_indent as usize),
                        SpanMeta::unselectable(),
                    );
                }
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
    let continuation_indent = runs_continuation_indent(spec, width);
    spec.lines
        .0
        .iter()
        .map(|spans| wrap_styled_runs(spans, width, continuation_indent).len() as u16)
        .sum()
}

fn render_line_spec(
    out: &mut LineBuilder,
    spec: &LineSpec,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_start > 0 || row_count == 0 {
        return 0;
    }
    if let Some(gutter) = gutter {
        out.print_gutter(&gutter.text);
    }
    let default_hl = spec.hl_group.as_deref();
    print_styled_spans(out, &spec.spans, default_hl);
    out.newline();
    1
}

fn measure_line_spec(_spec: &LineSpec) -> u16 {
    1
}

fn render_markdown_spec(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    if spec.inline {
        return render_inline_markdown_spec(
            out,
            spec,
            width,
            row_start,
            row_count,
            gutter,
            inline_options,
        );
    }

    let total = measure_markdown_spec(spec, width, inline_options);
    out.save_style();
    if spec.italic {
        out.set_italic();
    }
    let rows = if row_start == 0 && row_count >= total && gutter.is_none() {
        render_markdown_inner_with_options(
            out,
            &spec.content,
            width as usize,
            "",
            spec.dim,
            None,
            inline_options,
        )
    } else {
        render_via_temp(out, width, row_start, row_count, gutter, |col| {
            render_markdown_inner_with_options(
                col,
                &spec.content,
                width as usize,
                "",
                spec.dim,
                None,
                inline_options,
            )
        })
    };
    out.pop_style();
    rows
}

fn measure_markdown_spec(spec: &MarkdownSpec, width: u16, inline_options: &InlineOptions) -> u16 {
    if spec.inline {
        return measure_inline_markdown_spec(spec, width, inline_options);
    }
    measure_markdown_inner_with_options(
        &spec.content,
        width as usize,
        "",
        spec.dim,
        None,
        inline_options,
    )
}

fn render_inline_markdown_spec(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    inline_options: &InlineOptions,
) -> u16 {
    let max_cols = width.max(1) as usize;
    let mut seen = 0u16;
    let mut rows = 0u16;
    'outer: for line in spec.content.lines() {
        let spans = inline_markdown_spans(line, spec.dim, spec.italic, inline_options);
        let wrapped = wrap_inline_spans(&spans, max_cols);
        if wrapped.len() > 1 {
            out.mark_wrapped();
        }
        for row_spans in &wrapped {
            if seen < row_start {
                seen = seen.saturating_add(1);
                continue;
            }
            if rows >= row_count {
                break 'outer;
            }
            if let Some(gutter) = gutter {
                if gutter.styled {
                    out.save_style();
                    if spec.dim {
                        out.set_dim();
                    }
                    if spec.italic {
                        out.set_italic();
                    }
                    out.print_gutter(&gutter.text);
                    out.pop_style();
                } else {
                    out.print_gutter(&gutter.text);
                }
            }
            emit_inline_spans(out, row_spans);
            out.newline();
            rows = rows.saturating_add(1);
            seen = seen.saturating_add(1);
        }
    }
    rows
}

fn measure_inline_markdown_spec(
    spec: &MarkdownSpec,
    width: u16,
    inline_options: &InlineOptions,
) -> u16 {
    let max_cols = width.max(1) as usize;
    spec.content
        .lines()
        .map(|line| {
            let spans = inline_markdown_spans(line, spec.dim, spec.italic, inline_options);
            wrap_inline_spans(&spans, max_cols).len() as u16
        })
        .sum()
}

fn inline_markdown_spans(
    line: &str,
    dim: bool,
    italic: bool,
    inline_options: &InlineOptions,
) -> Vec<InlineSpan> {
    let mut spans = parse_inline_spans_with_options(line, dim, inline_options);
    if italic {
        for span in &mut spans {
            span.style.italic = true;
        }
    }
    spans
}

fn render_code_spec(
    out: &mut LineBuilder,
    spec: &CodeSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let block = code_block_from_spec(spec);
    let total = measure_code_block(&block, width as usize) as u16;
    if row_start == 0 && row_count >= total && gutter.is_none() {
        return render_code_block(out, &block, width as usize, false, None, false);
    }

    render_via_temp(out, width, row_start, row_count, gutter, |col| {
        render_code_block(col, &block, width as usize, false, None, false)
    })
}

fn measure_code_spec(spec: &CodeSpec, width: u16) -> u16 {
    measure_code_block(&code_block_from_spec(spec), width as usize) as u16
}

fn code_block_from_spec(spec: &CodeSpec) -> smelt_core::content::code_block::CodeBlock {
    let lines: Vec<&str> = if spec.content.is_empty() {
        vec![""]
    } else {
        spec.content.lines().collect()
    };
    parse_code_block(&lines, &spec.lang)
}

fn render_elapsed_spec(
    out: &mut LineBuilder,
    spec: &ElapsedSpec,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
) -> u16 {
    if row_start > 0 || row_count == 0 {
        return 0;
    }
    if let Some(gutter) = gutter {
        out.print_gutter(&gutter.text);
    }
    let text = elapsed_text_for_spec(spec, history);
    out.save_style();
    if let Some(group) = spec.hl_group.as_deref() {
        out.set_hl(intern(group));
    }
    if spec.dim {
        out.set_dim();
    }
    let meta = SpanMeta {
        selectable: spec.selectable,
        copy_as: None,
        action: None,
    };
    out.print_with_meta(&text, meta);
    out.pop_style();
    out.newline();
    1
}

fn elapsed_text_for_spec(spec: &ElapsedSpec, history: Option<&BlockHistory>) -> String {
    let (status, secs) = history
        .and_then(|history| history.tool_state(&spec.call_id))
        .map(|state| (state.status, state.elapsed.map(|elapsed| elapsed.as_secs())))
        .unwrap_or((spec.status, spec.fallback_secs));
    secs.and_then(|secs| tool_elapsed_text(status, secs))
        .unwrap_or_default()
}

fn tool_elapsed_text(status: ToolStatus, secs: u64) -> Option<String> {
    (!matches!(status, ToolStatus::Confirm) && secs >= 1).then(|| format_elapsed_secs(secs))
}

fn format_elapsed_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn render_separator_spec(
    out: &mut LineBuilder,
    spec: &SeparatorSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    if row_start > 0 || row_count == 0 {
        return 0;
    }
    if let Some(gutter) = gutter {
        out.print_gutter(&gutter.text);
    }
    if spec.dim {
        out.push_dim();
    }
    let label_width = styled_spans_width(&spec.label);
    let remaining = (width as usize).saturating_sub(label_width);
    let left = remaining / 2;
    let right = remaining - left;
    let fill_meta = SpanMeta {
        selectable: spec.selectable,
        copy_as: None,
        action: None,
    };
    out.print_with_meta(&"─".repeat(left), fill_meta.clone());
    print_styled_spans(out, &spec.label, None);
    out.print_with_meta(&"─".repeat(right), fill_meta);
    if spec.dim {
        out.pop_style();
    }
    out.newline();
    1
}

fn measure_separator_spec(_spec: &SeparatorSpec) -> u16 {
    1
}

fn render_via_temp(
    out: &mut LineBuilder,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    render: impl FnOnce(&mut LineBuilder) -> u16,
) -> u16 {
    let theme = out.theme().clone();
    let inherited_style = out.current_style();
    let mut buf = smelt_core::buffer::Buffer::new(
        smelt_core::buffer::BufId(0),
        smelt_core::buffer::BufCreateOpts::default(),
    );
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, &theme, width.max(1));
        col.push(None, inherited_style);
        render(&mut col);
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let total = outcome.line_count.min(u16::MAX as usize) as u16;
    let end = row_start.saturating_add(row_count).min(total);
    let mut rows = 0u16;
    for row in row_start..end {
        if let Some(gutter) = gutter {
            print_temp_gutter(out, gutter);
        }
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out);
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn print_temp_gutter(out: &mut LineBuilder, gutter: &GutterSpec) {
    if gutter.styled {
        out.print_gutter(&gutter.text);
    } else {
        out.save_style();
        out.reset_style();
        out.print_gutter(&gutter.text);
        out.pop_style();
    }
}

fn apply_temp_decoration(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    row: usize,
    copy_fill_bg: bool,
) {
    let dec = buf.decoration_at(row).clone();
    if let Some(source) = dec.source_text.as_deref() {
        out.set_source_text(source);
    }
    if let Some(source_line) = dec.source_line {
        out.set_source_line(source_line);
    }
    if dec.soft_wrapped {
        out.mark_soft_wrap_continuation();
    } else if dec.copy_continuation {
        out.mark_copy_continuation();
    }
    if dec.cell_selectable {
        out.mark_cell_selectable();
    }
    if dec.block_selectable {
        out.mark_block_selectable();
    }
    if copy_fill_bg {
        if let Some(bg) = dec.fill_bg {
            out.fill_line_bg(bg);
        }
    }
}

fn styled_spans_width(spans: &[protocol::StyledSpan]) -> usize {
    spans.iter().map(|span| display_width(&span.text)).sum()
}

fn print_styled_spans(
    out: &mut LineBuilder,
    spans: &[protocol::StyledSpan],
    default_hl: Option<&str>,
) {
    for span in spans {
        print_styled_span_range(out, span, default_hl, 0..span.text.len());
    }
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
        _ => out.print_with_meta(piece, SpanMeta::unselectable()),
    }
    out.pop_style();
}

#[allow(clippy::too_many_arguments)]
fn render_ir_style(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &StyleSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    out.save_style();
    apply_style_spec(out, spec);
    let rows = render_layout_ir_range(
        out,
        child,
        width,
        row_start,
        row_count,
        gutter,
        history,
        inline_options,
    );
    out.pop_style();
    rows
}

fn apply_style_spec(out: &mut LineBuilder, spec: &StyleSpec) {
    if let Some(group) = spec.hl_group.as_deref() {
        out.set_hl(intern(group));
    }
    if let Some(c) = spec.fg.as_deref().and_then(|name| out.theme().get(name).fg) {
        out.set_fg(c);
    }
    if let Some(c) = spec.bg.as_deref().and_then(|name| out.theme().get(name).bg) {
        out.set_bg(c);
    }
    if spec.dim {
        out.set_dim();
    }
    if spec.bold {
        out.set_bold();
    }
    if spec.italic {
        out.set_italic();
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_panel(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &PanelSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let total = measure_ir_panel(child, spec, width, inline_options);
    let end = row_start.saturating_add(row_count).min(total);
    let child_width = panel_child_width(width, spec.padding);
    let child_rows = measure_layout_ir_full(child, child_width, inline_options);
    let panel_hl = intern(&spec.hl_group);
    let panel_bg = out
        .theme()
        .resolve(panel_hl)
        .bg
        .unwrap_or(smelt_core::style::Color::Reset);
    let pad_text = " ".repeat(spec.padding as usize);
    let pad_meta = SpanMeta::unselectable();
    let theme = out.theme().clone();
    let mut rows = 0u16;

    for panel_row in row_start..end {
        if let Some(gutter) = gutter {
            out.print_gutter(&gutter.text);
        }
        out.set_hl(panel_hl);
        if !pad_text.is_empty() {
            out.print_with_meta(&pad_text, pad_meta.clone());
        }
        if let Some(child_row) = panel_row
            .checked_sub(spec.padding)
            .filter(|row| *row < child_rows)
        {
            let mut buf = smelt_core::buffer::Buffer::new(
                smelt_core::buffer::BufId(0),
                smelt_core::buffer::BufCreateOpts::default(),
            );
            let outcome = {
                let mut col = LineBuilder::new(&mut buf, &theme, child_width);
                render_layout_ir_range(
                    &mut col,
                    child,
                    child_width,
                    child_row,
                    1,
                    None,
                    history,
                    inline_options,
                );
                col.finish()
            };
            if outcome.was_wrapped {
                out.mark_wrapped();
            }
            if outcome.line_count > 0 {
                apply_temp_decoration(out, &buf, 0, false);
                emit_buffer_row_clipped(&buf, 0, child_width, out);
            }
        }
        out.fill_line_bg(panel_bg);
        out.reset_style();
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn measure_ir_panel(
    child: &LayoutIr,
    spec: &PanelSpec,
    width: u16,
    inline_options: &InlineOptions,
) -> u16 {
    let child_width = panel_child_width(width, spec.padding);
    let child_rows = measure_layout_ir_full(child, child_width, inline_options);
    child_rows.saturating_add(spec.padding.saturating_mul(2))
}

fn panel_child_width(width: u16, padding: u16) -> u16 {
    width.saturating_sub(padding.saturating_mul(2)).max(1)
}

#[derive(Clone, Copy)]
enum CapRow {
    Child(u16),
    Marker {
        skipped: u16,
        kept: u16,
        total: Option<u64>,
        direction: &'static str,
    },
}

fn cap_rows(child_rows: u16, spec: &smelt_core::content::block_layout::CapSpec) -> Vec<CapRow> {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    let truncated = child_rows > spec.rows;
    let mut rows = Vec::new();
    match spec.keep {
        CapKeep::Head { marker } => {
            let kept = child_rows.min(spec.rows);
            if truncated && marker == Some(CapMarker::Above) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "above",
                });
            }
            rows.extend((0..kept).map(CapRow::Child));
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                });
            }
        }
        CapKeep::Tail { marker } => {
            let kept = child_rows.min(spec.rows);
            if truncated && marker == Some(CapMarker::Above) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: spec.total_rows.filter(|total| *total > kept as u64),
                    direction: "above",
                });
            }
            rows.extend((child_rows.saturating_sub(kept)..child_rows).map(CapRow::Child));
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                });
            }
        }
        CapKeep::HeadTail { head, marker } => {
            if !truncated {
                rows.extend((0..child_rows).map(CapRow::Child));
            } else {
                let head_rows = head.min(spec.rows);
                let tail_rows = spec.rows.saturating_sub(head_rows);
                rows.extend((0..head_rows).map(CapRow::Child));
                if marker {
                    rows.push(CapRow::Marker {
                        skipped: child_rows.saturating_sub(spec.rows),
                        kept: spec.rows,
                        total: None,
                        direction: "omitted",
                    });
                }
                rows.extend((child_rows.saturating_sub(tail_rows)..child_rows).map(CapRow::Child));
            }
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn render_ir_cap(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &smelt_core::content::block_layout::CapSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let child_rows =
        measure_layout_ir_full_with_gutter(child, width, gutter_width(gutter), inline_options);
    let rows = cap_rows(child_rows, spec);
    let mut written = 0u16;
    for row in rows
        .into_iter()
        .skip(row_start as usize)
        .take(row_count as usize)
    {
        match row {
            CapRow::Child(child_start) => {
                written = written.saturating_add(render_layout_ir_range(
                    out,
                    child,
                    width,
                    child_start,
                    1,
                    gutter,
                    history,
                    inline_options,
                ));
            }
            CapRow::Marker {
                skipped,
                kept,
                total,
                direction,
            } => {
                render_cap_marker(out, skipped, kept, total, direction, gutter);
                written = written.saturating_add(1);
            }
        }
    }
    written
}

fn render_cap_marker(
    out: &mut LineBuilder,
    skipped: u16,
    kept: u16,
    total: Option<u64>,
    direction: &str,
    gutter: Option<&GutterSpec>,
) {
    if let Some(gutter) = gutter.filter(|g| !g.styled) {
        out.print_gutter(&gutter.text);
    }
    out.push_dim();
    if let Some(gutter) = gutter.filter(|g| g.styled) {
        out.print_gutter(&gutter.text);
    }
    let text = if direction == "above" {
        if let Some(total) = total {
            format!(
                "… showing last {} of {}",
                kept,
                pluralize(total as usize, "line", "lines")
            )
        } else {
            format!(
                "… {} {direction}",
                pluralize(skipped as usize, "line", "lines")
            )
        }
    } else if direction == "omitted" {
        format!(
            "… {} omitted …",
            pluralize(skipped as usize, "line", "lines")
        )
    } else {
        format!(
            "… {} {direction}",
            pluralize(skipped as usize, "line", "lines")
        )
    };
    out.print(&text);
    out.pop_style();
    out.newline();
}

fn solve_ir_hbox_widths(
    items: &[smelt_core::content::block_layout::HboxItem<IrLeaf>],
    total_width: u16,
) -> Vec<u16> {
    let constraints: Vec<_> = items.iter().map(|item| item.constraint).collect();
    let fit_widths: Vec<_> = items
        .iter()
        .map(|item| intrinsic_layout_width(&item.layout, total_width))
        .collect();
    solve_hbox_widths_with_fit(&constraints, &fit_widths, total_width)
}

// `Constraint::Fit` uses renderer-defined intrinsic widths. Most leaves can
// report their unwrapped content width; width-dependent leaves fall back to a
// safe cap so a fit column never asks for more than the parent can provide.
fn intrinsic_layout_width(layout: &LayoutIr, total_width: u16) -> u16 {
    match layout {
        BlockLayout::Empty => 0,
        BlockLayout::Leaf(leaf) => intrinsic_leaf_width(leaf, total_width),
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|child| intrinsic_layout_width(child, total_width))
            .max()
            .unwrap_or(0),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| intrinsic_layout_width(&item.layout, total_width))
            .fold(0u16, u16::saturating_add),
        BlockLayout::Gutter { child, spec } => {
            display_width_u16(&spec.text).saturating_add(intrinsic_layout_width(child, total_width))
        }
        BlockLayout::Panel { child, spec } => intrinsic_layout_width(child, total_width)
            .saturating_add(spec.padding.saturating_mul(2)),
        BlockLayout::Style { child, .. } | BlockLayout::Cap { child, .. } => {
            intrinsic_layout_width(child, total_width)
        }
    }
}

fn intrinsic_leaf_width(leaf: &IrLeaf, total_width: u16) -> u16 {
    match leaf {
        IrLeaf::Text(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Runs(spec) => spec
            .lines
            .0
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| display_width_u16(&span.text))
                    .fold(0u16, u16::saturating_add)
            })
            .max()
            .unwrap_or(0),
        IrLeaf::Line(spec) => spec
            .spans
            .iter()
            .map(|span| display_width_u16(&span.text))
            .fold(0u16, u16::saturating_add),
        IrLeaf::Markdown(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Code(spec) => spec
            .content
            .lines()
            .map(display_width_u16)
            .max()
            .unwrap_or(0),
        IrLeaf::Elapsed(_) => 8,
        IrLeaf::Separator(spec) => styled_spans_width(&spec.label).min(u16::MAX as usize) as u16,
        IrLeaf::SourceView(_) => total_width,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_ir_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::HboxItem<IrLeaf>],
    total_width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let widths = solve_ir_hbox_widths(items, total_width);
    let total_rows = items
        .iter()
        .zip(widths.iter().copied())
        .map(|(item, w)| measure_layout_ir_full(&item.layout, w, inline_options))
        .max()
        .unwrap_or(0);
    let end = row_start.saturating_add(row_count).min(total_rows);
    let mut written = 0u16;
    let theme = out.theme().clone();
    for row in row_start..end {
        if let Some(gutter) = gutter {
            out.print_gutter(&gutter.text);
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
                render_layout_ir_range(
                    &mut col,
                    &item.layout,
                    col_w,
                    row,
                    1,
                    None,
                    history,
                    inline_options,
                );
                col.finish();
            }
            let emitted = emit_buffer_row_clipped(&buf, 0, col_w, out);
            if emitted < col_w {
                out.print_with_meta(
                    &" ".repeat((col_w - emitted) as usize),
                    SpanMeta::unselectable(),
                );
            }
        }
        out.newline();
        written = written.saturating_add(1);
    }
    written
}

fn gutter_width(gutter: Option<&GutterSpec>) -> u16 {
    gutter.map(|g| display_width_u16(&g.text)).unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
    use smelt_core::content::block_layout::{CapKeep, CapMarker, CapSpec, LayoutLeaf, TextSpec};

    fn render_lines(layout: &LayoutIr, width: u16) -> Vec<String> {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        {
            let mut out = LineBuilder::new(&mut buf, &theme, width);
            render_layout_ir_into(&mut out, layout, width);
            out.finish();
        }
        (0..buf.line_count())
            .filter_map(|row| buf.get_line(row).map(str::to_string))
            .collect()
    }

    #[test]
    fn runs_continuation_indent_aligns_soft_wrapped_rows() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Runs(RunsSpec {
            lines: protocol::StyledLines(vec![vec![
                protocol::StyledSpan {
                    text: "* grep ".into(),
                    selectable: false,
                    ..Default::default()
                },
                protocol::StyledSpan {
                    text: "alpha beta gamma delta epsilon".into(),
                    ..Default::default()
                },
            ]]),
            hl_group: None,
            continuation_indent: 7,
        }));

        assert_eq!(
            render_lines(&layout, 20),
            vec![
                "* grep alpha beta ",
                "       gamma delta ",
                "       epsilon"
            ]
        );
        assert_eq!(measure_layout_ir(&layout, 20), 3);
    }

    #[test]
    fn cap_tail_marker_uses_total_rows_when_available() {
        let layout = BlockLayout::Cap {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Text(TextSpec {
                content: "one\ntwo\nthree\nfour".into(),
                hl_group: None,
                ansi: false,
            }))),
            spec: CapSpec {
                rows: 2,
                keep: CapKeep::Tail {
                    marker: Some(CapMarker::Above),
                },
                total_rows: Some(100),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["… showing last 2 of 100 lines", "three", "four"]
        );
    }
}
