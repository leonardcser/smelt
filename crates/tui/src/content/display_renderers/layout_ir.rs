use std::borrow::Cow;

use super::markdown::{
    markdown_range_has_visible_text_with_options, measure_markdown_inner_with_options,
    render_markdown_inner_range_with_options,
};
use super::temp_rows::{apply_temp_decoration, emit_buffer_row_clipped};
use crate::content::source_view::{render_source_view, SourceView, SourceViewTarget};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::block_layout::{
    solve_hbox_widths_with_fit, tool_elapsed_text, BlockLayout, CodeSpec, ElapsedSpec, GutterSpec,
    IrLeaf, LayoutIr, LineSpec, MarkdownSpec, PanelSpec, RowPrefixSpec, RunsSpec, SeparatorSpec,
    StyleSpec, TextSpec,
};
use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, parse_inline_spans_with_options, render_code_block, wrap_inline_spans,
    InlineOptions, InlineSpan, InlineSyntax,
};
use smelt_core::content::inline_line::{BreakPolicy, InlineLine, InlineRun, WrappedRun};
use smelt_core::theme::{intern, Theme};
use smelt_core::transcript_model::BlockHistory;

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

pub(crate) fn render_layout_ir_range_into(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: u16,
    row_count: u16,
    inline_options: &InlineOptions,
) -> u16 {
    render_layout_ir_range(
        out,
        layout,
        width,
        row_start,
        row_count,
        None,
        None,
        inline_options,
    )
}

pub(crate) fn render_layout_ir_range_into_with_history(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: u16,
    row_start: u16,
    row_count: u16,
    history: &BlockHistory,
    inline_options: &InlineOptions,
) -> u16 {
    render_layout_ir_range(
        out,
        layout,
        width,
        row_start,
        row_count,
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
        BlockLayout::RowPrefix { child, spec } => render_ir_row_prefix(
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
        BlockLayout::RowPrefix { child, spec } => {
            if let Some(rows) = measure_row_prefix_special(child, spec, width, inline_options) {
                rows
            } else {
                let prefix_width = row_prefix_width(spec);
                let child_width = child_width_after_gutter(width, prefix_width);
                measure_layout_ir_full_with_gutter(child, child_width, gutter_cells, inline_options)
            }
        }
        BlockLayout::Panel { child, spec } => measure_ir_panel(child, spec, width, inline_options),
        BlockLayout::Style { child, .. } => {
            measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options)
        }
        BlockLayout::Cap { child, spec } => {
            let child_rows =
                measure_layout_ir_full_with_gutter(child, width, gutter_cells, inline_options);
            let theme = Theme::default();
            cap_rows_for_child(child_rows, spec, child, width, None, inline_options, &theme)
                .len()
                .min(u16::MAX as usize) as u16
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

fn expand_tabs(line: &str) -> Cow<'_, str> {
    if line.contains('\t') {
        Cow::Owned(line.replace('\t', "    "))
    } else {
        Cow::Borrowed(line)
    }
}

fn wrap_plain_line_ranges(line: &str, width: u16) -> Vec<(usize, usize)> {
    smelt_buffer::wrap::wrap_line_ranges(line, width as usize)
}

fn count_plain_line_ranges(line: &str, width: u16) -> u16 {
    smelt_buffer::wrap::count_line_ranges(line, width as usize).min(u16::MAX as usize) as u16
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
        let expanded = expand_tabs(line);
        let ansi_wrapped = spec.ansi.then(|| wrap_ansi(&expanded, width as usize));
        let plain_ranges;
        let ranges: &[(usize, usize)] = if let Some((_, ranges, _)) = &ansi_wrapped {
            ranges
        } else {
            let line_rows = count_plain_line_ranges(&expanded, width);
            if line_rows > 1 {
                out.mark_wrapped();
            }
            if seen.saturating_add(line_rows) <= row_start {
                seen = seen.saturating_add(line_rows);
                continue;
            }
            plain_ranges = wrap_plain_line_ranges(&expanded, width);
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
            let expanded = expand_tabs(line);
            if spec.ansi {
                let (_, ranges, _) = wrap_ansi(&expanded, width as usize);
                ranges.len() as u16
            } else {
                count_plain_line_ranges(&expanded, width)
            }
        })
        .sum()
}

fn wrap_styled_runs_with_widths(
    spans: &[protocol::StyledSpan],
    first_width: u16,
    continuation_width: u16,
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
    line.wrap_fragments_with_widths(
        first_width.max(1) as usize,
        continuation_width.max(1) as usize,
    )
}

fn wrap_styled_runs(
    spans: &[protocol::StyledSpan],
    width: u16,
    continuation_indent: u16,
) -> Vec<Vec<WrappedRun<usize>>> {
    let width = width.max(1) as usize;
    let indent = continuation_indent as usize;
    let continuation_width = width
        .saturating_sub(indent.min(width.saturating_sub(1)))
        .max(1);
    wrap_styled_runs_with_widths(spans, width as u16, continuation_width as u16)
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

    if gutter.is_none() {
        if spec.italic {
            out.save_style();
            out.set_italic();
        }
        let rows = render_markdown_inner_range_with_options(
            out,
            &spec.content,
            width as usize,
            "",
            spec.dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if spec.italic {
            out.pop_style();
        }
        return rows;
    }

    render_markdown_spec_with_gutter(
        out,
        spec,
        width,
        row_start,
        row_count,
        gutter.expect("guttered markdown render requires gutter"),
        inline_options,
    )
}

fn render_markdown_spec_with_gutter(
    out: &mut LineBuilder,
    spec: &MarkdownSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: &GutterSpec,
    inline_options: &InlineOptions,
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
        if spec.italic {
            col.save_style();
            col.set_italic();
        }
        render_markdown_inner_range_with_options(
            &mut col,
            &spec.content,
            width as usize,
            "",
            spec.dim,
            None,
            inline_options,
            row_start,
            row_count,
        );
        if spec.italic {
            col.pop_style();
        }
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }
    let style_overlay = gutter.styled.then_some((spec.dim, spec.italic));
    let rows = outcome.line_count.min(u16::MAX as usize) as u16;
    for row in 0..rows {
        print_row_gutter(out, gutter);
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out, style_overlay);
        out.newline();
    }
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

    render_via_temp(out, width, row_start, row_count, gutter, None, |col| {
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
    styled_gutter: Option<(bool, bool)>,
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
            print_row_gutter(out, gutter);
        }
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, width, out, styled_gutter);
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
}

