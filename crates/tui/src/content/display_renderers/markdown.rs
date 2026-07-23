use std::{
    cell::RefCell,
    collections::VecDeque,
    hash::{Hash, Hasher},
    sync::Arc,
};

use super::temp_rows::{apply_temp_decoration, emit_buffer_row_clipped};
use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, inline_spans_width, measure_markdown_table_with_options,
    parse_inline_spans_with_options, render_code_block, render_markdown_table_with_options,
    wrap_inline_spans, InlineOptions, InlineSpan, InlineStyle,
};
use smelt_core::content::inline_line::BreakPolicy;
use smelt_core::content::markdown_ir::{
    parse_markdown_with_options, MarkdownLine, MarkdownNode, MarkdownTextKind,
};
use smelt_core::content::{is_markdown_list_item, split_markdown_list_prefix};
use smelt_core::theme::intern;

pub fn render_markdown_inner(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
) -> u16 {
    render_markdown_inner_with_options(
        out,
        content,
        width,
        indent,
        dim,
        bctx,
        &InlineOptions::default(),
    )
}

pub fn render_markdown_inner_with_options(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:markdown");
    let block = parse_markdown_cached(content, inline_options);
    render_markdown_block(
        out,
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_markdown_inner_range_with_options(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    row_start: u16,
    row_count: u16,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let _perf = smelt_perf::perf::begin("render:markdown:range");
    let before = out.line_count();
    let block = parse_markdown_cached(content, inline_options);
    render_markdown_block(
        out,
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        Some(RowClip {
            start: row_start,
            end: row_start.saturating_add(row_count),
        }),
    );
    out.line_count()
        .saturating_sub(before)
        .min(u16::MAX as usize) as u16
}

pub fn measure_markdown_inner_with_options(
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> u16 {
    let block = parse_markdown_cached(content, inline_options);
    measure_markdown_block(block.as_doc(), width, indent, dim, bctx, inline_options)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn markdown_range_has_visible_text_with_options(
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    row_start: u16,
    row_count: u16,
) -> bool {
    if row_count == 0 {
        return false;
    }
    let block = parse_markdown_cached(content, inline_options);
    markdown_block_range_has_visible_text(
        block.as_doc(),
        width,
        indent,
        dim,
        bctx,
        inline_options,
        RowClip {
            start: row_start,
            end: row_start.saturating_add(row_count),
        },
    )
}

const MARKDOWN_PARSE_CACHE_CAP: usize = 128;
const MARKDOWN_PARSE_CACHE_BYTES: usize = 16 * 1024 * 1024;

thread_local! {
    static MARKDOWN_PARSE_CACHE: RefCell<VecDeque<MarkdownParseCacheEntry>> = const { RefCell::new(VecDeque::new()) };
}

#[derive(Clone)]
struct ParsedMarkdown {
    source: Arc<str>,
    nodes: Arc<[MarkdownNode]>,
}

impl ParsedMarkdown {
    fn as_doc(&self) -> MarkdownDoc<'_> {
        MarkdownDoc {
            source: self.source.as_ref(),
            nodes: self.nodes.as_ref(),
        }
    }
}

#[derive(Clone, Copy)]
struct MarkdownDoc<'a> {
    source: &'a str,
    nodes: &'a [MarkdownNode],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MarkdownParseCacheKey {
    content_len: usize,
    content_hash: u64,
    file_icon_options_hash: u64,
}

struct MarkdownParseCacheEntry {
    key: MarkdownParseCacheKey,
    inline_options: InlineOptions,
    parsed: ParsedMarkdown,
}

fn parse_markdown_cached(content: &str, inline_options: &InlineOptions) -> ParsedMarkdown {
    let key = markdown_parse_cache_key(content, inline_options);
    if let Some(parsed) = MARKDOWN_PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let pos = cache.iter().position(|entry| {
            entry.key == key
                && entry.inline_options == *inline_options
                && entry.parsed.source.as_ref() == content
        })?;
        let entry = cache.remove(pos)?;
        let parsed = entry.parsed.clone();
        cache.push_front(entry);
        Some(parsed)
    }) {
        return parsed;
    }

    let block = parse_markdown_with_options(content, inline_options);
    let parsed = ParsedMarkdown {
        source: Arc::from(content),
        nodes: Arc::from(block.nodes.into_boxed_slice()),
    };
    MARKDOWN_PARSE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.push_front(MarkdownParseCacheEntry {
            key,
            inline_options: inline_options.clone(),
            parsed: parsed.clone(),
        });
        while cache.len() > MARKDOWN_PARSE_CACHE_CAP
            || markdown_cache_bytes(&cache) > MARKDOWN_PARSE_CACHE_BYTES
        {
            cache.pop_back();
        }
    });
    parsed
}

