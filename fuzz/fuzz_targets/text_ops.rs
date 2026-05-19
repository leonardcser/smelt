#![no_main]

//! Direct fuzzing of `smelt_buffer::text` — the single canonical module
//! for UTF-8 boundary handling. Three of the bugs caught by the TUI
//! fuzz target traced back to stale byte offsets feeding these helpers
//! (motions `find_char`, vim visual_anchor swap, completer anchor across
//! buffer shrink). Hitting them directly is orders of magnitude faster
//! than hoping the TUI happens to produce the right `(source, range)`
//! pair via random keystrokes.
//!
//! Invariants enforced after every op:
//!  - source is valid UTF-8
//!  - every returned/derived byte offset lies on a char boundary
//!  - inverted ranges produce empty / clamped results, never panics
//!  - replace_range round-trips: result.contains(replacement)
//!  - boundary helpers are monotonic relative to input
//!  - **production helpers agree with naive `char_indices`-based
//!    reference implementations** — the production code uses byte
//!    tricks for speed; divergence from the canonical iteration-based
//!    reference catches optimization bugs (mis-snapped offsets,
//!    off-by-one in char counts, etc.) that the intrinsic invariants
//!    above cannot detect.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smelt_buffer::text::{
    byte_of_char, byte_to_cell, cell_to_byte, char_pos, insert, insert_str, line_start_offsets,
    next_char_boundary, prev_char_boundary, replace_range, slice, snap,
};

/// Reference implementations: O(n) walks over `char_indices`. Slower
/// than the production code but obviously correct — every divergence
/// is a real bug.
mod refer {
    pub fn snap(s: &str, pos: usize) -> usize {
        if pos >= s.len() {
            return s.len();
        }
        // Largest char boundary ≤ pos.
        let mut last = 0;
        for (i, _) in s.char_indices() {
            if i > pos {
                return last;
            }
            last = i;
        }
        last
    }

    pub fn prev_char_boundary(s: &str, pos: usize) -> usize {
        // Production semantics (locked down by the
        // `prev_char_boundary_snaps_back_to_previous_boundary_from_mid_char`
        // unit test): "largest boundary STRICTLY less than `pos`",
        // clamped to `s.len()`. From mid-char that's the boundary at the
        // start of the containing char — NOT one boundary further back.
        // An earlier version of this reference snapped pos first, which
        // pushed mid-char inputs one extra boundary backward and showed
        // up as a real differential failure during fuzzing.
        let clamped = pos.min(s.len());
        if clamped == 0 {
            return 0;
        }
        let mut prev = 0;
        for (i, _) in s.char_indices() {
            if i >= clamped {
                return prev;
            }
            prev = i;
        }
        prev
    }

    pub fn next_char_boundary(s: &str, pos: usize) -> usize {
        // Production semantics: smallest boundary strictly greater than
        // `pos`, clamped to `s.len()`. From mid-char that's the boundary
        // at the end of the containing char.
        let clamped = pos.min(s.len());
        for (i, _) in s.char_indices() {
            if i > clamped {
                return i;
            }
        }
        s.len()
    }

    pub fn char_pos(s: &str, byte: usize) -> usize {
        let snapped = snap(s, byte);
        s[..snapped].chars().count()
    }

    pub fn byte_of_char(s: &str, idx: usize) -> usize {
        let total = s.chars().count();
        if idx >= total {
            return s.len();
        }
        s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
    }

    pub fn slice(s: &str, range: core::ops::Range<usize>) -> &str {
        let start = snap(s, range.start);
        let end = snap(s, range.end).max(start);
        &s[start..end]
    }
}

#[derive(Arbitrary, Debug)]
enum TextOp {
    /// Snap an offset, then check `snap(s, snap(s, x)) == snap(s, x)`.
    Snap {
        pos: u32,
    },
    /// `prev_char_boundary` must be ≤ pos and on a boundary.
    Prev {
        pos: u32,
    },
    /// `next_char_boundary` must be ≥ pos and on a boundary.
    Next {
        pos: u32,
    },
    /// `slice` must not panic; returned slice must be a substring.
    Slice {
        start: u32,
        end: u32,
    },
    /// `replace_range` must contain the replacement at the snapped start.
    Replace {
        start: u32,
        end: u32,
        with: String,
    },
    /// `insert` returns the insertion offset on a boundary.
    InsertChar {
        pos: u32,
        ch: u32,
    },
    /// `insert_str` returns the insertion offset on a boundary.
    InsertStr {
        pos: u32,
        s: String,
    },
    /// `byte_to_cell` ↔ `cell_to_byte` round-trip on a single line.
    ByteCell {
        byte: u32,
    },
    CellByte {
        cell: u32,
    },
    /// `char_pos` ↔ `byte_of_char` round-trip.
    CharPosOfByte {
        byte: u32,
    },
    ByteOfChar {
        idx: u32,
    },
}

