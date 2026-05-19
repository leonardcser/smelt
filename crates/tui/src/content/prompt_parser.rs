//! Buffer parser for the prompt input: wraps lines, highlights attachments / @refs,
//! and writes source↔display coord maps to the buffer for cursor mapping.

use crate::content::selection::{
    build_char_kinds, build_display_spans, spans_to_string, Span, SpanKind,
};
use smelt_buffer::attachment::{AttachmentStore, ATTACHMENT_MARKER};
use smelt_buffer::buffer::{Buffer, BufferCopy, BufferParser, CopyOutput, SpanMeta};
use smelt_buffer::coords::ProjectionMaps;
use smelt_core::theme::intern;
use std::sync::{Arc, Mutex};

pub struct PromptBufferParser {
    store: Arc<Mutex<AttachmentStore>>,
}

impl PromptBufferParser {
    pub fn new(store: Arc<Mutex<AttachmentStore>>) -> Self {
        Self { store }
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
            Span::Attachment(label) => {
                let label_chars = label.chars().count();
                let marker_src = s_cur;
                s_cur += 1;
                if label_chars == 0 {
                    s2d.push(d_cur);
                    continue;
                }
                // Inner label chars attribute to the marker; last entry attributes
                // to the next source char (so byte_at_display_pos at the boundary
                // resolves to the char after the marker).
                for _ in 0..label_chars.saturating_sub(1) {
                    d_cur += 1;
                    d2s.push(marker_src);
                }
                d_cur += 1;
                d2s.push(s_cur);
                s2d.push(d_cur);
            }
        }
    }
    (s2d, d2s)
}

impl BufferParser for PromptBufferParser {
    fn parse(&self, buf: &mut Buffer, source: &str, width: u16) {
        let store = self.store.lock().unwrap();
        let spans = build_display_spans(source, &buf.attachment_ids, &store);
        drop(store);

        let display_buf = spans_to_string(&spans);
        let char_kinds = build_char_kinds(&spans);
        let wrap =
            crate::content::selection::wrap_with_offsets(&display_buf, &char_kinds, width as usize);
        let visual_lines = wrap.visual_lines;
        let row_offsets = wrap.row_offsets;

        let lines: Vec<String> = visual_lines.iter().map(|(l, _)| l.clone()).collect();

        // Determine if we need special command/exec styling on the first line.
        let single_line = !source.contains('\n');
        let is_command = single_line && smelt_core::commands::is_command(source.trim());
        let is_exec =
            single_line && matches!(source.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = single_line && source == "!";

        // Set lines and clear old highlights.
        buf.set_all_lines(lines);
        let line_count = buf.line_count();
        buf.clear_highlights(0, line_count.max(1));

        // Theme-group extmarks: the renderer resolves group → style at paint
        // time, so live theme updates flow through without re-parsing.
        let accent_group = intern("SmeltAccent");
        let exec_group = intern("SmeltExecPrefix");

        // Char index of the first whitespace on the first visual line, which
        // marks the end of the leading `/command` token. Highlighting only
        // extends up to here so a typed argument's first char doesn't pick up
        // the accent.
        let command_token_end = if is_command {
            visual_lines
                .first()
                .and_then(|(line, _)| line.chars().position(|c| c.is_whitespace()))
                .unwrap_or(usize::MAX)
        } else {
            0
        };

        for (li, (line, kinds)) in visual_lines.iter().enumerate() {
            let mut col = 0u16;
            for (i, kind) in kinds.iter().enumerate() {
                let ch = line.chars().nth(i).unwrap_or('\0');
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                match kind {
                    SpanKind::Attachment | SpanKind::AtRef => {
                        if ch_width > 0 {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                col + ch_width,
                                accent_group,
                                SpanMeta::default(),
                            );
                        }
                    }
                    SpanKind::Plain => {
                        if is_command && li == 0 && i < command_token_end && ch_width > 0 {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                col + ch_width,
                                accent_group,
                                SpanMeta::default(),
                            );
                        }
                        if (is_exec || is_exec_invalid) && li == 0 && i == 0 && ch == '!' {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                col + ch_width,
                                exec_group,
                                SpanMeta::default(),
                            );
                        }
                    }
                }
                col += ch_width;
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
    fn parser_reserves_trailing_row_when_line_is_full() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        // 78 chars at width 78: line fills, an empty trailing row is added so
        // a cursor at end-of-line has a visible home (neovim-style wrap).
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("a".repeat(78));
        buf.ensure_rendered_at(78);
        assert_eq!(
            buf.line_count(),
            2,
            "78 ascii chars at width 78 reserves a padding row"
        );
        assert_eq!(buf.get_line(0).unwrap().chars().count(), 78);
        assert_eq!(buf.get_line(1).unwrap(), "");
        // 79 chars wraps the last char to its own row; that row has 1 char and
        // isn't full, so no extra padding row is appended.
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("a".repeat(79));
        buf.ensure_rendered_at(78);
        assert_eq!(buf.line_count(), 2, "79 ascii chars at width 78 wraps");
    }

    #[test]
    fn parser_wraps_plain_text() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("hello world".into());
        buf.ensure_rendered_at(10);
        assert_eq!(buf.line_count(), 2);
        // Word-wraps at the space, so "hello " and "world".
        assert_eq!(buf.get_line(0).unwrap(), "hello ");
        assert_eq!(buf.get_line(1).unwrap(), "world");
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
        // Range covers only the marker — attachment index aligns with markers before `range.start`.
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
