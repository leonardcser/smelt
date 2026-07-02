//! Terminal cell-width rules shared by layout, buffers, and renderers.
//!
//! Every layer that stores or paints column offsets must use these helpers so
//! wrapping, highlight spans, grid continuation cells, snapshots, and diff
//! flushing agree on the width of emitted text.

/// Width of one emitted character in terminal cells.
pub fn char_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch)
        .unwrap_or(1)
        .max(1)
}

/// Saturating `u16` form of [`char_width`].
pub fn char_width_u16(ch: char) -> u16 {
    char_width(ch).min(u16::MAX as usize) as u16
}

/// Width of emitted text in terminal cells.
///
/// `UnicodeWidthStr` preserves grapheme-sequence behavior for prefixes so
/// variation selectors, ZWJ sequences, and combining marks match terminal cell
/// measurements used elsewhere.
pub fn text_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

/// Saturating `u16` form of [`text_width`].
pub fn text_width_u16(text: &str) -> u16 {
    text_width(text).min(u16::MAX as usize) as u16
}

/// Whether `ch` is in a Unicode private-use range.
pub fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_use_icons_follow_unicode_width() {
        assert_eq!(char_width('\u{e6b2}'), 1);
        assert_eq!(text_width("\u{e6b2} "), 2);
    }

    #[test]
    fn controls_still_occupy_cells() {
        assert_eq!(char_width('\0'), 1);
        assert_eq!(text_width("\0\0x"), 3);
    }

    #[test]
    fn text_width_matches_character_sum_for_grid_text() {
        for text in ["abc", "a漢b", "\u{e6b2} ", "\0\0x"] {
            let summed: usize = text.chars().map(char_width).sum();
            assert_eq!(text_width(text), summed, "text width drift for {text:?}");
        }
    }
}
