//! Vim text-object selection (iw/aw, i"/a", i(/a(, ip/ap, etc.) over `&str` buffers.

use super::text::{char_class, line_end, line_start, next_char_boundary, CharClass};

pub(crate) fn text_object(
    buf: &str,
    cpos: usize,
    inner: bool,
    kind: char,
) -> Option<(usize, usize)> {
    match kind {
        'w' => text_object_word(buf, cpos, inner, CharClass::Word),
        'W' => text_object_word(buf, cpos, inner, CharClass::WORD),
        '"' | '\'' | '`' => text_object_quote(buf, cpos, inner, kind),
        '(' | ')' | 'b' => text_object_pair(buf, cpos, inner, '(', ')'),
        '[' | ']' => text_object_pair(buf, cpos, inner, '[', ']'),
        '{' | '}' | 'B' => text_object_pair(buf, cpos, inner, '{', '}'),
        '<' | '>' => text_object_pair(buf, cpos, inner, '<', '>'),
        'p' => text_object_paragraph(buf, cpos, inner),
        _ => None,
    }
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
    let chars: Vec<(usize, char)> = buf.char_indices().collect();
    let ci = chars.iter().position(|(i, _)| *i >= cpos)?;
    let cur_char = chars[ci].1;
    let cur_class = char_class(cur_char, mode);

    if cur_char == '\n' {
        let byte_pos = chars[ci].0;
        return Some((byte_pos, byte_pos + 1));
    }

    let mut start = ci;
    while start > 0 {
        let prev = chars[start - 1].1;
        if prev == '\n' || char_class(prev, mode) != cur_class {
            break;
        }
        start -= 1;
    }
    let mut end = ci;
    while end + 1 < chars.len() {
        let next = chars[end + 1].1;
        if next == '\n' || char_class(next, mode) != cur_class {
            break;
        }
        end += 1;
    }

    let byte_start = chars[start].0;
    let byte_end = if end + 1 < chars.len() {
        chars[end + 1].0
    } else {
        buf.len()
    };

    if inner {
        Some((byte_start, byte_end))
    } else {
        // "a word" includes trailing whitespace, or leading if none trailing.
        let mut a_end = byte_end;
        while a_end < buf.len() && matches!(buf.as_bytes()[a_end], b' ' | b'\t') {
            a_end += 1;
        }
        if a_end > byte_end {
            Some((byte_start, a_end))
        } else {
            let mut a_start = byte_start;
            while a_start > 0 && matches!(buf.as_bytes()[a_start - 1], b' ' | b'\t') {
                a_start -= 1;
            }
            Some((a_start, byte_end))
        }
    }
}

fn text_object_quote(buf: &str, cpos: usize, inner: bool, quote: char) -> Option<(usize, usize)> {
    // Mirrors vim's `current_quote` (textobject.c): scan backward for an
    // opening quote, then forward for a closing one. `\` escapes the next
    // char (vim's default `'quoteescape'`).
    let line_s = line_start(buf, cpos);
    let line_e = line_end(buf, cpos);
    let line = &buf[line_s..line_e];
    let rel = cpos - line_s;
    let qlen = quote.len_utf8();

    let mut open = find_prev_quote(line, rel, quote, true);
    if open.is_none() {
        open = find_next_quote(line, rel, quote, false);
    }
    let open = open?;
    let close = find_next_quote(line, open + qlen, quote, true)?;

    let abs_open = line_s + open;
    let abs_close = line_s + close;

    if inner {
        return Some((abs_open + qlen, abs_close));
    }
    let bytes = buf.as_bytes();
    let after = abs_close + qlen;
    let mut a_end = after;
    while a_end < line_e && matches!(bytes[a_end], b' ' | b'\t') {
        a_end += 1;
    }
    if a_end > after {
        return Some((abs_open, a_end));
    }
    let mut a_start = abs_open;
    while a_start > line_s && matches!(bytes[a_start - 1], b' ' | b'\t') {
        a_start -= 1;
    }
    Some((a_start, after))
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

fn text_object_paragraph(buf: &str, cpos: usize, inner: bool) -> Option<(usize, usize)> {
    if buf.is_empty() {
        return None;
    }

    // Build line table: (start_byte, end_byte_exclusive_including_newline, is_blank).
    let mut lines: Vec<(usize, usize, bool)> = Vec::new();
    let bytes = buf.as_bytes();
    let mut start = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let line = &buf[start..i];
            let is_blank = line.bytes().all(|b| b == b' ' || b == b'\t');
            lines.push((start, i + 1, is_blank));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        let line = &buf[start..];
        let is_blank = line.bytes().all(|b| b == b' ' || b == b'\t');
        lines.push((start, bytes.len(), is_blank));
    }

    if lines.is_empty() {
        return None;
    }

    let clamped = cpos.min(buf.len());
    let li = lines
        .iter()
        .position(|(s, e, _)| clamped >= *s && clamped < *e)
        .unwrap_or(lines.len() - 1);

    let blank = lines[li].2;

    // Expand to the run of same-blank-status lines containing `li`.
    let mut lo = li;
    while lo > 0 && lines[lo - 1].2 == blank {
        lo -= 1;
    }
    let mut hi = li;
    while hi + 1 < lines.len() && lines[hi + 1].2 == blank {
        hi += 1;
    }

    if inner {
        return Some((lines[lo].0, lines[hi].1));
    }

    // Mirrors vim's `current_par`:
    // - With a trailing run of opposite-status lines, extend through it.
    // - With no trailing run but cursor in a non-blank paragraph, fall back
    //   to leading blank lines (the "no white in front, none in back" branch).
    // - With cursor in a trailing blank run at EOF, `ap` fails — return None.
    if hi + 1 < lines.len() {
        let other = lines[hi + 1].2;
        let mut hi2 = hi + 1;
        while hi2 + 1 < lines.len() && lines[hi2 + 1].2 == other {
            hi2 += 1;
        }
        Some((lines[lo].0, lines[hi2].1))
    } else if !blank && lo > 0 && lines[lo - 1].2 {
        let mut lo2 = lo - 1;
        while lo2 > 0 && lines[lo2 - 1].2 {
            lo2 -= 1;
        }
        Some((lines[lo2].0, lines[hi].1))
    } else if !blank {
        Some((lines[lo].0, lines[hi].1))
    } else {
        None
    }
}

fn text_object_pair(
    buf: &str,
    cpos: usize,
    inner: bool,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    let mut open_pos = None;
    let snapped = smelt_buffer::text::snap(buf, cpos);
    let upper = next_char_boundary(buf, snapped);
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
                return if inner {
                    Some((open_pos + open.len_utf8(), close_pos))
                } else {
                    Some((open_pos, close_pos + close.len_utf8()))
                };
            }
            depth -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(buf: &str, range: (usize, usize)) -> &str {
        &buf[range.0..range.1]
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
        // cursor on `a` — only the open quote is on this line.
        assert_eq!(text_object(s, 1, true, '"'), None);
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