fn markdown_cache_bytes(cache: &VecDeque<MarkdownParseCacheEntry>) -> usize {
    cache.iter().map(|entry| entry.parsed.source.len()).sum()
}

fn markdown_parse_cache_key(
    content: &str,
    inline_options: &InlineOptions,
) -> MarkdownParseCacheKey {
    MarkdownParseCacheKey {
        content_len: content.len(),
        content_hash: hash_value(content),
        file_icon_options_hash: hash_value(&inline_options.file_icons),
    }
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy)]
struct RowClip {
    start: u16,
    end: u16,
}

impl RowClip {
    fn contains(self, row: u16) -> bool {
        row >= self.start && row < self.end
    }

    fn intersects(self, start: u16, rows: u16) -> bool {
        let end = start.saturating_add(rows);
        start < self.end && end > self.start
    }
}

fn should_emit(clip: Option<RowClip>, row: u16) -> bool {
    clip.is_none_or(|clip| clip.contains(row))
}

#[allow(clippy::too_many_arguments)]
fn render_markdown_block(
    out: &mut LineBuilder,
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: Option<RowClip>,
) -> u16 {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    let ctx = RenderTextCtx {
        max_cols,
        indent,
        dim,
        bctx,
        inline_options,
        clip,
    };
    let mut state = RenderState::default();

    for node in block.nodes {
        if clip.is_some_and(|clip| state.rows >= clip.end) {
            break;
        }
        match node {
            MarkdownNode::Source { range } => {
                let source = smelt_buffer::text::slice(block.source, range.clone());
                render_source_lines(out, source, MarkdownTextKind::Paragraph, &ctx, &mut state);
            }
            MarkdownNode::Text { lines, kind, .. } => {
                render_text_lines(out, block.source, lines, *kind, &ctx, &mut state);
            }
            MarkdownNode::Code { lang, body, .. } => {
                render_block_gap(out, &mut state, clip);
                let code_lines: Vec<&str> = body
                    .iter()
                    .flat_map(|range| {
                        smelt_buffer::text::slice(block.source, range.clone()).lines()
                    })
                    .collect();
                let code_block = parse_code_block(&code_lines, lang);
                if let Some(clip) = clip {
                    let rows = measure_code_block(&code_block, width) as u16;
                    if clip.intersects(state.rows, rows) {
                        let theme = out.theme().clone();
                        let inherited_style = out.current_style();
                        let mut buf = smelt_core::buffer::Buffer::new(
                            smelt_core::buffer::BufId(0),
                            smelt_core::buffer::BufCreateOpts::default(),
                        );
                        let outcome = {
                            let mut col = LineBuilder::new(&mut buf, &theme, width.max(1) as u16);
                            col.push(None, inherited_style);
                            render_code_block(&mut col, &code_block, width, dim, bctx, true);
                            col.finish()
                        };
                        if outcome.was_wrapped {
                            out.mark_wrapped();
                        }
                        emit_temp_rows(out, &buf, width, state.rows, rows, clip, None);
                    }
                    state.rows = state.rows.saturating_add(rows);
                } else {
                    state.rows += render_code_block(out, &code_block, width, dim, bctx, true);
                }
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                range,
                alignments,
                rows,
            } => {
                render_block_gap(out, &mut state, clip);
                if let Some(clip) = clip {
                    let table_rows = measure_markdown_table_with_options(
                        rows,
                        alignments,
                        width,
                        dim,
                        bctx,
                        indent,
                        inline_options,
                    );
                    if clip.intersects(state.rows, table_rows) {
                        let theme = out.theme().clone();
                        let inherited_style = out.current_style();
                        let mut buf = smelt_core::buffer::Buffer::new(
                            smelt_core::buffer::BufId(0),
                            smelt_core::buffer::BufCreateOpts::default(),
                        );
                        let outcome = {
                            let mut col = LineBuilder::new(&mut buf, &theme, width.max(1) as u16);
                            col.push(None, inherited_style);
                            let start = col.line_count();
                            render_markdown_table_with_options(
                                &mut col,
                                rows,
                                alignments,
                                width,
                                dim,
                                bctx,
                                indent,
                                inline_options,
                            );
                            let source = smelt_buffer::text::slice(block.source, range.clone())
                                .trim_end_matches(['\r', '\n']);
                            col.stamp_copy_group(start, source);
                            col.finish()
                        };
                        if outcome.was_wrapped {
                            out.mark_wrapped();
                        }
                        emit_temp_rows(out, &buf, width, state.rows, table_rows, clip, None);
                    }
                    state.rows = state.rows.saturating_add(table_rows);
                } else {
                    let start = out.line_count();
                    state.rows += render_markdown_table_with_options(
                        out,
                        rows,
                        alignments,
                        width,
                        dim,
                        bctx,
                        indent,
                        inline_options,
                    );
                    let source = smelt_buffer::text::slice(block.source, range.clone())
                        .trim_end_matches(['\r', '\n']);
                    out.stamp_copy_group(start, source);
                }
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                render_block_gap(out, &mut state, clip);
                if should_emit(clip, state.rows) {
                    render_horizontal_rule(out, bctx, indent);
                }
                state.rows = state.rows.saturating_add(1);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }

    state.rows
}

fn emit_temp_rows(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    width: usize,
    base_row: u16,
    rows: u16,
    clip: RowClip,
    style_overlay: Option<(bool, bool)>,
) {
    let start = clip.start.saturating_sub(base_row).min(rows);
    let end = clip.end.saturating_sub(base_row).min(rows);
    for row in start..end {
        apply_temp_decoration(out, buf, row as usize, true);
        emit_buffer_row_clipped(
            buf,
            row,
            width.min(u16::MAX as usize) as u16,
            out,
            style_overlay,
        );
        out.newline();
    }
}

#[allow(clippy::too_many_arguments)]
fn markdown_block_range_has_visible_text(
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
    clip: RowClip,
) -> bool {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    let mut state = FlowState::default();

    for node in block.nodes {
        if state.rows >= clip.end {
            break;
        }
        match node {
            MarkdownNode::Source { range } => {
                let source = smelt_buffer::text::slice(block.source, range.clone());
                if source_lines_range_has_visible_text(
                    source,
                    MarkdownTextKind::Paragraph,
                    max_cols,
                    dim,
                    inline_options,
                    clip,
                    &mut state,
                ) {
                    return true;
                }
            }
            MarkdownNode::Text { lines, kind, .. } => {
                if text_lines_range_has_visible_text(
                    block.source,
                    lines,
                    *kind,
                    max_cols,
                    dim,
                    clip,
                    &mut state,
                ) {
                    return true;
                }
            }
            MarkdownNode::Code { lang, body, .. } => {
                measure_block_gap(&mut state);
                let code_lines: Vec<&str> = body
                    .iter()
                    .flat_map(|range| {
                        smelt_buffer::text::slice(block.source, range.clone()).lines()
                    })
                    .collect();
                let code_block = parse_code_block(&code_lines, lang);
                let rows = measure_code_block(&code_block, width) as u16;
                if clip.intersects(state.rows, rows)
                    && code_lines.iter().any(|line| !line.trim().is_empty())
                {
                    return true;
                }
                state.rows = state.rows.saturating_add(rows);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                alignments, rows, ..
            } => {
                measure_block_gap(&mut state);
                let table_rows = measure_markdown_table_with_options(
                    rows,
                    alignments,
                    width,
                    dim,
                    bctx,
                    indent,
                    inline_options,
                );
                if clip.intersects(state.rows, table_rows)
                    && rows.iter().flatten().any(|cell| !cell.trim().is_empty())
                {
                    return true;
                }
                state.rows = state.rows.saturating_add(table_rows);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                measure_block_gap(&mut state);
                if clip.contains(state.rows) {
                    return true;
                }
                state.rows = state.rows.saturating_add(1);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }

    false
}

fn measure_markdown_block(
    block: MarkdownDoc<'_>,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> u16 {
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(display_width(indent) + 1)
    };
    let mut state = MeasureState::default();

    for node in block.nodes {
        match node {
            MarkdownNode::Source { range } => {
                let source = smelt_buffer::text::slice(block.source, range.clone());
                measure_source_lines(
                    source,
                    MarkdownTextKind::Paragraph,
                    max_cols,
                    dim,
                    inline_options,
                    &mut state,
                );
            }
            MarkdownNode::Text { lines, kind, .. } => {
                measure_text_lines(block.source, lines, *kind, max_cols, dim, &mut state);
            }
            MarkdownNode::Code { lang, body, .. } => {
                measure_block_gap(&mut state);
                let code_lines: Vec<&str> = body
                    .iter()
                    .flat_map(|range| {
                        smelt_buffer::text::slice(block.source, range.clone()).lines()
                    })
                    .collect();
                let code_block = parse_code_block(&code_lines, lang);
                state.rows = state
                    .rows
                    .saturating_add(measure_code_block(&code_block, width) as u16);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                alignments, rows, ..
            } => {
                measure_block_gap(&mut state);
                state.rows = state
                    .rows
                    .saturating_add(measure_markdown_table_with_options(
                        rows,
                        alignments,
                        width,
                        dim,
                        bctx,
                        indent,
                        inline_options,
                    ));
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                measure_block_gap(&mut state);
                state.rows = state.rows.saturating_add(1);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }

    state.rows
}

#[derive(Default)]
struct FlowState {
    rows: u16,
    last_content_was_heading: bool,
    pending_blank: bool,
    prev_was_block: bool,
}

type MeasureState = FlowState;
type RenderState = FlowState;

fn measure_block_gap(state: &mut MeasureState) {
    let mut gap_emitted = false;
    if state.pending_blank {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
        gap_emitted = true;
    }
    if state.rows > 0 && !gap_emitted && !state.last_content_was_heading {
        state.rows = state.rows.saturating_add(1);
    }
}

fn measure_text_gap(state: &mut MeasureState, kind: MarkdownTextKind) -> bool {
    if state.rows == 0 {
        state.pending_blank = false;
        return false;
    }
    if kind == MarkdownTextKind::List && !state.prev_was_block {
        state.pending_blank = false;
        return false;
    }
    let before = state.rows;
    measure_block_gap(state);
    state.rows != before
}

fn measure_source_lines(
    source: &str,
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    inline_options: &InlineOptions,
    state: &mut MeasureState,
) {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = MeasureSourceSink {
        max_cols,
        dim,
        kind,
        inline_options,
    };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
}

fn measure_text_lines(
    source: &str,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    state: &mut MeasureState,
) {
    let mut sink = MeasureIrSink {
        lines,
        max_cols,
        dim,
        kind,
    };
    walk_text_lines(
        lines.len(),
        |i| smelt_buffer::text::slice(source, lines[i].source.clone()),
        |i| is_markdown_list_item(smelt_buffer::text::slice(source, lines[i].source.clone())),
        kind,
        state,
        &mut sink,
    );
}

fn source_lines_range_has_visible_text(
    source: &str,
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    inline_options: &InlineOptions,
    clip: RowClip,
    state: &mut FlowState,
) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = ProbeSourceSink {
        max_cols,
        dim,
        kind,
        inline_options,
        clip,
        found: false,
    };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
    sink.found
}

fn text_lines_range_has_visible_text(
    source: &str,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    max_cols: usize,
    dim: bool,
    clip: RowClip,
    state: &mut FlowState,
) -> bool {
    let mut sink = ProbeIrSink {
        lines,
        max_cols,
        dim,
        kind,
        clip,
        found: false,
    };
    walk_text_lines(
        lines.len(),
        |i| smelt_buffer::text::slice(source, lines[i].source.clone()),
        |i| is_markdown_list_item(smelt_buffer::text::slice(source, lines[i].source.clone())),
        kind,
        state,
        &mut sink,
    );
    sink.found
}

fn render_block_gap(out: &mut LineBuilder, state: &mut RenderState, clip: Option<RowClip>) {
    let mut gap_emitted = false;
    if state.pending_blank {
        if should_emit(clip, state.rows) {
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
        gap_emitted = true;
    }
    if state.rows > 0 && !gap_emitted && !state.last_content_was_heading {
        if should_emit(clip, state.rows) {
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
    }
}

struct RenderTextCtx<'a> {
    max_cols: usize,
    indent: &'a str,
    dim: bool,
    bctx: Option<&'a smelt_core::content::BoxContext>,
    inline_options: &'a InlineOptions,
    clip: Option<RowClip>,
}

fn render_text_gap(
    out: &mut LineBuilder,
    state: &mut RenderState,
    kind: MarkdownTextKind,
    clip: Option<RowClip>,
) -> bool {
    if state.rows == 0 {
        state.pending_blank = false;
        return false;
    }
    if kind == MarkdownTextKind::List && !state.prev_was_block {
        state.pending_blank = false;
        return false;
    }
    let before = state.rows;
    render_block_gap(out, state, clip);
    state.rows != before
}

fn render_source_lines(
    out: &mut LineBuilder,
    source: &str,
    kind: MarkdownTextKind,
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let lines: Vec<&str> = source.lines().collect();
    let mut sink = RenderSourceSink { out, ctx, kind };
    walk_text_lines(
        lines.len(),
        |i| lines[i],
        |i| is_markdown_list_item(lines[i]),
        kind,
        state,
        &mut sink,
    );
}

fn render_text_lines(
    out: &mut LineBuilder,
    source: &str,
    lines: &[MarkdownLine],
    kind: MarkdownTextKind,
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let mut sink = RenderIrSink {
        out,
        ctx,
        lines,
        kind,
    };
    walk_text_lines(
        lines.len(),
        |i| smelt_buffer::text::slice(source, lines[i].source.clone()),
        |i| is_markdown_list_item(smelt_buffer::text::slice(source, lines[i].source.clone())),
        kind,
        state,
        &mut sink,
    );
}

trait TextFlowSink {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool;
    fn blank_line(&mut self, state: &mut FlowState);
    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState);
    fn done(&self, _state: &FlowState) -> bool {
        false
    }
}

fn walk_text_lines<'a>(
    len: usize,
    mut line_at: impl FnMut(usize) -> &'a str,
    mut is_list_item_at: impl FnMut(usize) -> bool,
    kind: MarkdownTextKind,
    state: &mut FlowState,
    sink: &mut impl TextFlowSink,
) {
    let mut started = false;
    let mut i = 0;
    while i < len {
        if sink.done(state) {
            break;
        }
        let line = line_at(i);
        if line.trim().is_empty() {
            let mut next_i = i + 1;
            while next_i < len && line_at(next_i).trim().is_empty() {
                next_i += 1;
            }
            if state.rows > 0
                && !state.last_content_was_heading
                && next_i < len
                && !is_list_item_at(next_i)
            {
                state.pending_blank = true;
            }
            i = next_i;
            continue;
        }

        let mut gap_emitted = false;
        if !started {
            gap_emitted = sink.text_gap(state, kind);
            started = true;
        }
        if state.pending_blank {
            sink.blank_line(state);
            if sink.done(state) {
                break;
            }
            gap_emitted = true;
        }
        if state.prev_was_block && !gap_emitted {
            sink.blank_line(state);
            if sink.done(state) {
                break;
            }
        }
        sink.emit_line(i, line, state);
        state.last_content_was_heading = kind == MarkdownTextKind::Heading;
        state.prev_was_block = false;
        i += 1;
    }
}

struct MeasureSourceSink<'a> {
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    inline_options: &'a InlineOptions,
}

impl TextFlowSink for MeasureSourceSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans = fallback_markdown_line_spans(line, self.kind, self.dim, self.inline_options);
        state.rows = state
            .rows
            .saturating_add(wrap_inline_spans(&spans, self.max_cols).len() as u16);
    }
}

