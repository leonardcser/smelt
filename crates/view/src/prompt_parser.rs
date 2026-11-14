//! Buffer parser for the prompt input: projects attachments / @refs into
//! display rows, highlights them, and writes source↔display coord maps to the
//! buffer for cursor mapping. Window layout owns soft wrapping.

use crate::prompt_spans::{
    build_char_kinds, build_display_spans, spans_to_string, Span, SpanKind, TAB_DISPLAY,
};
use smelt_buffer::attachment::{AttachmentId, AttachmentStore, ATTACHMENT_MARKER};
use smelt_buffer::buffer::{
    Buffer, BufferCopy, BufferParser, CopyOutput, LineCursorPolicy, LineDecoration, SpanMeta,
};
use smelt_buffer::cell_width;
use smelt_buffer::coords::{ProjectionCheckpoint, ProjectionMaps};
use smelt_buffer::theme::intern;
use std::sync::{Arc, Mutex};

pub struct PromptBufferParser {
    store: Arc<Mutex<AttachmentStore>>,
    placeholder: Arc<Mutex<Option<String>>>,
    commands: Arc<smelt_core::commands::CommandCatalog>,
}

impl PromptBufferParser {
    #[cfg(any(test, feature = "harness"))]
    pub fn new(store: Arc<Mutex<AttachmentStore>>) -> Self {
        Self::with_placeholder(
            store,
            Arc::new(Mutex::new(None)),
            Arc::new(smelt_core::commands::CommandCatalog::default()),
        )
    }

