use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratex_font::symbols::{get_symbol, Mode as SymbolMode};
use ratex_parser::parse_node::{AtomFamily, ParseNode};
use rust_latex_parser::{AccentKind, EqNode, MathFontKind, MatrixKind};

use crate::content::highlight::{InlineOptions, InlineSpan, InlineStyle};
use crate::content::inline_line::BreakPolicy;
use crate::content::ColumnAlignment;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownBlock<'a> {
    pub source: &'a str,
    pub nodes: Vec<MarkdownNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownLine {
    pub source: Range<usize>,
    pub spans: Vec<InlineSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkdownNode {
    Source {
        range: Range<usize>,
    },
    Text {
        range: Range<usize>,
        kind: MarkdownTextKind,
        lines: Vec<MarkdownLine>,
    },
    Code {
        range: Range<usize>,
        lang: String,
        body: Vec<Range<usize>>,
    },
    Math {
        range: Range<usize>,
    },
    Table {
        range: Range<usize>,
        alignments: Vec<ColumnAlignment>,
        rows: Vec<Vec<String>>,
    },
    Rule {
        range: Range<usize>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkdownTextKind {
    Paragraph,
    Heading,
    BlockQuote,
    List,
}

impl MarkdownBlock<'_> {
    pub fn dynamic_retained_bytes(&self) -> usize {
        self.nodes
            .capacity()
            .saturating_mul(std::mem::size_of::<MarkdownNode>())
            .saturating_add(
                self.nodes
                    .iter()
                    .map(MarkdownNode::dynamic_retained_bytes)
                    .sum::<usize>(),
            )
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl MarkdownLine {
    pub fn dynamic_retained_bytes(&self) -> usize {
        self.spans
            .capacity()
            .saturating_mul(std::mem::size_of::<InlineSpan>())
            .saturating_add(
                self.spans
                    .iter()
                    .map(InlineSpan::dynamic_retained_bytes)
                    .sum::<usize>(),
            )
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl MarkdownNode {
    pub fn dynamic_retained_bytes(&self) -> usize {
        match self {
            Self::Source { .. } | Self::Math { .. } | Self::Rule { .. } => 0,
            Self::Text { lines, .. } => lines
                .capacity()
                .saturating_mul(std::mem::size_of::<MarkdownLine>())
                .saturating_add(
                    lines
                        .iter()
                        .map(MarkdownLine::dynamic_retained_bytes)
                        .sum::<usize>(),
                ),
            Self::Code { lang, body, .. } => lang.capacity().saturating_add(
                body.capacity()
                    .saturating_mul(std::mem::size_of::<Range<usize>>()),
            ),
            Self::Table {
                alignments, rows, ..
            } => alignments
                .capacity()
                .saturating_mul(std::mem::size_of::<ColumnAlignment>())
                .saturating_add(
                    rows.capacity()
                        .saturating_mul(std::mem::size_of::<Vec<String>>()),
                )
                .saturating_add(
                    rows.iter()
                        .map(|row| {
                            row.capacity()
                                .saturating_mul(std::mem::size_of::<String>())
                                .saturating_add(row.iter().map(String::capacity).sum::<usize>())
                        })
                        .sum::<usize>(),
                ),
        }
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

pub fn markdown_nodes_retained_bytes(nodes: &[MarkdownNode]) -> usize {
    nodes
        .len()
        .saturating_mul(std::mem::size_of::<MarkdownNode>())
        .saturating_add(
            nodes
                .iter()
                .map(MarkdownNode::dynamic_retained_bytes)
                .sum::<usize>(),
        )
}

#[derive(Clone, Debug)]
enum SpecialBlock {
    Text {
        range: Range<usize>,
        kind: MarkdownTextKind,
        lines: Vec<MarkdownLine>,
    },
    Code {
        range: Range<usize>,
        lang: String,
        body: Vec<Range<usize>>,
    },
    Math {
        range: Range<usize>,
    },
    Table {
        range: Range<usize>,
        alignments: Vec<ColumnAlignment>,
        rows: Vec<Vec<String>>,
    },
    Rule {
        range: Range<usize>,
    },
}

impl SpecialBlock {
    fn range(&self) -> Range<usize> {
        match self {
            SpecialBlock::Text { range, .. }
            | SpecialBlock::Code { range, .. }
            | SpecialBlock::Math { range }
            | SpecialBlock::Table { range, .. }
            | SpecialBlock::Rule { range } => range.clone(),
        }
    }

    fn into_node(self) -> MarkdownNode {
        match self {
            SpecialBlock::Text { range, kind, lines } => MarkdownNode::Text { range, kind, lines },
            SpecialBlock::Code { range, lang, body } => MarkdownNode::Code { range, lang, body },
            SpecialBlock::Math { range } => MarkdownNode::Math { range },
            SpecialBlock::Table {
                range,
                alignments,
                rows,
            } => MarkdownNode::Table {
                range,
                alignments,
                rows,
            },
            SpecialBlock::Rule { range } => MarkdownNode::Rule { range },
        }
    }
}

fn collect_math_blocks(source: &str) -> Vec<Range<usize>> {
    let mut code_ranges = Vec::new();
    let mut code_start = None;
    for (event, range) in Parser::new_ext(source, markdown_options()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_start = Some(range.start),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_start.take() {
                    code_ranges.push(start..range.end);
                }
            }
            _ => {}
        }
    }
    let mut ranges = Vec::new();
    let mut code_index = 0;
    let mut math: Option<(usize, &str)> = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        while code_ranges
            .get(code_index)
            .is_some_and(|range| range.end <= offset)
        {
            code_index += 1;
        }
        if code_ranges
            .get(code_index)
            .is_some_and(|range| range.start <= offset && offset < range.end)
        {
            math = None;
            offset += line.len();
            continue;
        }
        let body = strip_markdown_indent(line.trim_end());
        if let Some((start, closing)) = math {
            if body == closing {
                ranges.push(start..offset + line.len());
                math = None;
            }
        } else if ((body.starts_with(r"\[") && body.ends_with(r"\]"))
            || (body.starts_with("$$") && body.ends_with("$$")))
            && body.len() > 4
        {
            ranges.push(offset..offset + line.len());
        } else if body == r"\[" || body == "$$" {
            math = Some((offset, if body == "$$" { "$$" } else { r"\]" }));
        }
        offset += line.len();
    }
    ranges
}

pub fn parse_markdown(source: &str) -> MarkdownBlock<'_> {
    parse_markdown_with_options(source, &InlineOptions::default())
}

pub fn parse_markdown_with_options<'a>(
    source: &'a str,
    inline_options: &InlineOptions,
) -> MarkdownBlock<'a> {
    let math_ranges = collect_math_blocks(source);
    let protected =
        crate::content::highlight::inline::protected_math_source(source, markdown_options());
    let mut specials = collect_special_blocks(source, &protected, inline_options);
    for range in math_ranges {
        specials = specials
            .into_iter()
            .flat_map(|block| without_math_range(block, &range))
            .collect();
        specials.push(SpecialBlock::Math { range });
    }
    specials.sort_by_key(|block| block.range().start);
    specials.dedup_by(|a, b| a.range() == b.range());

    let mut nodes = Vec::new();
    let mut pos = 0usize;
    for block in specials {
        let range = block.range();
        if range.start < pos || range.start > source.len() || range.end > source.len() {
            continue;
        }
        if pos < range.start {
            nodes.push(MarkdownNode::Source {
                range: pos..range.start,
            });
        }
        pos = range.end;
        nodes.push(block.into_node());
    }
    if pos < source.len() || nodes.is_empty() {
        nodes.push(MarkdownNode::Source {
            range: pos..source.len(),
        });
    }

    MarkdownBlock { source, nodes }
}

fn without_math_range(block: SpecialBlock, math: &Range<usize>) -> Vec<SpecialBlock> {
    let range = block.range();
    if range.end <= math.start || range.start >= math.end {
        return vec![block];
    }
    if let SpecialBlock::Text { kind, lines, .. } = block {
        let mut parts = Vec::new();
        let before: Vec<_> = lines
            .iter()
            .filter(|line| line.source.end <= math.start)
            .cloned()
            .collect();
        if !before.is_empty() {
            parts.push(SpecialBlock::Text {
                range: range.start..math.start,
                kind,
                lines: before,
            });
        }
        let after: Vec<_> = lines
            .into_iter()
            .filter(|line| line.source.start >= math.end)
            .collect();
        if !after.is_empty() {
            parts.push(SpecialBlock::Text {
                range: math.end..range.end,
                kind,
                lines: after,
            });
        }
        parts
    } else {
        Vec::new()
    }
}

pub fn math_body(source: &str) -> &str {
    let source = source.trim();
    source
        .strip_prefix(r"\[")
        .and_then(|s| s.strip_suffix(r"\]"))
        .or_else(|| source.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")))
        .unwrap_or(source)
        .trim()
}

pub fn inline_math_text(source: &str) -> String {
    let Some(ast) = parse_unicode_math(source) else {
        return source.to_owned();
    };
    let rendered = term_maths::layout::layout(&linearize_fractions(ast.clone())).to_string();
    if rendered.lines().count() == 1 {
        return rendered.trim().to_owned();
    }
    let flattened = term_maths::layout::layout(&flatten_inline_math(ast)).to_string();
    if flattened.lines().count() == 1 {
        flattened.trim().to_owned()
    } else {
        source.to_owned()
    }
}

pub fn math_rows(source: &str, width: usize) -> Vec<String> {
    let Some(ast) = parse_unicode_math(math_body(source)) else {
        let rows: Vec<_> = source
            .lines()
            .flat_map(|line| wrap_math_line(line, width.max(1)))
            .collect();
        return if rows.is_empty() {
            vec![String::new()]
        } else {
            rows
        };
    };
    let width = width.max(1);
    let mut nodes = Vec::new();
    flatten_sequence(ast, &mut nodes);
    let mut current = Vec::new();
    let mut rows = Vec::new();
    for node in nodes {
        if is_math_relation(&node) && !current.is_empty() && {
            let block = term_maths::layout::layout(&EqNode::Seq(current.clone()));
            block.height() > 1 || block.width() > width
        } {
            append_math_group(&mut rows, std::mem::take(&mut current), width);
        }
        current.push(node);
    }
    if !current.is_empty() {
        append_math_group(&mut rows, current, width);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn flatten_sequence(node: EqNode, out: &mut Vec<EqNode>) {
    match node {
        EqNode::Seq(nodes) => {
            for node in nodes {
                flatten_sequence(node, out);
            }
        }
        other => out.push(other),
    }
}

fn is_math_relation(node: &EqNode) -> bool {
    matches!(node, EqNode::Text(text) if matches!(text.as_str(), "=" | "≈" | "≃" | "≅" | "≡" | "≠" | "≤" | "≥" | "<" | ">" | "→" | "⇒"))
}

fn append_math_group(rows: &mut Vec<String>, group: Vec<EqNode>, width: usize) {
    let mut block = term_maths::layout::layout(&EqNode::Seq(group.clone()));
    if block.width() > width {
        block = term_maths::layout::layout(&linearize_fractions(EqNode::Seq(group)));
    }
    for line in block.to_string().lines() {
        if block.height() == 1 || block.width() > width {
            rows.extend(wrap_math_line(line, width));
        } else {
            rows.push(line.to_owned());
        }
    }
}

fn wrap_math_line(line: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cols = 0;
    for grapheme in smelt_buffer::cell_width::graphemes(line) {
        let n = smelt_buffer::cell_width::text_width(grapheme);
        if cols + n > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            cols = 0;
        }
        if n <= width {
            row.push_str(grapheme);
            cols += n;
        }
    }
    rows.push(row);
    rows
}

fn linearize_fractions(node: EqNode) -> EqNode {
    match node {
        EqNode::Frac(numerator, denominator) => EqNode::Seq(vec![
            EqNode::Text("(".into()),
            linearize_fractions(*numerator),
            EqNode::Text(")/(".into()),
            linearize_fractions(*denominator),
            EqNode::Text(")".into()),
        ]),
        EqNode::Seq(nodes) => EqNode::Seq(nodes.into_iter().map(linearize_fractions).collect()),
        EqNode::Sup(base, sup) => EqNode::Sup(
            Box::new(linearize_fractions(*base)),
            Box::new(linearize_fractions(*sup)),
        ),
        EqNode::Sub(base, sub) => EqNode::Sub(
            Box::new(linearize_fractions(*base)),
            Box::new(linearize_fractions(*sub)),
        ),
        EqNode::SupSub(base, sup, sub) => EqNode::SupSub(
            Box::new(linearize_fractions(*base)),
            Box::new(linearize_fractions(*sup)),
            Box::new(linearize_fractions(*sub)),
        ),
        EqNode::Sqrt(body) => EqNode::Sqrt(Box::new(linearize_fractions(*body))),
        EqNode::Accent(body, kind) => EqNode::Accent(Box::new(linearize_fractions(*body)), kind),
        EqNode::MathFont { kind, content } => EqNode::MathFont {
            kind,
            content: Box::new(linearize_fractions(*content)),
        },
        EqNode::Delimited {
            left,
            right,
            content,
        } => EqNode::Delimited {
            left,
            right,
            content: Box::new(linearize_fractions(*content)),
        },
        EqNode::BigOp {
            symbol,
            lower,
            upper,
        } => EqNode::BigOp {
            symbol,
            lower: lower.map(|node| Box::new(linearize_fractions(*node))),
            upper: upper.map(|node| Box::new(linearize_fractions(*node))),
        },
        EqNode::Limit { name, lower } => EqNode::Limit {
            name,
            lower: lower.map(|node| Box::new(linearize_fractions(*node))),
        },
        EqNode::Matrix { kind, rows } => EqNode::Matrix {
            kind,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(linearize_fractions).collect())
                .collect(),
        },
        EqNode::Cases { rows } => EqNode::Cases {
            rows: rows
                .into_iter()
                .map(|(value, condition)| {
                    (
                        linearize_fractions(value),
                        condition.map(linearize_fractions),
                    )
                })
                .collect(),
        },
        EqNode::Binom(top, bottom) => EqNode::Binom(
            Box::new(linearize_fractions(*top)),
            Box::new(linearize_fractions(*bottom)),
        ),
        EqNode::Brace {
            content,
            label,
            over,
        } => EqNode::Brace {
            content: Box::new(linearize_fractions(*content)),
            label: label.map(|node| Box::new(linearize_fractions(*node))),
            over,
        },
        EqNode::StackRel {
            base,
            annotation,
            over,
        } => EqNode::StackRel {
            base: Box::new(linearize_fractions(*base)),
            annotation: Box::new(linearize_fractions(*annotation)),
            over,
        },
        other => other,
    }
}

fn parse_unicode_math(source: &str) -> Option<EqNode> {
    if source.len() > 8192 {
        return None;
    }
    let nodes = ratex_parser::parse(source).ok()?;
    // Never display a partially converted formula: an unsupported node can change its meaning.
    unicode_nodes(&nodes)
}

fn unicode_nodes(nodes: &[ParseNode]) -> Option<EqNode> {
    let mut result = Vec::with_capacity(nodes.len());
    let mut previous_is_operand = false;
    for node in nodes {
        if let ParseNode::Atom {
            family: AtomFamily::Bin | AtomFamily::Rel,
            ..
        } = node
        {
            if previous_is_operand {
                result.push(EqNode::Space(4.0));
            }
            result.push(unicode_node(node)?);
            if previous_is_operand {
                result.push(EqNode::Space(4.0));
            }
            previous_is_operand = false;
        } else {
            result.push(unicode_node(node)?);
            previous_is_operand = match node {
                ParseNode::SpacingNode { .. } => previous_is_operand,
                ParseNode::Atom {
                    family: AtomFamily::Open | AtomFamily::Punct,
                    ..
                } => false,
                _ => true,
            };
        }
    }
    Some(EqNode::Seq(result))
}

fn unicode_array_rows(rows: &[Vec<ParseNode>]) -> Option<Vec<Vec<EqNode>>> {
    rows.iter()
        .map(|row| row.iter().map(unicode_node).collect())
        .collect()
}

fn unicode_symbol(text: &str) -> Option<String> {
    if matches!(text, "*" | r"\cdot") {
        return Some("·".into());
    }
    get_symbol(text, SymbolMode::Math)
        .and_then(|symbol| symbol.codepoint)
        .map(|ch| ch.to_string())
        .or_else(|| (!text.starts_with('\\')).then(|| text.to_owned()))
}

fn unicode_delimiter(text: &str) -> Option<String> {
    if text == "." {
        Some(String::new())
    } else if text == r"\{" {
        Some("{".into())
    } else if text == r"\}" {
        Some("}".into())
    } else {
        unicode_symbol(text)
    }
}

fn unicode_node(node: &ParseNode) -> Option<EqNode> {
    Some(match node {
        ParseNode::Atom { text, .. }
        | ParseNode::MathOrd { text, .. }
        | ParseNode::TextOrd { text, .. }
        | ParseNode::OpToken { text, .. }
        | ParseNode::AccentToken { text, .. } => EqNode::Text(unicode_symbol(text)?),
        ParseNode::SpacingNode { text, .. } => {
            EqNode::Space(if matches!(text.as_str(), r"\quad" | r"\qquad") {
                18.0
            } else {
                4.0
            })
        }
        ParseNode::OrdGroup { body, .. }
        | ParseNode::Text { body, .. }
        | ParseNode::Styling { body, .. }
        | ParseNode::Sizing { body, .. }
        | ParseNode::Color { body, .. }
        | ParseNode::HBox { body, .. }
        | ParseNode::MClass { body, .. }
        | ParseNode::OperatorName { body, .. } => unicode_nodes(body)?,
        ParseNode::SupSub { base, sup, sub, .. } => {
            if let Some(base) = base {
                if let ParseNode::HorizBrace {
                    base: content,
                    is_over,
                    ..
                } = base.as_ref()
                {
                    if (if *is_over { sub } else { sup }).is_none() {
                        return Some(EqNode::Brace {
                            content: Box::new(unicode_node(content)?),
                            label: match if *is_over { sup } else { sub } {
                                Some(node) => Some(Box::new(unicode_node(node)?)),
                                None => None,
                            },
                            over: *is_over,
                        });
                    }
                }
                if let ParseNode::Op {
                    body: Some(body), ..
                } = base.as_ref()
                {
                    if let (Some(annotation), None) | (None, Some(annotation)) = (sup, sub) {
                        return Some(EqNode::StackRel {
                            base: Box::new(unicode_nodes(body)?),
                            annotation: Box::new(unicode_node(annotation)?),
                            over: sup.is_some(),
                        });
                    }
                }
                if let ParseNode::Op {
                    name: Some(name),
                    symbol: true,
                    ..
                } = base.as_ref()
                {
                    return Some(EqNode::BigOp {
                        symbol: unicode_symbol(name)?,
                        lower: match sub {
                            Some(node) => Some(Box::new(unicode_node(node)?)),
                            None => None,
                        },
                        upper: match sup {
                            Some(node) => Some(Box::new(unicode_node(node)?)),
                            None => None,
                        },
                    });
                }
            }
            let base = Box::new(match base {
                Some(base) => unicode_node(base)?,
                None => EqNode::Text(String::new()),
            });
            match (sup, sub) {
                (Some(sup), Some(sub)) => EqNode::SupSub(
                    base,
                    Box::new(unicode_node(sup)?),
                    Box::new(unicode_node(sub)?),
                ),
                (Some(sup), None) => EqNode::Sup(base, Box::new(unicode_node(sup)?)),
                (None, Some(sub)) => EqNode::Sub(base, Box::new(unicode_node(sub)?)),
                (None, None) => *base,
            }
        }
        ParseNode::GenFrac {
            numer,
            denom,
            has_bar_line,
            left_delim,
            right_delim,
            ..
        } => {
            let numer = Box::new(unicode_node(numer)?);
            let denom = Box::new(unicode_node(denom)?);
            if !*has_bar_line {
                if left_delim.as_deref() == Some("(") && right_delim.as_deref() == Some(")") {
                    EqNode::Binom(numer, denom)
                } else {
                    return None;
                }
            } else if left_delim.is_some() || right_delim.is_some() {
                let frac = EqNode::Frac(numer, denom);
                EqNode::Delimited {
                    left: left_delim
                        .as_deref()
                        .map(unicode_delimiter)
                        .unwrap_or(Some(String::new()))?,
                    right: right_delim
                        .as_deref()
                        .map(unicode_delimiter)
                        .unwrap_or(Some(String::new()))?,
                    content: Box::new(frac),
                }
            } else {
                EqNode::Frac(numer, denom)
            }
        }
        ParseNode::Sqrt {
            body, index: None, ..
        } => EqNode::Sqrt(Box::new(unicode_node(body)?)),
        ParseNode::Accent { label, base, .. } => {
            let kind = match label.as_str() {
                r"\hat" | r"\widehat" => AccentKind::Hat,
                r"\bar" | r"\overline" => AccentKind::Bar,
                r"\dot" => AccentKind::Dot,
                r"\ddot" => AccentKind::DoubleDot,
                r"\tilde" | r"\widetilde" => AccentKind::Tilde,
                r"\vec" => AccentKind::Vec,
                _ => return None,
            };
            EqNode::Accent(Box::new(unicode_node(base)?), kind)
        }
        ParseNode::Overline { body, .. } => {
            EqNode::Accent(Box::new(unicode_node(body)?), AccentKind::Bar)
        }
        ParseNode::HorizBrace { base, is_over, .. } => EqNode::Brace {
            content: Box::new(unicode_node(base)?),
            label: None,
            over: *is_over,
        },
        ParseNode::Op {
            body: Some(body), ..
        } => unicode_nodes(body)?,
        ParseNode::Op {
            name: Some(name),
            symbol,
            body: None,
            ..
        } => {
            if *symbol {
                EqNode::Text(unicode_symbol(name)?)
            } else {
                EqNode::Text(name.trim_start_matches('\\').into())
            }
        }
        ParseNode::Font { font, body, .. } => {
            let kind = match font.as_str() {
                "mathbf" | "boldsymbol" => MathFontKind::Bold,
                "mathbb" => MathFontKind::Blackboard,
                "mathcal" | "mathscr" => MathFontKind::Calligraphic,
                "mathrm" => MathFontKind::Roman,
                "mathfrak" => MathFontKind::Fraktur,
                "mathsf" => MathFontKind::SansSerif,
                "mathtt" => MathFontKind::Monospace,
                _ => return None,
            };
            EqNode::MathFont {
                kind,
                content: Box::new(unicode_node(body)?),
            }
        }
        ParseNode::LeftRight {
            body, left, right, ..
        } => {
            let left = unicode_delimiter(left)?;
            let right = unicode_delimiter(right)?;
            if let [ParseNode::Array { body: rows, .. }] = body.as_slice() {
                let rows = unicode_array_rows(rows)?;
                if left == "{"
                    && right.is_empty()
                    && rows.iter().all(|row| (1..=2).contains(&row.len()))
                {
                    EqNode::Cases {
                        rows: rows
                            .into_iter()
                            .map(|mut row| (row.remove(0), row.pop()))
                            .collect(),
                    }
                } else {
                    let kind = match (left.as_str(), right.as_str()) {
                        ("(", ")") => MatrixKind::Paren,
                        ("[", "]") => MatrixKind::Bracket,
                        ("|", "|") => MatrixKind::VBar,
                        ("‖", "‖") => MatrixKind::DoubleVBar,
                        ("{", "}") => MatrixKind::Brace,
                        ("", "") => MatrixKind::Plain,
                        _ => return None,
                    };
                    EqNode::Matrix { kind, rows }
                }
            } else {
                EqNode::Delimited {
                    left,
                    right,
                    content: Box::new(unicode_nodes(body)?),
                }
            }
        }
        ParseNode::Array { body, .. } => EqNode::Matrix {
            kind: MatrixKind::Plain,
            rows: unicode_array_rows(body)?,
        },
        ParseNode::Enclose { label, body, .. } if label == r"\fbox" => EqNode::Delimited {
            left: "⟦".into(),
            right: "⟧".into(),
            content: Box::new(unicode_node(body)?),
        },
        ParseNode::Kern { dimension, .. } => {
            EqNode::Space(dimension.number as f32 * if dimension.unit == "em" { 18.0 } else { 1.0 })
        }
        ParseNode::Verb { body, .. } => EqNode::TextBlock(body.clone()),
        _ => return None,
    })
}

fn inline_script(node: EqNode) -> EqNode {
    let node = flatten_inline_math(node);
    fn single_symbol(node: &EqNode) -> bool {
        match node {
            EqNode::Text(text) => text.chars().count() == 1,
            EqNode::Seq(nodes) if nodes.len() == 1 => single_symbol(&nodes[0]),
            _ => false,
        }
    }
    if single_symbol(&node) {
        node
    } else {
        EqNode::Seq(vec![
            EqNode::Text("(".into()),
            node,
            EqNode::Text(")".into()),
        ])
    }
}

fn flatten_inline_math(node: EqNode) -> EqNode {
    match node {
        EqNode::Frac(numer, denom) => EqNode::Seq(vec![
            EqNode::Text("(".into()),
            flatten_inline_math(*numer),
            EqNode::Text(")/(".into()),
            flatten_inline_math(*denom),
            EqNode::Text(")".into()),
        ]),
        EqNode::Sup(base, sup) => EqNode::Seq(vec![
            flatten_inline_math(*base),
            EqNode::Text("^".into()),
            inline_script(*sup),
        ]),
        EqNode::Sub(base, sub) => EqNode::Seq(vec![
            flatten_inline_math(*base),
            EqNode::Text("_".into()),
            inline_script(*sub),
        ]),
        EqNode::SupSub(base, sup, sub) => EqNode::Seq(vec![
            flatten_inline_math(*base),
            EqNode::Text("^".into()),
            inline_script(*sup),
            EqNode::Text("_".into()),
            inline_script(*sub),
        ]),
        EqNode::Seq(nodes) => EqNode::Seq(nodes.into_iter().map(flatten_inline_math).collect()),
        other => other,
    }
}

pub fn ends_with_heading(source: &str) -> bool {
    parse_last_markdown_block_kind(source) == Some(MarkdownTextKind::Heading)
}

fn parse_last_markdown_block_kind(source: &str) -> Option<MarkdownTextKind> {
    let mut previous_adjacent: Option<(&str, bool)> = None;
    let mut adjacent_candidate: Option<(&str, bool)> = None;
    let mut last_non_empty: Option<(&str, bool)> = None;
    let mut fence: Option<(char, usize)> = None;

    for line in source.lines() {
        let body = strip_markdown_indent(smelt_buffer::text::trim_end_whitespace(line));
        let blank = body.trim().is_empty();
        let current = if let Some((marker, len)) = fence {
            if is_closing_fence(body, marker, len) {
                fence = None;
            }
            (!blank).then_some((line, true))
        } else if let Some(open) = opening_fence(body) {
            fence = Some(open);
            Some((line, true))
        } else {
            (!blank).then_some((line, false))
        };

        if let Some(current) = current {
            previous_adjacent = adjacent_candidate;
            last_non_empty = Some(current);
            adjacent_candidate = Some(current);
        } else {
            adjacent_candidate = None;
        }
    }

    let (line, in_code) = last_non_empty?;
    if in_code {
        return Some(MarkdownTextKind::Paragraph);
    }
    if is_atx_heading(line) {
        return Some(MarkdownTextKind::Heading);
    }
    if is_setext_underline(line)
        && previous_adjacent.is_some_and(|(previous, in_code)| {
            !in_code && !is_thematic_break(previous) && !is_atx_heading(previous)
        })
    {
        return Some(MarkdownTextKind::Heading);
    }
    Some(MarkdownTextKind::Paragraph)
}

fn strip_markdown_indent(line: &str) -> &str {
    let mut end = 0usize;
    for (_, grapheme) in smelt_buffer::cell_width::grapheme_indices(line).take(3) {
        if !grapheme.starts_with(' ') {
            break;
        }
        end += grapheme.len();
    }
    &line[end..]
}

fn opening_fence(line: &str) -> Option<(char, usize)> {
    let marker = line.as_bytes().first().copied()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let len = line.bytes().take_while(|b| *b == marker).count();
    (len >= 3).then_some((marker as char, len))
}

fn is_closing_fence(line: &str, marker: char, open_len: usize) -> bool {
    let marker = marker as u8;
    if line.as_bytes().first().copied() != Some(marker) {
        return false;
    }
    let len = line.bytes().take_while(|b| *b == marker).count();
    len >= open_len && line[len..].trim().is_empty()
}

pub(crate) fn is_atx_heading(line: &str) -> bool {
    let line = strip_markdown_indent(smelt_buffer::text::trim_end_whitespace(line));
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    line.as_bytes()
        .get(hashes)
        .is_none_or(|b| b.is_ascii_whitespace())
}

pub(crate) fn is_setext_underline(line: &str) -> bool {
    let line = strip_markdown_indent(smelt_buffer::text::trim_end_whitespace(line));
    let mut marker = None;
    let mut saw_marker = false;
    for b in line.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b != b'=' && b != b'-' {
            return false;
        }
        if let Some(marker) = marker {
            if b != marker {
                return false;
            }
        } else {
            marker = Some(b);
        }
        saw_marker = true;
    }
    saw_marker
}

pub(crate) fn is_thematic_break(line: &str) -> bool {
    let line = strip_markdown_indent(smelt_buffer::text::trim_end_whitespace(line));
    let mut marker = None;
    let mut markers = 0usize;
    for b in line.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b != b'-' && b != b'_' && b != b'*' {
            return false;
        }
        if let Some(marker) = marker {
            if b != marker {
                return false;
            }
        } else {
            marker = Some(b);
        }
        markers += 1;
    }
    markers >= 3
}

fn markdown_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_MATH
}

