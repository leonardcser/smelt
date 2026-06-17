//! Incremental Markdown stabilization for transcript text.
//!
//! `MarkdownStream` is intentionally less eager than plain text streaming. It
//! withholds Markdown control lines whose meaning can change as more bytes
//! arrive, such as fence delimiters and table delimiters, and it keeps
//! incomplete table rows out of the visible preview. It is not a Markdown
//! parser; the stabilized subset is limited to fenced code blocks and pipe
//! tables. The stream only exposes content once the Markdown renderer can
//! produce stable rows; final Markdown parsing and formatting still belong to
//! the normal rendering path.

use crate::transcript_model::{Block, BlockHistory, BlockId, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceMarker {
    Backtick,
    Tilde,
}

impl FenceMarker {
    fn byte(self) -> u8 {
        match self {
            Self::Backtick => b'`',
            Self::Tilde => b'~',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownFence {
    pub marker: FenceMarker,
    pub len: usize,
    pub info: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    Not,
    Pending,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkdownStreamKind {
    Text,
    Thinking,
}

impl MarkdownStreamKind {
    fn block(self, content: String) -> Block {
        match self {
            Self::Text => Block::Text { content },
            Self::Thinking => Block::Thinking { content },
        }
    }
}

pub struct MarkdownStream {
    kind: MarkdownStreamKind,
    state: MarkdownStreamState,
}

#[derive(Default)]
struct MarkdownStreamState {
    current_line: String,
    current_line_id: Option<BlockId>,
    active: Option<ActiveMarkdownBlock>,
}

enum ActiveMarkdownBlock {
    Paragraph {
        content: String,
        id: Option<BlockId>,
    },
    TableCandidate {
        header: String,
    },
    Table {
        rows: Vec<String>,
        id: Option<BlockId>,
    },
    Code {
        content: String,
        fence: MarkdownFence,
        id: Option<BlockId>,
    },
}

impl Default for MarkdownStream {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownStream {
    pub fn new() -> Self {
        Self::text()
    }

    pub fn text() -> Self {
        Self::with_kind(MarkdownStreamKind::Text)
    }

    pub fn thinking() -> Self {
        Self::with_kind(MarkdownStreamKind::Thinking)
    }

    fn with_kind(kind: MarkdownStreamKind) -> Self {
        Self {
            kind,
            state: MarkdownStreamState::default(),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.state.current_line.is_empty()
            || self.state.current_line_id.is_some()
            || self.state.active.is_some()
    }

    pub fn clear(&mut self) {
        self.state = MarkdownStreamState::default();
    }

    pub fn append(&mut self, history: &mut BlockHistory, delta: &str) {
        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut self.state.current_line);
                self.process_line(history, &line);
            } else {
                self.state.current_line.push(ch);
            }
        }
        self.sync(history);
    }

    pub fn flush(&mut self, history: &mut BlockHistory) {
        if !self.state.current_line.is_empty() {
            let line = std::mem::take(&mut self.state.current_line);
            self.process_line(history, &line);
        }
        self.finish_active(history);
    }

    fn sync(&mut self, history: &mut BlockHistory) {
        let kind = self.kind;
        match self.state.active.as_mut() {
            Some(ActiveMarkdownBlock::Paragraph { content, id }) => {
                if opening_fence_candidate(&self.state.current_line) != Candidate::Not {
                    Self::sync_text(kind, history, id, content.clone());
                } else {
                    Self::sync_text(
                        kind,
                        history,
                        id,
                        joined_preview(content, &self.state.current_line),
                    );
                }
            }
            Some(ActiveMarkdownBlock::TableCandidate { header }) => {
                match table_delimiter_candidate(&self.state.current_line) {
                    Candidate::Not => {
                        Self::sync_text(
                            kind,
                            history,
                            &mut self.state.current_line_id,
                            joined_preview(header, &self.state.current_line),
                        );
                    }
                    Candidate::Pending | Candidate::Complete => {}
                }
            }
            Some(ActiveMarkdownBlock::Table { rows, id }) => {
                if rows.len() >= 3 {
                    Self::sync_text(kind, history, id, rows.join("\n"));
                }
            }
            Some(ActiveMarkdownBlock::Code { content, fence, id }) => {
                if closing_fence_candidate(&self.state.current_line, fence) != Candidate::Not {
                    Self::sync_text(kind, history, id, content.clone());
                } else {
                    Self::sync_text(
                        kind,
                        history,
                        id,
                        joined_preview(content, &self.state.current_line),
                    );
                }
            }
            None => {
                if self.state.current_line.trim().is_empty()
                    || opening_fence_candidate(&self.state.current_line) != Candidate::Not
                    || is_streaming_table_row(&self.state.current_line)
                {
                    return;
                }
                Self::sync_text(
                    kind,
                    history,
                    &mut self.state.current_line_id,
                    self.state.current_line.clone(),
                );
            }
        }
    }

    fn process_line(&mut self, history: &mut BlockHistory, line: &str) {
        match self.state.active.take() {
            Some(ActiveMarkdownBlock::Code {
                mut content,
                fence,
                id,
            }) => {
                append_line(&mut content, line);
                if markdown_closes_fence(&fence, line) {
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, id });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Code { content, fence, id });
                }
            }
            Some(ActiveMarkdownBlock::TableCandidate { header }) => {
                if markdown_table_delimiter(line) {
                    self.state.active = Some(ActiveMarkdownBlock::Table {
                        rows: vec![header, line.to_string()],
                        id: None,
                    });
                } else {
                    let id = self.state.current_line_id.take();
                    let mut content = header;
                    if !line.trim().is_empty() {
                        append_line(&mut content, line);
                    }
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, id });
                    if line.trim().is_empty() {
                        self.finish_active(history);
                    }
                }
            }
            Some(ActiveMarkdownBlock::Table { mut rows, id }) => {
                if is_streaming_table_row(line) {
                    rows.push(line.to_string());
                    self.state.active = Some(ActiveMarkdownBlock::Table { rows, id });
                } else if line.trim().is_empty() {
                    self.state.active = Some(ActiveMarkdownBlock::Table { rows, id });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Table { rows, id });
                    self.finish_active(history);
                    self.process_line(history, line);
                }
            }
            Some(ActiveMarkdownBlock::Paragraph { mut content, id }) => {
                if line.trim().is_empty() {
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, id });
                    self.finish_active(history);
                } else if let Some(fence) = markdown_opening_fence(line) {
                    append_line(&mut content, line);
                    self.state.active = Some(ActiveMarkdownBlock::Code { content, fence, id });
                } else {
                    append_line(&mut content, line);
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, id });
                }
            }
            None => {
                if line.trim().is_empty() {
                    return;
                }
                if let Some(fence) = markdown_opening_fence(line) {
                    self.state.active = Some(ActiveMarkdownBlock::Code {
                        content: line.to_string(),
                        fence,
                        id: self.state.current_line_id.take(),
                    });
                } else if is_streaming_table_row(line) {
                    self.state.current_line_id = None;
                    self.state.active = Some(ActiveMarkdownBlock::TableCandidate {
                        header: line.to_string(),
                    });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph {
                        content: line.to_string(),
                        id: self.state.current_line_id.take(),
                    });
                }
            }
        }
    }

    fn finish_active(&mut self, history: &mut BlockHistory) {
        let Some(active) = self.state.active.take() else {
            return;
        };
        match active {
            ActiveMarkdownBlock::Paragraph { content, id }
            | ActiveMarkdownBlock::Code { content, id, .. } => {
                Self::finish_text(self.kind, history, id, content);
            }
            ActiveMarkdownBlock::TableCandidate { header } => {
                Self::finish_text(self.kind, history, None, header);
            }
            ActiveMarkdownBlock::Table { rows, id } => {
                Self::finish_text(self.kind, history, id, rows.join("\n"));
            }
        }
    }

    fn sync_text(
        kind: MarkdownStreamKind,
        history: &mut BlockHistory,
        id: &mut Option<BlockId>,
        content: String,
    ) {
        if content.trim().is_empty() {
            return;
        }
        let block = kind.block(content);
        if let Some(id) = *id {
            history.rewrite(id, block);
        } else {
            let new_id = history.push(block);
            history.set_status(new_id, Status::Streaming);
            *id = Some(new_id);
        }
    }

    fn finish_text(
        kind: MarkdownStreamKind,
        history: &mut BlockHistory,
        id: Option<BlockId>,
        content: String,
    ) {
        let trimmed = content.trim().to_string();
        if let Some(id) = id {
            history.rewrite(id, kind.block(trimmed));
            history.set_status(id, Status::Done);
        } else if !trimmed.is_empty() {
            history.push(kind.block(trimmed));
        }
    }
}