struct MeasureIrSink<'a> {
    lines: &'a [MarkdownLine],
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
}

impl TextFlowSink for MeasureIrSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.dim);
        state.rows = state
            .rows
            .saturating_add(wrap_inline_spans(&spans, self.max_cols).len() as u16);
    }
}

struct ProbeSourceSink<'a> {
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    inline_options: &'a InlineOptions,
    clip: RowClip,
    found: bool,
}

impl TextFlowSink for ProbeSourceSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans = fallback_markdown_line_spans(line, self.kind, self.dim, self.inline_options);
        probe_markdown_line(&spans, self.max_cols, self.clip, state, &mut self.found);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.found || state.rows >= self.clip.end
    }
}

struct ProbeIrSink<'a> {
    lines: &'a [MarkdownLine],
    max_cols: usize,
    dim: bool,
    kind: MarkdownTextKind,
    clip: RowClip,
    found: bool,
}

impl TextFlowSink for ProbeIrSink<'_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        measure_text_gap(state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.dim);
        probe_markdown_line(&spans, self.max_cols, self.clip, state, &mut self.found);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.found || state.rows >= self.clip.end
    }
}

fn probe_markdown_line(
    spans: &[InlineSpan],
    max_cols: usize,
    clip: RowClip,
    state: &mut FlowState,
    found: &mut bool,
) {
    for row_spans in wrap_inline_spans(spans, max_cols) {
        if clip.contains(state.rows) && inline_spans_have_visible_text(&row_spans) {
            *found = true;
        }
        state.rows = state.rows.saturating_add(1);
        if *found || state.rows >= clip.end {
            break;
        }
    }
}

