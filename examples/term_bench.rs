//! Raw terminal write-speed benchmark.
//!
//! Measures how long the terminal takes to accept and render a block of
//! output under several scenarios so we can compare smelt's redraw cost
//! against the theoretical floor.
//!
//! Run with: `cargo run --release --bin term_bench`
//!
//! All timings are measured from "first byte written" to "flush() returns,"
//! which is when the kernel has delivered all bytes to the TTY. The
//! terminal's own rendering happens asynchronously and is not measured here
//! — but on modern terminals the kernel delivery is the dominant cost at
//! the user-space level.

use std::io::{self, BufWriter, Write};
use std::time::Instant;

const LINES: usize = 2000;
const WIDTH: usize = 120;

fn main() {
    eprintln!("term_bench: writing {LINES} lines at ~{WIDTH} cols each\n");

    run("1. plain ascii, single BufWriter flush", || {
        write_plain(false)
    });
    run("2. plain ascii, inside sync update", || write_plain(true));
    run("3. SGR-colored, single BufWriter flush", || {
        write_styled(false, false)
    });
    run("4. SGR-colored, inside sync update", || {
        write_styled(true, false)
    });
    run("5. SGR + bg fill to EOL, sync update", || {
        write_styled(true, true)
    });
    run(
        "6. plain ascii, 8 MiB buffer, sync update",
        write_plain_big_buffer,
    );
    run(
        "7. plain ascii, pre-assembled Vec, one write_all",
        write_plain_vec,
    );
    run(
        "8. high-density SGR (1 escape per 10 chars)",
        write_dense_sgr,
    );
    run(
        "9. smelt-like: diff rows w/ bg fill + syntax spans",
        write_smelt_like,
    );
    run(
        "10. smelt-like but 256-palette fg instead of 24-bit",
        write_smelt_like_256,
    );

    eprintln!("\ndone. if the slowest run is < 10ms, the terminal accepts bytes");
    eprintln!("quickly and smelt's flush time is dominated by the sync-update");
    eprintln!("processing / visual rendering step inside the terminal itself.");
}

fn run(label: &str, f: impl FnOnce()) {
    let start = Instant::now();
    f();
    let elapsed = start.elapsed();
    eprintln!("  {:>9.2?}  {}", elapsed, label);
}

/// Write 2000 lines of plain ascii using a 1 MiB BufWriter.
fn write_plain(sync: bool) {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    if sync {
        out.write_all(b"\x1b[?2026h").unwrap();
    }
    let line: String = "x".repeat(WIDTH);
    for _ in 0..LINES {
        out.write_all(line.as_bytes()).unwrap();
        out.write_all(b"\r\n").unwrap();
    }
    if sync {
        out.write_all(b"\x1b[?2026l").unwrap();
    }
    out.flush().unwrap();
}

/// Write 2000 lines with per-char SGR styling (mimics worst-case smelt).
fn write_styled(sync: bool, bg_fill: bool) {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    if sync {
        out.write_all(b"\x1b[?2026h").unwrap();
    }
    // One SGR color change per line, plus reset at end of line.
    let colors: [&[u8]; 6] = [
        b"\x1b[31m",
        b"\x1b[32m",
        b"\x1b[33m",
        b"\x1b[34m",
        b"\x1b[35m",
        b"\x1b[36m",
    ];
    let text = "x".repeat(WIDTH.saturating_sub(10));
    for i in 0..LINES {
        out.write_all(colors[i % colors.len()]).unwrap();
        out.write_all(b"some header ").unwrap();
        out.write_all(b"\x1b[0m").unwrap();
        out.write_all(text.as_bytes()).unwrap();
        if bg_fill {
            // Fill to column WIDTH with a background color.
            out.write_all(b"\x1b[41m").unwrap();
            out.write_all(b"          ").unwrap();
            out.write_all(b"\x1b[0m").unwrap();
        }
        out.write_all(b"\r\n").unwrap();
    }
    if sync {
        out.write_all(b"\x1b[?2026l").unwrap();
    }
    out.flush().unwrap();
}

/// Same as plain but with an 8 MiB BufWriter to verify buffer size isn't
/// the bottleneck.
fn write_plain_big_buffer() {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(8 << 20, stdout.lock());
    out.write_all(b"\x1b[?2026h").unwrap();
    let line: String = "x".repeat(WIDTH);
    for _ in 0..LINES {
        out.write_all(line.as_bytes()).unwrap();
        out.write_all(b"\r\n").unwrap();
    }
    out.write_all(b"\x1b[?2026l").unwrap();
    out.flush().unwrap();
}

