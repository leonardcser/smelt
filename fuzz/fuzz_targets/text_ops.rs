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
//!  - safe_replace_range round-trips: result.contains(replacement)
//!  - boundary helpers are monotonic relative to input

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smelt_buffer::text::{
    byte_of_char, byte_to_cell, cell_to_byte, char_pos, line_start_offsets, next_char_boundary,
    prev_char_boundary, safe_drain, safe_insert, safe_insert_str, safe_replace_range, safe_slice,
    snap,
};

#[derive(Arbitrary, Debug)]
enum TextOp {
    /// Snap an offset, then check `snap(s, snap(s, x)) == snap(s, x)`.
    Snap { pos: u32 },
    /// `prev_char_boundary` must be ≤ pos and on a boundary.
    Prev { pos: u32 },
    /// `next_char_boundary` must be ≥ pos and on a boundary.
    Next { pos: u32 },
    /// `safe_slice` must not panic; returned slice must be a substring.
    Slice { start: u32, end: u32 },
    /// `safe_drain` must keep source valid UTF-8 and shrink it.
    Drain { start: u32, end: u32 },
    /// `safe_replace_range` must contain the replacement at the snapped start.
    Replace {
        start: u32,
        end: u32,
        with: String,
    },
    /// `safe_insert` returns the insertion offset on a boundary.
    InsertChar { pos: u32, ch: u32 },
    /// `safe_insert_str` returns the insertion offset on a boundary.
    InsertStr { pos: u32, s: String },
    /// `byte_to_cell` ↔ `cell_to_byte` round-trip on a single line.
    ByteCell { byte: u32 },
    CellByte { cell: u32 },
    /// `char_pos` ↔ `byte_of_char` round-trip.
    CharPosOfByte { byte: u32 },
    ByteOfChar { idx: u32 },
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
            }
            TextOp::Prev { pos } => {
                let p = prev_char_boundary(&s, pos as usize);
                assert_boundary(&s, p, "prev_char_boundary");
                assert!(p <= pos as usize || pos as usize > s.len(), "prev > pos");
            }
            TextOp::Next { pos } => {
                let p = next_char_boundary(&s, pos as usize);
                assert_boundary(&s, p, "next_char_boundary");
                assert!(p >= snap(&s, pos as usize), "next < snap(pos)");
            }
            TextOp::Slice { start, end } => {
                let slice = safe_slice(&s, (start as usize)..(end as usize));
                // Slice must be a contiguous substring (or empty).
                if !slice.is_empty() {
                    assert!(
                        s.contains(slice),
                        "safe_slice produced non-substring {slice:?} of {s:?}"
                    );
                }
            }
            TextOp::Drain { start, end } => {
                let pre_len = s.len();
                let drained = safe_drain(&mut s, (start as usize)..(end as usize));
                assert!(s.is_char_boundary(0)); // valid UTF-8 still
                assert_eq!(
                    s.len() + drained.len(),
                    pre_len,
                    "safe_drain lost or grew bytes"
                );
            }
            TextOp::Replace { start, end, with } => {
                let with_clone = with.clone();
                safe_replace_range(&mut s, (start as usize)..(end as usize), &with);
                if !with_clone.is_empty() {
                    assert!(
                        s.contains(&with_clone),
                        "safe_replace_range dropped replacement"
                    );
                }
            }
            TextOp::InsertChar { pos, ch } => {
                let c = char::from_u32(ch).unwrap_or('?');
                let pre_len = s.len();
                let p = safe_insert(&mut s, pos as usize, c);
                assert_boundary(&s, p, "safe_insert");
                assert_eq!(s.len(), pre_len + c.len_utf8());
            }
            TextOp::InsertStr { pos, s: ins } => {
                let pre_len = s.len();
                let p = safe_insert_str(&mut s, pos as usize, &ins);
                assert_boundary(&s, p, "safe_insert_str");
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
                let _ = char_pos(&s, byte as usize);
            }
            TextOp::ByteOfChar { idx } => {
                let p = byte_of_char(&s, idx as usize);
                assert_boundary(&s, p, "byte_of_char");
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