fn print_row_gutter(out: &mut LineBuilder, gutter: &GutterSpec) {
    if gutter.styled {
        out.print_gutter(&gutter.text);
    } else {
        out.save_style();
        out.reset_style();
        out.print_gutter(&gutter.text);
        out.pop_style();
    }
}

fn print_row_prefix(out: &mut LineBuilder, prefix: &[protocol::StyledSpan], max_cols: u16) {
    let mut remaining = max_cols as usize;
    for span in prefix {
        if remaining == 0 {
            break;
        }
        let piece = clipped_cell_prefix(&span.text, remaining);
        print_styled_span_range(out, span, None, 0..piece.len());
        remaining = remaining.saturating_sub(display_width(piece));
        if piece.len() < span.text.len() {
            break;
        }
    }
}

fn clipped_cell_prefix(text: &str, max_cols: usize) -> &str {
    if display_width(text) <= max_cols {
        return text;
    }
    let mut end = smelt_buffer::text::cell_to_byte(text, max_cols);
    let mut prefix = smelt_buffer::text::slice(text, 0..end);
    if display_width(prefix) > max_cols {
        end = smelt_buffer::text::prev_char_boundary(text, end);
        prefix = smelt_buffer::text::slice(text, 0..end);
    }
    prefix
}

fn row_prefix_child_widths(spec: &RowPrefixSpec, width: u16) -> (u16, u16) {
    let first = styled_spans_width(&spec.first).min(u16::MAX as usize) as u16;
    let rest = styled_spans_width(&spec.rest).min(u16::MAX as usize) as u16;
    (
        child_width_after_gutter(width, first),
        child_width_after_gutter(width, rest),
    )
}

struct RowPrefixRunRow<'a> {
    spans: &'a [protocol::StyledSpan],
    fragments: Vec<WrappedRun<usize>>,
    soft_wrap: bool,
}

fn row_prefix_runs_wrapped_rows(
    spec: &RunsSpec,
    first_width: u16,
    rest_width: u16,
) -> Vec<RowPrefixRunRow<'_>> {
    let mut rows = Vec::new();
    for spans in &spec.lines.0 {
        let line_first_width = if rows.is_empty() {
            first_width
        } else {
            rest_width
        };
        let wrapped = wrap_styled_runs_with_widths(spans, line_first_width, rest_width);
        for (idx, fragments) in wrapped.into_iter().enumerate() {
            rows.push(RowPrefixRunRow {
                spans: spans.as_slice(),
                fragments,
                soft_wrap: idx > 0,
            });
        }
    }
    rows
}

