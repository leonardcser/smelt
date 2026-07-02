use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::content::highlight::{
    lower_inline_event_lines_with_options, InlineOptions, InlineSpan, InlineStyle,
};
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
            | SpecialBlock::Table { range, .. }
            | SpecialBlock::Rule { range } => range.clone(),
        }
    }

    fn into_node(self) -> MarkdownNode {
        match self {
            SpecialBlock::Text { range, kind, lines } => MarkdownNode::Text { range, kind, lines },
            SpecialBlock::Code { range, lang, body } => MarkdownNode::Code { range, lang, body },
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

pub fn parse_markdown(source: &str) -> MarkdownBlock<'_> {
    parse_markdown_with_options(source, &InlineOptions::default())
}

pub fn parse_markdown_with_options<'a>(
    source: &'a str,
    inline_options: &InlineOptions,
) -> MarkdownBlock<'a> {
    let mut specials = collect_special_blocks(source, inline_options);
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

pub fn ends_with_heading(source: &str) -> bool {
    parse_last_markdown_block_kind(source) == Some(MarkdownTextKind::Heading)
}

fn parse_last_markdown_block_kind(source: &str) -> Option<MarkdownTextKind> {
    let mut previous_adjacent: Option<(&str, bool)> = None;
    let mut adjacent_candidate: Option<(&str, bool)> = None;
    let mut last_non_empty: Option<(&str, bool)> = None;
    let mut fence: Option<(char, usize)> = None;

    for line in source.lines() {
        let body = strip_markdown_indent(line.trim_end());
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
    let spaces = line.bytes().take_while(|b| *b == b' ').take(3).count();
    &line[spaces..]
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

fn is_atx_heading(line: &str) -> bool {
    let line = strip_markdown_indent(line.trim_end());
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    line.as_bytes()
        .get(hashes)
        .is_none_or(|b| b.is_ascii_whitespace())
}

fn is_setext_underline(line: &str) -> bool {
    let line = strip_markdown_indent(line.trim_end());
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

fn is_thematic_break(line: &str) -> bool {
    let line = strip_markdown_indent(line.trim_end());
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
}

fn collect_special_blocks(source: &str, inline_options: &InlineOptions) -> Vec<SpecialBlock> {
    let options = markdown_options();
    let parser = Parser::new_ext(source, options).into_offset_iter();
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
    let inline_lines = lower_inline_event_lines_with_options(
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
    let trimmed = line.trim_start();
    let prefix_start = line_range.start + line.len() - trimmed.len();
    let prefix_end = events
        .iter()
        .filter_map(|(event, range)| visible_event_start(event).then_some(range.start))
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
        }]
    }
}

fn visible_event_start(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Text(_)
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
    source.get(range).unwrap_or("").trim().to_string()
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