    pub fn with_placeholder(
        store: Arc<Mutex<AttachmentStore>>,
        placeholder: Arc<Mutex<Option<String>>>,
        commands: Arc<smelt_core::commands::CommandCatalog>,
    ) -> Self {
        Self {
            store,
            placeholder,
            commands,
        }
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
        let range = smelt_buffer::text::snapped_grapheme_range(src, range);
        let raw = smelt_buffer::text::slice(src, range.clone());
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
        let mut att_idx = smelt_buffer::text::slice(src, 0..range_start)
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

/// Build coordinate maps and restart points outside multi-line tokens.
fn build_coord_maps(source: &str, spans: &[Span], row_offsets: Vec<usize>) -> ProjectionMaps {
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
    let mut source_chars = source.chars();
    let mut restart_safe = true;
    let mut source_byte = 0;
    let mut row = 0;
    let mut checkpoints = vec![ProjectionCheckpoint::default()];
    for span in spans {
        if matches!(span, Span::AtQuoteStart(_)) {
            restart_safe = false;
        }
        match span {
            Span::Plain(t) | Span::AtRef(t) | Span::AtToken(t) | Span::AtQuoteStart(t) => {
                for ch in t.chars() {
                    source_byte += source_chars
                        .next()
                        .expect("prompt span source character")
                        .len_utf8();
                    s_cur += 1;
                    d_cur += 1;
                    s2d.push(d_cur);
                    d2s.push(s_cur);
                    if ch == '\n' {
                        row += 1;
                        if restart_safe && matches!(span, Span::Plain(_)) {
                            checkpoints.push(ProjectionCheckpoint {
                                source_byte,
                                source_char: s_cur,
                                display_char: d_cur,
                                row,
                            });
                        }
                    }
                }
            }
            Span::Tab | Span::Attachment(_) => {
                source_byte += source_chars
                    .next()
                    .expect("prompt span source character")
                    .len_utf8();
                let label = match span {
                    Span::Attachment(label) => label.as_str(),
                    _ => TAB_DISPLAY,
                };
                map_expanded_source_char(
                    &mut s2d,
                    &mut d2s,
                    &mut s_cur,
                    &mut d_cur,
                    label.chars().count(),
                );
                row += label.bytes().filter(|&byte| byte == b'\n').count();
            }
        }
    }
    ProjectionMaps {
        source_char_to_display_char: s2d,
        display_char_to_source_char: d2s,
        row_offsets,
        checkpoints,
    }
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
    fn dependency_key(&self, source: &str) -> u64 {
        u64::from(
            self.commands
                .command_token(smelt_buffer::text::trim_whitespace(source))
                .is_some(),
        )
    }

    fn parse(&self, buf: &mut Buffer, source: &str, _width: u16) {
        self.parse_from(buf, source, ProjectionCheckpoint::default());
    }

    fn parse_incremental(&self, buf: &mut Buffer, source: &str, _width: u16, start: usize) -> bool {
        let Some(maps) = buf.projection_maps() else {
            return false;
        };
        let point = maps.checkpoint_at(start);
        if point.row == 0 {
            return false;
        }
        self.parse_from(buf, source, point);
        true
    }
}

impl PromptBufferParser {
    fn parse_from(&self, buf: &mut Buffer, source: &str, point: ProjectionCheckpoint) {
        let suffix = smelt_buffer::text::slice(source, point.source_byte..source.len());
        smelt_perf::perf::record_value("prompt:projected_source_bytes", suffix.len() as u64);
        let attachment_start = smelt_buffer::text::slice(source, 0..point.source_byte)
            .matches(ATTACHMENT_MARKER)
            .count();
        let placeholder = self.placeholder.lock().unwrap().clone();
        let store = self.store.lock().unwrap();
        let PromptDisplayRows {
            spans,
            visual_lines,
            row_offsets,
            ghost,
        } = build_prompt_display_rows(
            suffix,
            buf.attachment_ids
                .get(attachment_start..)
                .unwrap_or_default(),
            &store,
            placeholder.as_deref().filter(|_| point.row == 0),
        );
        drop(store);

        let lines: Vec<String> = visual_lines.iter().map(|(l, _)| l.clone()).collect();

        // Determine if we need special command/exec styling on the first line.
        let command_token_end = if point.row == 0 {
            self.commands
                .command_token(smelt_buffer::text::trim_whitespace(source))
                .map(|token| token.chars().count())
                .unwrap_or(0)
        } else {
            0
        };
        let single_line = point.row == 0 && !source.contains('\n');
        let is_exec =
            single_line && matches!(source.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = single_line && source == "!";

        if point.row == 0 {
            buf.set_all_lines(lines);
        } else {
            buf.set_lines(point.row, buf.line_count(), lines);
        }

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
                checkpoints: Vec::new(),
            });
            return;
        }

        // Theme-group extmarks: the renderer resolves group → style at paint
        // time, so live theme updates flow through without re-parsing.
        let accent_group = intern("SmeltAccent");
        let exec_group = intern("SmeltExecPrefix");

        for (offset, (line, kinds)) in visual_lines.iter().enumerate() {
            let li = point.row + offset;
            let mut col = 0u16;
            let mut char_index = 0usize;
            for grapheme in cell_width::graphemes(line) {
                let kind = kinds.get(char_index).copied().unwrap_or(SpanKind::Plain);
                let width = cell_width::text_width_u16(grapheme);
                let next_col = col.saturating_add(width);
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
                        if li == 0 && char_index < command_token_end && next_col > col {
                            buf.add_highlight_group_with_meta(
                                li,
                                col,
                                next_col,
                                accent_group,
                                SpanMeta::default(),
                            );
                        }
                        if (is_exec || is_exec_invalid)
                            && li == 0
                            && char_index == 0
                            && grapheme == "!"
                        {
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
                char_index += grapheme.chars().count();
            }
        }

        let maps = build_coord_maps(suffix, &spans, row_offsets);
        if point.row == 0 {
            buf.set_projection_maps(maps);
        } else {
            let mut retained = buf
                .take_projection_maps()
                .expect("incremental prompt projection");
            retained.replace_suffix(point, maps);
            buf.set_projection_maps(retained);
        }
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

    fn empty_commands() -> Arc<smelt_core::commands::CommandCatalog> {
        Arc::new(smelt_core::commands::CommandCatalog::default())
    }

    fn assert_matches_full_projection(buf: &Buffer, width: u16) {
        let mut full = buf.clone();
        full.invalidate_render_cache();
        full.ensure_rendered_at(width);
        assert_eq!(buf.lines(), full.lines(), "source: {:?}", buf.source());
        assert_eq!(buf.projection_maps(), full.projection_maps());
        for row in 0..buf.line_count() {
            assert_eq!(buf.highlights_at(row), full.highlights_at(row));
            assert_eq!(buf.decoration_at(row), full.decoration_at(row));
            for col in 0..=cell_width::text_width(buf.get_line(row).unwrap()) + 1 {
                assert_eq!(
                    buf.byte_at_display_pos(row, col),
                    full.byte_at_display_pos(row, col)
                );
            }
        }
        for byte in 0..=buf.source().len() + 1 {
            assert_eq!(buf.display_cursor_pos(byte), full.display_cursor_pos(byte));
        }
    }

    #[test]
    fn command_catalog_changes_refresh_cached_and_retained_rows() {
        let names = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let commands = Arc::new(smelt_core::commands::CommandCatalog::new(names.clone()));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            Arc::new(Mutex::new(AttachmentStore::new())),
            Arc::new(Mutex::new(None)),
            commands.clone(),
        ));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("/dynamic args\nlast".into());
        buf.ensure_rendered_at(80);
        assert!(buf.highlights_at(0).is_empty());

        names.lock().unwrap().insert("dynamic".into());
        assert!(buf.ensure_rendered_at(80));
        assert!(!buf.highlights_at(0).is_empty());
        assert!(!buf.ensure_rendered_at(80));

        names.lock().unwrap().remove("dynamic");
        let end = buf.source().len();
        buf.text_mut().insert(end, 'x');
        assert!(buf.ensure_rendered_at(80));
        assert!(buf.highlights_at(0).is_empty());
        assert_matches_full_projection(&buf, 80);

        commands.activate(Arc::new(Mutex::new(std::collections::HashSet::from([
            "dynamic".into(),
        ]))));
        assert!(buf.ensure_rendered_at(80));
        assert!(!buf.highlights_at(0).is_empty());
    }