fn measure_row_prefix_runs(spec: &RunsSpec, first_width: u16, rest_width: u16) -> u16 {
    row_prefix_runs_wrapped_rows(spec, first_width, rest_width)
        .len()
        .min(u16::MAX as usize) as u16
}

fn measure_row_prefix_special(
    child: &LayoutIr,
    prefix: &RowPrefixSpec,
    width: u16,
    _inline_options: &InlineOptions,
) -> Option<u16> {
    let (first_width, rest_width) = row_prefix_child_widths(prefix, width);
    match child {
        BlockLayout::Leaf(IrLeaf::Runs(spec)) => {
            Some(measure_row_prefix_runs(spec, first_width, rest_width))
        }
        BlockLayout::Cap { child, spec } => {
            let BlockLayout::Leaf(IrLeaf::Runs(runs)) = child.as_ref() else {
                return None;
            };
            let child_rows = measure_row_prefix_runs(runs, first_width, rest_width);
            Some(cap_rows(child_rows, spec).len().min(u16::MAX as usize) as u16)
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_row_prefix_runs(
    out: &mut LineBuilder,
    spec: &RunsSpec,
    prefix: &RowPrefixSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let (first_width, rest_width) = row_prefix_child_widths(prefix, width);
    let rows = row_prefix_runs_wrapped_rows(spec, first_width, rest_width);
    let default_hl = spec.hl_group.as_deref();
    let mut written = 0u16;
    for (source_row, row) in rows
        .iter()
        .enumerate()
        .skip(row_start as usize)
        .take(row_count as usize)
    {
        if let Some(gutter) = gutter {
            print_row_gutter(out, gutter);
        }
        let (row_prefix, child_width) = if source_row == 0 {
            (&prefix.first, first_width)
        } else {
            (&prefix.rest, rest_width)
        };
        print_row_prefix(out, row_prefix, width.saturating_sub(child_width));
        if row.soft_wrap {
            out.mark_soft_wrap_continuation();
        }
        for fragment in &row.fragments {
            let Some(span) = row.spans.get(fragment.run_index) else {
                continue;
            };
            print_styled_span_range(out, span, default_hl, fragment.range.clone());
        }
        out.newline();
        written = written.saturating_add(1);
    }
    written
}

#[allow(clippy::too_many_arguments)]
fn render_row_prefix_runs_cap(
    out: &mut LineBuilder,
    runs: &RunsSpec,
    cap: &smelt_core::content::block_layout::CapSpec,
    prefix: &RowPrefixSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let (first_width, rest_width) = row_prefix_child_widths(prefix, width);
    let child_rows = measure_row_prefix_runs(runs, first_width, rest_width);
    let rows = cap_rows(child_rows, cap);
    let mut written = 0u16;
    for (output_row, row) in rows
        .into_iter()
        .enumerate()
        .skip(row_start as usize)
        .take(row_count as usize)
    {
        match row {
            CapRow::Child(child_row) => {
                written = written.saturating_add(render_row_prefix_runs(
                    out, runs, prefix, width, child_row, 1, gutter,
                ));
            }
            CapRow::Marker {
                skipped,
                kept,
                total,
                direction,
                ..
            } => {
                if let Some(gutter) = gutter {
                    print_row_gutter(out, gutter);
                }
                let (row_prefix, child_width) = if output_row == 0 {
                    (&prefix.first, first_width)
                } else {
                    (&prefix.rest, rest_width)
                };
                print_row_prefix(out, row_prefix, width.saturating_sub(child_width));
                render_cap_marker_text(out, skipped, kept, total, direction, child_width);
                written = written.saturating_add(1);
            }
        }
    }
    written
}

fn render_cap_marker_text(
    out: &mut LineBuilder,
    skipped: u16,
    kept: u16,
    total: Option<u64>,
    direction: &str,
    max_cols: u16,
) {
    out.push_dim();
    let text = cap_marker_text(skipped, kept, total, direction);
    out.print(clipped_cell_prefix(&text, max_cols as usize));
    out.pop_style();
    out.newline();
}

// Row prefixes are applied after the child has chosen and capped its rows, so
// the renderer first materializes only the requested child rows into a scratch
// buffer, then replays those rows with chrome attached. The replay preserves row
// decorations and copy/source metadata via `apply_temp_decoration`.
#[allow(clippy::too_many_arguments)]
fn render_ir_row_prefix(
    out: &mut LineBuilder,
    child: &LayoutIr,
    spec: &RowPrefixSpec,
    width: u16,
    row_start: u16,
    row_count: u16,
    gutter: Option<&GutterSpec>,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    if let BlockLayout::Leaf(IrLeaf::Runs(runs)) = child {
        return render_row_prefix_runs(out, runs, spec, width, row_start, row_count, gutter);
    }
    if let BlockLayout::Cap {
        child: cap_child,
        spec: cap_spec,
    } = child
    {
        if let BlockLayout::Leaf(IrLeaf::Runs(runs)) = cap_child.as_ref() {
            return render_row_prefix_runs_cap(
                out, runs, cap_spec, spec, width, row_start, row_count, gutter,
            );
        }
    }

    let prefix_width = row_prefix_width(spec);
    let child_width = child_width_after_gutter(width, prefix_width);
    let theme = out.theme().clone();
    let inherited_style = out.current_style();
    let mut buf = smelt_core::buffer::Buffer::new(
        smelt_core::buffer::BufId(0),
        smelt_core::buffer::BufCreateOpts::default(),
    );
    let outcome = {
        let mut col = LineBuilder::new(&mut buf, &theme, child_width);
        col.push(None, inherited_style);
        render_layout_ir_range(
            &mut col,
            child,
            child_width,
            row_start,
            row_count,
            None,
            history,
            inline_options,
        );
        col.finish()
    };
    if outcome.was_wrapped {
        out.mark_wrapped();
    }

    let total = outcome.line_count.min(u16::MAX as usize) as u16;
    let mut rows = 0u16;
    for row in 0..total {
        if let Some(gutter) = gutter {
            print_row_gutter(out, gutter);
        }
        let source_row = row_start.saturating_add(row);
        let prefix = if source_row == 0 {
            &spec.first
        } else {
            &spec.rest
        };
        print_row_prefix(out, prefix, width.saturating_sub(child_width));
        apply_temp_decoration(out, &buf, row as usize, true);
        emit_buffer_row_clipped(&buf, row, child_width, out, None);
        out.newline();
        rows = rows.saturating_add(1);
    }
    rows
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
                emit_buffer_row_clipped(&buf, 0, child_width, out, None);
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
        omitted: Option<(u16, u16)>,
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
                    omitted: None,
                });
            }
            rows.extend((0..kept).map(CapRow::Child));
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                    omitted: None,
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
                    omitted: None,
                });
            }
            rows.extend((child_rows.saturating_sub(kept)..child_rows).map(CapRow::Child));
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapRow::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                    omitted: None,
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
                        omitted: Some((head_rows, child_rows.saturating_sub(tail_rows))),
                    });
                }
                rows.extend((child_rows.saturating_sub(tail_rows)..child_rows).map(CapRow::Child));
            }
        }
    }
    rows
}

