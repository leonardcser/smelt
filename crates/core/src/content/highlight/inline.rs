//! Inline markdown rendering: emphasis grammar (`*`, `_`, `` ` ``,
//! `~~`), inline-span flattening + word-wrap, and the markdown table
//! renderer that uses both.

use crate::content::builder::{display_width, LineBuilder};
use crate::content::inline_line::{BreakPolicy, InlineLine, InlineRun};
use crate::content::ColumnAlignment;
use crate::style::Color;
use crate::theme::{intern, HlGroup};
use unicode_width::UnicodeWidthStr;

use super::util::{
    breakable_positions, can_open_emphasis, find_closing_run, find_code_close, find_strike_close,
    run_length, strip_markdown_markers,
};

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
    if rows.is_empty() {
        return 0;
    }
    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return 0;
    }

    let max_table = match bctx {
        Some(b) => b.inner_w,
        None => width.saturating_sub(display_width(indent)),
    };
    let align_for = |c: usize| alignments.get(c).copied().unwrap_or_default();

    let start = out.line_count();
    let Some(col_widths) = fit_column_widths(rows, num_cols, max_table) else {
        let rendered = render_table_stacked(out, rows, max_table, dim, bctx, indent);
        out.stamp_chrome_delimited_block(start);
        return rendered;
    };

    let mut total_rows = 0u16;
    total_rows += render_border(out, &col_widths, bctx, indent, "┏", "┳", "┓");
    if let Some(header) = rows.first() {
        total_rows += render_table_row(out, header, &col_widths, align_for, dim, bctx, indent);
        total_rows += render_border(out, &col_widths, bctx, indent, "┣", "╋", "┫");
    }
    for row in rows.iter().skip(1) {
        total_rows += render_table_row(out, row, &col_widths, align_for, dim, bctx, indent);
    }
    total_rows += render_border(out, &col_widths, bctx, indent, "┗", "┻", "┛");
    out.stamp_chrome_delimited_block(start);
    total_rows
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

