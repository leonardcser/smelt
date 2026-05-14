//! Prompt text spans, wrapping, and styled-char rendering.

use crate::input::ATTACHMENT_MARKER;
pub(crate) use crate::smelt_term::text::wrap_line;
use smelt_core::attachment::{AttachmentId, AttachmentStore};
pub(crate) use smelt_core::content::selection::{scan_at_token, truncate_str, try_at_ref};
use unicode_width::UnicodeWidthChar;

/// Output of [`wrap_with_offsets`]: wrapped visual lines and the per-row
/// display-char start offset (so callers can index back into the flat
/// display-char stream without a second pass over `buf`).
pub(crate) struct WrapResult {
    pub(crate) visual_lines: Vec<(String, Vec<SpanKind>)>,
    pub(crate) row_offsets: Vec<usize>,
}

/// Word-wrap `buf` to `usable` display columns, tracking per-row start offsets
/// in the flat display-char stream (which keeps `\n` chars in cumulative
/// counts so callers can pair them with [`ProjectionMaps`] tables).
pub(crate) fn wrap_with_offsets(buf: &str, char_kinds: &[SpanKind], usable: usize) -> WrapResult {
    let _perf = smelt_perf::perf::begin("render:wrap_cursor");
    let mut state = WrapState::default();
    let max_col = usable.max(1);
    let prompt_col = 1usize;

    for text_line in buf.split('\n') {
        let chars: Vec<char> = text_line.chars().collect();
        if chars.is_empty() {
            state.push(state.chars_seen, &[], &[]);
            state.chars_seen += 1;
            continue;
        }

        let mut line_chars: Vec<char> = Vec::new();
        let mut line_kinds: Vec<SpanKind> = Vec::new();
        let mut line_width = 0usize;
        let mut line_start = state.chars_seen;
        let mut last_break: Option<usize> = None;
        let mut i = 0usize;

        while i < chars.len() {
            let ch = chars[i];
            let kind = char_kinds
                .get(state.chars_seen + i)
                .copied()
                .unwrap_or(SpanKind::Plain);
            let ch_width = display_char_width(ch, prompt_col + line_width);

            if !line_chars.is_empty() && line_width + ch_width > max_col {
                if let Some(break_idx) = last_break {
                    let carry_chars = line_chars.split_off(break_idx);
                    let carry_kinds = line_kinds.split_off(break_idx);
                    state.push(line_start, &line_chars, &line_kinds);
                    line_start += break_idx;
                    line_chars = carry_chars;
                    line_kinds = carry_kinds;
                    line_width = display_width(&line_chars, prompt_col);
                    last_break = line_chars
                        .iter()
                        .rposition(|&c| c == ' ')
                        .map(|idx| idx + 1);
                } else {
                    state.push(line_start, &line_chars, &line_kinds);
                    line_start += line_chars.len();
                    line_chars.clear();
                    line_kinds.clear();
                    line_width = 0;
                    last_break = None;
                }
                continue;
            }

            line_chars.push(ch);
            line_kinds.push(kind);
            line_width += ch_width;
            if ch == ' ' {
                last_break = Some(line_chars.len());
            }
            i += 1;
        }

        state.push(line_start, &line_chars, &line_kinds);
        state.chars_seen += chars.len() + 1;
    }
    if state.visual_lines.is_empty() {
        state.visual_lines.push((String::new(), Vec::new()));
        state.row_offsets.push(0);
    }
    // When the last visual line is full (display width reaches `max_col`), reserve
    // an empty trailing row so a cursor at end-of-line has a visible home — neovim-
    // style wrap. Without this, typing the char that fills the row leaves the
    // cursor one cell past the visible content and it goes invisible.
    let needs_padding_row = state
        .visual_lines
        .last()
        .map(|(line, _)| {
            let chars: Vec<char> = line.chars().collect();
            display_width(&chars, prompt_col) >= max_col
        })
        .unwrap_or(false);
    if needs_padding_row {
        // The padding row sits at the end of the display-char stream — its
        // offset is the previous row's start plus that row's char count.
        let pad_offset = state
            .row_offsets
            .last()
            .copied()
            .map(|prev| {
                prev + state
                    .visual_lines
                    .last()
                    .map(|(l, _)| l.chars().count())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        state.visual_lines.push((String::new(), Vec::new()));
        state.row_offsets.push(pad_offset);
    }
    WrapResult {
        visual_lines: state.visual_lines,
        row_offsets: state.row_offsets,
    }
}

#[derive(Default)]
struct WrapState {
    visual_lines: Vec<(String, Vec<SpanKind>)>,
    row_offsets: Vec<usize>,
    chars_seen: usize,
}

impl WrapState {
    fn push(&mut self, start_char: usize, line_chars: &[char], line_kinds: &[SpanKind]) {
        self.row_offsets.push(start_char);
        self.visual_lines
            .push((line_chars.iter().collect(), line_kinds.to_vec()));
    }
}

fn display_width(chars: &[char], start_col: usize) -> usize {
    let mut col = start_col;
    for &ch in chars {
        col += display_char_width(ch, col);
    }
    col.saturating_sub(start_col)
}

fn display_char_width(ch: char, col: usize) -> usize {
    if ch == '\t' {
        let tab_stop = 8usize;
        tab_stop - (col % tab_stop)
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
}

pub(crate) enum Span {
    Plain(String),
    Attachment(String),
    AtRef(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SpanKind {
    Plain,
    Attachment,
    AtRef,
}

pub(crate) fn build_char_kinds(spans: &[Span]) -> Vec<SpanKind> {
    let mut kinds = Vec::new();
    for span in spans {
        let (text, kind) = match span {
            Span::Plain(t) => (t.as_str(), SpanKind::Plain),
            Span::Attachment(t) => (t.as_str(), SpanKind::Attachment),
            Span::AtRef(t) => (t.as_str(), SpanKind::AtRef),
        };
        kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
    }
    kinds
}

pub(crate) fn build_display_spans(
    buf: &str,
    att_ids: &[AttachmentId],
    store: &AttachmentStore,
) -> Vec<Span> {
    let _perf = smelt_perf::perf::begin("render:display_spans");
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut att_idx = 0;

    let chars: Vec<char> = buf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ATTACHMENT_MARKER {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            let label = att_ids
                .get(att_idx)
                .map(|&id| store.display_label(id))
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
            i += 1;
        } else if let Some((token, end)) = try_at_ref(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtRef(token));
            i = end;
        } else if let Some((token, _, end)) = scan_at_token(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::Plain(token));
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

pub(crate) fn spans_to_string(spans: &[Span]) -> String {
    let mut s = String::new();
    for span in spans {
        match span {
            Span::Plain(t) | Span::Attachment(t) | Span::AtRef(t) => s.push_str(t),
        }
    }
    s
}

pub(crate) fn map_cursor(raw_cursor: usize, raw_buf: &str, spans: &[Span]) -> usize {
    let mut raw_pos = 0;
    let mut display_pos = 0;
    for span in spans {
        match span {
            Span::Plain(t) => {
                let chars = t.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
            Span::Attachment(label) => {
                if raw_cursor == raw_pos {
                    return display_pos;
                }
                raw_pos += 1;
                display_pos += label.chars().count();
            }
            Span::AtRef(token) => {
                let chars = token.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
        }
    }
    let _ = raw_buf;
    display_pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(buf: &str, usable: usize) -> WrapResult {
        let kinds = vec![SpanKind::Plain; buf.chars().count()];
        wrap_with_offsets(buf, &kinds, usable)
    }

    #[test]
    fn offsets_single_line() {
        assert_eq!(wrap("hello", 80).row_offsets, vec![0]);
    }

    #[test]
    fn offsets_two_logical_lines() {
        assert_eq!(wrap("aaa\nbbb", 80).row_offsets, vec![0, 4]);
    }

    #[test]
    fn offsets_three_logical_lines() {
        assert_eq!(wrap("aaa\nbbb\nccc", 80).row_offsets, vec![0, 4, 8]);
    }

    #[test]
    fn offsets_empty_line() {
        assert_eq!(wrap("aaa\n\nccc", 80).row_offsets, vec![0, 4, 5]);
    }

    #[test]
    fn offsets_wrapped_line() {
        // "abcdef" at width 3 → "abc", "def" — the last row fills exactly, so
        // a padding row is appended at offset 6.
        assert_eq!(wrap("abcdef", 3).row_offsets, vec![0, 3, 6]);
    }

    #[test]
    fn offsets_wrapped_multiline() {
        // "abcdef\nxy" at width 3: "abc" at 0, "def" at 3, then "\n" eaten and
        // "xy" lands at 7 (3 + 1 for \n + 3 carry-over from "def").
        assert_eq!(wrap("abcdef\nxy", 3).row_offsets, vec![0, 3, 7]);
    }

    #[test]
    fn ascii_wraps_at_max_col_plus_one() {
        let s = "a".repeat(79);
        let r = wrap(&s, 78);
        let lengths: Vec<usize> = r
            .visual_lines
            .iter()
            .map(|(l, _)| l.chars().count())
            .collect();
        assert_eq!(lengths, vec![78, 1]);
    }

    #[test]
    fn ascii_filling_row_reserves_padding_line_for_cursor() {
        // When the last visual line is exactly max_col chars wide, a trailing
        // empty row is added so a cursor at end-of-line has a visible home.
        let s = "a".repeat(78);
        let r = wrap(&s, 78);
        let lengths: Vec<usize> = r
            .visual_lines
            .iter()
            .map(|(l, _)| l.chars().count())
            .collect();
        assert_eq!(lengths, vec![78, 0]);
        assert_eq!(r.row_offsets, vec![0, 78]);
    }

    #[test]
    fn prompt_tabs_respect_prompt_column_without_forced_wrap() {
        let r = wrap("a\tb", 8);
        // Tab expands `a` to col 8, so the row fills exactly — a trailing empty
        // row is reserved. The text on row 0 is unaltered.
        assert_eq!(
            r.visual_lines
                .iter()
                .map(|(s, _)| s.as_str())
                .collect::<Vec<_>>(),
            vec!["a\tb", ""]
        );
    }
}
