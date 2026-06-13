use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

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
    Code {
        range: Range<usize>,
        lang: String,
        body: Vec<Range<usize>>,
    },
    Table {
        range: Range<usize>,
    },
    Rule {
        range: Range<usize>,
    },
}

#[derive(Clone, Debug)]
enum SpecialBlock {
    Code {
        range: Range<usize>,
        lang: String,
        body: Vec<Range<usize>>,
    },
    Table {
        range: Range<usize>,
    },
    Rule {
        range: Range<usize>,
    },
}

impl SpecialBlock {
    fn range(&self) -> Range<usize> {
        match self {
            SpecialBlock::Code { range, .. }
            | SpecialBlock::Table { range }
            | SpecialBlock::Rule { range } => range.clone(),
        }
    }

    fn into_node(self) -> MarkdownNode {
        match self {
            SpecialBlock::Code { range, lang, body } => MarkdownNode::Code { range, lang, body },
            SpecialBlock::Table { range } => MarkdownNode::Table { range },
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
    let mut code: Option<(usize, String, Vec<Range<usize>>)> = None;
    let mut table_start: Option<usize> = None;

    for (event, range) in parser {
        match event {
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
            Event::Text(_) => {
                if let Some((_, _, body)) = code.as_mut() {
                    body.push(range);
                }
            }
            Event::Start(Tag::Table(_)) => {
                table_start = Some(range.start);
            }
            Event::End(TagEnd::Table) => {
                if let Some(start) = table_start.take() {
                    out.push(SpecialBlock::Table {
                        range: start..range.end,
                    });
                }
            }
            Event::Rule => out.push(SpecialBlock::Rule { range }),
            _ => {}
        }
    }

    out
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
            MarkdownNode::Table { range } => Some(&source[range.clone()]),
            _ => None,
        });
        assert_eq!(
            table_source.map(str::trim_end),
            Some("| a | b |\n| - | - |\n| c | d |")
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
