use crate::content::inline_line::{BreakPolicy, InlineLine, InlineRun};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBlock {
    lang: String,
    lines: Vec<InlineLine<()>>,
}

impl CodeBlock {
    pub fn lang(&self) -> &str {
        &self.lang
    }

    pub fn lines(&self) -> &[InlineLine<()>] {
        &self.lines
    }

    pub fn line_text(&self, index: usize) -> Option<&str> {
        self.lines
            .get(index)
            .and_then(|line| line.runs.first())
            .map(|run| run.text.as_str())
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

pub fn parse_code_block(lines: &[&str], lang: &str) -> CodeBlock {
    CodeBlock {
        lang: lang.to_string(),
        lines: lines
            .iter()
            .map(|line| {
                InlineLine::new(vec![InlineRun::new(
                    line.replace('\t', "    "),
                    (),
                    BreakPolicy::PreserveSpaces,
                )])
            })
            .collect(),
    }
}

pub fn measure_code_block(block: &CodeBlock, width: usize) -> u16 {
    let text_w = width.max(1);
    block
        .lines()
        .iter()
        .map(|line| line.wrap_rows(text_w) as u16)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_code_block_expands_tabs() {
        let block = parse_code_block(&["\tindented"], "rust");
        assert_eq!(block.line_text(0), Some("    indented"));
    }

    #[test]
    fn measure_code_block_wraps_without_syntax() {
        let line = "x".repeat(100);
        let block = parse_code_block(&[line.as_str()], "rust");
        assert_eq!(measure_code_block(&block, 20), 5);
    }
}
