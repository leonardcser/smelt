use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::content::ColumnAlignment;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownBlock<'a> {
    pub source: &'a str,
    pub nodes: Vec<MarkdownNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkdownNode {
    Source {
        range: Range<usize>,
    },
    Text {
        range: Range<usize>,
        kind: MarkdownTextKind,
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
            SpecialBlock::Text { range, kind } => MarkdownNode::Text { range, kind },
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
    let mut specials = collect_special_blocks(source);
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

fn collect_special_blocks(source: &str) -> Vec<SpecialBlock> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = Parser::new_ext(source, options).into_offset_iter();
    let mut out = Vec::new();
    let mut text_stack: Vec<OpenText> = Vec::new();
    let mut code: Option<(usize, String, Vec<Range<usize>>)> = None;
    let mut table: Option<TableBuild> = None;

    for (event, range) in parser {
        match event {
            Event::Start(tag) if markdown_text_kind(&tag).is_some() => {
                text_stack.push(OpenText {
                    start: range.start,
                    kind: markdown_text_kind(&tag).unwrap(),
                    end: tag_end_for_text(&tag),
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
                out.push(SpecialBlock::Text {
                    range: open.start..range.end,
                    kind: open.kind,
                });
            }
            Event::Text(_) => {
                if let Some((_, _, body)) = code.as_mut() {
                    body.push(range);
                }
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
            _ => {}
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

struct OpenText {
    start: usize,
    kind: MarkdownTextKind,
    end: TagEnd,
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
    fn parse_markdown_extracts_rule() {
        let source = "before\n\n---\n\nafter";
        let block = parse_markdown(source);
        assert!(block.nodes.iter().any(
            |node| matches!(node, MarkdownNode::Rule { range } if source[range.clone()].trim() == "---")
        ));
    }
}
