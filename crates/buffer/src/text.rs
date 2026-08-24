//! Pure text helpers for byte↔cell mapping.

use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

/// Byte offset → terminal column, snapped to the start of its grapheme.
pub fn byte_to_cell(line: &str, byte: usize) -> usize {
    let byte = snap_grapheme(line, byte);
    crate::cell_width::text_width(&line[..byte])
}

/// Terminal column → byte offset at which the preceding text occupies `cell` columns.
///
/// Exact cell boundaries map after the preceding grapheme. A cell inside a wide
/// grapheme maps to that grapheme's start, so the result can never split a
/// combining sequence, variation-selector glyph, flag, or ZWJ emoji.
pub fn cell_to_byte(line: &str, cell: usize) -> usize {
    if cell == 0 {
        return 0;
    }

    let mut boundary = 0usize;
    let mut width = 0usize;
    for (byte, grapheme) in crate::cell_width::grapheme_indices(line) {
        let end = byte + grapheme.len();
        width = width.saturating_add(crate::cell_width::text_width(grapheme));
        if width > cell {
            return boundary;
        }
        boundary = end;
    }
    line.len()
}

/// Slice a string by terminal display columns.
///
/// Endpoints are converted through [`cell_to_byte`] and then passed through
/// [`slice`], so ranges clamp to the string and always land on UTF-8 boundaries.
pub fn slice_cells(s: &str, start: usize, end: usize) -> &str {
    let start = cell_to_byte(s, start);
    let end = cell_to_byte(s, end);
    slice(s, start..end)
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

/// Byte offset of the char boundary before `pos`. Returns 0 at start.
pub fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos.min(s.len()).saturating_sub(1);
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

/// Snap a byte offset to the start of its containing grapheme cluster.
/// Exact grapheme boundaries and the end of the string are preserved.
pub fn snap_grapheme(s: &str, pos: usize) -> usize {
    let pos = snap(s, pos);
    if pos == s.len() {
        return pos;
    }
    crate::cell_width::grapheme_indices(s)
        .take_while(|(start, _)| *start <= pos)
        .map(|(start, _)| start)
        .last()
        .unwrap_or(0)
}

/// Snap a byte offset to the nearest following grapheme boundary. Exact
/// boundaries are preserved. This is useful after insertion, when the byte just
/// after the inserted text can become the middle of a grapheme by joining with
/// text that was already to its right.
pub fn ceil_grapheme(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    if pos == s.len() {
        return pos;
    }
    for (start, grapheme) in crate::cell_width::grapheme_indices(s) {
        if pos == start {
            return start;
        }
        let end = start + grapheme.len();
        if pos < end {
            return end;
        }
    }
    s.len()
}

/// Byte offset of the grapheme boundary before `pos`. Returns 0 at start.
pub fn prev_grapheme_boundary(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    if pos == 0 {
        return 0;
    }

    let mut previous = 0;
    for (start, _) in crate::cell_width::grapheme_indices(s) {
        if start >= pos {
            return previous;
        }
        previous = start;
    }
    previous
}

/// Byte offset of the grapheme boundary after `pos`. Returns `s.len()` at end.
pub fn next_grapheme_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let pos = snap(s, pos);
    crate::cell_width::grapheme_indices(s)
        .map(|(start, grapheme)| start + grapheme.len())
        .find(|end| *end > pos)
        .unwrap_or(s.len())
}

/// Bytes at the start of `next` that join the final grapheme in `previous`.
pub fn joining_grapheme_prefix_len(previous: &str, next: &str) -> usize {
    let Some((previous_start, previous_grapheme)) =
        crate::cell_width::grapheme_indices(previous).next_back()
    else {
        return 0;
    };
    if next.is_empty() {
        return 0;
    }

    // Include the previous grapheme in the cursor's chunk so rules such as
    // extended-pictographic + ZWJ can inspect both sides of the text boundary.
    let mut joined = String::with_capacity(previous_grapheme.len() + next.len());
    joined.push_str(previous_grapheme);
    joined.push_str(next);

    let boundary = previous.len();
    let mut cursor = GraphemeCursor::new(previous_start, boundary + next.len(), true);
    loop {
        match cursor.next_boundary(&joined, previous_start) {
            Ok(Some(end)) => return end.saturating_sub(boundary).min(next.len()),
            Ok(None) => return next.len(),
            Err(GraphemeIncomplete::PreContext(offset)) => {
                cursor.provide_context(&previous[..offset], 0);
            }
            Err(
                GraphemeIncomplete::NextChunk
                | GraphemeIncomplete::PrevChunk
                | GraphemeIncomplete::InvalidOffset,
            ) => return joining_grapheme_prefix_len_contiguous(previous, next),
        }
    }
}

