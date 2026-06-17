//! Inline markdown rendering: pulldown-cmark inline event lowering,
//! inline-span wrapping, and the markdown table renderer that uses both.

use super::action_refs::{action_for_destination, inline_file_reference, url_action};
use crate::buffer::{SpanAction, SpanMeta};
use crate::content::builder::{display_width, LineBuilder};
use crate::content::file_icons::{self, FileIcon, FileIconOptions};
use crate::content::inline_line::{BreakPolicy, InlineLine, InlineRun};
use crate::content::ColumnAlignment;
use crate::style::Color;
use crate::theme::{intern, HlGroup};
use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

/// Render a markdown table. `alignments` may be empty (defaults to left for all
/// columns) or shorter than the column count (missing entries default to left).
/// `width` is the column budget available when no [`BoxContext`] surrounds the
/// table; ignored otherwise.
pub fn render_markdown_table(
    out: &mut LineBuilder,
    rows: &[Vec<String>],
    alignments: &[ColumnAlignment],
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
) -> u16 {
    render_markdown_table_with_options(
        out,
        rows,
        alignments,
        width,
        dim,
        bctx,
        indent,
        &InlineOptions::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_markdown_table_with_options(
    out: &mut LineBuilder,
    rows: &[Vec<String>],
    alignments: &[ColumnAlignment],
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
    options: &InlineOptions,
) -> u16 {
    if rows.is_empty() {
        return 0;
    }
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return 0;
    }

    let max_table = markdown_table_width(width, bctx, indent);
    let align_for = |c: usize| alignments.get(c).copied().unwrap_or_default();

    let start = out.line_count();
    let Some(col_widths) = fit_column_widths(rows, num_cols, max_table) else {
        let rendered = render_table_stacked(out, rows, max_table, dim, bctx, indent, options);
        out.stamp_chrome_delimited_block(start);
        return rendered;
    };

    let mut total_rows = 0u16;
    total_rows += render_border(out, &col_widths, bctx, indent, "┏", "┳", "┓");
    if let Some(header) = rows.first() {
        total_rows += render_table_row(
            out,
            header,
            &col_widths,
            align_for,
            dim,
            bctx,
            indent,
            options,
        );
        total_rows += render_border(out, &col_widths, bctx, indent, "┣", "╋", "┫");
    }
    for row in rows.iter().skip(1) {
        total_rows +=
            render_table_row(out, row, &col_widths, align_for, dim, bctx, indent, options);
    }
    total_rows += render_border(out, &col_widths, bctx, indent, "┗", "┻", "┛");
    out.stamp_chrome_delimited_block(start);
    total_rows
}

pub fn measure_markdown_table(
    rows: &[Vec<String>],
    alignments: &[ColumnAlignment],
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
) -> u16 {
    measure_markdown_table_with_options(
        rows,
        alignments,
        width,
        dim,
        bctx,
        indent,
        &InlineOptions::default(),
    )
}

pub fn measure_markdown_table_with_options(
    rows: &[Vec<String>],
    alignments: &[ColumnAlignment],
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
    options: &InlineOptions,
) -> u16 {
    let _ = alignments;
    if rows.is_empty() {
        return 0;
    }
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return 0;
    }

    let max_table = markdown_table_width(width, bctx, indent);
    let Some(col_widths) = fit_column_widths(rows, num_cols, max_table) else {
        return measure_table_stacked(rows, max_table, dim, options);
    };

    let header_rows = rows
        .first()
        .map(|header| measure_table_row(header, &col_widths, dim, options))
        .unwrap_or(0);
    let body_rows: u16 = rows
        .iter()
        .skip(1)
        .map(|row| measure_table_row(row, &col_widths, dim, options))
        .sum();
    header_rows.saturating_add(body_rows).saturating_add(3) // top border, header separator, bottom border
}

fn markdown_table_width(
    width: usize,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
) -> usize {
    match bctx {
        Some(b) => b.inner_w,
        None => width.saturating_sub(display_width(indent)),
    }
}

fn measure_table_row(row: &[String], widths: &[usize], dim: bool, options: &InlineOptions) -> u16 {
    row.iter()
        .enumerate()
        .map(|(c, cell)| measure_cell_rows(cell, widths.get(c).copied().unwrap_or(0), dim, options))
        .max()
        .unwrap_or(1)
}

fn measure_table_stacked(
    rows: &[Vec<String>],
    max_table: usize,
    dim: bool,
    options: &InlineOptions,
) -> u16 {
    let header = match rows.first() {
        Some(h) => h,
        None => return 0,
    };
    let label_width = header
        .iter()
        .map(|h| strip_markdown_markers(h).width())
        .max()
        .unwrap_or(0);
    let content_width = max_table.max(1);
    let label_value_indent = 2 + label_width + 2;
    let side_by_side = label_width > 0 && label_value_indent < content_width;
    let value_width = content_width.saturating_sub(label_value_indent).max(1);
    let mut total_rows = 0u16;
    for (ri, row) in rows.iter().skip(1).enumerate() {
        if ri > 0 {
            total_rows = total_rows.saturating_add(1);
        }
        for (c, cell) in row.iter().enumerate() {
            let label = header.get(c).map(|s| s.as_str()).unwrap_or("");
            if side_by_side {
                total_rows =
                    total_rows.saturating_add(measure_cell_rows(cell, value_width, dim, options));
            } else {
                let inner_indent = content_width.min(2);
                let text_width = content_width.saturating_sub(inner_indent).max(1);
                if !label.is_empty() {
                    total_rows = total_rows
                        .saturating_add(measure_cell_rows(label, text_width, dim, options));
                }
                total_rows =
                    total_rows.saturating_add(measure_cell_rows(cell, text_width, dim, options));
            }
        }
    }
    total_rows
}

fn measure_cell_rows(text: &str, max_width: usize, dim: bool, options: &InlineOptions) -> u16 {
    let spans = parse_inline_spans_with_options(text, dim, options);
    wrap_inline_spans(&spans, max_width).len() as u16
}

/// Pick a final width per column that fits within `max_table` (including the
/// `" cell ┃"` overhead). `None` means the table can't fit even at min widths
/// and the caller should fall back to a stacked layout.
fn fit_column_widths(
    rows: &[Vec<String>],
    num_cols: usize,
    max_table: usize,
) -> Option<Vec<usize>> {
    let mut natural = vec![0usize; num_cols];
    let mut min = vec![0usize; num_cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            natural[c] = natural[c].max(strip_markdown_markers(cell).width());
            min[c] = min[c].max(min_visual_width(cell));
        }
    }
    let overhead = 3 * num_cols + 1;
    let avail = max_table.saturating_sub(overhead);
    if natural.iter().sum::<usize>() <= avail {
        return Some(natural);
    }
    if min.iter().sum::<usize>() > avail {
        return None;
    }
    let natural_total: usize = natural.iter().sum();
    if natural_total == 0 {
        return Some(natural);
    }
    let mut widths: Vec<usize> = natural
        .iter()
        .zip(min.iter())
        .map(|(&w, &m)| ((w * avail) / natural_total).max(m))
        .collect();
    while widths.iter().sum::<usize>() > avail {
        let excess = widths.iter().sum::<usize>() - avail;
        let shrinkable: Vec<usize> = (0..num_cols).filter(|&c| widths[c] > min[c]).collect();
        if shrinkable.is_empty() {
            break;
        }
        let per_col = (excess / shrinkable.len()).max(1);
        for c in shrinkable {
            let reduce = per_col.min(widths[c] - min[c]);
            widths[c] -= reduce;
        }
    }
    Some(widths)
}

/// Apply the dim "table border" style. No bg - borders ride on whatever bg the
/// surrounding block uses (transcript / box / code-block).
fn enter_border_style(out: &mut LineBuilder) {
    out.set_dim();
}