fn inline_spans_have_visible_text(spans: &[InlineSpan]) -> bool {
    spans.iter().any(|span| !span.text.trim().is_empty())
}

struct RenderSourceSink<'a, 'b, 'c> {
    out: &'a mut LineBuilder<'b>,
    ctx: &'a RenderTextCtx<'c>,
    kind: MarkdownTextKind,
}

impl TextFlowSink for RenderSourceSink<'_, '_, '_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        render_text_gap(self.out, state, kind, self.ctx.clip)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        if should_emit(self.ctx.clip, state.rows) {
            self.out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans =
            fallback_markdown_line_spans(line, self.kind, self.ctx.dim, self.ctx.inline_options);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.ctx.clip.is_some_and(|clip| state.rows >= clip.end)
    }
}

struct RenderIrSink<'a, 'b, 'c, 'd> {
    out: &'a mut LineBuilder<'b>,
    ctx: &'a RenderTextCtx<'c>,
    lines: &'d [MarkdownLine],
    kind: MarkdownTextKind,
}

impl TextFlowSink for RenderIrSink<'_, '_, '_, '_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        render_text_gap(self.out, state, kind, self.ctx.clip)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        if should_emit(self.ctx.clip, state.rows) {
            self.out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.ctx.dim);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
    }

    fn done(&self, state: &FlowState) -> bool {
        self.ctx.clip.is_some_and(|clip| state.rows >= clip.end)
    }
}

