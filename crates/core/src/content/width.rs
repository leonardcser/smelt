//! Display-cell width helpers. One home for "truncate / pad a string so
//! it fits in N terminal columns" so toast clipping, the `smelt.text.fit`
//! Lua API, and any future fixed-width slot stay byte-for-byte
//! consistent.

use smelt_buffer::cell_width;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RightPaddedText {
    pub text: String,
    pub body: String,
}

/// Greatest prefix of `s` whose display width is `<= max_cells`. Snaps to
/// char boundaries; a wide glyph that would straddle the cap is dropped
/// rather than split.
pub fn take_to_cells(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        let w = cell_width::char_width(ch);
        if col + w > max_cells {
            break;
        }
        out.push(ch);
        col += w;
    }
    out
}

/// Truncate `s` to at most `max_cells` display columns, appending
/// `suffix` when truncation actually happened. When `suffix` alone
/// overruns the budget we return as many of its leading chars as fit.
pub fn truncate_to_cells(s: &str, max_cells: usize, suffix: &str) -> String {
    if cell_width::text_width(s) <= max_cells {
        return s.to_string();
    }
    let suffix_w = cell_width::text_width(suffix);
    if suffix_w >= max_cells {
        return take_to_cells(suffix, max_cells);
    }
    let mut out = take_to_cells(s, max_cells - suffix_w);
    out.push_str(suffix);
    out
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

/// Build a padding string of exactly `gap` display cells using whole
/// repeats of `fill` plus a leading-char slice for any remainder.
/// `fill_w` is the cached width of `fill`; caller must ensure it's
/// non-zero (a zero-width fill could never close the gap).
pub fn pad_to_cells(fill: &str, fill_w: usize, gap: usize) -> String {
    debug_assert!(fill_w > 0, "pad_to_cells: fill must have non-zero width");
    if gap == 0 || fill_w == 0 {
        return String::new();
    }
    let whole = gap / fill_w;
    let remainder = gap - whole * fill_w;
    let mut pad = fill.repeat(whole);
    if remainder > 0 {
        pad.push_str(&take_to_cells(fill, remainder));
    }
    pad
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
    fn truncate_with_right_padding_reserves_edge_space() {
        let out = truncate_with_right_padding("hello world", 10, 2, "…");
        assert_eq!(out.body, "hello w…");
        assert_eq!(out.text, "hello w…  ");
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
        assert_eq!(pad_to_cells("ab", 2, 0), "");
    }
}