fn render_table_row(
    out: &mut LineBuilder,
    row: &[String],
    widths: &[usize],
    align_for: impl Fn(usize) -> ColumnAlignment,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
) -> u16 {
    let wrapped: Vec<Vec<Vec<InlineSpan>>> = row
        .iter()
        .enumerate()
        .map(|(c, cell)| wrap_cell_spans(out, cell, widths.get(c).copied().unwrap_or(0), dim))
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
                let wrapped = wrap_cell_spans(out, cell, value_width, dim);
                for (li, spans) in wrapped.iter().enumerate() {
                    render_row_prefix(out, bctx, indent);
                    if li == 0 {
                        out.print_gutter("  ");
                        emit_table_label(out, label, dim);
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
                    let labels = wrap_cell_spans(out, label, text_width, dim);
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

                let wrapped = wrap_cell_spans(out, cell, text_width, dim);
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
) -> Vec<Vec<InlineSpan>> {
    let spans = parse_inline_spans(text, dim);
    let rows = wrap_inline_spans(&spans, max_width);
    if rows.len() > 1 {
        out.mark_wrapped();
    }
    rows
}

fn emit_table_label(out: &mut LineBuilder, label: &str, dim: bool) {
    let spans = parse_inline_spans(label, dim);
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
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let breakable = breakable_positions(text);

    let mut max_w = 0usize;
    let mut seg_start = 0;
    for ci in 0..len {
        if breakable[ci] {
            if ci > seg_start {
                let seg: String = chars[seg_start..ci].iter().collect();
                max_w = max_w.max(strip_markdown_markers(&seg).width());
            }
            seg_start = ci + 1;
        }
    }
    if seg_start < len {
        let seg: String = chars[seg_start..].iter().collect();
        max_w = max_w.max(strip_markdown_markers(&seg).width());
    }
    max_w
}

// ── Inline markdown AST + parser ─────────────────────────────────────────
//
// A small `InlineNode` tree lets nested spans (bold containing italic, etc.) push styles
// rather than flatly resetting. Delimiter matching is strict on run length: `**text*`
// emits the whole unmatched run as literal so the trailing `*` never re-enters as an opener.

enum InlineNode {
    Text(String),
    Code(String),
    Strike(Vec<InlineNode>),
    Bold(Vec<InlineNode>),
    Italic(Vec<InlineNode>),
    BoldItalic(Vec<InlineNode>),
}

fn parse_inline(chars: &[char], start: usize, end: usize) -> Vec<InlineNode> {
    let mut nodes: Vec<InlineNode> = Vec::new();
    let mut plain = String::new();
    let mut i = start;

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                nodes.push(InlineNode::Text(std::mem::take(&mut plain)));
            }
        };
    }

    while i < end {
        // Code span (precedence over emphasis: CommonMark §6.1).
        if chars[i] == '`' {
            if let Some(close) = find_code_close(chars, i + 1, end) {
                flush_plain!();
                let content: String = chars[i + 1..close].iter().collect();
                nodes.push(InlineNode::Code(content));
                i = close + 1;
                continue;
            }
        }

        // Strikethrough `~~text~~`.
        if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(close) = find_strike_close(chars, i + 2, end) {
                flush_plain!();
                let inner = parse_inline(chars, i + 2, close);
                nodes.push(InlineNode::Strike(inner));
                i = close + 2;
                continue;
            }
        }

        // Emphasis: `*italic*`, `**bold**`, `***both***`.
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            let open_run = run_length(chars, i, end, marker);

            if (1..=3).contains(&open_run) && can_open_emphasis(chars, i, open_run, end, marker) {
                if let Some(close) = find_closing_run(chars, i + open_run, end, marker, open_run) {
                    flush_plain!();
                    let inner = parse_inline(chars, i + open_run, close);
                    let node = match open_run {
                        1 => InlineNode::Italic(inner),
                        2 => InlineNode::Bold(inner),
                        3 => InlineNode::BoldItalic(inner),
                        _ => unreachable!("run length checked by contains()"),
                    };
                    nodes.push(node);
                    i = close + open_run;
                    continue;
                }
            }

            // No match: emit the whole run as literal to avoid re-entry as a new opener.
            for _ in 0..open_run {
                plain.push(marker);
            }
            i += open_run;
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }

    flush_plain!();
    nodes
}

// ── Parse-then-wrap pipeline ─────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub crossedout: bool,
    /// Theme group for the span; `None` for plain text.
    pub group: Option<HlGroup>,
}

#[derive(Clone, Debug)]
pub struct InlineSpan {
    pub text: String,
    pub style: InlineStyle,
}

pub fn parse_inline_spans(text: &str, dim: bool) -> Vec<InlineSpan> {
    let chars: Vec<char> = text.chars().collect();
    let nodes = parse_inline(&chars, 0, chars.len());
    let base = InlineStyle {
        dim,
        ..Default::default()
    };
    let mut out = Vec::new();
    flatten_nodes_into(&nodes, &base, &mut out);
    out
}