fn joined_preview(content: &str, current_line: &str) -> String {
    if current_line.is_empty() {
        content.to_string()
    } else {
        format!("{content}\n{current_line}")
    }
}

fn append_line(content: &mut String, line: &str) {
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(line);
}

pub fn markdown_opening_fence(line: &str) -> Option<MarkdownFence> {
    let (_, marker, len, rest) = scan_fence_run(line)?;
    if len < 3 {
        return None;
    }
    if marker == FenceMarker::Backtick && rest.contains('`') {
        return None;
    }
    Some(MarkdownFence {
        marker,
        len,
        info: rest.trim().to_string(),
    })
}

pub fn markdown_closes_fence(opening: &MarkdownFence, line: &str) -> bool {
    closing_fence_candidate(line, opening) == Candidate::Complete
}

fn opening_fence_candidate(line: &str) -> Candidate {
    if line.bytes().all(|b| b == b' ') && line.len() <= 3 {
        return Candidate::Pending;
    }
    let Some((_, marker, len, rest)) = scan_fence_run(line) else {
        return Candidate::Not;
    };
    if len < 3 {
        return if rest.is_empty() {
            Candidate::Pending
        } else {
            Candidate::Not
        };
    }
    if marker == FenceMarker::Backtick && rest.contains('`') {
        Candidate::Not
    } else {
        Candidate::Complete
    }
}