fn joining_grapheme_prefix_len_contiguous(previous: &str, next: &str) -> usize {
    let mut joined = String::with_capacity(previous.len() + next.len());
    joined.push_str(previous);
    joined.push_str(next);
    let consumed = crate::cell_width::grapheme_indices(&joined)
        .find_map(|(start, grapheme)| {
            let end = start + grapheme.len();
            (start < previous.len() && end > previous.len()).then_some(end - previous.len())
        })
        .unwrap_or(0);
    consumed
}

/// Longest prefix whose complete grapheme clusters fit within `max_bytes`.
pub fn grapheme_prefix(s: &str, max_bytes: usize) -> &str {
    &s[..snap_grapheme(s, max_bytes)]
}

/// Longest suffix whose complete grapheme clusters fit within `max_bytes`.
pub fn grapheme_suffix(s: &str, max_bytes: usize) -> &str {
    let start = crate::cell_width::grapheme_indices(s)
        .rev()
        .take_while(|(start, _)| s.len() - start <= max_bytes)
        .map(|(start, _)| start)
        .last()
        .unwrap_or(s.len());
    &s[start..]
}

/// Trim leading whitespace without splitting a grapheme cluster.
///
/// A cluster is removed only when every scalar in it is whitespace. This
/// preserves unusual but valid clusters such as a space plus a combining mark.
pub fn trim_start_whitespace(s: &str) -> &str {
    let start = crate::cell_width::grapheme_indices(s)
        .take_while(|(_, grapheme)| grapheme.chars().all(char::is_whitespace))
        .map(|(start, grapheme)| start + grapheme.len())
        .last()
        .unwrap_or(0);
    &s[start..]
}

/// Trim trailing whitespace without splitting a grapheme cluster.
pub fn trim_end_whitespace(s: &str) -> &str {
    let end = crate::cell_width::grapheme_indices(s)
        .rev()
        .take_while(|(_, grapheme)| grapheme.chars().all(char::is_whitespace))
        .map(|(start, _)| start)
        .last()
        .unwrap_or(s.len());
    &s[..end]
}

/// Trim leading and trailing whitespace without splitting grapheme clusters.
pub fn trim_whitespace(s: &str) -> &str {
    trim_end_whitespace(trim_start_whitespace(s))
}

/// Byte offset → character index. Tolerates a `byte_idx` that lands mid-char
/// (it snaps backwards) and clamps past `s.len()`.
pub fn char_pos(s: &str, byte_idx: usize) -> usize {
    let snapped = snap(s, byte_idx);
    s[..snapped].chars().count()
}

