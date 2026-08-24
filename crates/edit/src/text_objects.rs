//! Vim text-object selection (iw/aw, i"/a", i(/a(, ip/ap, etc.) over `&str` buffers.

use super::text::{
    char_class, line_end, line_start, next_grapheme_boundary, prev_grapheme_boundary, CharClass,
};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SurroundDelimiters {
    pub open_start: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub close_end: usize,
}

impl SurroundDelimiters {
    pub fn inner_range(self) -> (usize, usize) {
        (self.open_end, self.close_start)
    }

    pub fn outer_range(self) -> (usize, usize) {
        (self.open_start, self.close_end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextObjectSpec {
    pub inner: bool,
    pub kind: TextObjectKind,
}

impl TextObjectSpec {
    pub fn new(inner: bool, kind: char) -> Option<Self> {
        Some(Self {
            inner,
            kind: TextObjectKind::from_char(kind)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextObjectKind {
    Word,
    BigWord,
    Quote(char),
    AnyQuote,
    Pair { open: char, close: char },
    Tag,
    Paragraph,
}

impl TextObjectKind {
    fn from_char(kind: char) -> Option<Self> {
        match kind {
            'w' => Some(Self::Word),
            'W' => Some(Self::BigWord),
            '"' | '\'' | '`' => Some(Self::Quote(kind)),
            'q' | 's' => Some(Self::AnyQuote),
            '(' | ')' | 'b' => Some(Self::Pair {
                open: '(',
                close: ')',
            }),
            '[' | ']' | 'r' => Some(Self::Pair {
                open: '[',
                close: ']',
            }),
            '{' | '}' | 'B' => Some(Self::Pair {
                open: '{',
                close: '}',
            }),
            '<' | '>' | 'a' => Some(Self::Pair {
                open: '<',
                close: '>',
            }),
            't' => Some(Self::Tag),
            'p' | 'P' => Some(Self::Paragraph),
            _ => None,
        }
    }
}

pub(crate) fn text_object(
    buf: &str,
    cpos: usize,
    inner: bool,
    kind: char,
) -> Option<(usize, usize)> {
    let spec = TextObjectSpec::new(inner, kind)?;
    text_object_for_spec(buf, cpos, spec)
}

pub(crate) fn text_object_for_spec(
    buf: &str,
    cpos: usize,
    spec: TextObjectSpec,
) -> Option<(usize, usize)> {
    match spec.kind {
        TextObjectKind::Word => text_object_word(buf, cpos, spec.inner, CharClass::Word),
        TextObjectKind::BigWord => text_object_word(buf, cpos, spec.inner, CharClass::WORD),
        TextObjectKind::Quote(quote) => text_object_quote(buf, cpos, spec.inner, quote),
        TextObjectKind::AnyQuote => text_object_any_quote(buf, cpos, spec.inner),
        TextObjectKind::Pair { open, close } => {
            text_object_pair(buf, cpos, spec.inner, open, close)
        }
        TextObjectKind::Tag => surrounding_delimiters(buf, cpos, 't').map(|d| {
            if spec.inner {
                d.inner_range()
            } else {
                d.outer_range()
            }
        }),
        TextObjectKind::Paragraph => text_object_paragraph(buf, cpos, spec.inner),
    }
}

fn is_horizontal_space(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, ' ' | '\t'))
}

fn following_horizontal_space_end(buf: &str, mut pos: usize, limit: usize) -> usize {
    while pos < limit {
        let next = next_grapheme_boundary(buf, pos);
        if next > limit || !is_horizontal_space(&buf[pos..next]) {
            break;
        }
        pos = next;
    }
    pos
}

fn preceding_horizontal_space_start(buf: &str, mut pos: usize, limit: usize) -> usize {
    while pos > limit {
        let previous = prev_grapheme_boundary(buf, pos);
        if previous < limit || !is_horizontal_space(&buf[previous..pos]) {
            break;
        }
        pos = previous;
    }
    pos
}

fn text_object_word(
    buf: &str,
    cpos: usize,
    inner: bool,
    mode: CharClass,
) -> Option<(usize, usize)> {
    if buf.is_empty() || cpos >= buf.len() {
        return None;
    }
    let cpos = smelt_buffer::text::snap_grapheme(buf, cpos);
    let graphemes: Vec<(usize, &str)> = smelt_buffer::cell_width::grapheme_indices(buf).collect();
    let ci = graphemes.iter().position(|(byte, _)| *byte >= cpos)?;
    let cur_grapheme = graphemes[ci].1;
    let cur_char = cur_grapheme.chars().next()?;
    let cur_class = char_class(cur_char, mode);

    if cur_grapheme == "\n" {
        let byte_pos = graphemes[ci].0;
        return Some((byte_pos, byte_pos + 1));
    }

    let mut start = ci;
    while start > 0 {
        let prev = graphemes[start - 1].1;
        let prev_char = prev.chars().next()?;
        if prev == "\n" || char_class(prev_char, mode) != cur_class {
            break;
        }
        start -= 1;
    }
    let mut end = ci;
    while end + 1 < graphemes.len() {
        let next = graphemes[end + 1].1;
        let next_char = next.chars().next()?;
        if next == "\n" || char_class(next_char, mode) != cur_class {
            break;
        }
        end += 1;
    }

    let byte_start = graphemes[start].0;
    let byte_end = if end + 1 < graphemes.len() {
        graphemes[end + 1].0
    } else {
        buf.len()
    };

    if inner {
        Some((byte_start, byte_end))
    } else {
        // "a word" includes trailing whitespace, or leading if none trailing.
        let a_end = following_horizontal_space_end(buf, byte_end, buf.len());
        if a_end > byte_end {
            Some((byte_start, a_end))
        } else {
            Some((
                preceding_horizontal_space_start(buf, byte_start, 0),
                byte_end,
            ))
        }
    }
}

fn text_object_quote(buf: &str, cpos: usize, inner: bool, quote: char) -> Option<(usize, usize)> {
    // Mirrors vim's `current_quote` (textobject.c): scan backward for an
    // opening quote, then forward for a closing one. `\` escapes the next
    // char (vim's default `'quoteescape'`).
    let delims = quote_delimiters(buf, cpos, quote)?;
    if inner {
        return Some(delims.inner_range());
    }

    let line_s = line_start(buf, cpos);
    let line_e = line_end(buf, cpos);
    let a_end = following_horizontal_space_end(buf, delims.close_end, line_e);
    if a_end > delims.close_end {
        return Some((delims.open_start, a_end));
    }
    Some((
        preceding_horizontal_space_start(buf, delims.open_start, line_s),
        delims.close_end,
    ))
}

fn find_next_quote(line: &str, from: usize, quote: char, respect_escape: bool) -> Option<usize> {
    let bytes = line.as_bytes();
    let qbyte = quote as u32;
    debug_assert!(qbyte < 0x80, "quote chars are ASCII");
    let q = qbyte as u8;
    let mut i = from;
    while i < bytes.len() {
        let b = bytes[i];
        if respect_escape && b == b'\\' {
            i += 1;
            if i >= bytes.len() {
                return None;
            }
            i += 1;
            continue;
        }
        if b == q {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_prev_quote(line: &str, from: usize, quote: char, respect_escape: bool) -> Option<usize> {
    let bytes = line.as_bytes();
    let qbyte = quote as u32;
    debug_assert!(qbyte < 0x80, "quote chars are ASCII");
    let q = qbyte as u8;
    let mut i = from;
    while i > 0 {
        i -= 1;
        if bytes[i] == q {
            if respect_escape {
                let mut esc = 0usize;
                while i > esc && bytes[i - esc - 1] == b'\\' {
                    esc += 1;
                }
                if esc % 2 == 1 {
                    continue;
                }
            }
            return Some(i);
        }
    }
    None
}

fn text_object_any_quote(buf: &str, cpos: usize, inner: bool) -> Option<(usize, usize)> {
    nearest_quote_delimiters(buf, cpos).map(|d| {
        if inner {
            d.inner_range()
        } else {
            d.outer_range()
        }
    })
}

fn nearest_quote_delimiters(buf: &str, cpos: usize) -> Option<SurroundDelimiters> {
    let cursor = smelt_buffer::text::snap_grapheme(buf, cpos.min(buf.len()));
    ['"', '\'', '`']
        .into_iter()
        .filter_map(|quote| quote_delimiters(buf, cursor, quote))
        .filter(|d| cursor >= d.open_start && cursor <= d.close_end)
        .min_by_key(|d| d.close_end.saturating_sub(d.open_start))
}

pub(crate) fn surrounding_delimiters(
    buf: &str,
    cpos: usize,
    kind: char,
) -> Option<SurroundDelimiters> {
    match kind {
        'q' | 's' => nearest_quote_delimiters(buf, cpos),
        '"' | '\'' | '`' => quote_delimiters(buf, cpos, kind),
        '(' | ')' | 'b' => pair_delimiters(buf, cpos, '(', ')'),
        '[' | ']' | 'r' => pair_delimiters(buf, cpos, '[', ']'),
        '{' | '}' | 'B' => pair_delimiters(buf, cpos, '{', '}'),
        '<' | '>' | 'a' => pair_delimiters(buf, cpos, '<', '>'),
        't' => tag_delimiters(buf, cpos),
        _ => None,
    }
}

fn grapheme_delimiters(
    buf: &str,
    open_start: usize,
    open_end: usize,
    close_start: usize,
    close_end: usize,
) -> Option<SurroundDelimiters> {
    let open = smelt_buffer::text::covering_grapheme_range(buf, open_start..open_end);
    let close = smelt_buffer::text::covering_grapheme_range(buf, close_start..close_end);
    let delimiters = SurroundDelimiters {
        open_start: open.start,
        open_end: open.end,
        close_start: close.start,
        close_end: close.end,
    };
    (delimiters.open_end <= delimiters.close_start).then_some(delimiters)
}

fn quote_delimiters(buf: &str, cpos: usize, quote: char) -> Option<SurroundDelimiters> {
    let line_s = line_start(buf, cpos);
    let line_e = line_end(buf, cpos);
    let line = &buf[line_s..line_e];
    let rel = cpos.saturating_sub(line_s);
    let qlen = quote.len_utf8();

    let mut open = find_prev_quote(line, rel, quote, true);
    if open.is_none() {
        open = find_next_quote(line, rel, quote, false);
    }
    let open = open?;
    let close = find_next_quote(line, open + qlen, quote, true)?;
    grapheme_delimiters(
        buf,
        line_s + open,
        line_s + open + qlen,
        line_s + close,
        line_s + close + qlen,
    )
}

fn pair_delimiters(buf: &str, cpos: usize, open: char, close: char) -> Option<SurroundDelimiters> {
    find_pair_delimiters(buf, cpos, open, close)
}

#[derive(Clone, Debug)]
struct HtmlTag<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    closing: bool,
    self_closing: bool,
}

// Lightweight HTML/XML tag matching for Vim `it`/`at` and surround `t`.
// It is deliberately syntax-only: quoted attributes are skipped while looking
// for `>`, self-closing tags are ignored, and malformed tags do not match.
fn tag_delimiters(buf: &str, cpos: usize) -> Option<SurroundDelimiters> {
    let cursor = smelt_buffer::text::snap_grapheme(buf, cpos.min(buf.len()));
    let mut stack: Vec<HtmlTag<'_>> = Vec::new();
    let mut best: Option<SurroundDelimiters> = None;
    let mut i = 0usize;

    while i < buf.len() {
        let Some(rel) = buf[i..].find('<') else { break };
        let start = i + rel;
        let Some(tag) = parse_html_tag(buf, start) else {
            i = start + 1;
            continue;
        };
        i = tag.end;

        if tag.closing {
            if let Some(open_idx) = stack
                .iter()
                .rposition(|open| open.name.eq_ignore_ascii_case(tag.name))
            {
                let open = stack.remove(open_idx);
                let Some(candidate) =
                    grapheme_delimiters(buf, open.start, open.end, tag.start, tag.end)
                else {
                    continue;
                };
                if cursor >= candidate.open_start && cursor <= candidate.close_end {
                    let replace = best
                        .map(|current| {
                            candidate.close_end - candidate.open_start
                                < current.close_end - current.open_start
                        })
                        .unwrap_or(true);
                    if replace {
                        best = Some(candidate);
                    }
                }
            }
        } else if !tag.self_closing {
            stack.push(tag);
        }
    }

    best
}

fn parse_html_tag<'a>(buf: &'a str, start: usize) -> Option<HtmlTag<'a>> {
    let bytes = buf.as_bytes();
    if bytes.get(start) != Some(&b'<') {
        return None;
    }

    let mut i = start + 1;
    let mut closing = false;
    if bytes.get(i) == Some(&b'/') {
        closing = true;
        i += 1;
    }
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }

    let name_start = i;
    while i < bytes.len() && is_tag_name_byte(bytes[i]) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = &buf[name_start..i];

    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'>' {
            let mut j = i;
            while j > start && matches!(bytes[j - 1], b' ' | b'\t' | b'\n' | b'\r') {
                j -= 1;
            }
            return Some(HtmlTag {
                start,
                end: i + 1,
                name,
                closing,
                self_closing: !closing && j > start && bytes[j - 1] == b'/',
            });
        }
        i += 1;
    }
    None
}

fn is_tag_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParagraphLine {
    pub is_blank: bool,
}

pub(crate) fn paragraph_line_range(
    lines: &[ParagraphLine],
    cursor_line: usize,
    inner: bool,
) -> Option<Range<usize>> {
    if lines.is_empty() {
        return None;
    }
    let li = cursor_line.min(lines.len().saturating_sub(1));
    let blank = lines[li].is_blank;

    let mut lo = li;
    while lo > 0 && lines[lo - 1].is_blank == blank {
        lo -= 1;
    }
    let mut hi = li;
    while hi + 1 < lines.len() && lines[hi + 1].is_blank == blank {
        hi += 1;
    }

    if inner {
        return Some(lo..hi + 1);
    }

    // Mirrors vim's `current_par`:
    // - With a trailing run of opposite-status lines, extend through it.
    // - With no trailing run but cursor in a non-blank paragraph, fall back
    //   to leading blank lines (the "no white in front, none in back" branch).
    // - With cursor in a trailing blank run at EOF, `ap` fails - return None.
    if hi + 1 < lines.len() {
        let other = lines[hi + 1].is_blank;
        let mut hi2 = hi + 1;
        while hi2 + 1 < lines.len() && lines[hi2 + 1].is_blank == other {
            hi2 += 1;
        }
        Some(lo..hi2 + 1)
    } else if !blank && lo > 0 && lines[lo - 1].is_blank {
        let mut lo2 = lo - 1;
        while lo2 > 0 && lines[lo2 - 1].is_blank {
            lo2 -= 1;
        }
        Some(lo2..hi + 1)
    } else if !blank {
        Some(lo..hi + 1)
    } else {
        None
    }
}

fn text_object_paragraph(buf: &str, cpos: usize, inner: bool) -> Option<(usize, usize)> {
    if buf.is_empty() {
        return None;
    }

    // Build line table: (start_byte, end_byte_exclusive_including_newline, is_blank).
    let mut lines: Vec<(usize, usize, ParagraphLine)> = Vec::new();
    let bytes = buf.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let line = &buf[start..i];
            let is_blank = line.bytes().all(|b| b == b' ' || b == b'\t');
            lines.push((start, i + 1, ParagraphLine { is_blank }));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        let line = &buf[start..];
        let is_blank = line.bytes().all(|b| b == b' ' || b == b'\t');
        lines.push((start, bytes.len(), ParagraphLine { is_blank }));
    }

    let clamped = cpos.min(buf.len());
    let li = lines
        .iter()
        .position(|(s, e, _)| clamped >= *s && clamped < *e)
        .unwrap_or(lines.len().saturating_sub(1));
    let paragraph_lines: Vec<ParagraphLine> = lines.iter().map(|(_, _, line)| *line).collect();
    let range = paragraph_line_range(&paragraph_lines, li, inner)?;
    Some((lines[range.start].0, lines[range.end - 1].1))
}

fn find_pair_delimiters(
    buf: &str,
    cpos: usize,
    open: char,
    close: char,
) -> Option<SurroundDelimiters> {
    let mut depth = 0i32;
    let mut open_pos = None;
    let snapped = smelt_buffer::text::snap_grapheme(buf, cpos);
    let upper = next_grapheme_boundary(buf, snapped);
    for (i, c) in buf[..upper].char_indices().rev() {
        if c == close && i != snapped {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                open_pos = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let open_pos = open_pos?;

    depth = 0;
    let search_start = open_pos + open.len_utf8();
    for (i, c) in buf[search_start..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                let close_pos = search_start + i;
                return grapheme_delimiters(
                    buf,
                    open_pos,
                    open_pos + open.len_utf8(),
                    close_pos,
                    close_pos + close.len_utf8(),
                );
            }
            depth -= 1;
        }
    }
    None
}

fn text_object_pair(
    buf: &str,
    cpos: usize,
    inner: bool,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let delimiters = pair_delimiters(buf, cpos, open, close)?;
    Some(if inner {
        delimiters.inner_range()
    } else {
        delimiters.outer_range()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(buf: &str, range: (usize, usize)) -> &str {
        &buf[range.0..range.1]
    }

    fn assert_grapheme_range(buf: &str, range: (usize, usize)) {
        let boundaries: Vec<usize> = smelt_buffer::cell_width::grapheme_indices(buf)
            .map(|(start, _)| start)
            .chain(std::iter::once(buf.len()))
            .collect();
        assert!(boundaries.contains(&range.0), "invalid start: {range:?}");
        assert!(boundaries.contains(&range.1), "invalid end: {range:?}");
    }

    #[test]
    fn capital_p_alias_selects_paragraph() {
        let s = "before\n\npara a\npara b\n\nafter";
        let r = text_object(s, s.find("para a").unwrap(), true, 'P').unwrap();
        assert_eq!(slice(s, r), "para a\npara b\n");
    }

    // ── dispatcher ───────────────────────────────────────────────────────

    #[test]
    fn unknown_text_object_kind_returns_none() {
        assert_eq!(text_object("hello", 0, true, 'z'), None);
        assert_eq!(text_object("hello", 0, false, '?'), None);
    }

    // ── word: iw / aw ────────────────────────────────────────────────────

    #[test]
    fn iw_selects_just_the_word_under_cursor() {
        let s = "the quick brown";
        let r = text_object(s, 5, true, 'w').unwrap();
        assert_eq!(slice(s, r), "quick");
    }

    #[test]
    fn word_objects_never_split_grapheme_clusters() {
        let decomposed = "one e\u{301}cho two";
        let start = decomposed.find("e\u{301}").unwrap();
        let range = text_object(decomposed, start + 1, true, 'w').unwrap();
        assert_eq!(slice(decomposed, range), "e\u{301}cho");

        for grapheme in ["👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a {grapheme} z");
            let start = text.find(grapheme).unwrap();
            let range = text_object(&text, start, true, 'w').unwrap();
            assert_eq!(slice(&text, range), grapheme);
        }
    }

    #[test]
    fn aw_extends_to_trailing_whitespace_when_present() {
        let s = "the quick brown";
        let r = text_object(s, 5, false, 'w').unwrap();
        assert_eq!(slice(s, r), "quick ");
    }

    #[test]
    fn aw_falls_back_to_leading_whitespace_at_eol() {
        let s = "the quick";
        let r = text_object(s, 5, false, 'w').unwrap();
        assert_eq!(slice(s, r), " quick");
    }

    #[test]
    fn aw_whitespace_extension_keeps_graphemes_atomic() {
        for text in ["\u{600} word", " \u{301}word"] {
            let range = text_object(text, text.find("word").unwrap(), false, 'w').unwrap();
            assert_grapheme_range(text, range);
        }
        let combining_space = " \u{301}word";
        let range = text_object(
            combining_space,
            combining_space.find("word").unwrap(),
            false,
            'w',
        )
        .unwrap();
        assert_eq!(slice(combining_space, range), combining_space);
    }

    #[test]
    fn iw_in_punctuation_class_selects_the_contiguous_punctuation_run() {
        // `iw` groups same-class chars; `(`, `.`, `)` are all punctuation.
        let s = "foo(...)bar";
        let r = text_object(s, 4, true, 'w').unwrap();
        assert_eq!(slice(s, r), "(...)");
    }

    #[test]
    fn capital_iw_treats_word_plus_punctuation_as_a_single_word() {
        let s = "foo(bar) baz";
        let r = text_object(s, 0, true, 'W').unwrap();
        assert_eq!(slice(s, r), "foo(bar)");
    }

    #[test]
    fn iw_on_a_newline_selects_just_the_newline() {
        let s = "ab\ncd";
        let r = text_object(s, 2, true, 'w').unwrap();
        assert_eq!(slice(s, r), "\n");
    }

    #[test]
    fn iw_on_empty_or_past_end_buffer_returns_none() {
        assert_eq!(text_object("", 0, true, 'w'), None);
        assert_eq!(text_object("ab", 2, true, 'w'), None);
    }

    // ── quote: i" / a" ───────────────────────────────────────────────────

    #[test]
    fn i_quote_selects_inside_quotes_only() {
        let s = r#"x = "hello" end"#;
        let r = text_object(s, 6, true, '"').unwrap();
        assert_eq!(slice(s, r), "hello");
    }

    #[test]
    fn a_quote_includes_trailing_whitespace_when_present() {
        // vim: `a"` includes trailing whitespace if any, else leading.
        let s = r#"x = "hello" end"#;
        let r = text_object(s, 6, false, '"').unwrap();
        assert_eq!(slice(s, r), "\"hello\" ");
    }

    #[test]
    fn a_quote_falls_back_to_leading_whitespace_at_eol() {
        let s = r#"x = "hello""#;
        let r = text_object(s, 6, false, '"').unwrap();
        assert_eq!(slice(s, r), " \"hello\"");
    }

    #[test]
    fn quote_text_object_finds_inner_string_under_cursor() {
        let s = r#""a" "b" "c""#;
        let r = text_object(s, 5, true, '"').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn quote_text_object_on_whitespace_between_strings_selects_the_gap() {
        // Quirk of vim's prev-then-next algorithm: cursor in the space
        // between two quoted strings treats the closing+opening quote pair
        // as a "string". `i"` yields the lone space character.
        let s = r#""a" "b" "c""#;
        let r = text_object(s, 3, true, '"').unwrap();
        assert_eq!(slice(s, r), " ");
    }

    #[test]
    fn quote_text_object_respects_backslash_escape() {
        // `\"` is escaped, not a closing quote (vim's default 'quoteescape').
        let s = r#"x = "a\"b" y"#;
        let r = text_object(s, 6, true, '"').unwrap();
        assert_eq!(slice(s, r), r#"a\"b"#);
    }

    #[test]
    fn quote_text_object_returns_none_when_no_quotes_on_line() {
        let s = "no quotes here";
        assert_eq!(text_object(s, 3, true, '"'), None);
    }

    #[test]
    fn quote_text_object_does_not_cross_line_boundaries() {
        let s = "\"a\nb\"";
        // cursor on `a` - only the open quote is on this line.
        assert_eq!(text_object(s, 1, true, '"'), None);
    }

    #[test]
    fn q_alias_selects_the_nearest_quoted_string() {
        let s = r#""a" `b` 'c'"#;
        let r = text_object(s, 5, true, 'q').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn s_alias_selects_the_nearest_quoted_string() {
        let s = r#"call("hello")"#;
        let r = text_object(s, 7, true, 's').unwrap();
        assert_eq!(slice(s, r), "hello");
        let r = text_object(s, 7, false, 's').unwrap();
        assert_eq!(slice(s, r), "\"hello\"");
    }

    #[test]
    fn tag_text_objects_select_inner_and_outer_html_tags() {
        let s = r#"x <div class="a"><span>hello</span></div> y"#;
        let inner = text_object(s, 25, true, 't').unwrap();
        assert_eq!(slice(s, inner), "hello");
        let outer = text_object(s, 25, false, 't').unwrap();
        assert_eq!(slice(s, outer), "<span>hello</span>");
    }

    #[test]
    fn tag_text_object_handles_nested_same_name_tags() {
        let s = "<div>outer <div>inner</div> tail</div>";
        let inner = text_object(s, 17, false, 't').unwrap();
        assert_eq!(slice(s, inner), "<div>inner</div>");
        let outer = text_object(s, 30, false, 't').unwrap();
        assert_eq!(slice(s, outer), s);
    }

    #[test]
    fn delimiter_text_objects_include_complete_graphemes() {
        for (text, kind) in [
            ("\"\u{301}x\"\u{301}", '"'),
            ("(\u{301}x)\u{301}", '('),
            ("<b>\u{301}x</b>\u{301}", 't'),
            ("\u{600}(x)", '('),
        ] {
            let cursor = text.find('x').unwrap();
            let inner = text_object(text, cursor, true, kind).unwrap();
            let outer = text_object(text, cursor, false, kind).unwrap();
            assert_grapheme_range(text, inner);
            assert_grapheme_range(text, outer);
            assert_eq!(slice(text, inner), "x");
            assert_eq!(slice(text, outer), text);
        }
    }

    // ── pair: i( / a( / i{ / a{ / i[ / i< ───────────────────────────────

    #[test]
    fn i_paren_selects_inside_the_innermost_pair() {
        let s = "f(a, g(b), c)";
        let r = text_object(s, 7, true, '(').unwrap();
        assert_eq!(slice(s, r), "b");
    }

    #[test]
    fn a_paren_includes_the_delimiters() {
        let s = "f(a, g(b), c)";
        let r = text_object(s, 7, false, '(').unwrap();
        assert_eq!(slice(s, r), "(b)");
    }

    #[test]
    fn b_alias_is_equivalent_to_paren() {
        let s = "(hello)";
        let r1 = text_object(s, 3, true, '(').unwrap();
        let r2 = text_object(s, 3, true, 'b').unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn capital_b_alias_is_equivalent_to_brace() {
        let s = "{hello}";
        let r1 = text_object(s, 3, true, '{').unwrap();
        let r2 = text_object(s, 3, true, 'B').unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn brace_pair_selection_respects_nesting() {
        let s = "{a {b} c}";
        let r = text_object(s, 4, true, '{').unwrap();
        assert_eq!(slice(s, r), "b");
        let r = text_object(s, 1, true, '{').unwrap();
        assert_eq!(slice(s, r), "a {b} c");
    }

    #[test]
    fn bracket_pair_spans_multiple_lines() {
        let s = "[a\nb\nc]";
        let r = text_object(s, 3, true, '[').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc");
    }

    #[test]
    fn angle_pair_works_for_lt_and_gt_keys() {
        let s = "x<a>y";
        let r = text_object(s, 2, true, '<').unwrap();
        assert_eq!(slice(s, r), "a");
        let r = text_object(s, 2, false, '>').unwrap();
        assert_eq!(slice(s, r), "<a>");
    }

    #[test]
    fn pair_with_no_enclosing_open_returns_none() {
        let s = "no parens here";
        assert_eq!(text_object(s, 4, true, '('), None);
    }

    #[test]
    fn pair_with_open_but_missing_close_returns_none() {
        let s = "f(a, b, c";
        assert_eq!(text_object(s, 4, true, '('), None);
    }

    // ── paragraph: ip / ap ───────────────────────────────────────────────

    #[test]
    fn ip_selects_run_of_nonblank_lines_around_cursor() {
        let s = "a\nb\nc\n\nd\n";
        let r = text_object(s, 2, true, 'p').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc\n");
    }

    #[test]
    fn ap_extends_to_trailing_blank_lines() {
        let s = "a\nb\nc\n\n\nd\n";
        let r = text_object(s, 0, false, 'p').unwrap();
        assert_eq!(slice(s, r), "a\nb\nc\n\n\n");
    }

    #[test]
    fn ip_on_a_blank_line_selects_the_blank_run() {
        let s = "a\n\n\n\nb\n";
        let r = text_object(s, 3, true, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\n\n");
    }

    #[test]
    fn ap_on_blank_run_extends_to_trailing_nonblank_run() {
        // The two `\n` bytes at positions 2 and 3 each terminate one empty
        // (blank) line. Cursor sits in that blank run; `ap` glues it to the
        // following non-blank paragraph.
        let s = "a\n\n\nb\nc\n";
        let r = text_object(s, 2, false, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\nb\nc\n");
    }

    #[test]
    fn ap_in_trailing_blank_run_at_eof_returns_none() {
        // vim's `current_par` returns FAIL when cursor is in a trailing
        // blank run with no following non-blank paragraph.
        let s = "a\nb\n\n\n";
        assert_eq!(text_object(s, 4, false, 'p'), None);
    }

    #[test]
    fn ap_on_nonblank_with_only_leading_blanks_extends_backward() {
        // vim: when there are no trailing blanks but leading ones exist,
        // `ap` includes the leading blank run.
        let s = "\n\na\nb\n";
        let r = text_object(s, 2, false, 'p').unwrap();
        assert_eq!(slice(s, r), "\n\na\nb\n");
    }

    #[test]
    fn ip_in_single_paragraph_buffer_selects_whole_buffer() {
        let s = "only line\nsecond line";
        let r = text_object(s, 0, true, 'p').unwrap();
        assert_eq!(slice(s, r), "only line\nsecond line");
    }

    #[test]
    fn paragraph_on_empty_buffer_returns_none() {
        assert_eq!(text_object("", 0, true, 'p'), None);
        assert_eq!(text_object("", 0, false, 'p'), None);
    }

    #[test]
    fn ip_treats_whitespace_only_line_as_blank() {
        let s = "a\n   \n\t\nb\n";
        let r = text_object(s, 2, true, 'p').unwrap();
        assert_eq!(slice(s, r), "   \n\t\n");
    }
}