fn flatten_nodes_into(nodes: &[InlineNode], style: &InlineStyle, out: &mut Vec<InlineSpan>) {
    for node in nodes {
        match node {
            InlineNode::Text(s) if !s.is_empty() => {
                out.push(InlineSpan {
                    text: s.clone(),
                    style: *style,
                });
            }
            InlineNode::Text(_) => {}
            InlineNode::Code(s) => {
                out.push(InlineSpan {
                    text: s.clone(),
                    style: InlineStyle {
                        group: Some(intern("SmeltAccent")),
                        ..*style
                    },
                });
            }
            InlineNode::Bold(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        bold: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::Italic(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        italic: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::BoldItalic(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        bold: true,
                        italic: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::Strike(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        crossedout: true,
                        ..*style
                    },
                    out,
                );
            }
        }
    }
}

pub fn wrap_inline_spans(spans: &[InlineSpan], max_cols: usize) -> Vec<Vec<InlineSpan>> {
    let line = InlineLine::new(
        spans
            .iter()
            .map(|span| InlineRun::new(span.text.clone(), span.style, BreakPolicy::Normal))
            .collect(),
    );
    line.wrap_ranges(max_cols)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|run| InlineSpan {
                    text: run.text,
                    style: run.meta,
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
                crossedout: span.style.crossedout,
                ..Default::default()
            },
        );
        out.print(&span.text);
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

    /// Parse `text` into styled inline spans and return a compact
    /// `Vec<(tag, text)>` representation.
    /// Tags: "plain", "bold", "italic", "bi" (bold+italic), "code",
    /// "strike". Adjacent spans with the same style are merged.
    fn parse(text: &str) -> Vec<(&'static str, String)> {
        parse_inline_spans(text, false)
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (tag_for(&s.style), s.text))
            .collect()
    }

    fn tag_for(style: &InlineStyle) -> &'static str {
        let is_code = style.group == Some(intern("SmeltAccent"));
        match (style.bold, style.italic, style.crossedout, is_code) {
            (false, false, false, false) => "plain",
            (true, false, false, false) => "bold",
            (false, true, false, false) => "italic",
            (true, true, false, false) => "bi",
            (false, false, true, false) => "strike",
            (false, false, false, true) => "code",
            (true, false, false, true) => "bold+code",
            (false, true, false, true) => "italic+code",
            (true, true, false, true) => "bi+code",
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

    // ── Plain ──────────────────────────────────────────────────────────

    #[test]
    fn plain_text() {
        assert_eq!(parse("hello world"), vec![p("hello world")]);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse(""), vec![]);
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

    /// Regression: `**text*` (3 stars) is an unclosed bold, NOT an
    /// opened bold that collapses to italic. Previously the parser
    /// dropped the leading `*` and produced an italic, giving the user
    /// an "inverted" result (italic instead of bold).
    #[test]
    fn odd_star_count_does_not_invert_emphasis() {
        assert_eq!(parse("**text*"), vec![p("**text*")]);
    }

    #[test]
    fn odd_star_count_trailing_double() {
        assert_eq!(parse("*text**"), vec![p("*text**")]);
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
        // Runs of 4+ delimiters have no standard meaning; keep them literal.
        assert_eq!(parse("****text****"), vec![p("****text****")]);
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
        // The old parser produced `*` + italic("text") for `**text*`,
        // giving width=4 after stripping. The new parser keeps the run
        // literal, so stripping should return the whole thing.
        assert_eq!(strip_markdown_markers("**text*"), "**text*");
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

    /// Source-text round-trip for fenced code blocks: opening fence on
    /// the first row, closing fence on the last, raw line per row in
    /// between. Lets vim-visual / click-drag selections that cover any
    /// subset of code rows reconstruct the markdown - fences re-attach
    /// when the first / last row is in the selection.
    #[test]
    fn render_code_block_with_fence_attaches_source_text_per_line() {
        let lines = ["let x = 1;", "let y = 2;", "let z = 3;"];
        let block = render_test(80, |sink| {
            render_code_block(sink, &lines, "rust", 80, false, None, true);
        });
        assert_eq!(block.lines.len(), 3);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("```rust\nlet x = 1;")
        );
        assert_eq!(block.lines[1].source_text.as_deref(), Some("let y = 2;"));
        assert_eq!(
            block.lines[2].source_text.as_deref(),
            Some("let z = 3;\n```")
        );
    }

    #[test]
    fn render_code_block_single_line_wraps_with_both_fences() {
        let block = render_test(80, |sink| {
            render_code_block(sink, &["let x = 1;"], "rust", 80, false, None, true);
        });
        assert_eq!(block.lines.len(), 1);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
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
            },
            InlineSpan {
                text: "cd".into(),
                style: InlineStyle::default(),
            },
        ];
        assert_eq!(inline_spans_width(&spans), 4);
    }

    #[test]
    fn wrap_inline_spans_zero_max_cols_returns_input_as_one_row() {
        let spans = vec![InlineSpan {
            text: "hello world".into(),
            style: InlineStyle::default(),
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