/// Character index → byte offset.
pub fn byte_of_char(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

// ── Panic-free slicing/mutation helpers ─────────────────────────────────────
//
// Stored byte offsets (kill-ring source_range, vim visual_anchor, attachment
// offsets, undo snapshots) can survive source edits and land mid-char by the
// time they're consumed. Raw `&s[a..b]` / `s.drain(a..b)` panic on
// non-boundaries; these helpers snap and clamp.

/// Borrowed slice that never panics. Out-of-range / non-boundary endpoints
/// snap to the nearest valid positions; an inverted range yields `""`.
pub fn slice(s: &str, range: core::ops::Range<usize>) -> &str {
    let start = snap(s, range.start);
    let end = snap(s, range.end);
    if start >= end {
        return "";
    }
    &s[start..end]
}

/// Cursor range with both endpoints snapped backward to grapheme boundaries.
pub fn snapped_grapheme_range(s: &str, range: core::ops::Range<usize>) -> core::ops::Range<usize> {
    let start = snap_grapheme(s, range.start);
    let end = snap_grapheme(s, range.end);
    start..end.max(start)
}

/// Smallest grapheme-aligned range that contains every byte in `range`.
pub fn covering_grapheme_range(s: &str, range: core::ops::Range<usize>) -> core::ops::Range<usize> {
    let start = snap_grapheme(s, range.start);
    let end = ceil_grapheme(s, range.end);
    start..end.max(start)
}

/// Largest grapheme-aligned range fully contained in `range`.
pub fn contained_grapheme_range(
    s: &str,
    range: core::ops::Range<usize>,
) -> core::ops::Range<usize> {
    let start = ceil_grapheme(s, range.start);
    let end = snap_grapheme(s, range.end);
    start..end.max(start)
}

/// Borrow a cursor range after snapping both endpoints backward.
pub fn slice_snapped_graphemes(s: &str, range: core::ops::Range<usize>) -> &str {
    slice(s, snapped_grapheme_range(s, range))
}

/// Replace `s[range]` with `with`. Snaps endpoints to char boundaries and
/// clamps to `s.len()`. Inverted ranges insert `with` at the snapped start
/// (so a degenerate input still does the closest sane thing instead of
/// silently dropping the write).
pub fn replace_range(s: &mut String, range: core::ops::Range<usize>, with: &str) {
    let start = snap(s, range.start);
    let end = snap(s, range.end).max(start);
    s.replace_range(start..end, with);
}

/// Insert `ch` at `pos`. Snaps and clamps; returns the snapped insertion point
/// so callers can advance cursors correctly.
pub fn insert(s: &mut String, pos: usize, ch: char) -> usize {
    let p = snap(s, pos);
    s.insert(p, ch);
    p
}

/// Insert `ins` at `pos`. Snaps and clamps; returns the snapped insertion point
/// so callers can advance cursors correctly.
pub fn insert_str(s: &mut String, pos: usize, ins: &str) -> usize {
    let p = snap(s, pos);
    s.insert_str(p, ins);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9 bytes, 3 chars - drives non-boundary offsets.
    const CJK: &str = "日本語";

    #[test]
    fn slice_snaps_non_boundary_endpoints() {
        // Mid-char start (inside '日'), mid-char end (inside '語') → "日本".
        let out = slice(CJK, 1..7);
        assert_eq!(out, "日本");
    }

    #[test]
    fn slice_clamps_past_end() {
        assert_eq!(slice(CJK, 0..9999), CJK);
        assert_eq!(slice(CJK, 100..200), "");
    }

    #[test]
    fn slice_inverted_range_returns_empty() {
        // `#[allow]` because the empty range is the point of the test.
        #[allow(clippy::reversed_empty_ranges)]
        let r = 4..2;
        assert_eq!(slice("hello", r), "");
    }

    #[test]
    fn grapheme_range_policies_never_return_partial_clusters() {
        let text = "ae\u{301}🇨🇦x";
        assert_eq!(
            slice_snapped_graphemes(text, 2..text.len() - 1),
            "e\u{301}🇨🇦"
        );
        assert_eq!(slice_snapped_graphemes(text, 3..text.len() - 2), "e\u{301}");
        assert_eq!(
            slice(text, covering_grapheme_range(text, 3..text.len() - 2)),
            "e\u{301}🇨🇦"
        );
        assert_eq!(
            slice(text, contained_grapheme_range(text, 2..text.len() - 1)),
            "🇨🇦"
        );
        assert_eq!(
            snapped_grapheme_range(text, text.len()..1),
            text.len()..text.len()
        );
        assert_eq!(
            covering_grapheme_range(text, text.len()..1),
            text.len()..text.len()
        );
    }

    #[test]
    fn replace_range_with_empty_drains_snapped_range() {
        let mut s = format!("a{CJK}b");
        replace_range(&mut s, 2..5, "");
        assert_eq!(s, "a本語b");
    }

    #[test]
    fn replace_range_handles_inverted_input() {
        let mut s = String::from("abc");
        #[allow(clippy::reversed_empty_ranges)]
        let r = 2..1;
        // Degenerate ranges insert at the snapped start.
        replace_range(&mut s, r, "X");
        assert_eq!(s, "abXc");
    }

    #[test]
    fn replace_range_clamps_past_end_and_snaps() {
        let mut s = CJK.to_string();
        replace_range(&mut s, 4..200, "_");
        assert_eq!(s, "日_");
    }

    #[test]
    fn insert_snaps_non_boundary_pos() {
        let mut s = CJK.to_string();
        let actual = insert(&mut s, 4, 'X');
        assert_eq!(actual, 3);
        assert_eq!(s, "日X本語");
    }

    #[test]
    fn insert_str_snaps_and_clamps() {
        let mut s = CJK.to_string();
        let actual = insert_str(&mut s, 9999, "_end");
        assert_eq!(actual, s.len() - "_end".len());
        assert_eq!(s, "日本語_end");
    }

    #[test]
    fn snap_idempotent() {
        for i in 0..=CJK.len() {
            let once = snap(CJK, i);
            assert_eq!(snap(CJK, once), once);
            assert!(CJK.is_char_boundary(once));
        }
    }

    // ── prev_char_boundary / next_char_boundary ───────────────────────────
    // The "move cursor by one char" primitives. Must be safe for any byte
    // offset (including mid-char and past-end) without panicking.

    #[test]
    fn prev_char_boundary_steps_back_by_one_char_at_a_boundary() {
        // "a日b" has boundaries at 0, 1, 4, 5.
        let s = "a日b";
        assert_eq!(prev_char_boundary(s, 5), 4);
        assert_eq!(prev_char_boundary(s, 4), 1);
        assert_eq!(prev_char_boundary(s, 1), 0);
        assert_eq!(prev_char_boundary(s, 0), 0, "0 is the floor");
    }

    #[test]
    fn prev_char_boundary_snaps_back_to_previous_boundary_from_mid_char() {
        // Byte 2 and 3 are inside '日'; the boundary before is 1.
        let s = "a日b";
        assert_eq!(prev_char_boundary(s, 2), 1);
        assert_eq!(prev_char_boundary(s, 3), 1);
    }

    #[test]
    fn prev_char_boundary_clamps_past_end() {
        let s = "ab";
        assert_eq!(prev_char_boundary(s, 1000), 1);
    }

    #[test]
    fn next_char_boundary_steps_forward_by_one_char() {
        let s = "a日b"; // boundaries 0, 1, 4, 5
        assert_eq!(next_char_boundary(s, 0), 1);
        assert_eq!(next_char_boundary(s, 1), 4);
        assert_eq!(next_char_boundary(s, 4), 5);
        assert_eq!(next_char_boundary(s, 5), 5, "len is the ceiling");
    }

    #[test]
    fn next_char_boundary_walks_to_next_boundary_from_mid_char() {
        // Bytes 2, 3 are inside '日'; the next boundary is 4.
        let s = "a日b";
        assert_eq!(next_char_boundary(s, 2), 4);
        assert_eq!(next_char_boundary(s, 3), 4);
    }

    #[test]
    fn next_char_boundary_clamps_past_end() {
        let s = "ab";
        assert_eq!(next_char_boundary(s, 1000), 2);
    }

    #[test]
    fn grapheme_boundaries_keep_terminal_glyphs_atomic() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a{grapheme}b");
            let start = 1;
            let end = start + grapheme.len();

            assert_eq!(next_grapheme_boundary(&text, start), end, "{grapheme:?}");
            assert_eq!(prev_grapheme_boundary(&text, end), start, "{grapheme:?}");
            assert_eq!(ceil_grapheme(&text, start), start, "{grapheme:?}");
            assert_eq!(ceil_grapheme(&text, end), end, "{grapheme:?}");
            for byte in start..end {
                assert_eq!(snap_grapheme(&text, byte), start, "{grapheme:?} at {byte}");
                if byte > start {
                    assert_eq!(ceil_grapheme(&text, byte), end, "{grapheme:?} at {byte}");
                }
            }
        }
    }

    #[test]
    fn joining_prefix_follows_resegmentation_across_text_boundaries() {
        for (previous, next, expected) in [
            ("e", "\u{301}x", "\u{301}".len()),
            ("9", "\u{fe0f}x", "\u{fe0f}".len()),
            ("👩", "\u{200d}💻x", "\u{200d}💻".len()),
            ("🇨", "🇦x", "🇦".len()),
            ("\u{600}", " x", " ".len()),
            ("a", "bc", 0),
        ] {
            assert_eq!(
                joining_grapheme_prefix_len(previous, next),
                expected,
                "previous={previous:?} next={next:?}"
            );
        }
    }

    #[test]
    fn joining_prefix_matches_contiguous_segmentation_at_every_split() {
        for text in [
            "e\u{301}x",
            "9\u{fe0f}x",
            "👩\u{200d}💻x",
            "🇦🇧🇨🇩🇪x",
            "\u{600}\u{600} x",
            "\u{915}\u{94d}\u{937}x",
            "👨\u{200d}👩\u{200d}👧\u{200d}👦x",
        ] {
            for split in text
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(core::iter::once(text.len()))
            {
                let expected = crate::cell_width::grapheme_indices(text)
                    .find_map(|(start, grapheme)| {
                        let end = start + grapheme.len();
                        (start < split && split < end).then(|| end - split)
                    })
                    .unwrap_or(0);
                assert_eq!(
                    joining_grapheme_prefix_len(&text[..split], &text[split..]),
                    expected,
                    "text={text:?} split={split}"
                );
            }
        }
    }

    #[test]
    fn grapheme_boundaries_tolerate_stale_offsets() {
        let text = "e\u{301}x";
        assert_eq!(prev_grapheme_boundary(text, usize::MAX), "e\u{301}".len());
        assert_eq!(next_grapheme_boundary(text, usize::MAX), text.len());
        assert_eq!(snap_grapheme(text, usize::MAX), text.len());
    }

    #[test]
    fn byte_budgets_keep_grapheme_clusters_atomic() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a{grapheme}b");
            let budget_inside = 1 + grapheme.len() - 1;

            assert_eq!(grapheme_prefix(&text, budget_inside), "a", "{grapheme:?}");
            assert_eq!(grapheme_suffix(&text, budget_inside), "b", "{grapheme:?}");
            assert_eq!(
                grapheme_prefix(&text, 1 + grapheme.len()),
                format!("a{grapheme}")
            );
            assert_eq!(
                grapheme_suffix(&text, grapheme.len() + 1),
                format!("{grapheme}b")
            );
        }
    }

    #[test]
    fn whitespace_trimming_keeps_graphemes_atomic() {
        assert_eq!(trim_start_whitespace(" \ttext"), "text");
        assert_eq!(trim_end_whitespace("text \r\n"), "text");
        assert_eq!(trim_whitespace(" \ttext \r\n"), "text");

        let leading = " \u{301}text";
        let trailing = "text\u{600} ";
        assert_eq!(trim_start_whitespace(leading), leading);
        assert_eq!(trim_end_whitespace(trailing), trailing);
        assert_eq!(
            trim_whitespace(" \u{301}text\u{600} "),
            " \u{301}text\u{600} "
        );
    }

    // ── byte_to_cell / cell_to_byte ───────────────────────────────────────
    // Conversions between byte offsets and terminal display columns. Wide
    // chars (CJK, most emoji) occupy 2 cells.

    #[test]
    fn byte_to_cell_counts_terminal_columns_of_preceding_text() {
        assert_eq!(byte_to_cell("abc", 0), 0);
        assert_eq!(byte_to_cell("abc", 2), 2);
        assert_eq!(
            byte_to_cell("abc", 3),
            3,
            "end-of-line is at column == width"
        );
    }

    #[test]
    fn byte_to_cell_treats_wide_chars_as_two_columns() {
        assert_eq!(byte_to_cell("日本", 3), 2, "after 日 we are at column 2");
        assert_eq!(byte_to_cell("日本", 6), 4, "after 日本 we are at column 4");
        assert_eq!(byte_to_cell("a日b", 4), 3, "a(1) + 日(2)");
    }

    #[test]
    fn byte_to_cell_snaps_mid_char_byte_backward() {
        // Byte 2 is inside '日' (starts at byte 0, 3 bytes long). The
        // column should equal what's covered up to '日's start: 0.
        assert_eq!(byte_to_cell("日本", 2), 0);
    }

    #[test]
    fn cell_to_byte_returns_byte_at_cell_or_clamps_to_end() {
        assert_eq!(cell_to_byte("abc", 0), 0);
        assert_eq!(cell_to_byte("abc", 2), 2);
        assert_eq!(cell_to_byte("abc", 100), 3, "past end clamps to len");
    }

    #[test]
    fn cell_to_byte_lands_at_the_start_of_a_wide_char() {
        // "a日": columns 0(a) and 1..=2(日). Cell 1 lands at byte 1 (start of 日).
        assert_eq!(cell_to_byte("a日", 1), 1);
    }

    #[test]
    fn cell_to_byte_snaps_cells_inside_wide_graphemes_to_their_start() {
        let s = "中x";
        assert_eq!(cell_to_byte(s, 0), 0);
        assert_eq!(cell_to_byte(s, 1), 0);
        assert_eq!(cell_to_byte(s, 2), "中".len());
        assert_eq!(cell_to_byte(s, 3), s.len());
        assert_eq!(cell_to_byte(s, 99), s.len());
    }

    #[test]
    fn cell_and_byte_coordinates_never_split_graphemes() {
        for grapheme in ["9\u{fe0f}", "👩\u{200d}💻", "🇨🇦"] {
            assert_eq!(crate::cell_width::text_width(grapheme), 2);
            assert_eq!(cell_to_byte(grapheme, 1), 0, "{grapheme:?}");
            assert_eq!(cell_to_byte(grapheme, 2), grapheme.len(), "{grapheme:?}");
        }
        for (text, split) in [
            ("e\u{301}x", "e".len()),
            ("9\u{fe0f}x", "9".len()),
            ("👩\u{200d}💻x", "👩".len()),
            ("🇨🇦x", "🇨".len()),
        ] {
            assert_eq!(byte_to_cell(text, split), 0, "{text:?}");
        }
        assert_eq!(slice_cells("⌚\u{fe0e}X", 0, 1), "⌚\u{fe0e}");
    }

    #[test]
    fn cell_to_byte_uses_string_prefix_widths() {
        for (s, pos) in [
            ("9\u{fe0f}?", "9\u{fe0f}".len()),
            ("e\u{301}x", "e\u{301}".len()),
            ("👩\u{200d}💻x", "👩\u{200d}💻".len()),
        ] {
            let cell = byte_to_cell(s, pos);
            assert_eq!(cell_to_byte(s, cell), pos, "{s:?}");
        }
    }

    #[test]
    fn slice_cells_uses_display_columns_and_safe_boundaries() {
        assert_eq!(slice_cells("a界界x", 1, 5), "界界");
        assert_eq!(slice_cells("a界界x", 2, 4), "界");
        assert_eq!(slice_cells("abc", 2, 99), "c");
        assert_eq!(slice_cells("abc", 3, 2), "");
    }

    // ── char_pos / byte_of_char ───────────────────────────────────────────

    #[test]
    fn char_pos_counts_chars_before_a_byte_offset() {
        assert_eq!(char_pos("abc", 0), 0);
        assert_eq!(char_pos("abc", 2), 2);
        assert_eq!(char_pos("日本", 3), 1, "3 is the start of 本");
        assert_eq!(char_pos("日本", 6), 2);
    }

    #[test]
    fn char_pos_snaps_mid_char_input_backward() {
        // Byte 1 is inside '日'; count should be the chars before '日' = 0.
        assert_eq!(char_pos("日本", 1), 0);
    }

    #[test]
    fn char_pos_clamps_past_end() {
        assert_eq!(char_pos("abc", 100), 3);
    }

    #[test]
    fn byte_of_char_returns_byte_offset_of_nth_char() {
        assert_eq!(byte_of_char("abc", 0), 0);
        assert_eq!(byte_of_char("abc", 2), 2);
        assert_eq!(byte_of_char("日本", 1), 3, "本 starts at byte 3");
    }

    #[test]
    fn byte_of_char_past_end_returns_len() {
        assert_eq!(byte_of_char("abc", 100), 3);
        assert_eq!(byte_of_char("", 5), 0);
    }
}
