//! Buffer parser for the prompt input: projects attachments / @refs into
//! display rows, highlights them, and writes source↔display coord maps to the
//! buffer for cursor mapping. Window layout owns soft wrapping.

use crate::content::prompt_spans::{
    build_char_kinds, build_display_spans, spans_to_string, Span, SpanKind, TAB_DISPLAY,
};
use smelt_buffer::attachment::{AttachmentId, AttachmentStore, ATTACHMENT_MARKER};
use smelt_buffer::buffer::{
    Buffer, BufferCopy, BufferParser, CopyOutput, LineCursorPolicy, LineDecoration, SpanMeta,
};
use smelt_buffer::cell_width;
use smelt_buffer::coords::ProjectionMaps;
use smelt_core::theme::intern;
use std::sync::{Arc, Mutex};

pub struct PromptBufferParser {
    store: Arc<Mutex<AttachmentStore>>,
    placeholder: Arc<Mutex<Option<String>>>,
}

impl PromptBufferParser {
    #[cfg(test)]
    pub fn new(store: Arc<Mutex<AttachmentStore>>) -> Self {
        Self::with_placeholder(store, Arc::new(Mutex::new(None)))
    }

    pub fn with_placeholder(
        store: Arc<Mutex<AttachmentStore>>,
        placeholder: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self { store, placeholder }
    }
}

/// Yank transform for the prompt: `kill_ring` keeps raw `\u{FFFC}` markers
/// (so vim `y`/`p` round-trips preserve attachments), while `clipboard`
/// expands each marker to its `[label]` for human-readable paste.
pub struct PromptCopier {
    store: Arc<Mutex<AttachmentStore>>,
}

impl PromptCopier {
    pub fn new(store: Arc<Mutex<AttachmentStore>>) -> Self {
        Self { store }
    }
}

impl BufferCopy for PromptCopier {
    fn copy(&self, buf: &Buffer, src: &str, range: std::ops::Range<usize>) -> CopyOutput {
        let raw = &src[range.start..range.end];
        self.expand_attachments(buf, src, raw, range.start)
    }
}

impl PromptCopier {
    fn expand_attachments(
        &self,
        buf: &Buffer,
        src: &str,
        raw: &str,
        range_start: usize,
    ) -> CopyOutput {
        if !raw.contains(ATTACHMENT_MARKER) {
            return CopyOutput::same(raw.to_string());
        }
        // Count markers before `range_start` to align with `buf.attachment_ids`.
        let mut att_idx = src[..range_start]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let store = self.store.lock().unwrap();
        let mut clipboard = String::with_capacity(raw.len());
        for ch in raw.chars() {
            if ch == ATTACHMENT_MARKER {
                let label = buf
                    .attachment_ids
                    .get(att_idx)
                    .map(|&id| store.display_label(id))
                    .unwrap_or_else(|| "[?]".into());
                clipboard.push_str(&label);
                att_idx += 1;
            } else {
                clipboard.push(ch);
            }
        }
        CopyOutput {
            kill_ring: raw.to_string(),
            clipboard,
        }
    }
}

/// Build source-char ↔ display-char maps from a span stream.
/// Returns `(source_char_to_display_char, display_char_to_source_char)`.
fn build_coord_maps(spans: &[Span]) -> (Vec<usize>, Vec<usize>) {
    fn map_expanded_source_char(
        s2d: &mut Vec<usize>,
        d2s: &mut Vec<usize>,
        s_cur: &mut usize,
        d_cur: &mut usize,
        display_chars: usize,
    ) {
        let source_char = *s_cur;
        *s_cur += 1;
        if display_chars == 0 {
            s2d.push(*d_cur);
            return;
        }
        for _ in 0..display_chars.saturating_sub(1) {
            *d_cur += 1;
            d2s.push(source_char);
        }
        *d_cur += 1;
        d2s.push(*s_cur);
        s2d.push(*d_cur);
    }

    let mut s2d = vec![0usize];
    let mut d2s = vec![0usize];
    let mut s_cur = 0usize;
    let mut d_cur = 0usize;
    for span in spans {
        match span {
            Span::Plain(t) | Span::AtRef(t) => {
                for _ in t.chars() {
                    s_cur += 1;
                    d_cur += 1;
                    s2d.push(d_cur);
                    d2s.push(s_cur);
                }
            }
            Span::Tab => map_expanded_source_char(
                &mut s2d,
                &mut d2s,
                &mut s_cur,
                &mut d_cur,
                TAB_DISPLAY.chars().count(),
            ),
            Span::Attachment(label) => map_expanded_source_char(
                &mut s2d,
                &mut d2s,
                &mut s_cur,
                &mut d_cur,
                label.chars().count(),
            ),
        }
    }
    (s2d, d2s)
}