fn collect_special_blocks(
    source: &str,
    protected: &str,
    inline_options: &InlineOptions,
) -> Vec<SpecialBlock> {
    let parser = Parser::new_ext(protected, markdown_options()).into_offset_iter();
    let mut out = Vec::new();
    let mut text_stack: Vec<OpenText> = Vec::new();
    let mut code: Option<(usize, String, Vec<Range<usize>>)> = None;
    let mut table: Option<TableBuild> = None;

    for (event, range) in parser {
        match event {
            Event::Start(tag) if markdown_text_kind(&tag).is_some() => {
                push_open_text_event(&mut text_stack, Event::Start(tag.clone()), range.clone());
                text_stack.push(OpenText {
                    start: range.start,
                    kind: markdown_text_kind(&tag).unwrap(),
                    end: tag_end_for_text(&tag),
                    events: Vec::new(),
                });
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((range.start, lang, Vec::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, lang, body)) = code.take() {
                    out.push(SpecialBlock::Code {
                        range: start..range.end,
                        lang,
                        body,
                    });
                }
            }
            Event::End(end) if text_stack.last().is_some_and(|open| open.end == end) => {
                let open = text_stack.pop().unwrap();
                let text_range = open.start..range.end;
                let lines = if text_stack.is_empty() {
                    Some(markdown_lines(
                        source,
                        text_range.clone(),
                        open.kind,
                        &open.events,
                        inline_options,
                    ))
                } else {
                    None
                };
                push_open_text_event(&mut text_stack, Event::End(end), range);
                if let Some(lines) = lines {
                    out.push(SpecialBlock::Text {
                        range: text_range,
                        kind: open.kind,
                        lines,
                    });
                }
            }
            Event::DisplayMath(_) if text_stack.is_empty() => {
                out.push(SpecialBlock::Math { range });
            }
            Event::Text(_) => {
                if let Some((_, _, body)) = code.as_mut() {
                    body.push(range.clone());
                }
                push_open_text_event(&mut text_stack, event, range);
            }
            Event::Start(Tag::Table(alignments)) => {
                table = Some(TableBuild {
                    start: range.start,
                    alignments: alignments.into_iter().map(map_alignment).collect(),
                    rows: Vec::new(),
                    current_row: None,
                });
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = table.take() {
                    out.push(SpecialBlock::Table {
                        range: table.start..range.end,
                        alignments: table.alignments,
                        rows: table.rows,
                    });
                }
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                if let Some(table) = table.as_mut() {
                    table.current_row = Some(Vec::new());
                }
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                if let Some(table) = table.as_mut() {
                    if let Some(row) = table.current_row.take() {
                        table.rows.push(row);
                    }
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = table.as_mut() {
                    if let Some(row) = table.current_row.as_mut() {
                        row.push(trim_cell_source(source, range));
                    }
                }
            }
            Event::Rule => out.push(SpecialBlock::Rule { range }),
            event => push_open_text_event(&mut text_stack, event, range),
        }
    }

    out
}

struct TableBuild {
    start: usize,
    alignments: Vec<ColumnAlignment>,
    rows: Vec<Vec<String>>,
    current_row: Option<Vec<String>>,
}

struct OpenText<'a> {
    start: usize,
    kind: MarkdownTextKind,
    end: TagEnd,
    events: Vec<(Event<'a>, Range<usize>)>,
}

fn push_open_text_event<'a>(
    text_stack: &mut [OpenText<'a>],
    event: Event<'a>,
    range: Range<usize>,
) {
    for open in text_stack {
        open.events.push((event.clone(), range.clone()));
    }
}

fn markdown_text_kind(tag: &Tag<'_>) -> Option<MarkdownTextKind> {
    match tag {
        Tag::Paragraph => Some(MarkdownTextKind::Paragraph),
        Tag::Heading { .. } => Some(MarkdownTextKind::Heading),
        Tag::BlockQuote(_) => Some(MarkdownTextKind::BlockQuote),
        Tag::List(_) => Some(MarkdownTextKind::List),
        _ => None,
    }
}

fn tag_end_for_text(tag: &Tag<'_>) -> TagEnd {
    match tag {
        Tag::Paragraph => TagEnd::Paragraph,
        Tag::Heading { level, .. } => TagEnd::Heading(*level),
        Tag::BlockQuote(kind) => TagEnd::BlockQuote(*kind),
        Tag::List(start) => TagEnd::List(start.is_some()),
        _ => unreachable!("only text container tags are converted"),
    }
}

fn markdown_lines<'a>(
    source: &str,
    range: Range<usize>,
    kind: MarkdownTextKind,
    events: &[(Event<'a>, Range<usize>)],
    inline_options: &InlineOptions,
) -> Vec<MarkdownLine> {
    let ranges = line_ranges(source, range);
    let inline_lines = crate::content::highlight::inline::lower_inline_event_lines_with_source(
        source,
        events.iter().cloned(),
        &ranges,
        false,
        inline_options,
    );

    ranges
        .into_iter()
        .zip(inline_lines)
        .map(|(line_range, inline_spans)| {
            let mut spans = structural_prefix_spans(source, line_range.clone(), kind, events);
            spans.extend(inline_spans);
            MarkdownLine {
                source: line_range,
                spans,
            }
        })
        .collect()
}

fn line_ranges(source: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let text = smelt_buffer::text::slice(source, range.clone());
    let mut out = Vec::new();
    let mut start = range.start;
    for line in text.split_inclusive('\n') {
        let line_len = line.trim_end_matches(['\r', '\n']).len();
        out.push(start..start + line_len);
        start += line.len();
    }
    if !text.ends_with('\n') && out.is_empty() && !text.is_empty() {
        out.push(range.start..range.end);
    }
    out
}

fn structural_prefix_spans<'a>(
    source: &str,
    line_range: Range<usize>,
    kind: MarkdownTextKind,
    events: &[(Event<'a>, Range<usize>)],
) -> Vec<InlineSpan> {
    if !matches!(
        kind,
        MarkdownTextKind::Heading | MarkdownTextKind::BlockQuote
    ) {
        return Vec::new();
    }

    let line = smelt_buffer::text::slice(source, line_range.clone());
    let trimmed = smelt_buffer::text::trim_start_whitespace(line);
    let prefix_start = line_range.start + line.len() - trimmed.len();
    let prefix_end = events
        .iter()
        .filter_map(|(event, range)| structural_prefix_end(event).then_some(range.start))
        .filter(|&start| start >= prefix_start && start < line_range.end)
        .min()
        .unwrap_or(line_range.end);
    let prefix = smelt_buffer::text::slice(source, prefix_start..prefix_end);
    if prefix.is_empty() {
        Vec::new()
    } else {
        vec![InlineSpan {
            text: prefix.to_string(),
            style: InlineStyle::default(),
            meta: Default::default(),
            break_policy: BreakPolicy::Normal,
            math: None,
        }]
    }
}

fn structural_prefix_end(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Start(
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ) | Event::Text(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::Code(_)
            | Event::TaskListMarker(_)
            | Event::FootnoteReference(_)
            | Event::Rule
    )
}

fn map_alignment(alignment: Alignment) -> ColumnAlignment {
    match alignment {
        Alignment::Center => ColumnAlignment::Center,
        Alignment::Right => ColumnAlignment::Right,
        Alignment::Left | Alignment::None => ColumnAlignment::Left,
    }
}

fn trim_cell_source(source: &str, range: Range<usize>) -> String {
    smelt_buffer::text::trim_whitespace(source.get(range).unwrap_or("")).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_markdown_extracts_fenced_code() {
        let source = "before\n\n```rust\nfn main() {}\n```\nafter";
        let block = parse_markdown(source);
        assert!(block.nodes.iter().any(|node| matches!(
            node,
            MarkdownNode::Code { lang, body, .. }
                if lang == "rust" && body.iter().any(|range| source[range.clone()].contains("fn main"))
        )));
    }

    #[test]
    fn parse_markdown_keeps_table_source_range() {
        let source = "before\n\n| a | b |\n| - | - |\n| c | d |\n\nafter";
        let block = parse_markdown(source);
        let table_source = block.nodes.iter().find_map(|node| match node {
            MarkdownNode::Table { range, .. } => Some(&source[range.clone()]),
            _ => None,
        });
        assert_eq!(
            table_source.map(str::trim_end),
            Some("| a | b |\n| - | - |\n| c | d |")
        );
    }

    #[test]
    fn parse_markdown_table_uses_parser_cell_boundaries() {
        let source = "| System | Mechanism | Outcome |\n|---|---|---|\n| **Smelt** | Unix `flock(LOCK_EX\\|LOCK_NB)` | Second |\n";
        let block = parse_markdown(source);
        let rows = block.nodes.iter().find_map(|node| match node {
            MarkdownNode::Table { rows, .. } => Some(rows),
            _ => None,
        });

        assert_eq!(
            rows,
            Some(&vec![
                vec!["System".into(), "Mechanism".into(), "Outcome".into()],
                vec![
                    "**Smelt**".into(),
                    "Unix `flock(LOCK_EX\\|LOCK_NB)`".into(),
                    "Second".into(),
                ],
            ])
        );
    }

    #[test]
    fn retained_accounting_includes_table_cells_and_inline_spans() {
        let cell = "x".repeat(64 * 1024);
        let table_source = format!("| value |\n| --- |\n| {cell} |\n");
        let table = parse_markdown(&table_source);
        assert!(
            table.retained_bytes()
                >= std::mem::size_of::<MarkdownBlock<'_>>().saturating_add(cell.len())
        );

        let span_source = "**word** ".repeat(2_048);
        let spans = parse_markdown(&span_source);
        let span_count = spans
            .nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Text { lines, .. } => {
                    Some(lines.iter().map(|line| line.spans.len()).sum::<usize>())
                }
                _ => None,
            })
            .sum::<usize>();
        assert!(span_count >= 2_048);
        assert!(spans.dynamic_retained_bytes() > span_source.len());
    }

    #[test]
    fn parse_markdown_table_trimming_keeps_graphemes_atomic() {
        let source = "| \u{301}x | y\u{600}  |\n|---|---|\n";
        let block = parse_markdown(source);
        let rows = block.nodes.iter().find_map(|node| match node {
            MarkdownNode::Table { rows, .. } => Some(rows),
            _ => None,
        });

        assert_eq!(
            rows,
            Some(&vec![vec![" \u{301}x".into(), "y\u{600} ".into()],])
        );
    }

    #[test]
    fn parse_markdown_classifies_text_blocks_from_parser_events() {
        let source = "# Title\n\nParagraph text.\n\n> quote\n\n- item\n";
        let block = parse_markdown(source);
        let kinds: Vec<MarkdownTextKind> = block
            .nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Text { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();

        assert_eq!(
            kinds,
            vec![
                MarkdownTextKind::Heading,
                MarkdownTextKind::Paragraph,
                MarkdownTextKind::BlockQuote,
                MarkdownTextKind::List,
            ]
        );
    }

    #[test]
    fn parse_markdown_lowers_inline_spans_into_text_lines() {
        let source = "Paragraph with **bold** and `code`.\n\n- **item**\n";
        let block = parse_markdown(source);
        let mut text_nodes = block.nodes.iter().filter_map(|node| match node {
            MarkdownNode::Text { kind, lines, .. } => Some((*kind, lines)),
            _ => None,
        });

        let (paragraph_kind, paragraph_lines) = text_nodes.next().expect("paragraph");
        assert_eq!(paragraph_kind, MarkdownTextKind::Paragraph);
        assert_eq!(paragraph_lines.len(), 1);
        assert_eq!(paragraph_lines[0].spans[1].text, "bold");
        assert!(paragraph_lines[0].spans[1].style.bold);
        assert_eq!(paragraph_lines[0].spans[3].text, "code");
        assert!(paragraph_lines[0].spans[3].style.group.is_some());

        let (list_kind, list_lines) = text_nodes.next().expect("list");
        assert_eq!(list_kind, MarkdownTextKind::List);
        assert_eq!(list_lines.len(), 1);
        assert_eq!(list_lines[0].spans[0].text, "item");
        assert!(list_lines[0].spans[0].style.bold);
    }

    #[test]
    fn display_math_is_a_block_but_fenced_math_is_code() {
        let source = concat!(
            "The gradient is\n\n\\[\n\\frac{\\partial E}{\\partial\\theta_0}\\approx\\boxed{0.37062}\n\\]\n\n",
            "```text\n\\[\n\\frac{x}{y}\n\\]\n```\n\n",
            "$$\n\\theta_1\\approx0.88358\n$$\n"
        );
        let nodes = parse_markdown(source).nodes;
        let math = nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Math { range } => Some(&source[range.clone()]),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(math.len(), 2);
        assert!(math[0].starts_with(r"\["));
        assert!(math[1].starts_with("$$"));
        assert!(nodes
            .iter()
            .any(|node| matches!(node, MarkdownNode::Code { .. })));
    }

    #[test]
    fn math_block_preserves_markdown_references_across_it() {
        for math in [r"\[x=1\]", "$$x=1$$"] {
            let source =
                format!("[details][equation]\n\n{math}\n\n[equation]: https://example.test/math\n");
            let nodes = parse_markdown(&source).nodes;
            let spans = nodes
                .iter()
                .find_map(|node| match node {
                    MarkdownNode::Text { lines, .. } => Some(&lines[0].spans),
                    _ => None,
                })
                .unwrap();
            assert!(
                spans
                    .iter()
                    .any(|span| span.text == "details" && span.meta.action.is_some()),
                "{math}: {spans:#?}"
            );
            assert!(
                nodes
                    .iter()
                    .any(|node| matches!(node, MarkdownNode::Math { .. })),
                "{math}: {nodes:#?}"
            );
        }
    }

    #[test]
    fn math_block_keeps_adjacent_paragraph_lines() {
        let source = "before\n\\[x=1\\]\nafter\n";
        let nodes = parse_markdown(source).nodes;
        let text = nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Text { lines, .. } => Some(
                    lines
                        .iter()
                        .flat_map(|line| &line.spans)
                        .map(|span| span.text.as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            text.iter().any(|line| line.contains("before")),
            "{nodes:#?}"
        );
        assert!(text.iter().any(|line| line.contains("after")), "{nodes:#?}");
        assert!(nodes
            .iter()
            .any(|node| matches!(node, MarkdownNode::Math { .. })));
    }

    #[test]
    fn math_operators_are_not_markdown_emphasis() {
        for source in [r"\(a*b*c\)", r"$a*b*c$"] {
            let spans = crate::content::highlight::parse_inline_spans(source, false);
            let math: Vec<_> = spans.iter().filter(|span| span.math.is_some()).collect();
            assert_eq!(math.len(), 1, "{source}: {spans:#?}");
            assert_eq!(
                math[0].math.as_deref(),
                Some("a*b*c"),
                "{source}: {spans:#?}"
            );
        }
    }

    #[test]
    fn inline_fraction_fallback_preserves_numerator_and_denominator() {
        let spans = crate::content::highlight::parse_inline_spans(r"$\frac{a}{b}$", false);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].text.contains("(a)/(b)"), "{spans:#?}");
    }

    #[test]
    fn unclosed_math_before_fence_does_not_capture_code_or_later_math() {
        let source = "\\[\n1+2\n```text\n\\]\n```\n\\[\n3+4\n\\]\n";
        let math = parse_markdown(source)
            .nodes
            .into_iter()
            .filter(|node| matches!(node, MarkdownNode::Math { .. }))
            .count();
        assert_eq!(math, 1);
    }

    #[test]
    fn unicode_math_uses_ratex_symbols_and_structures() {
        assert_eq!(inline_math_text(r"\top"), "⊤");
        assert_eq!(inline_math_text(r"A^\top"), "A^⊤");
        assert_eq!(inline_math_text(r"\mathbb{R}"), "ℝ");
        assert_eq!(inline_math_text(r"\def\truth{\top}\truth"), "⊤");
        assert_eq!(inline_math_text(r"\not_a_command{x}"), r"\not_a_command{x}");
        let matrix = math_rows(r"\[\begin{pmatrix}1&2\\3&4\end{pmatrix}\]", 80).join("\n");
        assert!(matrix.contains('1') && matrix.contains('4'), "{matrix}");
        assert!(!matrix.contains(r"\begin"), "{matrix}");
        let sum = math_rows(r"\[\sum_{i=1}^n x_i\]", 80).join("\n");
        assert!(sum.contains('∑') && sum.contains('n'), "{sum}");
        let stacked = math_rows(r"\[\overset{def}{=}\quad\overbrace{a+b}^{n}\]", 80).join("\n");
        assert!(
            stacked.contains("def") && stacked.contains('n'),
            "{stacked}"
        );
        assert!(!stacked.contains(r"\overset"), "{stacked}");
    }

    #[test]
    fn unicode_math_groups_inline_scripts() {
        assert_eq!(inline_math_text(r"x^{n+1}"), "xⁿ⁺¹");
        assert_eq!(inline_math_text(r"x_{i+1}"), "xᵢ₊₁");
        assert_eq!(inline_math_text(r"x^{n^2+1}"), "x^(n^2 + 1)");
        assert_eq!(inline_math_text(r"x^{\frac{n+1}{2}}"), "x^((n + 1)/(2))");
        assert_eq!(inline_math_text(r"A^\top"), "A^⊤");
        let flattened = flatten_inline_math(parse_unicode_math(r"x^{n+1}").unwrap());
        assert_eq!(
            term_maths::layout::layout(&flattened).to_string(),
            "x^(n + 1)"
        );
    }

    #[test]
    fn unicode_math_spaces_infix_but_not_unary_operators() {
        for (source, expected) in [
            ("-x", "−x"),
            ("x-y", "x − y"),
            ("a+-b", "a + −b"),
            ("(-x)", "(−x)"),
            ("x=-y", "x = −y"),
        ] {
            let rows = math_rows(source, 80);
            assert_eq!(rows, [expected], "{source}");
        }
    }

    #[test]
    fn unicode_math_preserves_unsupported_formula_source() {
        let source = r"\sqrt[3]{x}+\top";
        assert_eq!(inline_math_text(source), source);
        let display = format!(r"\[{source}\]");
        assert_eq!(math_rows(&display, 80), [display]);
    }

    #[test]
    fn unicode_math_preserves_boxes_and_fractions() {
        let rows = math_rows(
            r"\[\frac{\partial E}{\partial\theta_0}\approx\boxed{0.37062}\]",
            80,
        );
        assert!(rows.iter().any(|row| row.contains('─')));
        assert!(rows.iter().any(|row| row.contains("⟦0.37062⟧")));
        assert!(rows.iter().any(|row| row.contains("∂θ₀")));
    }

    #[test]
    fn narrow_display_math_preserves_fraction_alignment() {
        let input = r"\[
\frac{\partial E}{\partial\theta_0}
=\frac{(0.5-0)+(0.73106-0)+(0.88080-1)}{3}
\approx\boxed{0.37062}.
\]";
        let rows = math_rows(input, 56);
        assert!(rows
            .iter()
            .all(|line| smelt_buffer::cell_width::text_width(line) <= 56));
        assert!(rows.iter().any(|line| line.contains("⟦0.37062⟧")));
        let fraction_rows = rows.iter().filter(|line| line.contains('─')).count();
        assert_eq!(fraction_rows, 2, "{rows:#?}");
        assert!(
            rows.iter()
                .all(|line| !line.trim_start().starts_with("+ (0.88080")),
            "{rows:#?}"
        );
    }

    #[test]
    fn slope_fraction_wraps_after_denominator() {
        let input = r"\[\frac{0(0.5-0)+1(0.73106-0)+2(0.88080-1)}{3}\approx0.16422.\]";
        assert!(parse_markdown(input)
            .nodes
            .iter()
            .any(|node| matches!(node, MarkdownNode::Math { .. })));
        for width in [56, 72, 90, 120] {
            let rows = math_rows(input, width);
            let bar = rows.iter().position(|line| line.contains('─')).unwrap();
            assert!(rows[bar - 1].contains("0(0.5"), "width={width}: {rows:#?}");
            assert!(
                rows[bar + 1].trim().contains('3'),
                "width={width}: {rows:#?}"
            );
            assert!(
                rows[bar + 2].contains("≈ 0.16422"),
                "width={width}: {rows:#?}"
            );
        }
    }

    #[test]
    fn paragraph_math_supports_both_inline_delimiters() {
        let source = r"The contribution is \((\lambda/n)\theta_1\) and $\alpha=0.1$.";
        let nodes = parse_markdown(source).nodes;
        let text = nodes
            .iter()
            .find_map(|node| match node {
                MarkdownNode::Text { lines, .. } => Some(
                    lines[0]
                        .spans
                        .iter()
                        .map(|span| span.text.as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .unwrap();
        assert!(text.contains('λ'), "{text}");
        assert!(text.contains('θ'), "{text}");
        assert!(text.contains('α'), "{text}");
        assert!(!text.contains(r"\("), "{text}");
    }

    #[test]
    fn escaped_math_delimiters_and_entities_keep_markdown_text() {
        let source = r"&amp; \\(literal\\) and \(x\)";
        let spans = crate::content::highlight::parse_inline_spans(source, false);
        let rendered: String = spans.iter().map(|span| span.text.as_str()).collect();
        assert!(rendered.contains('&'), "{rendered:?}");
        assert!(rendered.contains(r"\(literal\)"), "{rendered:?}");
        assert!(rendered.contains('x'), "{rendered:?}");
    }

    #[test]
    fn inline_math_coexists_with_code_and_emphasis() {
        let source = "**value \\(\\alpha\\)** and `\\(literal\\)` then \\(\\beta\\).";
        let nodes = parse_markdown(source).nodes;
        let spans = nodes
            .iter()
            .find_map(|node| match node {
                MarkdownNode::Text { lines, .. } => Some(&lines[0].spans),
                _ => None,
            })
            .unwrap();
        assert!(
            spans
                .iter()
                .any(|span| span.text.contains('α') && span.style.bold),
            "{spans:#?}"
        );
        assert!(
            spans.iter().any(|span| span.text.contains(r"\(literal\)")),
            "{spans:#?}"
        );
        assert!(
            spans.iter().any(|span| span.text.contains('β')),
            "{spans:#?}"
        );
    }

    #[test]
    fn parse_markdown_keeps_inline_style_across_source_lines() {
        let source = "para **bold\nstill** tail\n";
        let block = parse_markdown(source);
        let lines = block
            .nodes
            .iter()
            .find_map(|node| match node {
                MarkdownNode::Text { lines, .. } => Some(lines),
                _ => None,
            })
            .expect("text node");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[1].text, "bold");
        assert!(lines[0].spans[1].style.bold);
        assert_eq!(lines[1].spans[0].text, "still");
        assert!(lines[1].spans[0].style.bold);
        assert_eq!(lines[1].spans[1].text, " tail");
        assert!(!lines[1].spans[1].style.bold);
    }
    #[test]
    fn parse_markdown_preserves_nested_structural_prefixes() {
        let source = "# Title\n\n> - item\n";
        let block = parse_markdown(source);
        let rendered_lines: Vec<(MarkdownTextKind, String)> = block
            .nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Text { kind, lines, .. } => Some((*kind, lines)),
                _ => None,
            })
            .flat_map(|(kind, lines)| {
                lines.iter().map(move |line| {
                    let text = line.spans.iter().map(|span| span.text.as_str()).collect();
                    (kind, text)
                })
            })
            .collect();

        assert_eq!(
            rendered_lines,
            vec![
                (MarkdownTextKind::Heading, "# Title".into()),
                (MarkdownTextKind::BlockQuote, "> - item".into()),
            ]
        );
    }

    #[test]
    fn parse_markdown_excludes_inline_markup_from_structural_prefixes() {
        let source = concat!(
            "### **Option 4: Keep Core but migrate from mdadm/ext4 to ZFS**\n\n",
            "> *quoted emphasis*\n",
        );
        let block = parse_markdown(source);
        let rendered_lines: Vec<String> = block
            .nodes
            .iter()
            .filter_map(|node| match node {
                MarkdownNode::Text { lines, .. } => Some(lines),
                _ => None,
            })
            .flatten()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect();

        assert_eq!(
            rendered_lines,
            vec![
                "### Option 4: Keep Core but migrate from mdadm/ext4 to ZFS",
                "> quoted emphasis",
            ]
        );
    }

    #[test]
    fn ends_with_heading_matches_markdown_tail_blocks() {
        assert!(ends_with_heading("Paragraph\n\n# Tail\n"));
        assert!(ends_with_heading("Paragraph\n---\n"));
        assert!(!ends_with_heading("Paragraph\n\n---\n"));
        assert!(!ends_with_heading("# Not tail\n\nParagraph"));
        assert!(!ends_with_heading("> # Quoted heading\n"));
        assert!(!ends_with_heading("```markdown\n# Not heading\n```"));
    }

    #[test]
    fn parse_markdown_extracts_rule() {
        let source = "before\n\n---\n\nafter";
        let block = parse_markdown(source);
        assert!(block.nodes.iter().any(
            |node| matches!(node, MarkdownNode::Rule { range } if source[range.clone()].trim() == "---")
        ));
    }
}