    #[test]
    fn unchanged_file_references_refresh_on_full_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reference.txt");
        std::fs::write(&path, "reference").unwrap();
        let parser = Arc::new(PromptBufferParser::new(Arc::new(Mutex::new(
            AttachmentStore::new(),
        ))));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source(format!("@\"{}\"\nlast", path.display()));
        buf.ensure_rendered_at(80);
        let highlights = buf.highlights_at(0).to_vec();
        assert!(!highlights.is_empty());
        std::fs::remove_file(&path).unwrap();

        let end = buf.source().len();
        buf.text_mut().insert(end, 'x');
        buf.ensure_rendered_at(80);
        assert_eq!(buf.highlights_at(0), highlights);

        buf.invalidate_render_cache();
        buf.ensure_rendered_at(80);
        assert!(buf.highlights_at(0).is_empty());
        std::fs::write(&path, "reference").unwrap();
        buf.ensure_rendered_at(79);
        assert!(!buf.highlights_at(0).is_empty());
    }

    #[test]
    fn incremental_prompt_projection_matches_full_parse() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let id = store
            .lock()
            .unwrap()
            .insert_image("中.png".into(), "data:x".into());
        let second_id = store
            .lock()
            .unwrap()
            .insert_image("other.png".into(), "data:y".into());
        let parser = Arc::new(PromptBufferParser::new(store));
        for source in [
            "first\nsecond\nlast".to_string(),
            format!("first\n@\"{}\"\nlast", env!("CARGO_MANIFEST_DIR")),
            "first\ne\u{301}中👩\u{200d}💻\tlast".to_string(),
            "!ls\nlast".to_string(),
            "first\n@\"missing\n@nested\"\nlast".to_string(),
            "first\n@\"missing\nlast".to_string(),
            "first\r\ncontrol\0\nlast".to_string(),
            format!("{ATTACHMENT_MARKER}\nrow\t{ATTACHMENT_MARKER}\nlast"),
            format!("first\n@\"missing{ATTACHMENT_MARKER}\"\n{ATTACHMENT_MARKER} last"),
        ] {
            let ids = [id, second_id]
                .into_iter()
                .cycle()
                .take(source.matches(ATTACHMENT_MARKER).count())
                .collect();
            let mut initial = make_buf_with_parser(parser.clone());
            initial.text_mut().install(source.clone(), ids);
            initial.ensure_rendered_at(20);
            for byte in 0..=source.len() {
                for replacement in ["", "x", "\n", "\t", "\u{301}", "\""] {
                    let mut buf = initial.clone();
                    buf.text_mut()
                        .replace_range(byte..(byte + 1).min(source.len()), replacement);
                    buf.ensure_rendered_at(20);
                    assert_matches_full_projection(&buf, 20);
                    let end = buf.source().len();
                    buf.text_mut().insert_str(end, "\nnext");
                    buf.text_mut().insert_str(end.saturating_sub(2), "中");
                    buf.ensure_rendered_at(20);
                    assert_matches_full_projection(&buf, 20);
                    buf.ensure_rendered_at(9);
                    assert_matches_full_projection(&buf, 9);
                }
            }
        }
    }

    #[test]
    fn projection_positions_stay_bounded_before_reparsing_a_shrunk_source() {
        let parser = Arc::new(PromptBufferParser::new(Arc::new(Mutex::new(
            AttachmentStore::new(),
        ))));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("first\n中\nlast".into());
        buf.ensure_rendered_at(20);
        buf.text_mut().clear();
        for row in 0..buf.line_count() {
            assert_eq!(buf.byte_at_display_pos(row, 100), 0);
        }
        buf.ensure_rendered_at(20);
        assert_matches_full_projection(&buf, 20);
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
    fn parser_projects_grapheme_boundaries_to_terminal_cells() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let parser = Arc::new(PromptBufferParser::new(store));
        let mut buf = make_buf_with_parser(parser);
        let source = "e\u{301}👩\u{200d}💻9\u{fe0f}🇨🇦";
        buf.set_source(source.into());
        buf.ensure_rendered_at(80);

        let mut col = 0usize;
        assert_eq!(buf.display_cursor_pos(0), (0, 0));
        for (byte, grapheme) in cell_width::grapheme_indices(source) {
            let end = byte + grapheme.len();
            col += cell_width::text_width(grapheme);
            assert_eq!(buf.display_cursor_pos(end), (0, col));
            assert_eq!(buf.byte_at_display_pos(0, col), end);
        }
    }

    #[test]
    fn parser_renders_empty_prompt_placeholder_as_ghost_display() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let placeholder = Arc::new(Mutex::new(Some("ghost prediction wraps".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            store,
            placeholder,
            empty_commands(),
        ));
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
            empty_commands(),
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
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            store,
            placeholder,
            empty_commands(),
        ));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("real".into());

        buf.ensure_rendered_at(10);

        assert_eq!(buf.get_line(0), Some("real"));
        assert!(buf.highlights_at(0).iter().all(|span| span.meta.selectable));
    }

    #[test]
    fn parser_highlights_multiline_command_token_only() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let names = Arc::new(Mutex::new(std::collections::HashSet::from([
            "simplify".to_string()
        ])));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            store,
            Arc::new(Mutex::new(None)),
            Arc::new(smelt_core::commands::CommandCatalog::new(names)),
        ));
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
    fn prompt_copier_expands_stale_ranges_to_grapheme_boundaries() {
        let store = Arc::new(Mutex::new(AttachmentStore::new()));
        let copier = PromptCopier::new(store);
        let mut buf = Buffer::new(smelt_buffer::buffer::BufId(0), BufCreateOpts::default());
        buf.set_copier(Arc::new(copier));
        buf.set_source("ae\u{301}b".into());

        let out = buf.copy_range(2.."ae\u{301}".len());

        assert_eq!(out.kill_ring, "e\u{301}");
        assert_eq!(out.clipboard, "e\u{301}");
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