fn render_row_prefix(out: &mut LineBuilder, bctx: Option<&super::super::BoxContext>, indent: &str) {
    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print_gutter(indent);
    }
}

fn render_row_suffix(
    out: &mut LineBuilder,
    bctx: Option<&super::super::BoxContext>,
    line_cols: usize,
) {
    if let Some(b) = bctx {
        b.print_right(out, line_cols);
    }
    out.newline();
}

fn render_border(
    out: &mut LineBuilder,
    widths: &[usize],
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
    l: &str,
    j: &str,
    r: &str,
) -> u16 {
    render_row_prefix(out, bctx, indent);
    enter_border_style(out);
    out.print_gutter(l);
    let mut line_cols = 1;
    for (c, width) in widths.iter().enumerate() {
        let seg = width + 2;
        out.print_gutter(&"━".repeat(seg));
        line_cols += seg;
        if c + 1 < widths.len() {
            out.print_gutter(j);
            line_cols += 1;
        }
    }
    out.print_gutter(r);
    line_cols += 1;
    out.reset_style();
    render_row_suffix(out, bctx, line_cols);
    1
}

#[allow(clippy::too_many_arguments)]
fn render_table_row(
    out: &mut LineBuilder,
    row: &[String],
    widths: &[usize],
    align_for: impl Fn(usize) -> ColumnAlignment,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
    options: &InlineOptions,
) -> u16 {
    let wrapped: Vec<Vec<Vec<InlineSpan>>> = row
        .iter()
        .enumerate()
        .map(|(c, cell)| {
            wrap_cell_spans(out, cell, widths.get(c).copied().unwrap_or(0), dim, options)
        })
        .collect();
    let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);

    for vline in 0..height {
        render_row_prefix(out, bctx, indent);
        enter_border_style(out);
        out.print_gutter("┃");
        out.reset_style();
        let mut line_cols = 1;
        for (c, width) in widths.iter().enumerate() {
            let spans: &[InlineSpan] = wrapped
                .get(c)
                .and_then(|w| w.get(vline))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let pad = width.saturating_sub(inline_spans_width(spans));
            let (left_pad, right_pad) = match align_for(c) {
                ColumnAlignment::Left => (0, pad),
                ColumnAlignment::Right => (pad, 0),
                ColumnAlignment::Center => (pad / 2, pad - pad / 2),
            };
            out.print_gutter(" ");
            if left_pad > 0 {
                out.print_gutter(&" ".repeat(left_pad));
            }
            emit_inline_spans(out, spans);
            if right_pad > 0 {
                out.print_gutter(&" ".repeat(right_pad));
            }
            out.print_gutter(" ");
            enter_border_style(out);
            out.print_gutter("┃");
            out.reset_style();
            line_cols += width + 3; // " content pad ┃"
        }
        render_row_suffix(out, bctx, line_cols);
    }
    height as u16
}

/// Stacked fallback: each data row becomes "Header  value" lines, used when
/// the table is too wide. The layout is still width-bounded; otherwise the
/// fallback itself creates horizontal overflow in pre-formatted panes.
fn render_table_stacked(
    out: &mut LineBuilder,
    rows: &[Vec<String>],
    max_table: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
    options: &InlineOptions,
) -> u16 {
    let header = match rows.first() {
        Some(h) => h,
        None => return 0,
    };
    out.mark_wrapped();

    let label_width = header
        .iter()
        .map(|h| strip_markdown_markers(h).width())
        .max()
        .unwrap_or(0);
    let content_width = max_table.max(1);
    let label_value_indent = 2 + label_width + 2;
    let side_by_side = label_width > 0 && label_value_indent < content_width;
    let value_width = content_width.saturating_sub(label_value_indent).max(1);

    let mut total_rows = 0u16;
    for (ri, row) in rows.iter().skip(1).enumerate() {
        if ri > 0 {
            if bctx.is_some() {
                render_row_prefix(out, bctx, indent);
                render_row_suffix(out, bctx, 0);
            } else {
                out.newline();
            }
            total_rows += 1;
        }
        for (c, cell) in row.iter().enumerate() {
            let label = header.get(c).map(|s| s.as_str()).unwrap_or("");
            if side_by_side {
                let label_visual = strip_markdown_markers(label).width();
                let pad = label_width.saturating_sub(label_visual);
                let wrapped = wrap_cell_spans(out, cell, value_width, dim, options);
                for (li, spans) in wrapped.iter().enumerate() {
                    render_row_prefix(out, bctx, indent);
                    if li == 0 {
                        out.print_gutter("  ");
                        emit_table_label(out, label, dim, options);
                        if pad > 0 {
                            out.print_gutter(&" ".repeat(pad));
                        }
                        out.print_gutter("  ");
                    } else {
                        out.print_gutter(&" ".repeat(label_value_indent));
                    }
                    emit_inline_spans(out, spans);
                    let line_cols = label_value_indent + inline_spans_width(spans);
                    render_row_suffix(out, bctx, line_cols);
                    total_rows += 1;
                }
            } else {
                let inner_indent = content_width.min(2);
                let text_width = content_width.saturating_sub(inner_indent).max(1);
                if !label.is_empty() {
                    let labels = wrap_cell_spans(out, label, text_width, dim, options);
                    for spans in &labels {
                        render_row_prefix(out, bctx, indent);
                        if inner_indent > 0 {
                            out.print_gutter(&" ".repeat(inner_indent));
                        }
                        emit_table_label_spans(out, spans, dim);
                        render_row_suffix(out, bctx, inner_indent + inline_spans_width(spans));
                        total_rows += 1;
                    }
                }

                let wrapped = wrap_cell_spans(out, cell, text_width, dim, options);
                for spans in &wrapped {
                    render_row_prefix(out, bctx, indent);
                    if inner_indent > 0 {
                        out.print_gutter(&" ".repeat(inner_indent));
                    }
                    emit_inline_spans(out, spans);
                    render_row_suffix(out, bctx, inner_indent + inline_spans_width(spans));
                    total_rows += 1;
                }
            }
        }
    }
    total_rows
}

fn wrap_cell_spans(
    out: &mut LineBuilder,
    text: &str,
    max_width: usize,
    dim: bool,
    options: &InlineOptions,
) -> Vec<Vec<InlineSpan>> {
    let spans = parse_inline_spans_with_options(text, dim, options);
    let rows = wrap_inline_spans(&spans, max_width);
    if rows.len() > 1 {
        out.mark_wrapped();
    }
    rows
}

fn emit_table_label(out: &mut LineBuilder, label: &str, dim: bool, options: &InlineOptions) {
    let spans = parse_inline_spans_with_options(label, dim, options);
    emit_table_label_spans(out, &spans, dim);
}

fn emit_table_label_spans(out: &mut LineBuilder, spans: &[InlineSpan], dim: bool) {
    for span in spans {
        out.save_style();
        out.set_fg(Color::DarkGrey);
        if dim || span.style.dim {
            out.set_dim();
        }
        if span.style.bold {
            out.set_bold();
        }
        if span.style.italic {
            out.set_italic();
        }
        if span.style.underline {
            out.set_underline();
        }
        if span.style.crossedout {
            out.set_crossedout();
        }
        if let Some(group) = span.style.group {
            out.set_hl(group);
        }
        out.print(&span.text);
        out.pop_style();
    }
}

/// Visual width of the longest unwrappable segment; used for minimum column widths.
fn min_visual_width(text: &str) -> usize {
    let mut max_w = 0usize;
    for span in parse_inline_spans(text, false) {
        let is_code = span.style.group == Some(intern("SmeltAccent"));
        if is_code {
            max_w = max_w.max(span.text.width());
        } else {
            max_w = max_w.max(
                span.text
                    .split(' ')
                    .map(UnicodeWidthStr::width)
                    .max()
                    .unwrap_or(0),
            );
        }
    }
    max_w
}

