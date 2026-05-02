//! Pure text-motion helpers shared by the vim keymap, the non-vim input
//! editor, and dialog input fields. All functions operate on `&str` buffers
//! and byte positions; they never mutate state.

/// Wrap a line to fit within `width` display columns, breaking at
/// word boundaries. Words longer than `width` are broken
/// character-by-character. Width is measured in terminal columns
/// (wide chars like CJK count as 2). Used by widgets (OptionList,
/// TabBar, custom dialog widgets) and by renderers that pre-wrap
/// content before emitting into `SpanCollector`.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    for logical_line in line.split('\n') {
        let mut current = String::new();
        let mut col = 0;
        for word in logical_line.split_inclusive(' ') {
            let wlen = UnicodeWidthStr::width(word);
            if col + wlen > width && col > 0 {
                chunks.push(current);
                current = String::new();
                col = 0;
            }
            if wlen > width {
                for ch in word.chars() {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if col + cw > width && col > 0 {
                        chunks.push(current);
                        current = String::new();
                        col = 0;
                    }
                    current.push(ch);
                    col += cw;
                }
            } else {
                current.push_str(word);
                col += wlen;
            }
        }
        chunks.push(current);
    }
    chunks
}

#[derive(Clone, Copy)]
pub enum CharClass {
    /// vim "word" boundaries: alphanumeric+underscore vs punctuation vs whitespace.
    Word,
    /// vim "WORD" boundaries: non-whitespace vs whitespace.
    #[allow(clippy::upper_case_acronyms)]
    WORD,
}