fn render_markdown_line(
    out: &mut LineBuilder,
    line: &str,
    spans: &[InlineSpan],
    ctx: &RenderTextCtx<'_>,
    state: &mut RenderState,
) {
    let wrapped = wrap_inline_spans(spans, ctx.max_cols);
    if wrapped.len() > 1 {
        out.mark_wrapped();
    }
    for (si, row_spans) in wrapped.iter().enumerate() {
        if should_emit(ctx.clip, state.rows) {
            if si == 0 {
                out.set_source_text(line);
            } else {
                out.mark_soft_wrap_continuation();
            }
            if let Some(b) = ctx.bctx {
                b.print_left(out);
                emit_inline_spans(out, row_spans);
                b.print_right(out, inline_spans_width(row_spans));
            } else {
                out.print(ctx.indent);
                emit_inline_spans(out, row_spans);
            }
            out.newline();
        }
        state.rows = state.rows.saturating_add(1);
        if ctx.clip.is_some_and(|clip| state.rows >= clip.end) {
            break;
        }
    }
}

fn fallback_markdown_line_spans(
    line: &str,
    kind: MarkdownTextKind,
    dim: bool,
    inline_options: &InlineOptions,
) -> Vec<InlineSpan> {
    let trimmed = line.trim_start();
    let body = if kind == MarkdownTextKind::List {
        split_markdown_list_prefix(trimmed).1
    } else {
        trimmed
    };
    markdown_line_spans(
        line,
        &parse_inline_spans_with_options(body, false, inline_options),
        kind,
        dim,
    )
}