fn strip_markdown_markers(text: &str) -> String {
    parse_inline_spans(text, false)
        .into_iter()
        .map(|span| span.text)
        .collect()
}

// ── Parse-then-wrap pipeline ─────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineOptions {
    pub file_icons: FileIconOptions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub underline: bool,
    pub crossedout: bool,
    /// Theme group for the span; `None` for plain text.
    pub group: Option<HlGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineSpan {
    pub text: String,
    pub style: InlineStyle,
    pub meta: SpanMeta,
}

pub fn parse_inline_spans(text: &str, dim: bool) -> Vec<InlineSpan> {
    parse_inline_spans_with_options(text, dim, &InlineOptions::default())
}

pub fn parse_inline_spans_with_options(
    text: &str,
    dim: bool,
    options: &InlineOptions,
) -> Vec<InlineSpan> {
    if text.is_empty() {
        return Vec::new();
    }

    let parser_options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    lower_inline_fragment_events(
        text,
        Parser::new_ext(text, parser_options).into_offset_iter(),
        dim,
        options,
    )
}

pub fn lower_inline_events<'a>(
    events: impl IntoIterator<Item = (Event<'a>, Range<usize>)>,
    dim: bool,
) -> Vec<InlineSpan> {
    lower_inline_events_with_options(events, dim, &InlineOptions::default())
}

pub fn lower_inline_events_with_options<'a>(
    events: impl IntoIterator<Item = (Event<'a>, Range<usize>)>,
    dim: bool,
    options: &InlineOptions,
) -> Vec<InlineSpan> {
    let mut styles = vec![InlineStyle {
        dim,
        ..Default::default()
    }];
    let mut out = Vec::new();
    let mut link_stack = Vec::new();

    for (event, _) in events {
        match event {
            Event::Start(tag) => {
                if let Some(link) = pending_link(&tag, options) {
                    link_stack.push(link);
                }
                push_tag_style(&mut styles, tag);
            }
            Event::End(TagEnd::Link) => {
                if styles.len() > 1 {
                    styles.pop();
                }
                if let Some(link) = link_stack.pop() {
                    if link.emit_suffix {
                        push_link_suffix(
                            &mut out,
                            &link.destination,
                            *styles.last().unwrap(),
                            options,
                        );
                    }
                }
            }
            Event::End(_) if styles.len() > 1 => {
                styles.pop();
            }
            event => lower_inline_event(
                event,
                &mut out,
                &styles,
                link_stack.last().and_then(|link| link.action.as_ref()),
                options,
            ),
        }
    }

    out
}

pub fn lower_inline_event_lines<'a>(
    events: impl IntoIterator<Item = (Event<'a>, Range<usize>)>,
    line_ranges: &[Range<usize>],
    dim: bool,
) -> Vec<Vec<InlineSpan>> {
    lower_inline_event_lines_with_options(events, line_ranges, dim, &InlineOptions::default())
}

pub fn lower_inline_event_lines_with_options<'a>(
    events: impl IntoIterator<Item = (Event<'a>, Range<usize>)>,
    line_ranges: &[Range<usize>],
    dim: bool,
    options: &InlineOptions,
) -> Vec<Vec<InlineSpan>> {
    let mut styles = vec![InlineStyle {
        dim,
        ..Default::default()
    }];
    let mut out = vec![Vec::new(); line_ranges.len()];
    let mut link_stack = Vec::new();

    for (event, range) in events {
        match event {
            Event::Start(tag) => {
                let line_index = line_index_for_event(line_ranges, &range);
                if let Some(link) = pending_link(&tag, options) {
                    link_stack.push((link, line_index));
                }
                push_tag_style(&mut styles, tag);
            }
            Event::End(TagEnd::Link) => {
                if styles.len() > 1 {
                    styles.pop();
                }
                if let Some((link, line_index)) = link_stack.pop() {
                    if link.emit_suffix {
                        if let Some(line_index) =
                            line_index.or_else(|| line_index_for_event(line_ranges, &range))
                        {
                            push_link_suffix(
                                &mut out[line_index],
                                &link.destination,
                                *styles.last().unwrap(),
                                options,
                            );
                        }
                    }
                }
            }
            Event::End(_) if styles.len() > 1 => {
                styles.pop();
            }
            event => {
                if let Some(line_index) = line_index_for_event(line_ranges, &range) {
                    lower_inline_event(
                        event,
                        &mut out[line_index],
                        &styles,
                        link_stack.last().and_then(|(link, _)| link.action.as_ref()),
                        options,
                    );
                    if let Some((_, link_line_index)) = link_stack.last_mut() {
                        *link_line_index = Some(line_index);
                    }
                }
            }
        }
    }

    out
}

fn line_index_for_event(line_ranges: &[Range<usize>], range: &Range<usize>) -> Option<usize> {
    line_ranges
        .iter()
        .position(|line| range.start >= line.start && range.start < line.end)
}

fn lower_inline_fragment_events<'a>(
    source: &str,
    events: impl IntoIterator<Item = (Event<'a>, Range<usize>)>,
    dim: bool,
    options: &InlineOptions,
) -> Vec<InlineSpan> {
    let mut styles = vec![InlineStyle {
        dim,
        ..Default::default()
    }];
    let mut pending_prefixes = Vec::new();
    let mut out = Vec::new();
    let mut link_stack = Vec::new();

    for (event, range) in events {
        match event {
            Event::Start(tag) => {
                flush_fragment_prefixes(
                    source,
                    &mut pending_prefixes,
                    range.start,
                    &mut out,
                    *styles.last().unwrap(),
                );
                if let Some(link) = pending_link(&tag, options) {
                    link_stack.push(link);
                }
                if fragment_tag_preserves_source_prefix(&tag) {
                    pending_prefixes.push(PendingPrefix {
                        start: range.start,
                        end: range.end,
                    });
                }
                push_tag_style(&mut styles, tag);
            }
            Event::End(TagEnd::Link) => {
                flush_fragment_prefixes(
                    source,
                    &mut pending_prefixes,
                    range.end,
                    &mut out,
                    *styles.last().unwrap(),
                );
                if styles.len() > 1 {
                    styles.pop();
                }
                if let Some(link) = link_stack.pop() {
                    if link.emit_suffix {
                        push_link_suffix(
                            &mut out,
                            &link.destination,
                            *styles.last().unwrap(),
                            options,
                        );
                    }
                }
            }
            Event::End(_) => {
                flush_fragment_prefixes(
                    source,
                    &mut pending_prefixes,
                    range.end,
                    &mut out,
                    *styles.last().unwrap(),
                );
                if styles.len() > 1 {
                    styles.pop();
                }
            }
            event => {
                flush_fragment_prefixes(
                    source,
                    &mut pending_prefixes,
                    range.start,
                    &mut out,
                    *styles.last().unwrap(),
                );
                lower_inline_event(
                    event,
                    &mut out,
                    &styles,
                    link_stack.last().and_then(|link| link.action.as_ref()),
                    options,
                );
            }
        }
    }

    out
}

#[derive(Clone, Copy)]
struct PendingPrefix {
    start: usize,
    end: usize,
}

fn flush_fragment_prefixes(
    source: &str,
    pending: &mut Vec<PendingPrefix>,
    up_to: usize,
    out: &mut Vec<InlineSpan>,
    style: InlineStyle,
) {
    let mut i = 0;
    while i < pending.len() {
        let prefix = pending[i];
        let end = up_to.min(prefix.end);
        if prefix.start < end {
            let text = smelt_buffer::text::slice(source, prefix.start..end);
            push_inline_span(out, text, style);
            pending.remove(i);
        } else if prefix.end <= up_to {
            pending.remove(i);
        } else {
            i += 1;
        }
    }
}

fn fragment_tag_preserves_source_prefix(tag: &Tag<'_>) -> bool {
    matches!(tag, Tag::Heading { .. } | Tag::BlockQuote(_) | Tag::Item)
}

