#![no_main]
#![allow(clippy::doc_lazy_continuation)]

//! `SnapshotFrame` text+styles round-trip invariance fuzz.
//!
//! The frame snapshot format claims `parse(text(f), styles_text(f)) == f`
//! for any `f` produced by `from_grid`. That claim is load-bearing: the
//! storybook viewer round-trips frames through the text/styles views and
//! relies on byte-equality to replay; the `insta`-style diff comparisons
//! assume the text view is the canonical projection.
//!
//! This target hammers the round-trip with random grids:
//!  - random `(width, height)` within small bounds
//!  - each cell gets a char from a small alphabet (single-width unicode
//!    + ASCII) and a `Style` from a curated palette covering every
//!    fg/bg/attr combination the parser exercises
//!  - construct a `Grid`, run `from_grid` → `text + styles_text` →
//!    `parse`, assert the parsed `SnapshotFrame` matches the original
//!
//! Wide-char placement is left to the existing snapshot unit tests —
//! the focus here is the `parse_spans` + `parse_row` path with random
//! styled runs that the unit tests cover only on a handful of cases.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smelt_style::style::{Color, Style};
use smelt_term::{Grid, SnapshotFrame};

#[derive(Arbitrary, Debug)]
struct Input {
    width: u8,
    height: u8,
    cells: Vec<Cell>,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
struct Cell {
    ch_idx: u8,
    style_idx: u8,
}

/// Single-width chars only — the round-trip story for wide chars is
/// covered by `snapshot::tests` in `crates/term`. Including ASCII
/// printable + a few common single-width unicode keeps the parser
/// branches exercised without spilling into wide-char territory.
const CHARS: &[char] = &[
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1',
    '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '@', 'A', 'B', 'a', 'b',
    '~', 'á', 'é', 'ñ', '€', '¶',
];

/// Palette covers: default; bare fg/bg; basic named colors; ANSI; RGB;
/// every attr individually and combined; fg+bg+attrs mix. The parser
/// dispatches on each token independently so this exercises every
/// `match` arm under `parse_color` / `parse_attrs`.
fn style(idx: u8) -> Style {
    match idx % 16 {
        0 => Style::default(),
        1 => Style::new().fg(Color::Red),
        2 => Style::new().bg(Color::Blue),
        3 => Style::new().fg(Color::AnsiValue(42)),
        4 => Style::new().fg(Color::Rgb { r: 12, g: 34, b: 56 }),
        5 => Style::new().fg(Color::Reset),
        6 => Style {
            bold: true,
            ..Style::default()
        },
        7 => Style {
            dim: true,
            ..Style::default()
        },
        8 => Style {
            italic: true,
            ..Style::default()
        },
        9 => Style {
            underline: true,
            ..Style::default()
        },
        10 => Style {
            crossedout: true,
            ..Style::default()
        },
        11 => Style {
            fg: Some(Color::Green),
            bg: Some(Color::DarkBlue),
            bold: true,
            ..Style::default()
        },
        12 => Style {
            fg: Some(Color::Rgb { r: 200, g: 0, b: 200 }),
            italic: true,
            underline: true,
            ..Style::default()
        },
        13 => Style {
            bold: true,
            dim: true,
            italic: true,
            underline: true,
            crossedout: true,
            ..Style::default()
        },
        14 => Style::new().fg(Color::White).bg(Color::Black),
        _ => Style::new().fg(Color::Magenta),
    }
}

fn ch(idx: u8) -> char {
    CHARS[(idx as usize) % CHARS.len()]
}

fn run(input: Input) {
    // Clamp to a small grid so iterations are fast and we don't OOM
    // libFuzzer on degenerate inputs.
    let w = ((input.width as u16) % 16) + 1;
    let h = ((input.height as u16) % 16) + 1;
    if input.cells.is_empty() {
        return;
    }

    let mut grid = Grid::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = (y as usize * w as usize + x as usize) % input.cells.len();
            let cell = input.cells[i];
            grid.set(x, y, ch(cell.ch_idx), style(cell.style_idx));
        }
    }

    let original = SnapshotFrame::from_grid(&grid);
    let text = original.text();
    let styles_text = original.styles_text();
    let parsed = SnapshotFrame::parse(&text, &styles_text);

    if original.width != parsed.width {
        panic!(
            "SNAPSHOT: width drifted: original={} parsed={}\nstyles_text:\n{}",
            original.width, parsed.width, styles_text
        );
    }
    if original.height != parsed.height {
        panic!(
            "SNAPSHOT: height drifted: original={} parsed={}",
            original.height, parsed.height
        );
    }
    if original.rows != parsed.rows {
        let mut diff = String::new();
        for (i, (a, b)) in original
            .rows
            .iter()
            .zip(parsed.rows.iter())
            .enumerate()
        {
            if a != b {
                diff.push_str(&format!(
                    "  row[{i}]: original={a:?}\n           parsed  ={b:?}\n"
                ));
            }
        }
        panic!(
            "SNAPSHOT: rows drifted:\n{diff}original.rows.len()={} parsed.rows.len()={}\ntext:\n{text}\nstyles_text:\n{styles_text}",
            original.rows.len(),
            parsed.rows.len()
        );
    }
    if original.styles != parsed.styles {
        // Find the first cell that differs and report it.
        for (y, (a_row, b_row)) in original
            .styles
            .iter()
            .zip(parsed.styles.iter())
            .enumerate()
        {
            for (x, (a, b)) in a_row.iter().zip(b_row.iter()).enumerate() {
                if a != b {
                    panic!(
                        "SNAPSHOT: styles[{y}][{x}] drifted:\n  original: {a:?}\n  parsed  : {b:?}\nstyles_text:\n{styles_text}",
                    );
                }
            }
            if a_row.len() != b_row.len() {
                panic!(
                    "SNAPSHOT: styles row {y} length drifted: original={} parsed={}",
                    a_row.len(),
                    b_row.len()
                );
            }
        }
    }
}

fuzz_target!(|input: Input| {
    run(input);
});
