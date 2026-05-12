//! Pure text helpers for byte↔cell mapping.

/// Byte offset → terminal column (sum of `unicode-width` cells of preceding chars).
pub fn byte_to_cell(line: &str, byte: usize) -> usize {
    use unicode_width::UnicodeWidthStr;
    let mut p = byte.min(line.len());
    while p > 0 && !line.is_char_boundary(p) {
        p -= 1;
    }
    UnicodeWidthStr::width(&line[..p])
}

/// Terminal column → byte offset at which the preceding text occupies `cell` columns.
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

/// Build byte offsets for the start of each line in `lines.join("\n")`.
pub fn line_start_offsets(lines: &[String]) -> Vec<usize> {
    let mut v = Vec::with_capacity(lines.len());
    let mut acc = 0usize;
    for line in lines {
        v.push(acc);
        acc += line.len() + 1; // +1 for '\n'
    }
    v
}

/// Snap a byte offset to the nearest preceding char boundary in `s`.
/// Clamps to `s.len()`; never panics on stale anchors.
pub fn snap(s: &str, pos: usize) -> usize {
    let mut p = pos.min(s.len());
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Byte offset → character index.
pub fn char_pos(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx.min(s.len())].chars().count()
}

/// Character index → byte offset.
pub fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}