fn closing_fence_candidate(line: &str, opening: &MarkdownFence) -> Candidate {
    if line.bytes().all(|b| b == b' ') && line.len() <= 3 {
        return Candidate::Pending;
    }
    let Some((_, marker, len, rest)) = scan_fence_run(line) else {
        return Candidate::Not;
    };
    if marker != opening.marker {
        return Candidate::Not;
    }
    if len < opening.len {
        return if rest.is_empty() {
            Candidate::Pending
        } else {
            Candidate::Not
        };
    }
    if rest.trim().is_empty() {
        Candidate::Complete
    } else {
        Candidate::Not
    }
}

fn scan_fence_run(line: &str) -> Option<(usize, FenceMarker, usize, &str)> {
    let bytes = line.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() && bytes[pos] == b' ' && pos < 4 {
        pos += 1;
    }
    if pos > 3 {
        return None;
    }
    let marker = match bytes.get(pos).copied()? {
        b'`' => FenceMarker::Backtick,
        b'~' => FenceMarker::Tilde,
        _ => return None,
    };
    let start = pos;
    while pos < bytes.len() && bytes[pos] == marker.byte() {
        pos += 1;
    }
    Some((start, marker, pos - start, &line[pos..]))
}

pub fn markdown_table_delimiter(line: &str) -> bool {
    table_delimiter_candidate(line) == Candidate::Complete
}

fn is_streaming_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn table_delimiter_candidate(line: &str) -> Candidate {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Candidate::Pending;
    }
    if !trimmed.starts_with('|') {
        return Candidate::Not;
    }

    let body = trimmed.trim_matches('|');
    let mut saw_complete_cell = false;
    let mut all_cells_complete = true;
    for cell in body.split('|') {
        match table_delimiter_cell_candidate(cell) {
            Candidate::Not => return Candidate::Not,
            Candidate::Pending => all_cells_complete = false,
            Candidate::Complete => saw_complete_cell = true,
        }
    }

    if saw_complete_cell && all_cells_complete {
        Candidate::Complete
    } else {
        Candidate::Pending
    }
}

