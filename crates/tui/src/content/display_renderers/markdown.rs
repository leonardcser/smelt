use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::code_block::{measure_code_block, parse_code_block};
use smelt_core::content::highlight::{
    emit_inline_spans, inline_spans_width, measure_markdown_table_with_options,
    parse_inline_spans_with_options, render_code_block, render_markdown_table_with_options,
    wrap_inline_spans, InlineOptions, InlineSpan, InlineStyle,
};
use smelt_core::content::markdown_ir::{
    parse_markdown_with_options, MarkdownBlock, MarkdownLine, MarkdownNode, MarkdownTextKind,
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
    let block = parse_markdown_with_options(content, inline_options);
    render_markdown_block(out, &block, width, indent, dim, bctx, inline_options)
}

pub fn measure_markdown_inner_with_options(
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    inline_options: &InlineOptions,
) -> u16 {
    let block = parse_markdown_with_options(content, inline_options);
    measure_markdown_block(&block, width, indent, dim, bctx, inline_options)
}

fn render_markdown_block(
    out: &mut LineBuilder,
    block: &MarkdownBlock<'_>,
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
    let ctx = RenderTextCtx {
        max_cols,
        indent,
        dim,
        bctx,
        inline_options,
    };
    let mut state = RenderState::default();

    for node in &block.nodes {
        match node {
            MarkdownNode::Source { range } => {
                let source = smelt_buffer::text::slice(block.source, range.clone());
                render_source_lines(out, source, MarkdownTextKind::Paragraph, &ctx, &mut state);
            }
            MarkdownNode::Text { lines, kind, .. } => {
                render_text_lines(out, block.source, lines, *kind, &ctx, &mut state);
            }
            MarkdownNode::Code { lang, body, .. } => {
                render_block_gap(out, &mut state);
                let code_lines: Vec<&str> = body
                    .iter()
                    .flat_map(|range| {
                        smelt_buffer::text::slice(block.source, range.clone()).lines()
                    })
                    .collect();
                let code_block = parse_code_block(&code_lines, lang);
                state.rows += render_code_block(out, &code_block, width, dim, bctx, true);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Table {
                range,
                alignments,
                rows,
            } => {
                render_block_gap(out, &mut state);
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
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
            MarkdownNode::Rule { .. } => {
                render_block_gap(out, &mut state);
                state.rows += render_horizontal_rule(out, bctx, indent);
                state.last_content_was_heading = false;
                state.prev_was_block = true;
            }
        }
    }

    state.rows
}

fn measure_markdown_block(
    block: &MarkdownBlock<'_>,
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

    for node in &block.nodes {
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

fn render_block_gap(out: &mut LineBuilder, state: &mut RenderState) {
    let mut gap_emitted = false;
    if state.pending_blank {
        out.newline();
        state.rows += 1;
        state.pending_blank = false;
        gap_emitted = true;
    }
    if state.rows > 0 && !gap_emitted && !state.last_content_was_heading {
        out.newline();
        state.rows += 1;
    }
}

struct RenderTextCtx<'a> {
    max_cols: usize,
    indent: &'a str,
    dim: bool,
    bctx: Option<&'a smelt_core::content::BoxContext>,
    inline_options: &'a InlineOptions,
}

fn render_text_gap(out: &mut LineBuilder, state: &mut RenderState, kind: MarkdownTextKind) -> bool {
    if state.rows == 0 {
        state.pending_blank = false;
        return false;
    }
    if kind == MarkdownTextKind::List && !state.prev_was_block {
        state.pending_blank = false;
        return false;
    }
    let before = state.rows;
    render_block_gap(out, state);
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
            gap_emitted = true;
        }
        if state.prev_was_block && !gap_emitted {
            sink.blank_line(state);
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

struct RenderSourceSink<'a, 'b, 'c> {
    out: &'a mut LineBuilder<'b>,
    ctx: &'a RenderTextCtx<'c>,
    kind: MarkdownTextKind,
}

impl TextFlowSink for RenderSourceSink<'_, '_, '_> {
    fn text_gap(&mut self, state: &mut FlowState, kind: MarkdownTextKind) -> bool {
        render_text_gap(self.out, state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        self.out.newline();
        state.rows += 1;
        state.pending_blank = false;
    }

    fn emit_line(&mut self, _index: usize, line: &str, state: &mut FlowState) {
        let spans =
            fallback_markdown_line_spans(line, self.kind, self.ctx.dim, self.ctx.inline_options);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
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
        render_text_gap(self.out, state, kind)
    }

    fn blank_line(&mut self, state: &mut FlowState) {
        self.out.newline();
        state.rows += 1;
        state.pending_blank = false;
    }

    fn emit_line(&mut self, index: usize, line: &str, state: &mut FlowState) {
        let spans = markdown_line_spans(line, &self.lines[index].spans, self.kind, self.ctx.dim);
        render_markdown_line(self.out, line, &spans, self.ctx, state);
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
    state.rows += wrapped.len() as u16;
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
                if let smelt_core::transcript_model::Block::Text { content } =
                    history.block_at(index)
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