struct PendingLink {
    destination: String,
    action: Option<SpanAction>,
    emit_suffix: bool,
}

fn pending_link(tag: &Tag<'_>, options: &InlineOptions) -> Option<PendingLink> {
    match tag {
        Tag::Link {
            link_type,
            dest_url,
            ..
        } => Some(PendingLink {
            destination: dest_url.to_string(),
            action: action_for_destination(dest_url, &options.file_icons),
            emit_suffix: !matches!(link_type, LinkType::Autolink | LinkType::Email),
        }),
        _ => None,
    }
}

fn push_link_suffix(
    out: &mut Vec<InlineSpan>,
    destination: &str,
    style: InlineStyle,
    options: &InlineOptions,
) {
    push_inline_span(out, " (", style);
    push_actionable_link_span(out, destination, link_style(style), options);
    push_inline_span(out, ")", style);
}

fn link_style(style: InlineStyle) -> InlineStyle {
    InlineStyle {
        underline: true,
        group: Some(intern("SmeltLink")),
        ..style
    }
}

fn file_reference_style(style: InlineStyle) -> InlineStyle {
    InlineStyle {
        group: Some(intern("SmeltLink")),
        ..style
    }
}

fn inline_file_icon(text: &str, options: &FileIconOptions) -> Option<FileIcon> {
    let path = inline_file_reference(text, options)?.path;
    file_icons::lookup_path(&path, options)
}

fn push_actionable_link_span(
    out: &mut Vec<InlineSpan>,
    text: &str,
    style: InlineStyle,
    options: &InlineOptions,
) {
    if let Some(action) = action_for_destination(text, &options.file_icons) {
        push_inline_span_meta(out, text, style, SpanMeta::action(action));
    } else {
        push_inline_span(out, text, style);
    }
}

fn lower_inline_event(
    event: Event<'_>,
    out: &mut Vec<InlineSpan>,
    styles: &[InlineStyle],
    link_action: Option<&SpanAction>,
    options: &InlineOptions,
) {
    match event {
        Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
            let style = *styles.last().unwrap();
            if let Some(action) = link_action {
                push_inline_span_meta(out, text.as_ref(), style, SpanMeta::action(action.clone()));
            } else if style.group == Some(intern("SmeltLink")) {
                push_actionable_link_span(out, text.as_ref(), style, options);
            } else {
                push_inline_span(out, text.as_ref(), style);
            }
        }
        Event::Code(text) => {
            let text = text.as_ref();
            let code_style = InlineStyle {
                group: Some(intern("SmeltAccent")),
                ..*styles.last().unwrap()
            };
            if let Some(action) = link_action {
                push_inline_span_meta(out, text, code_style, SpanMeta::action(action.clone()));
                return;
            }
            if let Some(icon) = inline_file_icon(text, &options.file_icons) {
                let mut icon_style = code_style;
                if let Some(group) = icon.group {
                    icon_style.group = Some(group);
                }
                let icon_text = format!("{} ", icon.icon);
                push_inline_span_meta(out, &icon_text, icon_style, SpanMeta::unselectable());
            }
            if let Some(file) = inline_file_reference(text, &options.file_icons) {
                push_inline_span_meta(
                    out,
                    text,
                    file_reference_style(*styles.last().unwrap()),
                    SpanMeta::action(SpanAction::OpenFile {
                        path: file.path,
                        line: file.line,
                        col: file.col,
                    }),
                );
            } else if let Some(action) = url_action(text) {
                push_inline_span_meta(
                    out,
                    text,
                    link_style(*styles.last().unwrap()),
                    SpanMeta::action(action),
                );
            } else {
                push_inline_span(out, text, code_style);
            }
        }
        Event::SoftBreak | Event::HardBreak => {
            push_inline_span(out, " ", *styles.last().unwrap());
        }
        Event::TaskListMarker(checked) => {
            push_inline_span(
                out,
                if checked { "[x] " } else { "[ ] " },
                *styles.last().unwrap(),
            );
        }
        Event::FootnoteReference(text) => {
            push_inline_span(out, text.as_ref(), *styles.last().unwrap());
        }
        Event::Rule => {
            push_inline_span(out, "---", *styles.last().unwrap());
        }
        _ => {}
    }
}

fn push_tag_style(styles: &mut Vec<InlineStyle>, tag: Tag<'_>) {
    let style = *styles.last().unwrap();
    let next = match tag {
        Tag::Emphasis => InlineStyle {
            italic: true,
            ..style
        },
        Tag::Strong => InlineStyle {
            bold: true,
            ..style
        },
        Tag::Strikethrough => InlineStyle {
            crossedout: true,
            ..style
        },
        Tag::Link {
            link_type: LinkType::Autolink | LinkType::Email,
            ..
        } => link_style(style),
        _ => style,
    };
    styles.push(next);
}

fn push_inline_span(out: &mut Vec<InlineSpan>, text: &str, style: InlineStyle) {
    push_inline_span_meta(out, text, style, SpanMeta::default());
}

fn push_inline_span_meta(
    out: &mut Vec<InlineSpan>,
    text: &str,
    style: InlineStyle,
    meta: SpanMeta,
) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out
        .last_mut()
        .filter(|span| span.style == style && span.meta == meta)
    {
        last.text.push_str(text);
    } else {
        out.push(InlineSpan {
            text: text.to_string(),
            style,
            meta,
        });
    }
}

pub fn wrap_inline_spans(spans: &[InlineSpan], max_cols: usize) -> Vec<Vec<InlineSpan>> {
    let line = InlineLine::new(
        spans
            .iter()
            .map(|span| {
                InlineRun::new(
                    span.text.clone(),
                    (span.style, span.meta.clone()),
                    BreakPolicy::Normal,
                )
            })
            .collect(),
    );
    line.wrap_ranges(max_cols)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|run| InlineSpan {
                    text: run.text,
                    style: run.meta.0,
                    meta: run.meta.1,
                })
                .collect()
        })
        .collect()
}

pub fn emit_inline_spans(out: &mut LineBuilder, spans: &[InlineSpan]) {
    use crate::style::Style;

    for span in spans {
        out.push(
            span.style.group,
            Style {
                bold: span.style.bold,
                italic: span.style.italic,
                dim: span.style.dim,
                underline: span.style.underline,
                crossedout: span.style.crossedout,
                ..Default::default()
            },
        );
        out.print_with_meta(&span.text, span.meta.clone());
        out.pop_style();
    }
}