/// Clamp `pos` to `buf.len()` and snap backward to the nearest char
/// boundary. Prevents byte-slicing panics when callers hand us an
/// offset that was computed on a different snapshot of the string.
pub fn snap(buf: &str, pos: usize) -> usize {
    let mut p = pos.min(buf.len());
    while p > 0 && !buf.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Convert a byte offset inside `line` to the terminal column the
/// character there would occupy (sum of `unicode-width` cells of every
/// preceding char). Handles offsets mid-multibyte-char by snapping
/// backward to the nearest char boundary first.
pub fn byte_to_cell(line: &str, byte: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(&line[..snap(line, byte)])
}

/// Shared vertical-motion helper: move `cpos` up or down by one line in
/// `buf`, preserving the preferred display column (`curswant`). Returns
/// `(new_cpos, new_curswant)` where `new_curswant` is the column the
/// caller should remember for the next vertical motion — either the
/// supplied one (if we landed short of it on a shorter line) or the
/// current cell column (on the first vertical motion after a horizontal
/// one, when `curswant` is `None`).
///
/// This is the single code path for every vertical-motion source that
/// wants vim's "stay on the column you wanted, even if the intermediate
/// line was too short" behaviour: shift+arrow, vim j/k in Normal,
/// vim j/k in Visual, mouse-wheel scroll, Ctrl+U/D half-page, etc.
///
/// Columns are in terminal display cells, so wide glyphs (`⏺`, `─`,
/// CJK) behave correctly — you end up under the glyph, not mid-bytes.
pub fn vertical_move(
    buf: &str,
    cpos: usize,
    delta_lines: isize,
    curswant: Option<usize>,
) -> (usize, usize) {
    let cpos = snap(buf, cpos);
    // Build (line_start, line_end) pairs by walking `\n`.
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (i, &b) in buf.as_bytes().iter().enumerate() {
        if b == b'\n' {
            lines.push((start, i));
            start = i + 1;
        }
    }
    lines.push((start, buf.len()));
    let cur_line = lines
        .iter()
        .position(|&(s, e)| cpos >= s && cpos <= e)
        .unwrap_or(lines.len() - 1);
    let (cur_sol, _) = lines[cur_line];
    let cur_col = byte_to_cell(&buf[cur_sol..], cpos - cur_sol);
    let want = curswant.unwrap_or(cur_col);
    if delta_lines == 0 {
        return (cpos, want);
    }
    let target_line = if delta_lines > 0 {
        (cur_line + delta_lines as usize).min(lines.len() - 1)
    } else {
        cur_line.saturating_sub((-delta_lines) as usize)
    };
    let (sol, eol) = lines[target_line];
    let byte_in_line = cell_to_byte(&buf[sol..eol], want);
    (sol + byte_in_line, want)
}

/// Byte offset of the char boundary before `pos`. Returns 0 at start.
pub fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Byte offset of the char boundary after `pos`. Returns `s.len()` at end.
pub fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Inverse of [`byte_to_cell`]: find the byte offset whose preceding
/// text occupies `cell` terminal columns. Wide glyphs that cross the
/// target land on their starting byte (never mid-glyph).
pub fn cell_to_byte(line: &str, cell: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut acc = 0usize;
    for (b, ch) in line.char_indices() {
        if acc >= cell {
            return b;
        }
        acc += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    line.len()
}

pub fn char_class(c: char, mode: CharClass) -> u8 {
    match mode {
        CharClass::Word => {
            if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        }
        CharClass::WORD => {
            if c.is_whitespace() {
                0
            } else {
                1
            }
        }
    }
}

pub fn word_forward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let cpos = snap(buf, cpos);
    let chars: Vec<(usize, char)> = buf[cpos..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    let start_class = char_class(chars[0].1, mode);
    // Skip same class.
    while i < chars.len() && char_class(chars[i].1, mode) == start_class {
        i += 1;
    }
    // Skip whitespace.
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i < chars.len() {
        cpos + chars[i].0
    } else {
        buf.len()
    }
}

pub fn word_backward_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let cpos = snap(buf, cpos);
    if cpos == 0 {
        return 0;
    }
    let chars: Vec<(usize, char)> = buf[..cpos].char_indices().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut i = chars.len() - 1;
    // Skip whitespace backward.
    while i > 0 && char_class(chars[i].1, mode) == 0 {
        i -= 1;
    }
    let target_class = char_class(chars[i].1, mode);
    // Skip same class backward.
    while i > 0 && char_class(chars[i - 1].1, mode) == target_class {
        i -= 1;
    }
    chars[i].0
}

pub fn line_start(buf: &str, cpos: usize) -> usize {
    let cpos = snap(buf, cpos);
    buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

pub fn line_end(buf: &str, cpos: usize) -> usize {
    let cpos = snap(buf, cpos);
    cpos + buf[cpos..].find('\n').unwrap_or(buf.len() - cpos)
}

/// Find word boundaries around the given byte offset inside `buf`.
/// A word is a contiguous run of alphanumeric characters plus `_`.
/// `transparent` lists byte positions that are treated as if they
/// were word characters during the walk — used to cross soft-wrap
/// `\n` boundaries so a word split by a display wrap still selects
/// as one unit. `transparent` must be sorted ascending.
/// Leading/trailing transparent bytes are trimmed from the returned
/// range. Returns `None` if `pos` is in whitespace / out of bounds.
pub fn word_range_at_transparent(
    buf: &str,
    pos: usize,
    transparent: &[usize],
) -> Option<(usize, usize)> {
    token_range_at_transparent(buf, pos, transparent, |c| c.is_alphanumeric() || c == '_')
}

/// Vim "WORD" range: any contiguous run of non-whitespace.
pub fn big_word_range_at_transparent(
    buf: &str,
    pos: usize,
    transparent: &[usize],
) -> Option<(usize, usize)> {
    token_range_at_transparent(buf, pos, transparent, |c| !c.is_whitespace())
}

fn token_range_at_transparent<F>(
    buf: &str,
    pos: usize,
    transparent: &[usize],
    is_word: F,
) -> Option<(usize, usize)>
where
    F: Fn(char) -> bool,
{
    let is_trans = |p: usize| transparent.binary_search(&p).is_ok();
    if pos >= buf.len() {
        return None;
    }
    let first = buf[pos..].chars().next()?;
    if !is_word(first) && !is_trans(pos) {
        return None;
    }
    let mut start = pos;
    while start > 0 {
        let prev = buf[..start]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        let c = buf[prev..].chars().next()?;
        if !is_word(c) && !is_trans(prev) {
            break;
        }
        start = prev;
    }
    let mut end = pos;
    while end < buf.len() {
        let c = buf[end..].chars().next()?;
        if !is_word(c) && !is_trans(end) {
            break;
        }
        end += c.len_utf8();
    }
    while start < end {
        let c = buf[start..].chars().next()?;
        if is_trans(start) && !is_word(c) {
            start += c.len_utf8();
        } else {
            break;
        }
    }
    while end > start {
        let prev = buf[..end].char_indices().next_back().map(|(i, _)| i)?;
        let c = buf[prev..].chars().next()?;
        if is_trans(prev) && !is_word(c) {
            end = prev;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    if !buf[start..end].chars().any(is_word) {
        return None;
    }
    Some((start, end))
}

/// Source-line range at `pos`. `hard_breaks` lists byte positions of
/// `\n` characters that are "real" line breaks (i.e. not soft-wrap
/// continuations). Returns the span bounded by the previous hard
/// break (exclusive) and the next hard break (exclusive), or the
/// buffer start/end. The returned range does not include the
/// trailing `\n`.
pub(crate) fn line_range_at(
    buf: &str,
    pos: usize,
    hard_breaks: &[usize],
) -> Option<(usize, usize)> {
    if buf.is_empty() {
        return None;
    }
    let pos = pos.min(buf.len());
    let start = match hard_breaks.binary_search(&pos) {
        Ok(_) => pos + 1,
        Err(idx) => {
            if idx == 0 {
                0
            } else {
                hard_breaks[idx - 1] + 1
            }
        }
    };
    let end = match hard_breaks.binary_search(&pos) {
        Ok(_) => pos,
        Err(idx) => {
            if idx < hard_breaks.len() {
                hard_breaks[idx]
            } else {
                buf.len()
            }
        }
    };
    if end <= start {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_range_at_plain() {
        let s = "hello world";
        assert_eq!(word_range_at_transparent(s, 0, &[]), Some((0, 5)));
        assert_eq!(word_range_at_transparent(s, 4, &[]), Some((0, 5)));
        assert_eq!(word_range_at_transparent(s, 6, &[]), Some((6, 11)));
        assert_eq!(word_range_at_transparent(s, 5, &[]), None); // on the space
    }

    #[test]
    fn word_range_at_treats_newline_as_non_word() {
        // Baseline: walk stops at '\n', so clicking on "world" only
        // selects "world", not "hello\nworld".
        assert_eq!(
            word_range_at_transparent("hello\nworld", 6, &[]),
            Some((6, 11))
        );
    }

    #[test]
    fn word_range_at_transparent_crosses_soft_wrap() {
        // "verylong" was soft-wrapped as "very\nlong". The \n at byte 4
        // is a soft-wrap — treat as transparent → whole "verylong"
        // selects regardless of which side was clicked.
        let s = "very\nlong";
        let transparent = [4usize];
        assert_eq!(word_range_at_transparent(s, 0, &transparent), Some((0, 9)));
        assert_eq!(word_range_at_transparent(s, 5, &transparent), Some((0, 9)));
        // Click on the transparent \n itself → still selects the word.
        assert_eq!(word_range_at_transparent(s, 4, &transparent), Some((0, 9)));
    }

    #[test]
    fn word_range_at_transparent_trims_trailing_wrap() {
        // "end\n\nrest" with the first \n soft and second hard. Click
        // on "end" should return [0, 3) — the trailing transparent \n
        // is trimmed because nothing word-like followed it in the
        // extended walk (the hard \n stops forward walk before "rest").
        let s = "end\n\nrest";
        let transparent = [3usize]; // first \n is soft
        assert_eq!(word_range_at_transparent(s, 0, &transparent), Some((0, 3)));
    }

    #[test]
    fn word_range_at_transparent_returns_none_on_punctuation() {
        assert_eq!(word_range_at_transparent("a, b", 1, &[]), None);
    }

    #[test]
    fn line_range_at_single_line() {
        let b = "just one line";
        assert_eq!(line_range_at(b, 0, &[]), Some((0, 13)));
        assert_eq!(line_range_at(b, 6, &[]), Some((0, 13)));
    }

    #[test]
    fn line_range_at_multiple_lines() {
        let b = "first\nsecond\nthird";
        let hard = [5usize, 12];
        assert_eq!(line_range_at(b, 2, &hard), Some((0, 5)));
        assert_eq!(line_range_at(b, 6, &hard), Some((6, 12)));
        assert_eq!(line_range_at(b, 13, &hard), Some((13, 18)));
    }

    #[test]
    fn line_range_at_joins_soft_wrapped_rows() {
        // "one long paragraph\nnext" with the first \n being a soft
        // wrap (not in hard_breaks) and the second a real line break.
        // Clicking in the wrapped part should return the full
        // paragraph span.
        let b = "one long\nparagraph\nnext";
        let hard = [18usize]; // only the second \n is hard
        assert_eq!(line_range_at(b, 4, &hard), Some((0, 18)));
        assert_eq!(line_range_at(b, 10, &hard), Some((0, 18)));
        assert_eq!(line_range_at(b, 19, &hard), Some((19, 23)));
    }

    #[test]
    fn line_range_at_on_hard_break_returns_empty_line() {
        let b = "a\n\nb";
        let hard = [1usize, 2];
        // Pos on the first hard break → empty line after it.
        assert_eq!(line_range_at(b, 1, &hard), None);
        // Pos on the second hard break → empty line between.
        assert_eq!(line_range_at(b, 2, &hard), None);
    }
}
