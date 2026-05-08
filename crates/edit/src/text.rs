//! Pure text-motion helpers over `&str` buffers and byte positions.

pub use smelt_buffer::wrap::wrap_line;

#[derive(Clone, Copy)]
pub enum CharClass {
    /// vim "word" boundaries: alphanumeric+underscore vs punctuation vs whitespace.
    Word,
    /// vim "WORD" boundaries: non-whitespace vs whitespace.
    #[allow(clippy::upper_case_acronyms)]
    WORD,
}

/// Clamp `pos` to `buf.len()` and snap back to the nearest char boundary.
pub fn snap(buf: &str, pos: usize) -> usize {
    let mut p = pos.min(buf.len());
    while p > 0 && !buf.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Byte offset → terminal column (sum of `unicode-width` cells of preceding chars).
pub fn byte_to_cell(line: &str, byte: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(&line[..snap(line, byte)])
}

/// Move `cpos` by `delta_lines` in `buf`, preserving the preferred display column (`curswant`).
/// Returns `(new_cpos, new_curswant)`. Columns are in terminal display cells (wide-char safe).
pub fn vertical_move(
    buf: &str,
    cpos: usize,
    delta_lines: isize,
    curswant: Option<usize>,
) -> (usize, usize) {
    let cpos = snap(buf, cpos);
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

/// Inverse of [`byte_to_cell`]: byte offset at which the preceding text occupies `cell` columns.
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
    while i < chars.len() && char_class(chars[i].1, mode) == start_class {
        i += 1;
    }
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
    while i > 0 && char_class(chars[i].1, mode) == 0 {
        i -= 1;
    }
    let target_class = char_class(chars[i].1, mode);
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

/// Word boundaries around `pos` (`[a-zA-Z0-9_]` runs).
/// `transparent` byte positions (sorted) are crossed as if they were word chars
/// (for soft-wrap `\n` so a wrapped word selects as one unit). Leading/trailing
/// transparent bytes are trimmed. Returns `None` on whitespace or out of bounds.
pub fn word_range_at_transparent(
    buf: &str,
    pos: usize,
    transparent: &[usize],
) -> Option<(usize, usize)> {
    token_range_at_transparent(buf, pos, transparent, |c| c.is_alphanumeric() || c == '_')
}

/// Vim WORD boundaries: any contiguous run of non-whitespace.
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

/// Source-line span at `pos`, bounded by surrounding hard breaks (excluded).
/// `hard_breaks` lists `\n` byte positions that are real line breaks, sorted ascending.
/// Returns `None` on an empty buffer. The returned range excludes the trailing `\n`.
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
        assert_eq!(
            word_range_at_transparent("hello\nworld", 6, &[]),
            Some((6, 11))
        );
    }

    #[test]
    fn word_range_at_transparent_crosses_soft_wrap() {
        let s = "very\nlong";
        let transparent = [4usize];
        assert_eq!(word_range_at_transparent(s, 0, &transparent), Some((0, 9)));
        assert_eq!(word_range_at_transparent(s, 5, &transparent), Some((0, 9)));
        assert_eq!(word_range_at_transparent(s, 4, &transparent), Some((0, 9)));
    }

    #[test]
    fn word_range_at_transparent_trims_trailing_wrap() {
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
        assert_eq!(line_range_at(b, 1, &hard), None);
        assert_eq!(line_range_at(b, 2, &hard), None);
    }
}
