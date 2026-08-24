#![no_main]

//! Random mutation sequences over `smelt_term::Grid` that pin two
//! correctness contracts:
//!
//! 1. **Wide-grapheme continuation invariant** after every mutation:
//!    - A continuation cell exists only immediately right of a width-2
//!      grapheme.
//!    - Every width-2 grapheme within the grid has a continuation cell.
//!
//! 2. **Diff-replay parity** at frame boundaries: after computing
//!    `curr.diff(&prev)` and feeding complete-symbol updates back through
//!    `Grid::set_symbol` on top of `prev`, the result must be cell-equal to
//!    `curr`. This catches coordinate and stale-cell regressions in the same
//!    updates consumed by `flush_diff`.
//!
//! The palette mixes narrow, wide, decomposed, variation-selector, ZWJ, and
//! regional-indicator graphemes so width-changing overwrites happen often.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use smelt_style::style::{Color, Style};
use smelt_term::geometry::Rect;
use smelt_term::grid::Grid;

/// Graphemes used by string and complete-symbol writes.
const GRAPHEMES: &[&str] = &[
    " ",
    ".",
    "a",
    "#",
    "a\u{308}",
    "漢",
    "🌟",
    "👩\u{200d}💻",
    "9\u{fe0f}",
    "🇨🇦",
];

/// Scalars used by APIs whose contract intentionally accepts one `char`.
const SCALARS: &[char] = &[' ', '.', 'a', '#', '漢', '🌟', '\u{308}', '\u{fe0f}'];

fn pick_char(u: &mut Unstructured<'_>) -> char {
    let idx = u.int_in_range(0..=(SCALARS.len() as u32 - 1)).unwrap_or(0) as usize;
    SCALARS[idx]
}

fn pick_grapheme_index(u: &mut Unstructured<'_>) -> arbitrary::Result<u8> {
    u.int_in_range(0..=(GRAPHEMES.len() as u8 - 1))
}

fn pick_style(u: &mut Unstructured<'_>) -> Style {
    let mut s = Style::default();
    // A handful of common colors - palette covers the SGR code paths that
    // flush_diff distinguishes (named, ANSI palette, default).
    let palette: [Option<Color>; 5] = [
        None,
        Some(Color::Red),
        Some(Color::Blue),
        Some(Color::AnsiValue(208)),
        Some(Color::Reset),
    ];
    let fg_idx = u.int_in_range(0..=(palette.len() as u32 - 1)).unwrap_or(0) as usize;
    let bg_idx = u.int_in_range(0..=(palette.len() as u32 - 1)).unwrap_or(0) as usize;
    s.fg = palette[fg_idx];
    s.bg = palette[bg_idx];
    s.bold = u.arbitrary().unwrap_or(false);
    s
}

#[derive(Debug)]
enum Op {
    SetSymbol {
        x: u16,
        y: u16,
        symbol: u8,
        style: Style,
    },
    PutStr {
        x: u16,
        y: u16,
        chars: Vec<u8>,
        style: Style,
    },
    PutChar {
        x: u16,
        y: u16,
        ch: char,
        fg: Color,
    },
    PutStrFg {
        x: u16,
        y: u16,
        chars: Vec<u8>,
        fg: Color,
    },
    Fill {
        rect: Rect,
        ch: char,
        style: Style,
    },
    Clear {
        rect: Rect,
    },
    ClearAll,
    /// Force a frame boundary: swap `curr` into `prev` and run the diff-replay
    /// check. Implemented in the harness, not as a Grid mutation.
    Frame,
}

impl<'a> Arbitrary<'a> for Op {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let tag = u.int_in_range(0..=7u8)?;
        Ok(match tag {
            0 => Op::SetSymbol {
                x: u.int_in_range(0..=15)? as u16,
                y: u.int_in_range(0..=5)? as u16,
                symbol: pick_grapheme_index(u)?,
                style: pick_style(u),
            },
            1 => Op::PutStr {
                x: u.int_in_range(0..=15)? as u16,
                y: u.int_in_range(0..=5)? as u16,
                chars: bounded_palette_indices(u)?,
                style: pick_style(u),
            },
            2 => Op::PutChar {
                x: u.int_in_range(0..=15)? as u16,
                y: u.int_in_range(0..=5)? as u16,
                ch: pick_char(u),
                fg: pick_style(u).fg.unwrap_or(Color::Reset),
            },
            3 => Op::PutStrFg {
                x: u.int_in_range(0..=15)? as u16,
                y: u.int_in_range(0..=5)? as u16,
                chars: bounded_palette_indices(u)?,
                fg: pick_style(u).fg.unwrap_or(Color::Reset),
            },
            4 => Op::Fill {
                rect: random_rect(u)?,
                ch: pick_char(u),
                style: pick_style(u),
            },
            5 => Op::Clear {
                rect: random_rect(u)?,
            },
            6 => Op::ClearAll,
            _ => Op::Frame,
        })
    }
}

