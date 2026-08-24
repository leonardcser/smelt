//! Display-cell width helpers. One home for "truncate / pad a string so
//! it fits in N terminal columns" so toast clipping, the `smelt.text.fit`
//! Lua API, and any future fixed-width slot stay byte-for-byte
//! consistent.

use smelt_buffer::{cell_width, text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RightPaddedText {
    pub text: String,
    pub body: String,
}

/// Greatest prefix of `s` whose display width is `<= max_cells`. Snaps to
/// grapheme boundaries; a wide glyph that would straddle the cap is dropped
/// rather than split.
pub fn take_to_cells(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for grapheme in cell_width::graphemes(s) {
        let width = cell_width::text_width(grapheme);
        if col + width > max_cells {
            break;
        }
        out.push_str(grapheme);
        col += width;
    }
    out
}

/// Truncate `s` to at most `max_cells` display columns, appending
/// `suffix` when truncation actually happened. When `suffix` alone
/// overruns the budget we return as many of its leading graphemes as fit.
pub fn truncate_to_cells(s: &str, max_cells: usize, suffix: &str) -> String {
    if cell_width::text_width(s) <= max_cells {
        return s.to_string();
    }
    let suffix_w = cell_width::text_width(suffix);
    if suffix_w >= max_cells {
        return take_to_cells(suffix, max_cells);
    }

    let mut prefix = take_to_cells(s, max_cells - suffix_w);
    loop {
        let mut out = prefix.clone();
        out.push_str(suffix);
        if cell_width::text_width(&out) <= max_cells {
            return out;
        }
        let len = prefix.len();
        let end = text::prev_grapheme_boundary(&prefix, len);
        text::replace_range(&mut prefix, end..len, "");
    }
}

/// Truncate `s` to fit beside fixed right padding. `width` is the total row
/// budget; up to `pad` spaces are reserved at the right edge. Returns both the
/// full padded text and the unpadded body so callers can style only the content.
pub fn truncate_with_right_padding(
    s: &str,
    width: usize,
    pad: usize,
    suffix: &str,
) -> RightPaddedText {
    let pad_w = pad.min(width);
    let body = truncate_to_cells(s, width - pad_w, suffix);
    let text = format!("{body}{}", " ".repeat(pad_w));
    RightPaddedText { text, body }
}

/// Build a padding string of exactly `gap` display cells. Whole repeats of
/// `fill` are preferred; spaces close any remainder that cannot hold a complete
/// grapheme. `fill_w` is the cached width of `fill` and must be non-zero.
pub fn pad_to_cells(fill: &str, fill_w: usize, gap: usize) -> String {
    debug_assert!(fill_w > 0, "pad_to_cells: fill must have non-zero width");
    if gap == 0 || fill_w == 0 {
        return String::new();
    }

    let repeats = gap.div_ceil(fill_w);
    let repeated = fill.repeat(repeats);
    let mut pad = take_to_cells(&repeated, gap);
    let remaining = gap.saturating_sub(cell_width::text_width(&pad));
    pad.push_str(&" ".repeat(remaining));
    pad
}

/// Add padding before `body` until the joined string occupies exactly
/// `target_cells`. Width is measured after joining so a fill ending in a ZWJ or
/// a body starting with a combining mark cannot invalidate the result.
pub fn pad_left_to_cells(body: &str, fill: &str, target_cells: usize) -> String {
    pad_joined_to_cells(body, fill, target_cells, true)
}

/// Add padding after `body` until the joined string occupies exactly
/// `target_cells`. Width is measured after joining so a fill starting with a
/// variation selector or combining mark cannot invalidate the result.
pub fn pad_right_to_cells(body: &str, fill: &str, target_cells: usize) -> String {
    pad_joined_to_cells(body, fill, target_cells, false)
}

fn pad_joined_to_cells(body: &str, fill: &str, target_cells: usize, left: bool) -> String {
    let body_width = cell_width::text_width(body);
    if body_width >= target_cells {
        return body.to_string();
    }

    let fill_width = cell_width::text_width(fill);
    debug_assert!(fill_width > 0, "padding fill must have non-zero width");
    if fill_width == 0 {
        let spaces = " ".repeat(target_cells - body_width);
        return if left {
            format!("{spaces}{body}")
        } else {
            format!("{body}{spaces}")
        };
    }

    let mut padding = pad_to_cells(fill, fill_width, target_cells - body_width);
    let joined_width = |padding: &str| {
        if left {
            cell_width::joined_text_width([padding, body])
        } else {
            cell_width::joined_text_width([body, padding])
        }
    };

    while !padding.is_empty() && joined_width(&padding) > target_cells {
        let len = padding.len();
        let end = text::prev_grapheme_boundary(&padding, len);
        text::replace_range(&mut padding, end..len, "");
    }
    let remaining = target_cells.saturating_sub(joined_width(&padding));
    let spaces = " ".repeat(remaining);
    if left {
        format!("{spaces}{padding}{body}")
    } else {
        format!("{body}{padding}{spaces}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_to_cells_caps_at_budget() {
        assert_eq!(take_to_cells("hello", 3), "hello"[..3]);
        assert_eq!(take_to_cells("", 5), "");
        assert_eq!(take_to_cells("abc", 0), "");
    }

    #[test]
    fn take_to_cells_skips_wide_glyph_that_would_straddle() {
        // "あ" is 2 cells wide; budget of 1 must drop it entirely.
        assert_eq!(take_to_cells("あ", 1), "");
        assert_eq!(take_to_cells("aあ", 2), "a");
        assert_eq!(take_to_cells("aあ", 3), "aあ");
    }

    #[test]
    fn take_to_cells_keeps_multi_scalar_graphemes_atomic() {
        assert_eq!(take_to_cells("e\u{301}x", 1), "e\u{301}");
        assert_eq!(take_to_cells("👩\u{200d}💻x", 1), "");
        assert_eq!(take_to_cells("👩\u{200d}💻x", 2), "👩\u{200d}💻");
        assert_eq!(take_to_cells("9\u{fe0f}x", 1), "");
        assert_eq!(take_to_cells("🇨🇦x", 2), "🇨🇦");
    }

    #[test]
    fn truncate_returns_input_when_fits() {
        assert_eq!(truncate_to_cells("hi", 10, "…"), "hi");
    }

    #[test]
    fn truncate_appends_suffix_when_clipped() {
        assert_eq!(truncate_to_cells("hello world", 8, "…"), "hello w…");
    }

    #[test]
    fn truncate_returns_suffix_prefix_when_suffix_overruns() {
        assert_eq!(truncate_to_cells("hello world", 2, "..."), "..");
    }

    #[test]
    fn truncate_measures_suffix_after_joining() {
        let out = truncate_to_cells("9abcdef", 1, "\u{fe0f}");
        assert!(cell_width::text_width(&out) <= 1, "{out:?}");
    }

    #[test]
    fn truncate_never_splits_suffix_graphemes() {
        assert_eq!(truncate_to_cells("abcdef", 1, "e\u{301}"), "e\u{301}");
        assert_eq!(truncate_to_cells("abcdef", 1, "👩\u{200d}💻"), "");
        assert_eq!(truncate_to_cells("abcdef", 2, "🇨🇦"), "🇨🇦");
    }

    #[test]
    fn truncate_with_right_padding_reserves_edge_space() {
        let out = truncate_with_right_padding("hello world", 10, 2, "…");
        assert_eq!(out.body, "hello w…");
        assert_eq!(out.text, "hello w…  ");
    }

    #[test]
    fn truncate_with_right_padding_is_exact_after_incomplete_zwj() {
        let out = truncate_with_right_padding("👩\u{200d}", 4, 2, "…");
        assert_eq!(cell_width::text_width(&out.body), 2);
        assert_eq!(cell_width::text_width(&out.text), 4);
    }

    #[test]
    fn truncate_with_right_padding_clamps_padding_to_width() {
        let out = truncate_with_right_padding("hello", 1, 2, "…");
        assert_eq!(out.body, "");
        assert_eq!(out.text, " ");
    }

    #[test]
    fn pad_to_cells_uses_whole_repeats_plus_remainder() {
        assert_eq!(pad_to_cells("ab", 2, 5), "abab".to_string() + "a");
        assert_eq!(pad_to_cells(" ", 1, 3), "   ");
        assert_eq!(pad_to_cells("中", 2, 3), "中 ");
        let joined_fill = pad_to_cells("👩\u{200d}", 2, 3);
        assert_eq!(cell_width::text_width(&joined_fill), 3, "{joined_fill:?}");
        assert_eq!(pad_to_cells("ab", 2, 0), "");
    }

    #[test]
    fn joined_padding_is_exact_across_grapheme_boundaries() {
        let right = pad_right_to_cells("9", "\u{fe0f} ", 4);
        assert_eq!(cell_width::text_width(&right), 4, "{right:?}");
        assert!(right.starts_with("9"));

        let left = pad_left_to_cells("💻", "x\u{200d}", 4);
        assert_eq!(cell_width::text_width(&left), 4, "{left:?}");
        assert!(left.ends_with("💻"));
    }

    #[test]
    fn joined_padding_is_exact_for_unicode_fill_and_body_combinations() {
        let bodies = [
            "x",
            "e\u{301}",
            "中",
            "9\u{fe0f}",
            "👩\u{200d}💻",
            "🇨🇦",
            "\u{301}",
            "\u{fe0f}",
            "👩\u{200d}",
            "🇨",
        ];
        let fills = [
            "x",
            "e\u{301}",
            "中",
            "9\u{fe0f}",
            "👩\u{200d}💻",
            "🇨🇦",
            "👩\u{200d}",
            "🇨",
        ];
        for body in bodies {
            for fill in fills {
                for target in 0..=8 {
                    let expected = cell_width::text_width(body).max(target);
                    let left = pad_left_to_cells(body, fill, target);
                    let right = pad_right_to_cells(body, fill, target);
                    assert_eq!(
                        cell_width::text_width(&left),
                        expected,
                        "left body={body:?} fill={fill:?} target={target} out={left:?}"
                    );
                    assert_eq!(
                        cell_width::text_width(&right),
                        expected,
                        "right body={body:?} fill={fill:?} target={target} out={right:?}"
                    );
                }
            }
        }
    }
}