fn markdown_line_spans(
    line: &str,
    base_spans: &[InlineSpan],
    kind: MarkdownTextKind,
    dim: bool,
) -> Vec<InlineSpan> {
    let trimmed = line.trim_start();
    let leading_ws = &line[..line.len() - trimmed.len()];
    let mut line_spans = Vec::new();

    if kind == MarkdownTextKind::Heading {
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.bold = true;
            span.style.dim |= dim;
            span.style.group = Some(intern("SmeltHeading"));
            span
        }));
    } else if kind == MarkdownTextKind::BlockQuote {
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.dim = true;
            span.style.italic = true;
            span
        }));
    } else {
        let prefix = if kind == MarkdownTextKind::List {
            split_markdown_list_prefix(trimmed).0
        } else {
            ""
        };
        if !leading_ws.is_empty() {
            line_spans.push(InlineSpan {
                text: leading_ws.to_string(),
                style: InlineStyle {
                    dim,
                    ..Default::default()
                },
                meta: Default::default(),
                break_policy: BreakPolicy::Normal,
            });
        }
        if !prefix.is_empty() {
            line_spans.push(InlineSpan {
                text: prefix.to_string(),
                style: InlineStyle {
                    dim: true,
                    ..Default::default()
                },
                meta: Default::default(),
                break_policy: BreakPolicy::Normal,
            });
        }
        line_spans.extend(base_spans.iter().cloned().map(|mut span| {
            span.style.dim |= dim;
            span
        }));
    }

    line_spans
}