fn random_rect(u: &mut Unstructured<'_>) -> arbitrary::Result<Rect> {
    let top = u.int_in_range(0..=5)? as u16;
    let left = u.int_in_range(0..=15)? as u16;
    let width = u.int_in_range(0..=10)? as u16;
    let height = u.int_in_range(0..=4)? as u16;
    Ok(Rect::new(top, left, width, height))
}

fn bounded_palette_indices(u: &mut Unstructured<'_>) -> arbitrary::Result<Vec<u8>> {
    let len = u.int_in_range(0..=8u8)?;
    let mut out = Vec::with_capacity(len as usize);
    for _ in 0..len {
        out.push(pick_grapheme_index(u)?);
    }
    Ok(out)
}

fn grapheme(index: u8) -> &'static str {
    GRAPHEMES[(index as usize) % GRAPHEMES.len()]
}

fn indices_to_string(indices: &[u8]) -> String {
    indices.iter().map(|&index| grapheme(index)).collect()
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    width: u8,
    height: u8,
    ops: Vec<Op>,
}

/// Walk every cell and assert the wide-char invariant. Panics inside the
/// fuzzer surface as a crashing input - exactly what we want.
fn assert_invariants(grid: &Grid, label: &str) {
    for y in 0..grid.height() {
        for x in 0..grid.width() {
            let cell = grid.cell(x, y);
            if cell.symbol.is_continuation() {
                assert!(x > 0, "[{label}] continuation at col 0 has no leading cell");
                let lead = &grid.cell(x - 1, y).symbol;
                let width = lead.width();
                assert_eq!(
                    width, 2,
                    "[{label}] orphaned continuation at ({x},{y}); leading symbol {lead:?} has width {width}"
                );
            }
            let width = cell.symbol.width();
            if width == 2 {
                assert!(
                    x + 1 < grid.width(),
                    "[{label}] wide grapheme at ({x},{y}) cannot fit a continuation cell"
                );
                let cont = &grid.cell(x + 1, y).symbol;
                assert!(
                    cont.is_continuation(),
                    "[{label}] wide grapheme at ({x},{y}) missing continuation; got {cont:?}"
                );
            }
        }
    }
}

/// Apply `curr.diff(&prev)` updates on top of a clone of `prev` using complete
/// grapheme symbols and assert the result is cell-equal to `curr`.
///
/// This is the *property* the diff-flush pipeline must uphold: a terminal
/// that started in `prev`'s state and received the diff stream ends up in
/// `curr`'s state.
fn assert_diff_replay_parity(prev: &Grid, curr: &Grid) {
    let mut replay = prev.clone();
    for update in curr.diff(prev) {
        replay.set_symbol(
            update.x,
            update.y,
            update.cell.symbol.as_str(),
            update.cell.style,
        );
    }
    for y in 0..curr.height() {
        for x in 0..curr.width() {
            let want = curr.cell(x, y);
            let got = replay.cell(x, y);
            assert_eq!(
                want, got,
                "diff replay mismatch at ({x},{y}): prev→curr replay produced {got:?}, expected {want:?}"
            );
        }
    }
}

fuzz_target!(|input: FuzzInput| {
    let width = (input.width as u16 % 20).max(1);
    let height = (input.height as u16 % 8).max(1);
    let mut curr = Grid::new(width, height);
    let mut prev = Grid::new(width, height);

    // Cap ops so a degenerate fuzz seed can't spin forever.
    for op in input.ops.into_iter().take(128) {
        match op {
            Op::SetSymbol {
                x,
                y,
                symbol,
                style,
            } => curr.set_symbol(x, y, grapheme(symbol), style),
            Op::PutStr { x, y, chars, style } => {
                curr.put_str(x, y, &indices_to_string(&chars), style);
            }
            Op::PutChar { x, y, ch, fg } => curr.put_char(x, y, ch, fg),
            Op::PutStrFg { x, y, chars, fg } => {
                curr.put_str_fg(x, y, &indices_to_string(&chars), fg);
            }
            Op::Fill { rect, ch, style } => curr.fill(rect, ch, style),
            Op::Clear { rect } => curr.clear(rect),
            Op::ClearAll => curr.clear_all(),
            Op::Frame => {
                assert_invariants(&curr, "frame-curr");
                assert_invariants(&prev, "frame-prev");
                assert_diff_replay_parity(&prev, &curr);
                std::mem::swap(&mut prev, &mut curr);
                curr.clear_all();
            }
        }
        assert_invariants(&curr, "after-op");
    }

    // Final frame check so trailing ops always get a parity assertion.
    assert_invariants(&curr, "final-curr");
    assert_invariants(&prev, "final-prev");
    assert_diff_replay_parity(&prev, &curr);
});