fn cap_rows_for_child(
    child_rows: u16,
    spec: &smelt_core::content::block_layout::CapSpec,
    child: &LayoutIr,
    width: u16,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    theme: &Theme,
) -> Vec<CapRow> {
    let mut rows = cap_rows(child_rows, spec);
    rows.retain(|row| match row {
        CapRow::Marker {
            direction: "omitted",
            omitted: Some((start, end)),
            ..
        } => omitted_rows_have_visible_text(
            child,
            width,
            *start,
            *end,
            history,
            inline_options,
            theme,
        ),
        _ => true,
    });
    rows
}

fn omitted_rows_have_visible_text(
    child: &LayoutIr,
    width: u16,
    start: u16,
    end: u16,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
    theme: &Theme,
) -> bool {
    if start >= end {
        return false;
    }
    if let Some(visible) = layout_range_has_visible_text(child, width, start, end, inline_options) {
        return visible;
    }
    let mut buf = smelt_core::buffer::Buffer::new(smelt_core::buffer::BufId(0), Default::default());
    let mut out = LineBuilder::new(&mut buf, theme, width);
    let rows = render_layout_ir_range(
        &mut out,
        child,
        width,
        start,
        end.saturating_sub(start),
        None,
        history,
        inline_options,
    );
    out.finish();
    rows > 0 && (0..rows as usize).any(|row| !buf.get_line(row).unwrap_or("").trim().is_empty())
}