fn table_delimiter_cell_candidate(cell: &str) -> Candidate {
    let trimmed = cell.trim();
    if trimmed.is_empty() {
        return Candidate::Pending;
    }

    let mut hyphens = 0usize;
    for (idx, ch) in trimmed.chars().enumerate() {
        match ch {
            '-' => hyphens += 1,
            ':' if idx == 0 || idx + ch.len_utf8() == trimmed.len() => {}
            _ => return Candidate::Not,
        }
    }

    if hyphens >= 3 {
        Candidate::Complete
    } else {
        Candidate::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (MarkdownStream, BlockHistory) {
        (MarkdownStream::new(), BlockHistory::new())
    }

    fn text_at(history: &BlockHistory, index: usize) -> &str {
        match history.block_at(index) {
            Block::Text { content } => content,
            block => panic!("expected text block, got {block:?}"),
        }
    }

    #[test]
    fn opening_backtick_fence_is_held_until_resolved() {
        let (mut stream, mut history) = setup();
        for chunk in ["`", "`", "`", "rust"] {
            stream.append(&mut history, chunk);
            assert_eq!(history.len(), 0);
        }
        stream.append(&mut history, "\nfn main()");
        assert_eq!(text_at(&history, 0), "```rust\nfn main()");
    }

    #[test]
    fn failed_opening_fence_releases_text() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "``");
        assert_eq!(history.len(), 0);
        stream.append(&mut history, "x");
        assert_eq!(text_at(&history, 0), "``x");
    }

    #[test]
    fn closing_backtick_fence_is_held_while_streaming() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "```rust\nfn main()\n");
        assert_eq!(text_at(&history, 0), "```rust\nfn main()");
        for chunk in ["`", "`", "`"] {
            stream.append(&mut history, chunk);
            assert_eq!(text_at(&history, 0), "```rust\nfn main()");
        }
        stream.append(&mut history, "\nafter");
        assert_eq!(text_at(&history, 0), "```rust\nfn main()\n```\nafter");
    }

    #[test]
    fn closing_fence_with_trailing_spaces_is_held() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "````\ninside\n````   ");
        assert_eq!(text_at(&history, 0), "````\ninside");
        stream.flush(&mut history);
        assert_eq!(text_at(&history, 0), "````\ninside\n````");
    }

    #[test]
    fn failed_closing_fence_releases_as_code() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "```\ninside\n```");
        assert_eq!(text_at(&history, 0), "```\ninside");
        stream.append(&mut history, "text");
        assert_eq!(text_at(&history, 0), "```\ninside\n```text");
    }

    #[test]
    fn tilde_fences_stream_like_backticks() {
        let (mut stream, mut history) = setup();
        for chunk in ["~", "~", "~", "python", "\nprint(1)\n", "~", "~", "~"] {
            stream.append(&mut history, chunk);
        }
        assert_eq!(text_at(&history, 0), "~~~python\nprint(1)");
        stream.flush(&mut history);
        assert_eq!(text_at(&history, 0), "~~~python\nprint(1)\n~~~");
    }

    #[test]
    fn unfinished_code_block_flushes_raw_content() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "```rust\npartial");
        stream.flush(&mut history);
        assert_eq!(text_at(&history, 0), "```rust\npartial");
    }

    #[test]
    fn table_header_delimiter_and_partial_row_are_held_until_resolved() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "| a | b |");
        assert_eq!(history.len(), 0);
        for chunk in ["\n|", "---", "|", "---", "|   ", "\n|"] {
            stream.append(&mut history, chunk);
            assert_eq!(history.len(), 0);
        }
        stream.append(&mut history, " 1 | 2 |");
        assert_eq!(history.len(), 0);
        stream.append(&mut history, "\n");
        assert_eq!(text_at(&history, 0), "| a | b |\n|---|---|   \n| 1 | 2 |");
    }

    #[test]
    fn failed_table_candidate_releases_text() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "| not a table |\nnope");
        assert_eq!(text_at(&history, 0), "| not a table |\nnope");
    }

    #[test]
    fn table_delimiter_matches_common_alignment_forms() {
        assert!(markdown_table_delimiter("| --- | :---: | ---: |"));
        assert!(markdown_table_delimiter("|---|---"));
        assert!(!markdown_table_delimiter("| --- | nope |"));
        assert!(!markdown_table_delimiter("| -- | --- |"));
    }

    #[test]
    fn opening_fence_allows_three_leading_spaces() {
        assert!(markdown_opening_fence("   ```rust").is_some());
        assert!(markdown_opening_fence("    ```rust").is_none());
    }

    #[test]
    fn closing_fence_matches_marker_and_len() {
        let fence = markdown_opening_fence("````rust").unwrap();
        assert!(markdown_closes_fence(&fence, "````   "));
        assert!(markdown_closes_fence(&fence, "`````"));
        assert!(!markdown_closes_fence(&fence, "```"));
        assert!(!markdown_closes_fence(&fence, "~~~~"));
        assert!(!markdown_closes_fence(&fence, "```` text"));
    }
}
