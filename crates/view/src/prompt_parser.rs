//! Buffer parser for the prompt input: projects attachments / @refs into
//! display rows, highlights them, and writes source↔display coord maps to the
//! buffer for cursor mapping. Window layout owns soft wrapping.

use crate::prompt_spans::{build_display_spans, spans_to_string, Span, TAB_DISPLAY};
use smelt_buffer::attachment::{Attachment, ATTACHMENT_MARKER};
use smelt_buffer::buffer::{Buffer, BufferParser, LineCursorPolicy, LineDecoration, SpanMeta};
use smelt_buffer::cell_width;
use smelt_buffer::coords::{ProjectionCheckpoint, ProjectionMaps};
use smelt_buffer::theme::{intern, HlGroup};
use std::sync::{Arc, Mutex};

/// Without a placeholder or known commands by default.
#[derive(Default)]
pub struct PromptBufferParser {
    placeholder: Arc<Mutex<Option<String>>>,
    commands: Arc<smelt_core::commands::CommandCatalog>,
}

impl PromptBufferParser {
    pub fn with_placeholder(
        placeholder: Arc<Mutex<Option<String>>>,
        commands: Arc<smelt_core::commands::CommandCatalog>,
    ) -> Self {
        Self {
            placeholder,
            commands,
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

/// Split the flat display buffer into display rows. Newline characters
/// delimit rows and are not part of any rendered row. Row offsets are in the
/// flat display-character stream, including newline delimiters.
fn display_rows_and_offsets(buf: &str) -> (Vec<String>, Vec<usize>) {
    let mut offsets = vec![0];
    let mut chars_seen = 0usize;
    for ch in buf.chars() {
        chars_seen += 1;
        if ch == '\n' {
            offsets.push(chars_seen);
        }
    }
    (buf.split('\n').map(str::to_string).collect(), offsets)
}

/// A highlighted cell range on a display row, relative to the first projected row.
struct PromptHighlight {
    row: usize,
    cells: std::ops::Range<u16>,
    group: HlGroup,
    meta: SpanMeta,
}

/// One highlight per attachment label and resolved @ref, split at newlines.
/// An attachment label stands in for the caret while the cursor sits on it.
fn span_highlights(spans: &[Span]) -> Vec<PromptHighlight> {
    let accent = intern("SmeltAccent");
    let attachment_cursor = intern("SmeltAttachmentCursor");
    let mut out = Vec::new();
    let (mut row, mut cell) = (0usize, 0u16);
    for span in spans {
        let (text, highlight) = match span {
            Span::Attachment(t) => (t.as_str(), Some(SpanMeta::cursor_hl(attachment_cursor))),
            Span::AtRef(t) => (t.as_str(), Some(SpanMeta::default())),
            Span::Plain(t) | Span::AtToken(t) | Span::AtQuoteStart(t) => (t.as_str(), None),
            Span::Tab => (TAB_DISPLAY, None),
        };
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                row += 1;
                cell = 0;
            }
            let end = cell.saturating_add(cell_width::text_width_u16(piece));
            if let Some(meta) = highlight.as_ref().filter(|_| end > cell) {
                out.push(PromptHighlight {
                    row,
                    cells: cell..end,
                    group: accent,
                    meta: meta.clone(),
                });
            }
            cell = end;
        }
    }
    out
}

struct PromptDisplayRows {
    spans: Vec<Span>,
    lines: Vec<String>,
    row_offsets: Vec<usize>,
    ghost: bool,
}

fn build_prompt_display_rows(
    source: &str,
    attachments: &[Attachment],
    placeholder: Option<&str>,
) -> PromptDisplayRows {
    if source.is_empty() {
        if let Some(text) = placeholder.filter(|text| !text.is_empty()) {
            return PromptDisplayRows {
                spans: Vec::new(),
                lines: vec![text.to_string()],
                row_offsets: vec![0],
                ghost: true,
            };
        }
    }
    let spans = build_display_spans(source, attachments);
    let (lines, row_offsets) = display_rows_and_offsets(&spans_to_string(&spans));
    PromptDisplayRows {
        spans,
        lines,
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
    /// Accent for a known command token and the shell `!` prefix, which only
    /// the first prompt row can carry.
    fn first_line_highlights(&self, source: &str, first: &str) -> Vec<PromptHighlight> {
        let mut out = Vec::new();
        if let Some(token) = self
            .commands
            .command_token(smelt_buffer::text::trim_whitespace(source))
        {
            let end = smelt_buffer::text::byte_of_char(first, token.chars().count());
            out.push(PromptHighlight {
                row: 0,
                cells: 0..cell_width::text_width_u16(smelt_buffer::text::slice(first, 0..end)),
                group: intern("SmeltAccent"),
                meta: SpanMeta::default(),
            });
        }
        let is_exec = !source.contains('\n')
            && (source == "!"
                || matches!(source.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace()));
        if is_exec {
            out.push(PromptHighlight {
                row: 0,
                cells: 0..1,
                group: intern("SmeltExecPrefix"),
                meta: SpanMeta::default(),
            });
        }
        out
    }

    fn parse_from(&self, buf: &mut Buffer, source: &str, point: ProjectionCheckpoint) {
        let suffix = smelt_buffer::text::slice(source, point.source_byte..source.len());
        smelt_perf::perf::record_value("prompt:projected_source_bytes", suffix.len() as u64);
        let attachment_start = smelt_buffer::text::slice(source, 0..point.source_byte)
            .matches(ATTACHMENT_MARKER)
            .count();
        let placeholder = self.placeholder.lock().unwrap().clone();
        let PromptDisplayRows {
            spans,
            lines,
            row_offsets,
            ghost,
        } = build_prompt_display_rows(
            suffix,
            buf.attachments.get(attachment_start..).unwrap_or_default(),
            placeholder.as_deref().filter(|_| point.row == 0),
        );

        // Theme-group extmarks: the renderer resolves group → style at paint
        // time, so live theme updates flow through without re-parsing.
        let highlights = if ghost {
            let ghost_group = intern("GhostText");
            lines
                .iter()
                .enumerate()
                .map(|(row, line)| PromptHighlight {
                    row,
                    cells: 0..cell_width::text_width_u16(line),
                    group: ghost_group,
                    meta: SpanMeta::unselectable(),
                })
                .collect()
        } else {
            let mut highlights = span_highlights(&spans);
            if let Some(first) = lines.first().filter(|_| point.row == 0) {
                highlights.extend(self.first_line_highlights(source, first));
            }
            highlights
        };
        let display_chars: usize = lines.iter().map(|line| line.chars().count()).sum();
        let line_count = lines.len();

        if point.row == 0 {
            buf.set_all_lines(lines);
        } else {
            buf.set_lines(point.row, buf.line_count(), lines);
        }
        for highlight in highlights.into_iter().filter(|h| !h.cells.is_empty()) {
            buf.add_highlight_group_with_meta(
                point.row + highlight.row,
                highlight.cells.start,
                highlight.cells.end,
                highlight.group,
                highlight.meta,
            );
        }

        if ghost {
            for row in 0..line_count {
                buf.set_decoration(
                    row,
                    LineDecoration {
                        cursor_policy: LineCursorPolicy::PreserveRequested,
                        ..Default::default()
                    },
                );
            }
            buf.set_projection_maps(ProjectionMaps {
                source_char_to_display_char: vec![0],
                display_char_to_source_char: vec![0; display_chars + 1],
                row_offsets,
                checkpoints: Vec::new(),
            });
            return;
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
        let parser = Arc::new(PromptBufferParser::default());
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
        let id = Attachment::image("中.png".into(), "data:x".into());
        let second_id = Attachment::image("other.png".into(), "data:y".into());
        let parser = Arc::new(PromptBufferParser::default());
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
            let ids = [id.clone(), second_id.clone()]
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
        let parser = Arc::new(PromptBufferParser::default());
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
        let parser = Arc::new(PromptBufferParser::default());
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("a".repeat(79));
        buf.ensure_rendered_at(10);
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.get_line(0).unwrap().chars().count(), 79);
    }

    #[test]
    fn parser_splits_source_newlines_only() {
        let parser = Arc::new(PromptBufferParser::default());
        let mut buf = make_buf_with_parser(parser.clone());
        buf.set_source("hello world\nsecond".into());
        buf.ensure_rendered_at(10);
        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.get_line(0).unwrap(), "hello world");
        assert_eq!(buf.get_line(1).unwrap(), "second");
    }

    #[test]
    fn parser_expands_tabs_for_display_without_changing_source_mapping() {
        let parser = Arc::new(PromptBufferParser::default());
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("a\tb".into());
        buf.ensure_rendered_at(80);

        assert_eq!(buf.get_line(0), Some("a    b"));
        assert_eq!(buf.display_cursor_pos(2), (0, 5));
        assert_eq!(buf.byte_at_display_pos(0, 5), 2);
    }

    #[test]
    fn parser_projects_grapheme_boundaries_to_terminal_cells() {
        let parser = Arc::new(PromptBufferParser::default());
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
        let placeholder = Arc::new(Mutex::new(Some("ghost prediction wraps".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
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
        let placeholder = Arc::new(Mutex::new(Some("ghost".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
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
        let placeholder = Arc::new(Mutex::new(Some("ghost".to_string())));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
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
        let names = Arc::new(Mutex::new(std::collections::HashSet::from([
            "simplify".to_string()
        ])));
        let parser = Arc::new(PromptBufferParser::with_placeholder(
            Arc::new(Mutex::new(None)),
            Arc::new(smelt_core::commands::CommandCatalog::new(names)),
        ));
        let mut buf = make_buf_with_parser(parser);
        buf.set_source("/simplify first line\nsecond line".into());
        buf.ensure_rendered_at(80);

        let first_row: Vec<_> = buf
            .highlights_at(0)
            .iter()
            .map(|h| (h.col_start, h.col_end))
            .collect();
        assert_eq!(first_row, [(0, 9)], "only /simplify is highlighted");
        assert!(
            buf.highlights_at(1).is_empty(),
            "expected following lines to stay unhighlighted, got {:?}",
            buf.highlights_at(1)
        );
    }

    #[test]
    fn parser_maps_cursor_past_attachment() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let parser = Arc::new(PromptBufferParser::default());
        let mut buf = make_buf_with_parser(parser.clone());
        let source = format!("a{ATTACHMENT_MARKER}b");
        buf.text_mut().install(
            source.clone(),
            vec![Attachment::image("img.png".into(), "data:xx".into())],
        );
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
    fn parser_gives_each_attachment_label_one_cursor_span() {
        use smelt_buffer::ATTACHMENT_MARKER;
        let parser = Arc::new(PromptBufferParser::default());
        let mut buf = make_buf_with_parser(parser);
        buf.text_mut().install(
            format!("hello {ATTACHMENT_MARKER}{ATTACHMENT_MARKER} end"),
            vec![
                Attachment::image("img.png".into(), "data:a".into()),
                Attachment::image("b.png".into(), "data:b".into()),
            ],
        );
        buf.ensure_rendered_at(80);

        let cursor_hl = Some(intern("SmeltAttachmentCursor"));
        let spans: Vec<_> = buf
            .highlights_at(0)
            .iter()
            .map(|h| (h.col_start, h.col_end, h.meta.cursor_hl))
            .collect();
        // "hello " then "[img.png]" (9 cells) and "[b.png]" (7 cells).
        assert_eq!(spans, [(6, 15, cursor_hl), (15, 22, cursor_hl)]);
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
