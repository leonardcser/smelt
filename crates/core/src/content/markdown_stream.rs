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

use crate::transcript_content::{ContentRead, TranscriptContent};
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
            Self::Text => Block::Text {
                content: content.into(),
            },
            Self::Thinking => Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                content: content.into(),
                kind: protocol::ReasoningKind::Raw,
            },
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
    current_visible: VisibleBlock,
    active: Option<ActiveMarkdownBlock>,
}

#[derive(Default)]
struct VisibleBlock {
    id: Option<BlockId>,
    len: usize,
}

impl MarkdownStreamKind {
    fn uses_thinking_section_policy(self) -> bool {
        matches!(self, Self::Thinking)
    }
}

pub(crate) fn normalize_thinking_title_spacing(content: TranscriptContent) -> TranscriptContent {
    let read = content.read();
    let line_count = read.split_line_count();
    let mut kept_runs = Vec::<std::ops::Range<usize>>::new();
    let mut drop_blank_after_title = false;
    let mut changed = false;

    for line in 0..line_count {
        let Some(range) = read.split_line_range(line) else {
            continue;
        };
        let trimmed = read.trimmed_range(range.clone());
        if drop_blank_after_title && trimmed.is_empty() {
            changed = true;
            continue;
        }
        drop_blank_after_title = retained_thinking_title(&read, trimmed);
        if let Some(run) = kept_runs
            .last_mut()
            .filter(|run| run.end.saturating_add(1) == range.start)
        {
            run.end = range.end;
        } else {
            kept_runs.push(range);
        }
    }
    drop(read);
    if changed {
        content.copy_ranges_joined(&kept_runs, "\n")
    } else {
        content
    }
}

fn retained_thinking_title(read: &ContentRead<'_>, trimmed: std::ops::Range<usize>) -> bool {
    if trimmed.len() <= 4
        || read.byte_at(trimmed.start) != Some(b'*')
        || read.byte_at(trimmed.start.saturating_add(1)) != Some(b'*')
        || read.byte_at(trimmed.end.saturating_sub(2)) != Some(b'*')
        || read.byte_at(trimmed.end.saturating_sub(1)) != Some(b'*')
    {
        return false;
    }
    !read
        .trimmed_range(trimmed.start.saturating_add(2)..trimmed.end.saturating_sub(2))
        .is_empty()
}