fn render_horizontal_rule(
    out: &mut LineBuilder,
    bctx: Option<&smelt_core::content::BoxContext>,
    indent: &str,
) -> u16 {
    let hr = "─".repeat(3);

    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print(indent);
    }

    out.push_dim();
    out.print_with_meta(&hr, smelt_core::buffer::SpanMeta::copy_as("---"));
    out.pop_style();

    if let Some(b) = bctx {
        b.print_right(out, 3);
    }

    out.newline();
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::builder::test_util::render_test;

    #[test]
    fn markdown_range_matches_full_render_slice() {
        let md = "# Heading\n\nParagraph with enough words to wrap over multiple terminal rows at a narrow width.\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| col | value |\n| --- | --- |\n| a | table cell that also wraps over several rows |\n\nAfter table text.";
        let full = render_test(36, |sink| {
            render_markdown_inner(sink, md, 36, "", false, None);
        });
        let full_rows: Vec<&str> = full.lines.iter().map(|line| line.text.as_str()).collect();

        for start in 0..full_rows.len() {
            for count in [1usize, 4, 8] {
                let range = render_test(36, |sink| {
                    render_markdown_inner_range_with_options(
                        sink,
                        md,
                        36,
                        "",
                        false,
                        None,
                        &InlineOptions::default(),
                        start as u16,
                        count as u16,
                    );
                });
                let range_rows: Vec<&str> =
                    range.lines.iter().map(|line| line.text.as_str()).collect();
                let end = start.saturating_add(count).min(full_rows.len());
                assert_eq!(
                    range_rows,
                    full_rows[start..end],
                    "start={start} count={count}"
                );
            }
        }
    }

    #[test]
    fn markdown_line_spans_use_shared_block_markers() {
        let options = InlineOptions::default();
        let heading = fallback_markdown_line_spans(
            "#not heading",
            MarkdownTextKind::Paragraph,
            false,
            &options,
        );
        assert_ne!(heading[0].style.group, Some(intern("SmeltHeading")));

        let bullet =
            fallback_markdown_line_spans("+ item", MarkdownTextKind::List, false, &options);
        assert_eq!(bullet[0].text, "+ ");
        assert!(bullet[0].style.dim);

        let ordered =
            fallback_markdown_line_spans("12) item", MarkdownTextKind::List, false, &options);
        assert_eq!(ordered[0].text, "12) ");
        assert!(ordered[0].style.dim);
    }

    #[test]
    fn markdown_collapses_leading_blank_run_before_code_block() {
        let md = "\n\nValidation run in the new worktree:\n\n```bash\ncargo test\n```\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert_eq!(rows[0], "Validation run in the new worktree:");
        assert_eq!(rows.iter().filter(|row| row.is_empty()).count(), 1);
        assert!(rows.iter().any(|row| row.contains("cargo test")));
        assert!(!rows.iter().any(|row| row.contains("``")), "rows: {rows:?}");
    }

    #[test]
    fn streamed_table_never_renders_raw_delimiter_row() {
        fn rendered_rows(history: &smelt_core::transcript_model::BlockHistory) -> Vec<String> {
            let mut rows = Vec::new();
            for index in 0..history.len() {
                if let Some(smelt_core::transcript_model::Block::Text { content }) =
                    history.materialized_block_at(index)
                {
                    let block = render_test(80, |sink| {
                        render_markdown_inner(sink, content, 80, "", false, None);
                    });
                    rows.extend(block.lines.into_iter().map(|line| line.text));
                }
            }
            rows
        }

        let mut parser = smelt_core::content::stream_parser::StreamParser::new();
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        let mut previous_row_count = 0;

        for (chunk, completes_table_row) in [
            ("| a | b |", false),
            ("\n|", false),
            ("---", false),
            ("|", false),
            ("---", false),
            ("|", false),
            ("\n|", false),
            (" 1 | 2 |", false),
            ("\n", true),
            ("\n|", false),
            (" 3 |", false),
            (" 4 |", false),
            ("\n", true),
        ] {
            parser.append_streaming_text(&mut transcript.history, chunk);
            let rows = rendered_rows(&transcript.history);
            assert!(
                !rows.iter().any(|row| row.contains("---")),
                "rows: {rows:?}"
            );
            assert!(!rows.iter().any(|row| row.trim() == "|"), "rows: {rows:?}");
            if !completes_table_row {
                assert_eq!(
                    rows.len(),
                    previous_row_count,
                    "chunk {chunk:?} changed rendered row count: {rows:?}"
                );
            }
            previous_row_count = rows.len();
        }
    }

    #[test]
    fn markdown_dim_survives_code_blocks() {
        let md = "before\n\n```rust\nlet x = 1;\n```\n\nafter";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", true, None);
        });

        for line in &block.lines {
            for span in &line.spans {
                if !span.text.trim().is_empty() {
                    assert!(span.style.dim, "dim missing on span '{}'", span.text);
                }
            }
        }
    }

    #[test]
    fn markdown_code_block_can_contain_shorter_fenced_block() {
        let md = "````markdown\n```rust\nfn main() {}\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("```rust")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("fn main()")),
            "rows: {rows:?}"
        );
        assert_eq!(rows.iter().filter(|row| row.contains("````")).count(), 0);
    }

    #[test]
    fn markdown_code_block_ignores_longer_opening_fence_line() {
        let md = "````markdown\n`````text\ninside\n`````\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("`````text")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("inside")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn markdown_code_block_keeps_fence_with_trailing_text_as_content() {
        let md = "````\n```` text\ninside\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["```` text", "inside"]);
    }

    #[test]
    fn markdown_code_block_closes_on_longer_plain_fence() {
        let md = "````\ninside\n`````\nafter";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["inside", "", "after"]);
    }

    #[test]
    fn markdown_adjacent_nested_code_blocks_preserve_inner_fences() {
        let md = "````\n```\n```\n````\n````\n```\nnested code block\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(
            rows,
            vec!["```", "```", "", "```", "nested code block", "```"]
        );
    }

    #[test]
    fn rendered_table_attaches_raw_source_to_first_row() {
        let md = "| col | val |\n| --- | --- |\n| a   | 1   |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        assert!(block.lines.len() >= 2);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("| col | val |\n| --- | --- |\n| a   | 1   |")
        );
        for line in &block.lines {
            assert!(line.cell_selectable);
            assert!(line.block_selectable);
        }
        for line in block.lines.iter().skip(1) {
            assert!(line.copy_continuation);
            assert!(!line.soft_wrapped);
            assert!(line.source_text.is_none());
        }
    }

    #[test]
    fn rendered_table_honors_separator_alignment_markers() {
        // Generous header widths so per-column padding is visible.
        let md = "| LLLL | CCCC | RRRR |\n|:-----|:----:|-----:|\n| x | y | z |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
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
    fn rendered_table_keeps_escaped_pipe_inside_code_cell() {
        let md = "| System | Mechanism | Outcome |\n|---|---|---|\n| **Smelt** | Unix `flock(LOCK_EX\\|LOCK_NB)` | Second |\n";
        let block = render_test(120, |sink| {
            render_markdown_inner(sink, md, 120, "", false, None);
        });
        let data_row = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .find(|s| s.contains("Smelt"))
            .expect("data row");

        assert_eq!(data_row.matches('┃').count(), 4, "row: {data_row:?}");
        assert!(data_row.contains("LOCK_NB"), "row: {data_row:?}");
    }
}