/// Split the flat display buffer into logical display rows while carrying the
/// per-display-char span kind used for highlighting. Newline characters delimit
/// rows and are not part of any rendered row. Row offsets are in the flat
/// display-character stream, including newline delimiters.
fn display_rows_with_kinds_and_offsets(
    buf: &str,
    char_kinds: &[SpanKind],
) -> (Vec<(String, Vec<SpanKind>)>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut offsets = vec![0];
    let mut line = String::new();
    let mut kinds = Vec::new();
    let mut chars_seen = 0usize;
    for (idx, ch) in buf.chars().enumerate() {
        chars_seen += 1;
        if ch == '\n' {
            rows.push((std::mem::take(&mut line), std::mem::take(&mut kinds)));
            offsets.push(chars_seen);
        } else {
            line.push(ch);
            kinds.push(char_kinds.get(idx).copied().unwrap_or(SpanKind::Plain));
        }
    }
    rows.push((line, kinds));
    (rows, offsets)
}

struct PromptDisplayRows {
    spans: Vec<Span>,
    visual_lines: Vec<(String, Vec<SpanKind>)>,
    row_offsets: Vec<usize>,
    ghost: bool,
}

pub(crate) fn build_prompt_display_lines(
    source: &str,
    attachment_ids: &[AttachmentId],
    store: &AttachmentStore,
    placeholder: Option<&str>,
) -> Vec<String> {
    build_prompt_display_rows(source, attachment_ids, store, placeholder)
        .visual_lines
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

pub(crate) fn prompt_display_uses_cursor_padding(source: &str, placeholder: Option<&str>) -> bool {
    !source.is_empty() || placeholder.is_none_or(str::is_empty)
}

fn build_prompt_display_rows(
    source: &str,
    attachment_ids: &[AttachmentId],
    store: &AttachmentStore,
    placeholder: Option<&str>,
) -> PromptDisplayRows {
    if source.is_empty() {
        if let Some(text) = placeholder.filter(|text| !text.is_empty()) {
            return PromptDisplayRows {
                spans: Vec::new(),
                visual_lines: vec![(
                    text.to_string(),
                    vec![SpanKind::Plain; text.chars().count()],
                )],
                row_offsets: vec![0],
                ghost: true,
            };
        }
    }
    let spans = build_display_spans(source, attachment_ids, store);
    let display_buf = spans_to_string(&spans);
    let char_kinds = build_char_kinds(&spans);
    let (visual_lines, row_offsets) =
        display_rows_with_kinds_and_offsets(&display_buf, &char_kinds);
    PromptDisplayRows {
        spans,
        visual_lines,
        row_offsets,
        ghost: false,
    }
}

impl BufferParser for PromptBufferParser {
    fn parse(&self, buf: &mut Buffer, source: &str, _width: u16) {
        let placeholder = self.placeholder.lock().unwrap().clone();
        let store = self.store.lock().unwrap();
        let PromptDisplayRows {
            spans,
            visual_lines,
            row_offsets,
            ghost,
        } = build_prompt_display_rows(source, &buf.attachment_ids, &store, placeholder.as_deref());
        drop(store);

        let lines: Vec<String> = visual_lines.iter().map(|(l, _)| l.clone()).collect();

        // Determine if we need special command/exec styling on the first line.
        let command_token_end = smelt_core::commands::registered_command_token(source.trim())
            .map(|token| token.chars().count())
            .unwrap_or(0);
        let single_line = !source.contains('\n');
        let is_exec =
            single_line && matches!(source.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = single_line && source == "!";

        // Set lines and clear old highlights.
        buf.set_all_lines(lines);
        let line_count = buf.line_count();
        buf.clear_highlights(0, line_count.max(1));

        if ghost {
            let ghost_group = intern("GhostText");
            for (li, (line, _)) in visual_lines.iter().enumerate() {
                buf.set_decoration(
                    li,
                    LineDecoration {
                        cursor_policy: LineCursorPolicy::PreserveRequested,
                        ..Default::default()
                    },
                );
                let width = smelt_buffer::text::byte_to_cell(line, line.len())
                    .min(u16::MAX as usize) as u16;
                if width > 0 {
                    buf.add_highlight_group_with_meta(
                        li,
                        0,
                        width,
                        ghost_group,
                        SpanMeta::unselectable(),
                    );
                }
            }
            let display_chars = visual_lines
                .iter()
                .map(|(line, _)| line.chars().count())
                .sum::<usize>();
            buf.set_projection_maps(ProjectionMaps {
                source_char_to_display_char: vec![0],
                display_char_to_source_char: vec![0; display_chars + 1],
                row_offsets,
            });
            return;
        }

        // Theme-group extmarks: the renderer resolves group → style at paint
        // time, so live theme updates flow through without re-parsing.
        let accent_group = intern("SmeltAccent");
        let exec_group = intern("SmeltExecPrefix");

        for (li, (line, kinds)) in visual_lines.iter().enumerate() {
            let mut col = 0u16;
            for (i, (ch, kind)) in line.chars().zip(kinds.iter()).enumerate() {
                let ch_width = cell_width::char_width_u16(ch);
                let next_col = col.saturating_add(ch_width);
                match kind {
                    SpanKind::Attachment | SpanKind::AtRef => {
                        if next_col > col {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                next_col,
                                accent_group,
                                SpanMeta::default(),
                            );
                        }
                    }
                    SpanKind::Plain => {
                        if li == 0 && i < command_token_end && next_col > col {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                next_col,
                                accent_group,
                                SpanMeta::default(),
                            );
                        }
                        if (is_exec || is_exec_invalid) && li == 0 && i == 0 && ch == '!' {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                next_col,
                                exec_group,
                                SpanMeta::default(),
                            );
                        }
                    }
                }
                col = next_col;
            }
        }

        let (s2d, d2s) = build_coord_maps(&spans);
        buf.set_projection_maps(ProjectionMaps {
            source_char_to_display_char: s2d,
            display_char_to_source_char: d2s,
            row_offsets,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_buffer::buffer::BufCreateOpts;

    fn make_buf_with_parser(parser: Arc<dyn BufferParser>) -> Buffer {
        let mut buf = Buffer::new(smelt_buffer::buffer::BufId(0), BufCreateOpts::default());
        buf.set_parser(parser);
        buf
    }

    #[test]
    fn parser_keeps_prompt_lines_unwrapped() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("a".repeat(79));
        buf.ensure_rendered_at(10);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0).unwrap().chars().count(), 79);
    }

    #[test]
    fn parser_splits_source_newlines_only() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("hello world\nsecond".into());
        buf.ensure_rendered_at(10);
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.get_line(0).unwrap(), "hello world");
        assert_eq!(buf.get_line(1).unwrap(), "second");
    }

    #[test]
    fn parser_expands_tabs_for_display_without_changing_source_mapping() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("a\tb".into());
        buf.ensure_rendered_at(80);

        assert_eq!(buf.get_line(0), Some("a    b"));
        assert_eq!(buf.display_cursor_pos(2), (0, 5));
        assert_eq!(buf.byte_at_display_pos(0, 5), 2);
    }

    #[test]
    fn parser_renders_empty_prompt_placeholder_as_ghost_display() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let placeholder = Arc::new(Mutex::new(Some("ghost prediction wraps".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(store, placeholder));
        let mut buf = make_buf_with_parser(parser);

        buf.ensure_rendered_at(10);

        assert_eq!(buf.get_line(0), Some("ghost prediction wraps"));
        assert_eq!(buf.byte_at_display_pos(0, 10), 0);
        assert!(buf
            .highlights_at(0)
            .iter()
            .any(|span| !span.meta.selectable));
        assert_eq!(
            buf.decoration_at(0).cursor_policy,
            LineCursorPolicy::PreserveRequested
        );
    }

    #[test]
    fn parser_clears_stale_placeholder_when_placeholder_disappears() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let placeholder = Arc::new(Mutex::new(Some("ghost".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            store,
            placeholder.clone(),
        ));
        let mut buf = make_buf_with_parser(parser);
        buf.ensure_rendered_at(10);

        *placeholder.lock().unwrap() = None;
        buf.invalidate_render_cache();
        buf.ensure_rendered_at(10);

        assert_eq!(buf.lines().len(), 1);
        assert!(buf.lines()[0].is_empty());
        assert_eq!(buf.byte_at_display_pos(0, 10), 0);
    }

    #[test]
    fn parser_ignores_placeholder_when_source_is_nonempty() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let placeholder = Arc::new(Mutex::new(Some("ghost".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(store, placeholder));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("real".into());

        buf.ensure_rendered_at(10);

        assert_eq!(buf.get_line(0), Some("real"));
        assert!(buf.highlights_at(0).iter().all(|span| span.meta.selectable));
    }

    #[test]
    fn parser_highlights_multiline_command_token_only() {
        let _g = crate::COMMAND_RESOLVER_GUARD.lock().unwrap();
        smelt_core::commands::set_command_resolver(|name| name == "simplify");
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("/simplify first line\nsecond line".into());
        buf.ensure_rendered_at(80);

        let first_row = buf.highlights_at(0);
        assert!(
            first_row.iter().any(|h| h.col_start == 0 && h.col_end == 1),
            "expected /simplify to be highlighted, got {first_row:?}"
        );
        assert!(
            first_row.iter().all(|h| h.col_end <= 9),
            "expected command arguments to stay unhighlighted, got {first_row:?}"
        );
        assert!(
            buf.highlights_at(1).is_empty(),
            "expected following lines to stay unhighlighted, got {:?}",
            buf.highlights_at(1)
        );
    }

    #[test]
    fn parser_maps_cursor_past_attachment() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let id = store
            .lock()
            .unwrap()
            .insert_image("img.png".into(), "data:xx".into());
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser.clone());
        let source = format!("a{ATTACHMENT_MARKER}b");
        buf.set_source(source.clone());
        buf.attachment_ids.push(id);
        buf.ensure_rendered_at(80);

        // Cursor at start of "b" (byte offset after marker).
        let marker_len = ATTACHMENT_MARKER.len_utf8();
        let b_byte = 1 + marker_len;
        let (row, col) = buf.display_cursor_pos(b_byte);
        assert_eq!(row, 0);
        // "a" + "[img.png]" = 1 + 9 = 10 chars, so "b" is at col 10.
        assert_eq!(col, 10);

        // Reverse: display pos (0, 10) → source byte at "b".
        let byte = buf.byte_at_display_pos(0, 10);
        assert_eq!(byte, b_byte);
        // Reference the parser so the borrow-checker doesn't optimize out the keep-alive.
        let _ = &parser;
    }

    #[test]
    fn prompt_copier_passes_through_plain_text() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let copier = PromptCopier::new(store.clone());
        let mut buf = Buffer::new(smelt_buffer::buffer::BufId(0), BufCreateOpts::default());
        buf.set_copier(Arc::new(copier));
        buf.set_source("hello world".into());
        let out = buf.copy_range(0..5);
        assert_eq!(out.kill_ring, "hello");
        assert_eq!(out.clipboard, "hello");
    }

    #[test]
    fn prompt_copier_expands_attachment_marker_in_clipboard() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let id = store
            .lock()
            .unwrap()
            .insert_image("photo.png".into(), "data:xx".into());
        let copier = PromptCopier::new(store.clone());
        let mut buf = Buffer::new(smelt_buffer::buffer::BufId(0), BufCreateOpts::default());
        buf.set_copier(Arc::new(copier));
        let source = format!("a{ATTACHMENT_MARKER}b");
        buf.set_source(source.clone());
        buf.attachment_ids.push(id);
        let out = buf.copy_range(0..source.len());
        // Kill ring keeps the raw marker so vim paste-back preserves the attachment.
        assert_eq!(out.kill_ring, source);
        // Clipboard expands to the human-readable label.
        assert_eq!(out.clipboard, "a[photo.png]b");
    }

    #[test]
    fn prompt_copier_range_inside_attachment_marker_only() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let id = store
            .lock()
            .unwrap()
            .insert_image("img.png".into(), "data:yy".into());
        let copier = PromptCopier::new(store.clone());
        let mut buf = Buffer::new(smelt_buffer::buffer::BufId(0), BufCreateOpts::default());
        buf.set_copier(Arc::new(copier));
        let source = format!("xy{ATTACHMENT_MARKER}z");
        buf.set_source(source.clone());
        buf.attachment_ids.push(id);
        // Range covers only the marker - attachment index aligns with markers before `range.start`.
        let marker_start = 2;
        let marker_end = marker_start + ATTACHMENT_MARKER.len_utf8();
        let out = buf.copy_range(marker_start..marker_end);
        assert_eq!(out.kill_ring, ATTACHMENT_MARKER.to_string());
        assert_eq!(out.clipboard, "[img.png]");
    }

    struct TestParser;
    impl BufferParser for TestParser {
        fn parse(&self, buf: &mut Buffer, _source: &str, _width: u16) {
            buf.set_all_lines(vec!["hello @ref".into()]);
            buf.add_highlight(0, 6, 10, smelt_term::Style::default());
        }
    }

    #[test]
    fn parser_can_add_highlight() {
        let parser: Arc<dyn BufferParser> = Arc::new(TestParser);
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("x".into());
        buf.ensure_rendered_at(80);
        let hls = buf.highlights_at(0);
        assert_eq!(hls.len(), 1);
        assert_eq!(hls[0].col_start, 6);
        assert_eq!(hls[0].col_end, 10);
    }

    #[test]
    fn parser_highlight_attachment() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let id = store
            .lock()
            .unwrap()
            .insert_image("img.png".into(), "data:xx".into());
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser.clone());
        let source = format!("hello {ATTACHMENT_MARKER}");
        buf.set_source(source);
        buf.attachment_ids.push(id);
        buf.ensure_rendered_at(80);
        let hls = buf.highlights_at(0);
        // Each char of "[img.png]" gets its own highlight (per-character granularity).
        assert!(
            hls.iter().any(|h| h.col_start == 6 && h.col_end == 7),
            "expected highlight for attachment, got {:?}",
            hls
        );
        assert!(
            hls.iter().any(|h| h.col_start == 14 && h.col_end == 15),
            "expected highlight for attachment end, got {:?}",
            hls
        );
    }

    #[test]
    fn manual_highlight_roundtrips() {
        let mut buf = Buffer::new(
            smelt_buffer::buffer::BufId(0),
            smelt_buffer::buffer::BufCreateOpts::default(),
        );
        buf.set_all_lines(vec!["hello @ref".into()]);
        buf.add_highlight(0, 6, 10, smelt_term::Style::default());
        let hls = buf.highlights_at(0);
        assert_eq!(hls.len(), 1);
        assert_eq!(hls[0].col_start, 6);
        assert_eq!(hls[0].col_end, 10);
    }
}