pub fn inline_spans_width(spans: &[InlineSpan]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.text.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::super::super::builder::test_util::render_test;
    use super::*;
    use crate::content::BoxContext;
    use std::path::{Path, PathBuf};

    fn file_icon_options(enabled: bool, base_dir: Option<PathBuf>) -> InlineOptions {
        InlineOptions {
            file_icons: FileIconOptions::new(enabled, false, false, base_dir),
        }
    }

    fn expected_icon_text(path: &Path, options: &InlineOptions) -> String {
        format!(
            "{} ",
            file_icons::lookup_path(path, &options.file_icons)
                .unwrap()
                .icon
        )
    }

    /// Parse `text` into styled inline spans and return a compact
    /// `Vec<(tag, text)>` representation.
    /// Tags: "plain", "bold", "italic", "bi" (bold+italic), "code",
    /// "strike", "link". Adjacent spans with the same style are merged.
    fn parse(text: &str) -> Vec<(&'static str, String)> {
        parse_inline_spans(text, false)
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (tag_for(&s.style), s.text))
            .collect()
    }

    fn tag_for(style: &InlineStyle) -> &'static str {
        let is_code = style.group == Some(intern("SmeltAccent"));
        let is_link = style.group == Some(intern("SmeltLink"));
        match (
            style.bold,
            style.italic,
            style.underline,
            style.crossedout,
            is_code,
            is_link,
        ) {
            (false, false, false, false, false, false) => "plain",
            (true, false, false, false, false, false) => "bold",
            (false, true, false, false, false, false) => "italic",
            (true, true, false, false, false, false) => "bi",
            (false, false, false, true, false, false) => "strike",
            (false, false, false, false, true, false) => "code",
            (true, false, false, false, true, false) => "bold+code",
            (false, true, false, false, true, false) => "italic+code",
            (true, true, false, false, true, false) => "bi+code",
            (false, false, false, false, false, true) => "file",
            (false, false, true, false, false, true) => "link",
            _ => "mixed",
        }
    }

    // Tag shorthands.
    fn p(s: &str) -> (&'static str, String) {
        ("plain", s.into())
    }
    fn b(s: &str) -> (&'static str, String) {
        ("bold", s.into())
    }
    fn i(s: &str) -> (&'static str, String) {
        ("italic", s.into())
    }
    fn bi(s: &str) -> (&'static str, String) {
        ("bi", s.into())
    }
    fn c(s: &str) -> (&'static str, String) {
        ("code", s.into())
    }
    fn s(s: &str) -> (&'static str, String) {
        ("strike", s.into())
    }
    fn l(s: &str) -> (&'static str, String) {
        ("link", s.into())
    }

    #[test]
    fn inline_code_existing_file_gets_nonselectable_icon() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let path = file.to_string_lossy();
        let options = file_icon_options(true, Some(dir.path().to_path_buf()));
        let spans = parse_inline_spans_with_options(&format!("`{path}`"), false, &options);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, expected_icon_text(&file, &options));
        assert!(!spans[0].meta.selectable);
        assert_eq!(spans[1].text, path);
        assert!(spans[1].meta.selectable);
        assert_eq!(
            spans[1].meta.action,
            Some(SpanAction::OpenFile {
                path: file.clone(),
                line: None,
                col: None,
            })
        );
        assert_eq!(spans[1].style.group, Some(intern("SmeltLink")));
        assert!(!spans[1].style.underline);
    }

    #[test]
    fn inline_code_workspace_relative_file_gets_icon() {
        let cwd = std::env::current_dir().unwrap();
        let options = file_icon_options(true, Some(cwd));
        let spans = parse_inline_spans_with_options("`Cargo.toml`", false, &options);

        assert_eq!(spans.len(), 2);
        assert_eq!(
            spans[0].text,
            expected_icon_text(Path::new("Cargo.toml"), &options)
        );
        assert!(!spans[0].meta.selectable);
        assert_eq!(spans[1].text, "Cargo.toml");
    }

    #[test]
    fn inline_code_file_icon_allows_line_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.py");
        std::fs::write(&file, "print('hi')\n").unwrap();
        let text = format!("{}:12:3", file.display());
        let options = file_icon_options(true, Some(dir.path().to_path_buf()));
        let spans = parse_inline_spans_with_options(&format!("`{text}`"), false, &options);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, expected_icon_text(&file, &options));
        assert!(!spans[0].meta.selectable);
        assert_eq!(spans[1].text, text);
        assert_eq!(
            spans[1].meta.action,
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(12),
                col: Some(3),
            })
        );
    }

    #[test]
    fn inline_code_file_icons_are_setting_gated() {
        let cwd = std::env::current_dir().unwrap();
        let options = file_icon_options(false, Some(cwd));
        let spans = parse_inline_spans_with_options("`Cargo.toml`", false, &options);

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Cargo.toml");
    }

    // ── Plain ──────────────────────────────────────────────────────────

    #[test]
    fn plain_text() {
        assert_eq!(parse("hello world"), vec![p("hello world")]);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse(""), vec![]);
    }

    #[test]
    fn inline_context_preserves_block_markers_as_text() {
        assert_eq!(parse("- item"), vec![p("- item")]);
        assert_eq!(parse("# heading"), vec![p("# heading")]);
    }

    // ── Bold ───────────────────────────────────────────────────────────

    #[test]
    fn bold_star() {
        assert_eq!(parse("**hello**"), vec![b("hello")]);
    }

    #[test]
    fn bold_underscore() {
        assert_eq!(parse("__hello__"), vec![b("hello")]);
    }

    #[test]
    fn bold_within_text() {
        assert_eq!(parse("a **bold** c"), vec![p("a "), b("bold"), p(" c")]);
    }

    // ── Italic ─────────────────────────────────────────────────────────

    #[test]
    fn italic_star() {
        assert_eq!(parse("*hello*"), vec![i("hello")]);
    }

    #[test]
    fn italic_underscore() {
        assert_eq!(parse("_hello_"), vec![i("hello")]);
    }

    #[test]
    fn italic_within_text() {
        assert_eq!(parse("a *word* b"), vec![p("a "), i("word"), p(" b")]);
    }

    // ── Bold + italic (triple delimiters) ──────────────────────────────

    #[test]
    fn bold_italic_star() {
        assert_eq!(parse("***both***"), vec![bi("both")]);
    }

    #[test]
    fn bold_italic_underscore() {
        assert_eq!(parse("___both___"), vec![bi("both")]);
    }

    // ── Inline code ────────────────────────────────────────────────────

    #[test]
    fn inline_code() {
        assert_eq!(parse("`foo`"), vec![c("foo")]);
    }

    #[test]
    fn inline_code_with_stars_inside() {
        // Stars inside backticks are literal.
        assert_eq!(parse("`*not bold*`"), vec![c("*not bold*")]);
    }

    #[test]
    fn markdown_link_appends_styled_destination() {
        assert_eq!(
            parse("[Google](https://www.google.com)"),
            vec![p("Google"), p(" ("), l("https://www.google.com"), p(")")]
        );
    }

    #[test]
    fn markdown_autolink_styles_destination_without_duplicate_suffix() {
        assert_eq!(
            parse("<https://example.com>"),
            vec![l("https://example.com")]
        );
    }

    #[test]
    fn markdown_link_keeps_code_distinct_from_destination() {
        assert_eq!(
            parse("[`Google`](https://www.google.com)"),
            vec![c("Google"), p(" ("), l("https://www.google.com"), p(")")]
        );

        let spans = parse_inline_spans("[`Google`](https://www.google.com)", false);
        assert_eq!(
            spans[0].meta.action,
            Some(SpanAction::OpenUrl("https://www.google.com".into()))
        );
    }

    #[test]
    fn inline_code_url_and_email_are_actionable_links() {
        let url = parse_inline_spans("`https://example.test/path`", false);
        assert_eq!(url.len(), 1);
        assert_eq!(tag_for(&url[0].style), "link");
        assert_eq!(
            url[0].meta.action,
            Some(SpanAction::OpenUrl("https://example.test/path".into()))
        );

        let email = parse_inline_spans("`dev@example.test`", false);
        assert_eq!(email.len(), 1);
        assert_eq!(tag_for(&email[0].style), "link");
        assert_eq!(
            email[0].meta.action,
            Some(SpanAction::OpenUrl("mailto:dev@example.test".into()))
        );
    }

    #[test]
    fn markdown_link_destination_gets_action_metadata() {
        let spans = parse_inline_spans("[site](https://example.test)", false);
        let text = spans
            .iter()
            .find(|span| span.text == "site")
            .expect("link text span");
        assert_eq!(tag_for(&text.style), "plain");
        assert_eq!(
            text.meta.action,
            Some(SpanAction::OpenUrl("https://example.test".into()))
        );

        let destination = spans
            .iter()
            .find(|span| span.text == "https://example.test")
            .expect("link destination span");
        assert_eq!(tag_for(&destination.style), "link");
        assert_eq!(
            destination.meta.action,
            Some(SpanAction::OpenUrl("https://example.test".into()))
        );
    }

    #[test]
    fn markdown_local_file_destination_gets_file_action() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let options = file_icon_options(false, Some(dir.path().to_path_buf()));
        let spans = parse_inline_spans_with_options("[source](lib.rs:4)", false, &options);
        let text = spans
            .iter()
            .find(|span| span.text == "source")
            .expect("file link text span");
        assert_eq!(tag_for(&text.style), "plain");
        assert_eq!(
            text.meta.action,
            Some(SpanAction::OpenFile {
                path: file.clone(),
                line: Some(4),
                col: None,
            })
        );

        let link = spans
            .iter()
            .find(|span| span.text == "lib.rs:4")
            .expect("file destination span");
        assert_eq!(tag_for(&link.style), "link");
        assert_eq!(
            link.meta.action,
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(4),
                col: None,
            })
        );
    }

    #[test]
    fn markdown_file_url_destination_gets_file_action() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("has space.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let destination = url::Url::from_file_path(&file).unwrap().to_string();
        let spans = parse_inline_spans(&format!("[source]({destination}:8:2)"), false);
        let text = spans
            .iter()
            .find(|span| span.text == "source")
            .expect("file URL link text span");
        assert_eq!(
            text.meta.action,
            Some(SpanAction::OpenFile {
                path: file,
                line: Some(8),
                col: Some(2),
            })
        );
    }

    #[test]
    fn inline_code_with_underscores_inside() {
        assert_eq!(parse("`_not italic_`"), vec![c("_not italic_")]);
    }

    #[test]
    fn inline_code_around_text() {
        assert_eq!(
            parse("call `foo()` please"),
            vec![p("call "), c("foo()"), p(" please")]
        );
    }

    // ── Strikethrough ──────────────────────────────────────────────────

    #[test]
    fn strikethrough_basic() {
        assert_eq!(parse("~~gone~~"), vec![s("gone")]);
    }

    // ── Intraword underscores (CommonMark: NOT emphasis) ──────────────

    #[test]
    fn intraword_underscore_identifier() {
        // `snake_case_variable` - underscores are part of the identifier.
        assert_eq!(parse("snake_case_variable"), vec![p("snake_case_variable")]);
    }

    #[test]
    fn intraword_underscore_in_url() {
        assert_eq!(
            parse("https://example.com/foo_bar_baz"),
            vec![p("https://example.com/foo_bar_baz")]
        );
    }

    #[test]
    fn intraword_underscore_between_letters() {
        assert_eq!(parse("foo_bar"), vec![p("foo_bar")]);
    }

    // ── Unclosed delimiters (should stay literal) ─────────────────────

    #[test]
    fn unclosed_bold_stays_literal() {
        assert_eq!(parse("**text"), vec![p("**text")]);
    }

    #[test]
    fn unclosed_italic_stays_literal() {
        assert_eq!(parse("*text"), vec![p("*text")]);
    }

    #[test]
    fn unclosed_code_stays_literal() {
        assert_eq!(parse("`unclosed"), vec![p("`unclosed")]);
    }

    /// CommonMark parses the trailing single `*` as an italic delimiter and
    /// leaves the unmatched leading `*` literal.
    #[test]
    fn odd_star_count_does_not_invert_emphasis() {
        assert_eq!(parse("**text*"), vec![p("*"), i("text")]);
    }

    #[test]
    fn odd_star_count_trailing_double() {
        assert_eq!(parse("*text**"), vec![i("text"), p("*")]);
    }

    // ── Nested emphasis (CommonMark supports this) ────────────────────

    #[test]
    fn bold_containing_italic() {
        // `**bold *italic* bold**` - inner italic must render inside bold.
        assert_eq!(
            parse("**bold *it* bold**"),
            vec![b("bold "), bi("it"), b(" bold")]
        );
    }

    #[test]
    fn italic_containing_bold() {
        assert_eq!(
            parse("*it **bold** it*"),
            vec![i("it "), bi("bold"), i(" it")]
        );
    }

    #[test]
    fn bold_containing_code() {
        // Code span nested inside bold inherits the outer bold, so the
        // inner span carries both attributes at once.
        assert_eq!(
            parse("**call `foo()` now**"),
            vec![b("call "), ("bold+code", "foo()".into()), b(" now")]
        );
    }

    // ── Precedence: code > emphasis ───────────────────────────────────

    #[test]
    fn code_inside_italic() {
        // `*a `code` b*` - italic wrapping, code inside. The inner code
        // span inherits italic, so it's italic+code.
        assert_eq!(
            parse("*a `code` b*"),
            vec![i("a "), ("italic+code", "code".into()), i(" b")]
        );
    }

    #[test]
    fn code_containing_italic_stars() {
        // The `*` inside a code span is literal.
        assert_eq!(
            parse("before `*x*` after"),
            vec![p("before "), c("*x*"), p(" after")]
        );
    }

    // ── Multiple runs on one line ─────────────────────────────────────

    #[test]
    fn bold_then_italic() {
        assert_eq!(parse("**a** and *b*"), vec![b("a"), p(" and "), i("b")]);
    }

    #[test]
    fn adjacent_bolds() {
        assert_eq!(parse("**a** **b**"), vec![b("a"), p(" "), b("b")]);
    }

    // ── Asterisk as literal ───────────────────────────────────────────

    #[test]
    fn asterisk_as_multiplication() {
        // `a * b` - stars with whitespace on both sides, not emphasis.
        assert_eq!(parse("a * b = c"), vec![p("a * b = c")]);
    }

    #[test]
    fn trailing_lone_star() {
        assert_eq!(parse("note*"), vec![p("note*")]);
    }

    #[test]
    fn star_right_after_word() {
        assert_eq!(parse("footnote*"), vec![p("footnote*")]);
    }

    // ── Stress: edge cases the spec cares about ──────────────────────

    #[test]
    fn space_before_closing_delim_rejects_emphasis() {
        // `**text **` - close preceded by space is NOT right-flanking.
        assert_eq!(parse("**text **"), vec![p("**text **")]);
    }

    #[test]
    fn space_after_opening_delim_rejects_emphasis() {
        // `** text**` - open followed by space is NOT left-flanking.
        assert_eq!(parse("** text**"), vec![p("** text**")]);
    }

    #[test]
    fn four_star_run_is_literal() {
        assert_eq!(parse("****text****"), vec![b("text")]);
    }

    #[test]
    fn deeply_nested_bold_italic_code() {
        // `**outer *inner `code` inner* outer**`
        assert_eq!(
            parse("**a *b `c` d* e**"),
            vec![
                b("a "),
                bi("b "),
                ("bi+code", "c".into()),
                bi(" d"),
                b(" e"),
            ]
        );
    }

    #[test]
    fn bold_italic_containing_plain_text() {
        assert_eq!(parse("***a b c***"), vec![bi("a b c")]);
    }

    #[test]
    fn two_italic_runs_separated_by_text() {
        assert_eq!(
            parse("start *a* mid *b* end"),
            vec![p("start "), i("a"), p(" mid "), i("b"), p(" end"),]
        );
    }

    #[test]
    fn mixed_underscore_and_star_dont_match() {
        // `*foo_` - `*` opener, `_` is just a literal char, not a closer.
        assert_eq!(parse("*foo_"), vec![p("*foo_")]);
    }

    #[test]
    fn underscore_surrounded_by_non_alnum_can_italic() {
        // `(_foo_)` - `_` is not intraword here because `(` and `)` are
        // not alphanumeric. CommonMark permits this as italic.
        assert_eq!(parse("(_foo_)"), vec![p("("), i("foo"), p(")")]);
    }

    #[test]
    fn star_can_italic_intraword() {
        // Unlike `_`, `*` does not have the intraword restriction.
        assert_eq!(parse("foo*bar*baz"), vec![p("foo"), i("bar"), p("baz")]);
    }

    #[test]
    fn code_with_backtick_literal() {
        // A backtick inside a code span closes it - our single-backtick
        // parser can't represent literal backticks inside a code span.
        // `` `a`b` `` → code("a") + plain("b`").
        assert_eq!(parse("`a`b`"), vec![c("a"), p("b`")]);
    }

    #[test]
    fn strip_markers_matches_parse_for_nested() {
        // The visible width used by wrapping code must match the text
        // that the parser actually emits.
        let text = "**bold *it* bold**";
        let stripped = strip_markdown_markers(text);
        assert_eq!(stripped, "bold it bold");
        // And matches what the inline span parser emits:
        let emitted: String = parse(text).into_iter().map(|(_, t)| t).collect();
        assert_eq!(emitted, stripped);
    }

    #[test]
    fn strip_markers_handles_intraword_underscore() {
        // Must not strip `_` that are intraword - they're part of the
        // identifier, not emphasis markers.
        assert_eq!(
            strip_markdown_markers("call foo_bar_baz() now"),
            "call foo_bar_baz() now"
        );
    }

    #[test]
    fn strip_markers_matches_parse_for_unclosed_bold() {
        // Keep stripping in lockstep with pulldown-cmark inline parsing.
        assert_eq!(strip_markdown_markers("**text*"), "*text");
    }

    fn render_code_block(
        out: &mut crate::content::builder::LineBuilder,
        lines: &[&str],
        lang: &str,
        width: usize,
        dim: bool,
        bctx: Option<&BoxContext>,
        fence: bool,
    ) -> u16 {
        let block = crate::content::code_block::parse_code_block(lines, lang);
        super::super::syntax::render_code_block(out, &block, width, dim, bctx, fence)
    }

    /// Source-text for fenced code blocks stores raw code for contained copies
    /// and fenced markdown as external source for transcript-spanning copies.
    #[test]
    fn render_code_block_with_fence_attaches_source_text_per_line() {
        let lines = ["let x = 1;", "let y = 2;", "let z = 3;"];
        let block = render_test(80, |sink| {
            render_code_block(sink, &lines, "rust", 80, false, None, true);
        });
        assert_eq!(block.lines.len(), 3);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("let x = 1;"));
        assert_eq!(
            block.lines[0].external_source_text.as_deref(),
            Some("```rust\nlet x = 1;")
        );
        assert_eq!(block.lines[1].source_text.as_deref(), Some("let y = 2;"));
        assert_eq!(
            block.lines[1].external_source_text.as_deref(),
            Some("let y = 2;")
        );
        assert_eq!(block.lines[2].source_text.as_deref(), Some("let z = 3;"));
        assert_eq!(
            block.lines[2].external_source_text.as_deref(),
            Some("let z = 3;\n```")
        );
    }

    #[test]
    fn render_code_block_single_line_wraps_with_both_fences() {
        let block = render_test(80, |sink| {
            render_code_block(sink, &["let x = 1;"], "rust", 80, false, None, true);
        });
        assert_eq!(block.lines.len(), 1);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("let x = 1;"));
        assert_eq!(
            block.lines[0].external_source_text.as_deref(),
            Some("```rust\nlet x = 1;\n```")
        );
    }

    #[test]
    fn render_code_block_without_fence_sets_raw_source_per_line() {
        // Block::CodeLine streaming path: no fences, but each line
        // still gets its raw source so partial selections preserve it.
        let block = render_test(80, |sink| {
            render_code_block(sink, &["let x = 1;"], "rust", 80, false, None, false);
        });
        assert_eq!(block.lines.len(), 1);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("let x = 1;"));
    }

    // ── parse_inline_spans / wrap_inline_spans / emit_inline_spans / width ─

    fn rows_to_text(rows: &[Vec<InlineSpan>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn parse_inline_spans_flattens_bold_and_code_with_styles() {
        let spans = parse_inline_spans("**a** `b`", false);
        let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["a", " ", "b"]);
        assert!(spans[0].style.bold);
        assert!(!spans[0].style.italic);
        assert!(spans[2].style.group.is_some());
    }

    #[test]
    fn parse_inline_spans_propagates_dim_flag_to_every_span() {
        let spans = parse_inline_spans("**a** *b*", true);
        assert!(!spans.is_empty());
        for s in &spans {
            assert!(s.style.dim);
        }
    }

    #[test]
    fn parse_inline_spans_skips_empty_text_nodes() {
        // Adjacent delimiters can produce empty Text nodes internally;
        // they must not surface as zero-width spans.
        let spans = parse_inline_spans("**a**b", false);
        for s in &spans {
            assert!(!s.text.is_empty());
        }
    }

    #[test]
    fn inline_spans_width_sums_unicode_widths() {
        let spans = vec![
            InlineSpan {
                text: "ab".into(),
                style: InlineStyle::default(),
                meta: SpanMeta::default(),
            },
            InlineSpan {
                text: "cd".into(),
                style: InlineStyle::default(),
                meta: SpanMeta::default(),
            },
        ];
        assert_eq!(inline_spans_width(&spans), 4);
    }

    #[test]
    fn wrap_inline_spans_zero_max_cols_returns_input_as_one_row() {
        let spans = vec![InlineSpan {
            text: "hello world".into(),
            style: InlineStyle::default(),
            meta: SpanMeta::default(),
        }];
        let rows = wrap_inline_spans(&spans, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[0][0].text, "hello world");
    }

    #[test]
    fn wrap_inline_spans_empty_input_returns_one_row() {
        let rows = wrap_inline_spans(&[], 10);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_empty());
    }

    #[test]
    fn wrap_inline_spans_wraps_on_word_boundaries() {
        let spans = parse_inline_spans("alpha beta gamma delta", false);
        let rows = wrap_inline_spans(&spans, 12);
        let texts = rows_to_text(&rows);
        let joined: String = texts.join("|");
        assert!(rows.len() >= 2, "expected wrap; got rows={texts:?}");
        // No row exceeds max_cols by visual width.
        for row in &rows {
            let width: usize = row.iter().map(|s| s.text.as_str().width()).sum();
            assert!(width <= 12, "row too wide: {row:?}");
        }
        // Round-trip: concatenation preserves every char (modulo trailing wrap space).
        assert!(joined.contains("alpha"));
        assert!(joined.contains("delta"));
    }

    #[test]
    fn wrap_inline_spans_breaks_oversized_word_by_char() {
        let spans = vec![InlineSpan {
            text: "abcdefghij".into(),
            style: InlineStyle::default(),
            meta: SpanMeta::default(),
        }];
        let rows = wrap_inline_spans(&spans, 3);
        assert!(rows.len() >= 3);
        let concat: String = rows
            .iter()
            .flat_map(|r| r.iter().map(|s| s.text.as_str()))
            .collect();
        assert_eq!(concat, "abcdefghij");
    }

    #[test]
    fn wrap_inline_spans_preserves_style_across_wrap_breaks() {
        let spans = parse_inline_spans("**aaaa bbbb cccc**", false);
        let rows = wrap_inline_spans(&spans, 6);
        assert!(rows.len() >= 2);
        for row in &rows {
            for span in row {
                assert!(span.style.bold);
            }
        }
    }

    #[test]
    fn emit_inline_spans_renders_styled_text_into_buffer() {
        let spans = parse_inline_spans("**bold** plain", false);
        let block = render_test(80, |out| emit_inline_spans(out, &spans));
        assert_eq!(block.lines.len(), 1);
        let line = &block.lines[0];
        assert!(line.text.contains("bold"));
        assert!(line.text.contains("plain"));
        // At least one span should be bold.
        assert!(line.spans.iter().any(|s| s.style.bold));
    }

    // ── min_visual_width (private; reached via render_markdown_table) ─────
    // We can't call it directly without exposing it, but we can verify the
    // table renderer behaves consistently when min width forces stacking.

    // ── wrap_cell_words via render_markdown_table ─────────────────────────

    #[test]
    fn render_markdown_table_empty_rows_returns_zero() {
        let mut total = 0u16;
        render_test(80, |out| {
            total = render_markdown_table(out, &[], &[], 80, false, None, "");
        });
        assert_eq!(total, 0);
    }

    #[test]
    fn render_markdown_table_zero_columns_returns_zero() {
        let mut total = 0u16;
        let rows: Vec<Vec<String>> = vec![vec![], vec![]];
        render_test(80, |out| {
            total = render_markdown_table(out, &rows, &[], 80, false, None, "");
        });
        assert_eq!(total, 0);
    }

    #[test]
    fn render_markdown_table_basic_two_row_emits_borders_and_separator() {
        let rows = vec![
            vec!["H1".to_string(), "H2".to_string()],
            vec!["a".to_string(), "b".to_string()],
        ];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, None, "");
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // 4 lines: top border, header, separator, data, bottom border = 5.
        assert!(block.lines.len() >= 5);
        // Borders use heavy box characters.
        assert!(joined.contains('┏'));
        assert!(joined.contains('┓'));
        assert!(joined.contains('┗'));
        assert!(joined.contains('┛'));
        assert!(joined.contains('┣'));
        assert!(joined.contains('┫'));
        assert!(joined.contains('┃'));
        assert!(joined.contains("H1"));
        assert!(joined.contains('a'));
    }

    #[test]
    fn render_markdown_table_with_indent_prefixes_each_row() {
        let rows = vec![vec!["H".to_string()], vec!["v".to_string()]];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, None, "  ");
        });
        // Indent prefixes the borders/rows.
        for line in &block.lines {
            if !line.text.is_empty() {
                assert!(
                    line.text.starts_with("  "),
                    "expected indent prefix; got {:?}",
                    line.text
                );
            }
        }
    }

    #[test]
    fn render_markdown_table_dim_passes_through() {
        let rows = vec![vec!["H".to_string()], vec!["v".to_string()]];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, true, None, "");
        });
        // Dim shouldn't crash and should still produce table output.
        assert!(block.lines.len() >= 4);
    }

    fn narrow_bctx(inner_w: usize) -> super::super::super::BoxContext {
        super::super::super::BoxContext {
            left: "",
            right: "",
            group: HlGroup::default(),
            inner_w,
        }
    }

    #[test]
    fn render_markdown_table_shrinks_wide_columns_to_fit_width() {
        // Wide content + narrow BoxContext forces column shrinking; table
        // should still render the box (not stack), with extra visual rows
        // because cell contents wrap.
        let long = "alpha beta gamma delta epsilon zeta eta theta".to_string();
        let rows = vec![vec!["Col".to_string()], vec![long.clone()]];
        let bctx = narrow_bctx(30);
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, Some(&bctx), "");
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains('┏'));
        assert!(joined.contains("alpha"));
        assert!(joined.contains("theta"));
    }

    #[test]
    fn render_markdown_table_stacks_when_content_unfit_at_min_width() {
        // Single huge unbreakable word + extremely narrow viewport forces
        // stacking (min_total > avail).
        let rows = vec![
            vec!["Header".to_string(), "Other".to_string()],
            vec![
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            ],
        ];
        let bctx = narrow_bctx(10);
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, Some(&bctx), "");
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains('┏'));
        assert!(joined.contains("Header"));
        assert!(joined.contains("Other"));
    }

    #[test]
    fn render_markdown_table_stacked_with_multiple_data_rows_separates_by_blank_line() {
        let rows = vec![
            vec!["H1".to_string(), "H2".to_string()],
            vec!["xxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_string(), "y".to_string()],
            vec!["zzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_string(), "w".to_string()],
        ];
        let bctx = narrow_bctx(8);
        let mut total = 0u16;
        render_test(80, |out| {
            total = render_markdown_table(out, &rows, &[], 80, false, Some(&bctx), "");
        });
        assert!(total > 0);
    }

    #[test]
    fn render_markdown_table_stacked_fallback_respects_width_budget() {
        let rows = vec![
            vec![
                "Approach".to_string(),
                "Worth it?".to_string(),
                "Risk".to_string(),
                "Notes".to_string(),
            ],
            vec![
                "Revert pre-pruning and add retry loop".to_string(),
                "Yes fixes cache and matches reference".to_string(),
                "Low".to_string(),
                "Post-compaction token recompute".to_string(),
            ],
        ];
        let block = render_test(24, |out| {
            render_markdown_table(out, &rows, &[], 24, false, None, "");
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains('┏'),
            "expected stacked fallback:\n{joined}"
        );
        for line in &block.lines {
            let width = line.text.width();
            assert!(
                width <= 24,
                "stacked row overflowed: width={width}, line={:?}",
                line.text
            );
        }
    }

    #[test]
    fn render_markdown_table_stacked_breaks_unspaced_values() {
        let rows = vec![
            vec!["Header".to_string(), "Other".to_string()],
            vec![
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            ],
        ];
        let bctx = narrow_bctx(10);
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, Some(&bctx), "");
        });
        for line in &block.lines {
            let width = line.text.width();
            assert!(
                width <= 10,
                "stacked row overflowed: width={width}, line={:?}",
                line.text
            );
        }
    }

    #[test]
    fn render_markdown_table_stacked_wraps_parsed_inline_spans() {
        let rows = vec![
            vec!["Header".to_string()],
            vec!["**abcdefghijklmnop**".to_string()],
        ];
        let bctx = narrow_bctx(8);
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, Some(&bctx), "");
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("**"), "raw markers leaked:\n{joined}");
        assert!(joined.contains("abc"));
        assert!(block
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bold && s.text.contains('a')));
        for line in &block.lines {
            let width = line.text.width();
            assert!(
                width <= 8,
                "stacked row overflowed: width={width}, line={:?}",
                line.text
            );
        }
    }

    #[test]
    fn render_markdown_table_inline_markdown_inside_cells_renders_styled() {
        let rows = vec![
            vec!["Name".to_string(), "Kind".to_string()],
            vec!["**bold**".to_string(), "*italic*".to_string()],
        ];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, None, "");
        });
        // The stripped form (without markers) appears in the output, and
        // some span carries the inline style.
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("bold"));
        assert!(joined.contains("italic"));
        assert!(!joined.contains("**bold**"));
        let any_bold = block
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bold);
        let any_italic = block
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.italic);
        assert!(any_bold);
        assert!(any_italic);
    }

    #[test]
    fn render_markdown_table_honors_per_column_alignment() {
        // Generous header widths so per-column padding is visible.
        let rows = vec![
            vec!["LLLL".to_string(), "CCCC".to_string(), "RRRR".to_string()],
            vec!["x".to_string(), "y".to_string(), "z".to_string()],
        ];
        let aligns = [
            ColumnAlignment::Left,
            ColumnAlignment::Center,
            ColumnAlignment::Right,
        ];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &aligns, 80, false, None, "");
        });
        let data_row = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .find(|s| s.contains('x') && s.contains('y') && s.contains('z'))
            .expect("data row");
        assert!(data_row.contains("┃ x    ┃"), "left: {data_row:?}");
        assert!(data_row.contains("┃  y   ┃"), "center: {data_row:?}");
        assert!(data_row.contains("┃    z ┃"), "right: {data_row:?}");
    }

    #[test]
    fn render_markdown_table_borders_carry_no_background() {
        let rows = vec![
            vec!["H".to_string(), "K".to_string()],
            vec!["a".to_string(), "b".to_string()],
        ];
        let block = render_test(80, |out| {
            render_markdown_table(out, &rows, &[], 80, false, None, "");
        });
        // Every span that prints a border glyph must have no bg set.
        for line in &block.lines {
            for span in &line.spans {
                if span.text.chars().any(|c| {
                    matches!(
                        c,
                        '┏' | '┓' | '┗' | '┛' | '┣' | '┫' | '┳' | '┻' | '╋' | '┃' | '━'
                    )
                }) {
                    assert!(
                        span.style.bg.is_none(),
                        "table border span carries bg ({:?})",
                        span.style.bg
                    );
                }
            }
        }
    }
}
