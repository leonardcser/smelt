//! Terminal cell-width rules shared by layout, buffers, and renderers.
//!
//! Every layer that stores or paints column offsets must use these helpers so
//! wrapping, highlight spans, grid continuation cells, snapshots, and diff
//! flushing agree on the width of emitted text.

/// Width of one Unicode scalar in terminal cells.
///
/// This can be zero for combining marks and joiners. Text layout should iterate
/// [`graphemes`] and measure each cluster with [`text_width`] rather than summing
/// scalar widths.
pub fn char_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1)
}

/// Saturating `u16` form of [`char_width`].
pub fn char_width_u16(ch: char) -> u16 {
    char_width(ch).min(u16::MAX as usize) as u16
}

/// Extended grapheme clusters in `text`.
pub fn graphemes(text: &str) -> impl DoubleEndedIterator<Item = &str> {
    use unicode_segmentation::UnicodeSegmentation;
    UnicodeSegmentation::graphemes(text, true)
}

/// Byte offsets and extended grapheme clusters in `text`.
pub fn grapheme_indices(text: &str) -> impl DoubleEndedIterator<Item = (usize, &str)> {
    use unicode_segmentation::UnicodeSegmentation;
    UnicodeSegmentation::grapheme_indices(text, true)
}

/// Width of emitted text in terminal cells.
///
/// `UnicodeWidthStr` preserves sequence behavior for variation selectors, ZWJ
/// emoji, regional indicators, and combining marks.
pub fn text_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(text)
}

/// Width of adjacent text runs measured as one terminal string.
///
/// Styling boundaries are not grapheme boundaries. Joining before measuring
/// preserves combining, variation-selector, ZWJ, and regional-indicator
/// sequences that a producer split across runs.
pub fn joined_text_width<'a>(parts: impl IntoIterator<Item = &'a str>) -> usize {
    let mut text = String::new();
    for part in parts {
        text.push_str(part);
    }
    text_width(&text)
}

/// Saturating `u16` form of [`joined_text_width`].
pub fn joined_text_width_u16<'a>(parts: impl IntoIterator<Item = &'a str>) -> u16 {
    joined_text_width(parts).min(u16::MAX as usize) as u16
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
    fn combining_marks_have_zero_scalar_width() {
        assert_eq!(char_width('\u{308}'), 0);
        assert_eq!(text_width("a\u{308}"), 1);
    }

    #[test]
    fn graphemes_keep_terminal_sequences_together() {
        for (text, expected) in [
            ("a\u{308}b", vec!["a\u{308}", "b"]),
            ("👩\u{200d}💻x", vec!["👩\u{200d}💻", "x"]),
            ("9\u{fe0f}?", vec!["9\u{fe0f}", "?"]),
        ] {
            assert_eq!(graphemes(text).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn joined_text_width_ignores_style_boundaries_inside_graphemes() {
        assert_eq!(joined_text_width(["e", "\u{301}"]), 1);
        assert_eq!(joined_text_width(["👩", "\u{200d}", "💻"]), 2);
        assert_eq!(joined_text_width(["9", "\u{fe0f}"]), 2);
        assert_eq!(joined_text_width(["🇨", "🇦"]), 2);
    }

    #[test]
    fn text_width_matches_grapheme_sum() {
        for text in [
            "abc",
            "a漢b",
            "a\u{308}b",
            "👩\u{200d}💻x",
            "\u{e6b2} ",
            "\0\0x",
        ] {
            let summed: usize = graphemes(text).map(text_width).sum();
            assert_eq!(text_width(text), summed, "text width drift for {text:?}");
        }
    }
}
