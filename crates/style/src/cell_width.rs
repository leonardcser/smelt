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
    grapheme_indices(text).map(|(_, grapheme)| grapheme)
}

/// Byte offsets and extended grapheme clusters in `text`.
pub fn grapheme_indices(text: &str) -> impl DoubleEndedIterator<Item = (usize, &str)> {
    use unicode_segmentation::UnicodeSegmentation;
    if text.is_ascii() {
        GraphemeIndices::Ascii {
            text,
            front: 0,
            back: text.len(),
        }
    } else {
        GraphemeIndices::Unicode(UnicodeSegmentation::grapheme_indices(text, true))
    }
}

/// In ASCII every byte is its own cluster except `\r\n`, so ASCII text skips
/// the general segmentation state machine.
enum GraphemeIndices<'a> {
    Ascii {
        text: &'a str,
        front: usize,
        back: usize,
    },
    Unicode(unicode_segmentation::GraphemeIndices<'a>),
}

impl<'a> Iterator for GraphemeIndices<'a> {
    type Item = (usize, &'a str);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ascii { text, front, back } => {
                let start = *front;
                if start >= *back {
                    return None;
                }
                let bytes = text.as_bytes();
                let crlf = bytes[start] == b'\r' && start + 1 < *back && bytes[start + 1] == b'\n';
                *front = start + 1 + usize::from(crlf);
                Some((start, &text[start..*front]))
            }
            Self::Unicode(inner) => inner.next(),
        }
    }
}

impl DoubleEndedIterator for GraphemeIndices<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ascii { text, front, back } => {
                let end = *back;
                if end <= *front {
                    return None;
                }
                let bytes = text.as_bytes();
                let crlf = bytes[end - 1] == b'\n' && end - 1 > *front && bytes[end - 2] == b'\r';
                *back = end - 1 - usize::from(crlf);
                Some((*back, &text[*back..end]))
            }
            Self::Unicode(inner) => inner.next_back(),
        }
    }
}

/// Width of emitted text in terminal cells.
///
/// `UnicodeWidthStr` preserves sequence behavior for variation selectors, ZWJ
/// emoji, regional indicators, and combining marks.
pub fn text_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    // Printable ASCII is one cell per byte; most painted text is ASCII.
    if text.bytes().all(|b| matches!(b, b' '..=b'~')) {
        return text.len();
    }
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
    fn ascii_fast_path_matches_unicode_segmentation() {
        use unicode_segmentation::UnicodeSegmentation;
        for text in [
            "",
            "a",
            "abc",
            "\r",
            "\n",
            "\r\n",
            "\r\r\n",
            "\n\r",
            "a\r\nb\n\r\nc\r",
            "\0\x1b[1m\t \x7f",
        ] {
            let expected: Vec<_> = text.grapheme_indices(true).collect();
            assert_eq!(
                grapheme_indices(text).collect::<Vec<_>>(),
                expected,
                "{text:?}"
            );
            let mut reversed: Vec<_> = grapheme_indices(text).rev().collect();
            reversed.reverse();
            assert_eq!(reversed, expected, "{text:?}");
            // Meeting in the middle from both ends must still yield each cluster once.
            for split in 0..=expected.len() {
                let mut iter = grapheme_indices(text);
                let mut front: Vec<_> = iter.by_ref().take(split).collect();
                let mut back: Vec<_> = iter.rev().collect();
                back.reverse();
                front.extend(back);
                assert_eq!(front, expected, "{text:?} split {split}");
            }
        }
    }

    #[test]
    fn ascii_fast_path_matches_unicode_width() {
        use unicode_width::UnicodeWidthStr;
        let printable: String = (b' '..=b'~').map(char::from).collect();
        assert_eq!(
            text_width(&printable),
            UnicodeWidthStr::width(printable.as_str())
        );
        for b in 0..=0x7f_u8 {
            let text = char::from(b).to_string();
            assert_eq!(
                text_width(&text),
                UnicodeWidthStr::width(text.as_str()),
                "{b:#x}"
            );
        }
    }

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