/// Heavy SGR churn: emit a new color every ~10 characters, resetting
/// between spans, across 2000 rows. Tests terminal-side SGR parsing cost
/// when content is extremely colorful.
fn write_dense_sgr() {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    out.write_all(b"\x1b[?2026h").unwrap();
    // 24-bit RGB escapes like syntect emits — worst case for parse cost.
    let colors: [&[u8]; 8] = [
        b"\x1b[38;2;200;60;60m",
        b"\x1b[38;2;60;200;60m",
        b"\x1b[38;2;60;60;200m",
        b"\x1b[38;2;200;200;60m",
        b"\x1b[38;2;200;60;200m",
        b"\x1b[38;2;60;200;200m",
        b"\x1b[38;2;180;180;180m",
        b"\x1b[38;2;120;120;120m",
    ];
    let chunk = b"abcdefghij"; // 10 chars per color
    let chunks_per_line = WIDTH / 10; // ~12 color spans per row
    for i in 0..LINES {
        for c in 0..chunks_per_line {
            out.write_all(colors[(i + c) % colors.len()]).unwrap();
            out.write_all(chunk).unwrap();
        }
        out.write_all(b"\x1b[0m").unwrap();
        out.write_all(b"\r\n").unwrap();
    }
    out.write_all(b"\x1b[?2026l").unwrap();
    out.flush().unwrap();
}

/// Closest mimic of smelt's diff paint: each row is prefix + syntax-
/// highlighted spans + bg fill to EOL. ~12 color spans per row + a bg
/// flood at the tail. This is the worst pattern smelt emits during a
/// big redraw.
fn write_smelt_like() {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    out.write_all(b"\x1b[?2026h").unwrap();
    let colors: [&[u8]; 6] = [
        b"\x1b[38;2;190;80;80m",
        b"\x1b[38;2;80;190;80m",
        b"\x1b[38;2;80;80;190m",
        b"\x1b[38;2;190;190;80m",
        b"\x1b[38;2;80;190;190m",
        b"\x1b[38;2;190;80;190m",
    ];
    let syntax_token = b"token "; // 6 chars per span
                                  // 20 syntax spans ≈ 120 visible chars.
    for i in 0..LINES {
        // Set diff row bg (dark green or dark red).
        if i % 2 == 0 {
            out.write_all(b"\x1b[48;2;20;50;20m").unwrap();
        } else {
            out.write_all(b"\x1b[48;2;60;20;20m").unwrap();
        }
        // 3-char prefix (line number + space).
        out.write_all(b"+12 ").unwrap();
        for c in 0..20 {
            out.write_all(colors[(i + c) % colors.len()]).unwrap();
            out.write_all(syntax_token).unwrap();
        }
        // Bg-fill padding to column 120: emit 4 spaces.
        out.write_all(b"    ").unwrap();
        out.write_all(b"\x1b[0m").unwrap();
        out.write_all(b"\r\n").unwrap();
    }
    out.write_all(b"\x1b[?2026l").unwrap();
    out.flush().unwrap();
}

/// Same pattern as `write_smelt_like` but with `\x1b[38;5;Nm` 256-palette
/// escapes instead of `\x1b[38;2;R;G;Bm` 24-bit. Used to measure the raw
/// parse-cost savings of the narrower form.
fn write_smelt_like_256() {
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    out.write_all(b"\x1b[?2026h").unwrap();
    // Six 256-palette indices that roughly match the cube in write_smelt_like.
    let colors: [&[u8]; 6] = [
        b"\x1b[38;5;167m",
        b"\x1b[38;5;114m",
        b"\x1b[38;5;67m",
        b"\x1b[38;5;179m",
        b"\x1b[38;5;73m",
        b"\x1b[38;5;175m",
    ];
    let syntax_token = b"token ";
    for i in 0..LINES {
        if i % 2 == 0 {
            out.write_all(b"\x1b[48;5;22m").unwrap(); // dark green bg
        } else {
            out.write_all(b"\x1b[48;5;52m").unwrap(); // dark red bg
        }
        out.write_all(b"+12 ").unwrap();
        for c in 0..20 {
            out.write_all(colors[(i + c) % colors.len()]).unwrap();
            out.write_all(syntax_token).unwrap();
        }
        out.write_all(b"    ").unwrap();
        out.write_all(b"\x1b[0m").unwrap();
        out.write_all(b"\r\n").unwrap();
    }
    out.write_all(b"\x1b[?2026l").unwrap();
    out.flush().unwrap();
}

/// Pre-assemble all bytes into a single Vec, then one write_all call.
/// Tests whether many small writes through BufWriter are worse than one
/// large write.
fn write_plain_vec() {
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    buf.extend_from_slice(b"\x1b[?2026h");
    let line: String = "x".repeat(WIDTH);
    for _ in 0..LINES {
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"\x1b[?2026l");
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(&buf).unwrap();
    handle.flush().unwrap();
}
