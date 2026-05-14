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
pub fn safe_slice(s: &str, range: core::ops::Range<usize>) -> &str {
    let start = snap(s, range.start);
    let end = snap(s, range.end);
    if start >= end {
        return "";
    }
    &s[start..end]
}

/// Drain `s[range]` with the same snap/clamp behavior as [`safe_slice`].
/// Returns the removed substring; never panics.
pub fn safe_drain(s: &mut String, range: core::ops::Range<usize>) -> String {
    let start = snap(s, range.start);
    let end = snap(s, range.end);
    if start >= end {
        return String::new();
    }
    s.drain(start..end).collect()
}

/// Replace `s[range]` with `with`. Snaps endpoints to char boundaries and
/// clamps to `s.len()`. Inverted ranges insert `with` at the snapped start
/// (so a degenerate input still does the closest sane thing instead of
/// silently dropping the write).
pub fn safe_replace_range(s: &mut String, range: core::ops::Range<usize>, with: &str) {
    let start = snap(s, range.start);
    let end = snap(s, range.end).max(start);
    s.replace_range(start..end, with);
}

/// Insert `ch` at `pos`. Snaps and clamps; returns the snapped insertion point
/// so callers can advance cursors correctly.
pub fn safe_insert(s: &mut String, pos: usize, ch: char) -> usize {
    let p = snap(s, pos);
    s.insert(p, ch);
    p
}

/// Insert `ins` at `pos`. Snaps and clamps; returns the snapped insertion point
/// so callers can advance cursors correctly.
pub fn safe_insert_str(s: &mut String, pos: usize, ins: &str) -> usize {
    let p = snap(s, pos);
    s.insert_str(p, ins);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9 bytes, 3 chars — drives non-boundary offsets.
    const CJK: &str = "日本語";

    #[test]
    fn safe_slice_snaps_non_boundary_endpoints() {
        // Mid-char start (inside '日'), mid-char end (inside '語') → "日本".
        let out = safe_slice(CJK, 1..7);
        assert_eq!(out, "日本");
    }

    #[test]
    fn safe_slice_clamps_past_end() {
        assert_eq!(safe_slice(CJK, 0..9999), CJK);
        assert_eq!(safe_slice(CJK, 100..200), "");
    }

    #[test]
    fn safe_slice_inverted_range_returns_empty() {
        // `#[allow]` because the empty range is the point of the test.
        #[allow(clippy::reversed_empty_ranges)]
        let r = 4..2;
        assert_eq!(safe_slice("hello", r), "");
    }

    #[test]
    fn safe_drain_does_not_panic_on_stale_offsets() {
        let mut s = format!("a{CJK}b");
        let removed = safe_drain(&mut s, 2..5);
        assert_eq!(removed, "日");
        assert_eq!(s, "a本語b");
    }

    #[test]
    fn safe_replace_range_handles_inverted_input() {
        let mut s = String::from("abc");
        #[allow(clippy::reversed_empty_ranges)]
        let r = 2..1;
        // Degenerate ranges insert at the snapped start.
        safe_replace_range(&mut s, r, "X");
        assert_eq!(s, "abXc");
    }

    #[test]
    fn safe_replace_range_clamps_past_end_and_snaps() {
        let mut s = CJK.to_string();
        safe_replace_range(&mut s, 4..200, "_");
        assert_eq!(s, "日_");
    }

    #[test]
    fn safe_insert_snaps_non_boundary_pos() {
        let mut s = CJK.to_string();
        let actual = safe_insert(&mut s, 4, 'X');
        assert_eq!(actual, 3);
        assert_eq!(s, "日X本語");
    }

    #[test]
    fn safe_insert_str_snaps_and_clamps() {
        let mut s = CJK.to_string();
        let actual = safe_insert_str(&mut s, 9999, "_end");
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