enum ActiveMarkdownBlock {
    Paragraph {
        content: String,
        visible: VisibleBlock,
    },
    TableCandidate {
        header: String,
    },
    Table {
        content: String,
        row_count: usize,
        visible: VisibleBlock,
    },
    Code {
        content: String,
        fence: MarkdownFence,
        visible: VisibleBlock,
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
            || self.state.current_visible.id.is_some()
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
            Some(ActiveMarkdownBlock::Paragraph { content, visible }) => {
                if opening_fence_candidate(&self.state.current_line) != Candidate::Not
                    || (kind.uses_thinking_section_policy()
                        && thinking_title_candidate(&self.state.current_line) != Candidate::Not)
                {
                    Self::sync_parts(kind, history, visible, &[content]);
                } else {
                    Self::sync_preview(kind, history, visible, content, &self.state.current_line);
                }
            }
            Some(ActiveMarkdownBlock::TableCandidate { header }) => {
                if table_delimiter_candidate(&self.state.current_line) == Candidate::Not {
                    Self::sync_parts(
                        kind,
                        history,
                        &mut self.state.current_visible,
                        &[header, "\n", &self.state.current_line],
                    );
                }
            }
            Some(ActiveMarkdownBlock::Table {
                content,
                row_count,
                visible,
            }) => {
                if *row_count >= 3 {
                    Self::sync_parts(kind, history, visible, &[content]);
                }
            }
            Some(ActiveMarkdownBlock::Code {
                content,
                fence,
                visible,
            }) => {
                if closing_fence_candidate(&self.state.current_line, fence) != Candidate::Not {
                    Self::sync_parts(kind, history, visible, &[content]);
                } else {
                    Self::sync_preview(kind, history, visible, content, &self.state.current_line);
                }
            }
            None => {
                if self.state.current_line.trim().is_empty()
                    || opening_fence_candidate(&self.state.current_line) != Candidate::Not
                    || is_streaming_table_row(&self.state.current_line)
                {
                    return;
                }
                Self::sync_parts(
                    kind,
                    history,
                    &mut self.state.current_visible,
                    &[&self.state.current_line],
                );
            }
        }
    }

    fn process_line(&mut self, history: &mut BlockHistory, line: &str) {
        match self.state.active.take() {
            Some(ActiveMarkdownBlock::Code {
                mut content,
                fence,
                visible,
            }) => {
                append_line(&mut content, line);
                if markdown_closes_fence(&fence, line) {
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, visible });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Code {
                        content,
                        fence,
                        visible,
                    });
                }
            }
            Some(ActiveMarkdownBlock::TableCandidate { header }) => {
                if markdown_table_delimiter(line) {
                    let mut content = header;
                    append_line(&mut content, line);
                    self.state.current_visible = VisibleBlock::default();
                    self.state.active = Some(ActiveMarkdownBlock::Table {
                        content,
                        row_count: 2,
                        visible: VisibleBlock::default(),
                    });
                } else {
                    let visible = std::mem::take(&mut self.state.current_visible);
                    let mut content = header;
                    if !line.trim().is_empty() {
                        append_line(&mut content, line);
                    }
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, visible });
                    if line.trim().is_empty() {
                        self.finish_active(history);
                    }
                }
            }
            Some(ActiveMarkdownBlock::Table {
                mut content,
                mut row_count,
                visible,
            }) => {
                if is_streaming_table_row(line) {
                    append_line(&mut content, line);
                    row_count += 1;
                    self.state.active = Some(ActiveMarkdownBlock::Table {
                        content,
                        row_count,
                        visible,
                    });
                } else if line.trim().is_empty() {
                    self.state.active = Some(ActiveMarkdownBlock::Table {
                        content,
                        row_count,
                        visible,
                    });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Table {
                        content,
                        row_count,
                        visible,
                    });
                    self.finish_active(history);
                    self.process_line(history, line);
                }
            }
            Some(ActiveMarkdownBlock::Paragraph {
                mut content,
                visible,
            }) => {
                if line.trim().is_empty() {
                    if self.kind.uses_thinking_section_policy() {
                        let follows_title = content
                            .rsplit('\n')
                            .find(|line| !line.trim().is_empty())
                            .and_then(thinking_title)
                            .is_some();
                        if !follows_title {
                            append_line(&mut content, line);
                        }
                        self.state.active =
                            Some(ActiveMarkdownBlock::Paragraph { content, visible });
                    } else {
                        self.state.active =
                            Some(ActiveMarkdownBlock::Paragraph { content, visible });
                        self.finish_active(history);
                    }
                } else if self.kind.uses_thinking_section_policy() && thinking_title(line).is_some()
                {
                    Self::finish_text(self.kind, history, visible.id, content);
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph {
                        content: line.to_string(),
                        visible: VisibleBlock::default(),
                    });
                } else if let Some(fence) = markdown_opening_fence(line) {
                    append_line(&mut content, line);
                    self.state.active = Some(ActiveMarkdownBlock::Code {
                        content,
                        fence,
                        visible,
                    });
                } else {
                    append_line(&mut content, line);
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph { content, visible });
                }
            }
            None => {
                let visible = std::mem::take(&mut self.state.current_visible);
                if line.trim().is_empty() {
                    return;
                }
                if let Some(fence) = markdown_opening_fence(line) {
                    self.state.active = Some(ActiveMarkdownBlock::Code {
                        content: line.to_string(),
                        fence,
                        visible,
                    });
                } else if is_streaming_table_row(line) {
                    self.state.active = Some(ActiveMarkdownBlock::TableCandidate {
                        header: line.to_string(),
                    });
                } else {
                    self.state.active = Some(ActiveMarkdownBlock::Paragraph {
                        content: line.to_string(),
                        visible,
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
            ActiveMarkdownBlock::Paragraph { content, visible }
            | ActiveMarkdownBlock::Code {
                content, visible, ..
            }
            | ActiveMarkdownBlock::Table {
                content, visible, ..
            } => {
                Self::finish_text(self.kind, history, visible.id, content);
            }
            ActiveMarkdownBlock::TableCandidate { header } => {
                Self::finish_text(self.kind, history, None, header);
            }
        }
    }

    fn sync_preview(
        kind: MarkdownStreamKind,
        history: &mut BlockHistory,
        visible: &mut VisibleBlock,
        content: &str,
        current_line: &str,
    ) {
        if current_line.is_empty() {
            Self::sync_parts(kind, history, visible, &[content]);
        } else {
            Self::sync_parts(kind, history, visible, &[content, "\n", current_line]);
        }
    }

    fn sync_parts(
        kind: MarkdownStreamKind,
        history: &mut BlockHistory,
        visible: &mut VisibleBlock,
        parts: &[&str],
    ) {
        if parts
            .iter()
            .all(|part| part.chars().all(char::is_whitespace))
        {
            return;
        }
        let target_len = parts.iter().map(|part| part.len()).sum::<usize>();
        debug_assert!(visible.len <= target_len);
        if let Some(block_id) = visible.id {
            let mut skip = visible.len;
            let suffixes = parts.iter().copied().filter_map(move |part| {
                if skip >= part.len() {
                    skip -= part.len();
                    None
                } else {
                    let suffix = part
                        .get(skip..)
                        .expect("visible Markdown offset is a UTF-8 boundary");
                    skip = 0;
                    Some(suffix)
                }
            });
            if history
                .append_live_text_segments(block_id, suffixes)
                .is_some()
            {
                visible.len = target_len;
            }
        } else {
            let mut content = String::with_capacity(target_len);
            for part in parts {
                content.push_str(part);
            }
            let new_id = history.push(kind.block(content));
            history.set_status(new_id, Status::Streaming);
            visible.id = Some(new_id);
            visible.len = target_len;
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
            let already_final = history.block(id).is_some_and(|block| match (kind, block) {
                (MarkdownStreamKind::Text, Block::Text { content })
                | (MarkdownStreamKind::Thinking, Block::Thinking { content, .. }) => {
                    content == &trimmed
                }
                _ => false,
            });
            if !already_final {
                history.rewrite(id, kind.block(trimmed));
            }
            history.set_status(id, Status::Done);
        } else if !trimmed.is_empty() {
            history.push(kind.block(trimmed));
        }
    }
}

fn append_line(content: &mut String, line: &str) {
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(line);
}

pub(crate) fn thinking_title(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix("**")?.strip_suffix("**")?;
    (!inner.trim().is_empty()).then_some(inner.trim())
}

fn thinking_title_candidate(line: &str) -> Candidate {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Candidate::Not;
    }
    let Some(inner) = trimmed.strip_prefix("**") else {
        return if "**".starts_with(trimmed) {
            Candidate::Pending
        } else {
            Candidate::Not
        };
    };
    if inner.is_empty() || "**".starts_with(inner) {
        return Candidate::Pending;
    }
    if trimmed.ends_with("**") {
        if thinking_title(line).is_some() {
            Candidate::Complete
        } else {
            Candidate::Not
        }
    } else {
        Candidate::Pending
    }
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

    fn thinking_setup() -> (MarkdownStream, BlockHistory) {
        (MarkdownStream::thinking(), BlockHistory::new())
    }

    fn text_at(history: &BlockHistory, index: usize) -> String {
        match history
            .materialized_block_at(index)
            .expect("materialized test block")
        {
            Block::Text { content } => content.snapshot(),
            block => panic!("expected text block, got {block:?}"),
        }
    }

    fn thinking_at(history: &BlockHistory, index: usize) -> String {
        match history
            .materialized_block_at(index)
            .expect("materialized test block")
        {
            Block::Thinking { content, .. } => content.snapshot(),
            block => panic!("expected thinking block, got {block:?}"),
        }
    }

    fn finalized_blocks(
        input: &str,
        kind: MarkdownStreamKind,
        one_char_chunks: bool,
    ) -> Vec<Block> {
        let mut stream = MarkdownStream::with_kind(kind);
        let mut history = BlockHistory::new();
        if one_char_chunks {
            let mut encoded = [0; 4];
            for ch in input.chars() {
                stream.append(&mut history, ch.encode_utf8(&mut encoded));
            }
        } else {
            stream.append(&mut history, input);
        }
        stream.flush(&mut history);
        (0..history.len())
            .map(|index| {
                history
                    .materialized_block_at(index)
                    .expect("finalized block")
                    .clone()
            })
            .collect()
    }

    #[test]
    fn continuation_delta_emits_append_patch_without_replacement() {
        let (mut stream, mut history) = setup();
        stream.append(&mut history, "a");
        let revision = history.patch_revision();

        stream.append(&mut history, "β");

        let patches = history
            .patches_since(revision)
            .expect("continuation patch is retained")
            .collect::<Vec<_>>();
        assert_eq!(patches.len(), 1);
        let id = history.last_block_id().expect("stream block");
        assert_eq!(
            patches[0].operations,
            vec![crate::transcript_model::TranscriptPatchOperation::Append {
                id,
                content_id: history
                    .content(id, crate::transcript_model::ContentChannel::Primary)
                    .expect("primary content")
                    .id(),
                channel: crate::transcript_model::ContentChannel::Primary,
                byte_range: 1..3,
            }]
        );
    }

    #[test]
    fn arbitrary_character_boundaries_match_one_shot_streaming() {
        let cases = [
            (
                MarkdownStreamKind::Text,
                "intro αβ\n```rust\nfn main() { println!(\"世界\"); }\n```\nafter",
            ),
            (
                MarkdownStreamKind::Text,
                "| name | value |\n| :--- | ---: |\n| café | 東京 |\n\nfinished",
            ),
            (
                MarkdownStreamKind::Thinking,
                "first paragraph\n\n**Assessing 世界**\n\nbody café",
            ),
        ];

        for (kind, input) in cases {
            assert_eq!(
                finalized_blocks(input, kind, true),
                finalized_blocks(input, kind, false),
                "stream result differs for {kind:?}"
            );
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
    fn thinking_preserves_blank_lines_inside_block() {
        let (mut stream, mut history) = thinking_setup();
        stream.append(&mut history, "first paragraph\n\nsecond paragraph");
        stream.flush(&mut history);

        assert_eq!(history.len(), 1);
        assert_eq!(
            thinking_at(&history, 0),
            "first paragraph\n\nsecond paragraph"
        );
    }

    #[test]
    fn thinking_bold_title_starts_new_block() {
        let (mut stream, mut history) = thinking_setup();
        stream.append(
            &mut history,
            "first paragraph\n\nsecond paragraph\n**Assessing directory exclusions**\n\nbody",
        );
        stream.flush(&mut history);

        assert_eq!(history.len(), 2);
        assert_eq!(
            thinking_at(&history, 0),
            "first paragraph\n\nsecond paragraph"
        );
        assert_eq!(
            thinking_at(&history, 1),
            "**Assessing directory exclusions**\nbody"
        );
    }

    #[test]
    fn thinking_title_spacing_removes_blank_before_body() {
        let normalize = |source: &str| {
            normalize_thinking_title_spacing(TranscriptContent::from(source)).snapshot()
        };
        assert_eq!(
            normalize("**Assessing directory exclusions**\n\nbody"),
            "**Assessing directory exclusions**\nbody"
        );
        assert_eq!(
            normalize("first paragraph\n\nsecond paragraph"),
            "first paragraph\n\nsecond paragraph"
        );
        assert_eq!(normalize("body\n"), "body\n");
        assert_eq!(normalize("**title**\r\n\r\nbody"), "**title**\r\nbody");
        assert_eq!(normalize("**title**\n"), "**title**");
    }

    #[test]
    fn thinking_title_spacing_normalizes_retained_chunks_without_a_snapshot() {
        let content = TranscriptContent::new();
        content.append_owned("**Plan**\n".into());
        content.append_owned("\n".into());
        content.append_owned("body\ncontinued".into());

        let normalized = normalize_thinking_title_spacing(content);

        assert_eq!(normalized.snapshot(), "**Plan**\nbody\ncontinued");
        assert_eq!(normalized.read().chunks().len(), 3);
    }

    #[test]
    fn streaming_thinking_title_does_not_preview_as_previous_paragraph() {
        let (mut stream, mut history) = thinking_setup();
        stream.append(&mut history, "previous sentence.\n**Assessing");
        assert_eq!(history.len(), 1);
        assert_eq!(thinking_at(&history, 0), "previous sentence.");

        stream.append(&mut history, " directory exclusions**\nbody");
        stream.flush(&mut history);
        assert_eq!(history.len(), 2);
        assert_eq!(
            thinking_at(&history, 1),
            "**Assessing directory exclusions**\nbody"
        );
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
