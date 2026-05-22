/// Wrap `line` to `width` display columns, breaking at word boundaries.
/// Words wider than `width` are broken character-by-character.
///
/// Returns byte ranges within `line`. When `line` contains newlines, each
/// logical line is wrapped independently and embedded newlines force breaks
/// (the `'\n'` byte itself is not included in any chunk).
///
/// At least one chunk is always returned (even for empty input).
pub fn wrap_line_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    if line.is_empty() {
        out.push((0, 0));
        return out;
    }
    if width == 0 {
        out.push((0, line.len()));
        return out;
    }
    let mut logical_start = 0usize;
    loop {
        let rel = line[logical_start..].find('\n');
        let logical_end = rel.map(|p| logical_start + p).unwrap_or(line.len());
        wrap_logical(line, logical_start, logical_end, width, &mut out);
        match rel {
            Some(p) => {
                logical_start = logical_start + p + 1; // skip '\n'
                if logical_start > line.len() {
                    break;
                }
                if logical_start == line.len() {
                    // Trailing newline → final empty logical line.
                    out.push((logical_start, logical_start));
                    break;
                }
            }
            None => break,
        }
    }
    out
}

fn wrap_logical(line: &str, start: usize, end: usize, width: usize, out: &mut Vec<(usize, usize)>) {
    use unicode_width::UnicodeWidthChar;
    if start == end {
        out.push((start, end));
        return;
    }
    let bytes = line.as_bytes();
    let mut chunk_start = start;
    let mut chunk_end = start;
    let mut col = 0usize;
    let mut word_start = start;
    let mut i = start;
    while i <= end {
        let at_end = i == end;
        let at_space = !at_end && bytes[i] == b' ';
        if !(at_space || at_end) {
            i += 1;
            continue;
        }
        // Process the word `line[word_start..i]`.
        let word_end = i;
        let word_w: usize = line[word_start..word_end]
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let trailing = if at_space { 1 } else { 0 };
        let total_w = word_w + trailing;
        // If word+space doesn't fit on current line and chunk has content, emit.
        if col + total_w > width && col > 0 {
            out.push((chunk_start, chunk_end));
            chunk_start = word_start;
            col = 0;
        }
        if word_w > width {
            // Char-break the word.
            let mut idx = word_start;
            for ch in line[word_start..word_end].chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col + cw > width && col > 0 {
                    out.push((chunk_start, chunk_end));
                    chunk_start = idx;
                    col = 0;
                }
                chunk_end = idx + ch.len_utf8();
                col += cw;
                idx += ch.len_utf8();
            }
        } else {
            chunk_end = word_end;
            col += word_w;
        }
        if at_space {
            // Append the space (may force a wrap if it overflows; rare with width≥1).
            if col + 1 > width && col > 0 {
                out.push((chunk_start, chunk_end));
                chunk_start = word_end + 1;
                chunk_end = word_end + 1;
                col = 0;
            } else {
                chunk_end = word_end + 1;
                col += 1;
            }
            i += 1;
            word_start = i;
        } else {
            break;
        }
    }
    out.push((chunk_start, chunk_end));
}

/// Wrap `line` to `width` display columns, breaking at word boundaries.
/// Words wider than `width` are broken character-by-character.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    wrap_line_ranges(line, width)
        .into_iter()
        .map(|(s, e)| line[s..e].to_string())
        .collect()
}

/// Like [`wrap_line`] but returns borrowed slices into `line` instead of
/// allocating a `String` per chunk. The caller must ensure `line` outlives
/// the returned slices.
pub fn wrap_line_borrowed(line: &str, width: usize) -> Vec<&str> {
    wrap_line_ranges(line, width)
        .into_iter()
        .map(|(s, e)| &line[s..e])
        .collect()
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn empty_line_returns_single_empty_chunk() {
        assert_eq!(wrap_line_ranges("", 10), vec![(0, 0)]);
    }

    #[test]
    fn zero_width_returns_whole_line() {
        assert_eq!(wrap_line_ranges("hello world", 0), vec![(0, 11)]);
    }

    #[test]
    fn no_wrap_when_within_width() {
        assert_eq!(wrap_line_ranges("hello", 10), vec![(0, 5)]);
    }

    #[test]
    fn breaks_at_word_boundary() {
        // "hello world" with width 7: "hello " (6) fits; "world" forces wrap.
        let r = wrap_line_ranges("hello world", 7);
        let chunks: Vec<&str> = r.iter().map(|(s, e)| &"hello world"[*s..*e]).collect();
        assert_eq!(chunks, vec!["hello ", "world"]);
    }

    #[test]
    fn oversized_word_char_breaks() {
        // "abcdefghij" with width 4 → "abcd", "efgh", "ij".
        let s = "abcdefghij";
        let r = wrap_line_ranges(s, 4);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn embedded_newline_forces_break() {
        let s = "a\nb";
        let r = wrap_line_ranges(s, 10);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["a", "b"]);
    }

    #[test]
    fn wrap_line_matches_ranges() {
        let s = "the quick brown fox";
        let by_string = wrap_line(s, 10);
        let by_ranges: Vec<String> = wrap_line_ranges(s, 10)
            .into_iter()
            .map(|(a, b)| s[a..b].to_string())
            .collect();
        assert_eq!(by_string, by_ranges);
    }
}