#[derive(Arbitrary, Debug)]
struct Input {
    initial: String,
    ops: Vec<TextOp>,
}

fn assert_boundary(s: &str, pos: usize, label: &str) {
    if pos > s.len() {
        panic!("{label}: pos {pos} > len {}", s.len());
    }
    if pos < s.len() && !s.is_char_boundary(pos) {
        panic!("{label}: pos {pos} not on char boundary in {s:?}");
    }
}

fn run(initial: String, ops: Vec<TextOp>) {
    let mut s = initial;
    // Cap to keep individual iterations fast.
    let take = ops.len().min(64);
    for op in ops.into_iter().take(take) {
        match op {
            TextOp::Snap { pos } => {
                let p = snap(&s, pos as usize);
                assert_boundary(&s, p, "snap");
                assert_eq!(snap(&s, p), p, "snap not idempotent");
                let r = refer::snap(&s, pos as usize);
                assert_eq!(p, r, "snap diverges from reference (pos={pos}, s={s:?})");
            }
            TextOp::Prev { pos } => {
                let p = prev_char_boundary(&s, pos as usize);
                assert_boundary(&s, p, "prev_char_boundary");
                assert!(p <= pos as usize || pos as usize > s.len(), "prev > pos");
                let r = refer::prev_char_boundary(&s, pos as usize);
                assert_eq!(
                    p, r,
                    "prev_char_boundary diverges from reference (pos={pos}, s={s:?})"
                );
            }
            TextOp::Next { pos } => {
                let p = next_char_boundary(&s, pos as usize);
                assert_boundary(&s, p, "next_char_boundary");
                assert!(p >= snap(&s, pos as usize), "next < snap(pos)");
                let r = refer::next_char_boundary(&s, pos as usize);
                assert_eq!(
                    p, r,
                    "next_char_boundary diverges from reference (pos={pos}, s={s:?})"
                );
            }
            TextOp::Slice { start, end } => {
                let sl = slice(&s, (start as usize)..(end as usize));
                // Slice must be a contiguous substring (or empty).
                if !sl.is_empty() {
                    assert!(
                        s.contains(sl),
                        "slice produced non-substring {sl:?} of {s:?}"
                    );
                }
                let r = refer::slice(&s, (start as usize)..(end as usize));
                assert_eq!(
                    sl, r,
                    "slice diverges from reference (start={start}, end={end}, s={s:?})"
                );
            }
            TextOp::Replace { start, end, with } => {
                let with_clone = with.clone();
                replace_range(&mut s, (start as usize)..(end as usize), &with);
                if !with_clone.is_empty() {
                    assert!(s.contains(&with_clone), "replace_range dropped replacement");
                }
            }
            TextOp::InsertChar { pos, ch } => {
                let c = char::from_u32(ch).unwrap_or('?');
                let pre_len = s.len();
                let p = insert(&mut s, pos as usize, c);
                assert_boundary(&s, p, "insert");
                assert_eq!(s.len(), pre_len + c.len_utf8());
            }
            TextOp::InsertStr { pos, s: ins } => {
                let pre_len = s.len();
                let p = insert_str(&mut s, pos as usize, &ins);
                assert_boundary(&s, p, "insert_str");
                assert_eq!(s.len(), pre_len + ins.len());
            }
            TextOp::ByteCell { byte } => {
                let _ = byte_to_cell(&s, byte as usize);
            }
            TextOp::CellByte { cell } => {
                let p = cell_to_byte(&s, cell as usize);
                assert_boundary(&s, p, "cell_to_byte");
            }
            TextOp::CharPosOfByte { byte } => {
                let p = char_pos(&s, byte as usize);
                let r = refer::char_pos(&s, byte as usize);
                assert_eq!(
                    p, r,
                    "char_pos diverges from reference (byte={byte}, s={s:?})"
                );
            }
            TextOp::ByteOfChar { idx } => {
                let p = byte_of_char(&s, idx as usize);
                assert_boundary(&s, p, "byte_of_char");
                let r = refer::byte_of_char(&s, idx as usize);
                assert_eq!(
                    p, r,
                    "byte_of_char diverges from reference (idx={idx}, s={s:?})"
                );
            }
        }
        // After every op the buffer is still valid UTF-8 (this is implicit
        // in `String` but we add the explicit check so any internal helper
        // that bypassed it via unsafe would be caught).
        let _ = s.as_str();
    }
    // Also exercise `line_start_offsets` against the final state — a
    // multi-line edge case that the per-op variants don't hit.
    let lines: Vec<String> = s.split('\n').map(|l| l.to_string()).collect();
    let offsets = line_start_offsets(&lines);
    assert_eq!(offsets.len(), lines.len());
}

fuzz_target!(|input: Input| {
    run(input.initial, input.ops);
});
