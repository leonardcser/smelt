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
}