fn layout_range_has_visible_text(
    layout: &LayoutIr,
    width: u16,
    start: u16,
    end: u16,
    inline_options: &InlineOptions,
) -> Option<bool> {
    if start >= end {
        return Some(false);
    }
    match layout {
        BlockLayout::Empty => Some(false),
        BlockLayout::Leaf(IrLeaf::Markdown(spec)) if !spec.inline => {
            Some(markdown_range_has_visible_text_with_options(
                &spec.content,
                width as usize,
                "",
                spec.dim,
                None,
                inline_options,
                start,
                end.saturating_sub(start),
            ))
        }
        BlockLayout::Vbox(items) => {
            vbox_range_has_visible_text(items, width, start, end, inline_options)
        }
        BlockLayout::Hbox(items) => {
            let widths = solve_ir_hbox_widths(items, width);
            let mut saw_unknown = false;
            for (item, item_width) in items.iter().zip(widths) {
                match layout_range_has_visible_text(
                    &item.layout,
                    item_width,
                    start,
                    end,
                    inline_options,
                ) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => saw_unknown = true,
                }
            }
            (!saw_unknown).then_some(false)
        }
        BlockLayout::Gutter { child, spec } => {
            let child_width = child_width_after_gutter(width, display_width_u16(&spec.text));
            layout_range_has_visible_text(child, child_width, start, end, inline_options)
        }
        BlockLayout::Style { child, .. } => {
            layout_range_has_visible_text(child, width, start, end, inline_options)
        }
        _ => None,
    }
}

fn vbox_range_has_visible_text(
    items: &[LayoutIr],
    width: u16,
    start: u16,
    end: u16,
    inline_options: &InlineOptions,
) -> Option<bool> {
    let mut base = 0u16;
    let mut saw_unknown = false;
    for child in items {
        if base >= end {
            break;
        }
        let rows = measure_layout_ir_full(child, width, inline_options);
        let child_end = base.saturating_add(rows);
        if child_end > start && base < end {
            let local_start = start.saturating_sub(base);
            let local_end = end.saturating_sub(base).min(rows);
            match layout_range_has_visible_text(
                child,
                width,
                local_start,
                local_end,
                inline_options,
            ) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => saw_unknown = true,
            }
        }
        base = child_end;
    }
    (!saw_unknown).then_some(false)
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
    let theme = out.theme().clone();
    let rows = cap_rows_for_child(
        child_rows,
        spec,
        child,
        width,
        history,
        inline_options,
        &theme,
    );
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
                ..
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
    let text = cap_marker_text(skipped, kept, total, direction);
    out.print(&text);
    out.pop_style();
    out.newline();
}

