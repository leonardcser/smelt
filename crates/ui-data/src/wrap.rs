//! Headless-safe text wrapping.

/// Wrap a line to fit within `width` display columns, breaking at
/// word boundaries. Words longer than `width` are broken
/// character-by-character. Width is measured in display columns
/// (wide chars like CJK count as 2).
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