fn cap_marker_text(skipped: u16, kept: u16, total: Option<u64>, direction: &str) -> String {
    if direction == "above" {
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
    }
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
        BlockLayout::RowPrefix { child, spec } => {
            row_prefix_width(spec).saturating_add(intrinsic_layout_width(child, total_width))
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
            let emitted = emit_buffer_row_clipped(&buf, 0, col_w, out, None);
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

fn row_prefix_width(spec: &RowPrefixSpec) -> u16 {
    styled_spans_width(&spec.first)
        .max(styled_spans_width(&spec.rest))
        .min(u16::MAX as usize) as u16
}

fn display_width_u16(text: &str) -> u16 {
    display_width(text).min(u16::MAX as usize) as u16
}

fn child_width_after_gutter(width: u16, gutter_width: u16) -> u16 {
    width.saturating_sub(gutter_width).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
    use smelt_core::content::block_layout::{
        CapKeep, CapMarker, CapSpec, GutterSpec, LayoutLeaf, LineSpec, MarkdownSpec, RowPrefixSpec,
        TextSpec,
    };

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
    fn markdown_range_render_does_not_materialize_full_block() {
        let content = (0..2_000)
            .map(|i| format!("Paragraph {i}: {}", "alpha beta gamma delta ".repeat(6)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content,
            dim: false,
            italic: false,
            inline: false,
        }));
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 60);
            render_layout_ir_range_into(&mut out, &layout, 60, 300, 3, &InlineOptions::default());
            out.finish();
        }
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        assert_eq!(buf.line_count(), 3);
        let full_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown")
            .map_or(0, |row| row.count);
        assert_eq!(
            full_renders, 0,
            "range render should not use full markdown rendering"
        );
        let range_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown:range")
            .map_or(0, |row| row.count);
        assert!(
            range_renders > 0,
            "range render should use markdown range rendering"
        );
    }

    #[test]
    fn large_markdown_tail_range_matches_measured_rows() {
        let layout = BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
            content: "x".repeat(2_100_000),
            dim: false,
            italic: false,
            inline: false,
        }));
        let width = 51;
        let options = InlineOptions::default();
        let measured = measure_layout_ir_with_options(&layout, width, &options);
        let row_count = 43;
        let row_start = measured - row_count;
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        let rendered = {
            let mut out = LineBuilder::new(&mut buf, &theme, width);
            let rendered = render_layout_ir_range_into(
                &mut out, &layout, width, row_start, row_count, &options,
            );
            let outcome = out.finish();
            assert_eq!(outcome.line_count, rendered as usize);
            rendered
        };

        assert_eq!(rendered, row_count);
    }

    #[test]
    fn guttered_markdown_range_render_stays_bounded() {
        let content = (0..2_000)
            .map(|i| format!("Paragraph {i}: {}", "alpha beta gamma delta ".repeat(6)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let layout = BlockLayout::Gutter {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Markdown(MarkdownSpec {
                content,
                dim: true,
                italic: true,
                inline: false,
            }))),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        };
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());

        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        {
            let mut out = LineBuilder::new(&mut buf, &theme, 60);
            render_layout_ir_range_into(&mut out, &layout, 60, 300, 3, &InlineOptions::default());
            out.finish();
        }
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        assert_eq!(buf.line_count(), 3);
        for row in 0..buf.line_count() {
            assert!(
                buf.get_line(row).is_some_and(|line| line.starts_with("│ ")),
                "row {row} missing gutter: {:?}",
                buf.get_line(row)
            );
        }
        let full_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown")
            .map_or(0, |row| row.count);
        assert_eq!(
            full_renders, 0,
            "guttered range render should not use full markdown rendering"
        );
        let range_renders = snapshot
            .durations
            .iter()
            .find(|row| row.label == "render:markdown:range")
            .map_or(0, |row| row.count);
        assert!(
            range_renders > 0,
            "range render should use markdown range rendering"
        );
    }

    #[test]
    fn clipped_rows_count_controls_as_cells() {
        let theme = Theme::default();
        let mut src = Buffer::new(BufId(1), BufCreateOpts::default());
        src.set_all_lines(vec!["abcdefghi\u{7f}j".into()]);
        let mut dst = Buffer::new(BufId(2), BufCreateOpts::default());

        let emitted = {
            let mut out = LineBuilder::new(&mut dst, &theme, 10);
            let emitted = emit_buffer_row_clipped(&src, 0, 10, &mut out, None);
            out.finish();
            emitted
        };

        let line = dst.get_line(0).expect("clipped row");
        assert_eq!(emitted, 10);
        assert_eq!(display_width(line), 10);
        assert_eq!(line, "abcdefghi\u{7f}");
    }

    #[test]
    fn row_prefix_clips_long_control_chrome_to_viewport() {
        let prefix = protocol::StyledSpan {
            text: format!("* {}", "\u{1c}".repeat(8)),
            ..Default::default()
        };
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![protocol::StyledSpan {
                    text: "x".into(),
                    ..Default::default()
                }],
                hl_group: None,
            }))),
            spec: RowPrefixSpec {
                first: vec![prefix.clone()],
                rest: vec![prefix],
            },
        };

        let lines = render_lines(&layout, 10);
        assert_eq!(lines, vec![format!("* {}x", "\u{1c}".repeat(7))]);
        assert!(lines.iter().all(|line| display_width(line) <= 10));
    }

    #[test]
    fn replaying_temp_rows_with_wide_chars_uses_display_columns() {
        let layout = BlockLayout::RowPrefix {
            child: Box::new(BlockLayout::Leaf(LayoutLeaf::Line(LineSpec {
                spans: vec![
                    protocol::StyledSpan {
                        text: "aaaaaaaaaaaaaaaaaaaaaaaaaaa界界".into(),
                        ..Default::default()
                    },
                    protocol::StyledSpan {
                        text: "x".into(),
                        hl: Some("accent".into()),
                        ..Default::default()
                    },
                ],
                hl_group: None,
            }))),
            spec: RowPrefixSpec {
                first: Vec::new(),
                rest: Vec::new(),
            },
        };

        assert_eq!(
            render_lines(&layout, 80),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaa界界x"]
        );
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
