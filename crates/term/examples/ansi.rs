//! ANSI pager: scroll piped, colored output. Without piped input it shows a sample.
//! Run with `cargo test 2>&1 | cargo run -p smelt-term --example ansi`.
//! Keys: `j` / `k` or arrows scroll, `q` or `Esc` quits.

use std::io::{IsTerminal, Read};

use crossterm::event::{Event, KeyCode};
use smelt_term::ansi::parse_ansi_lines;
use smelt_term::{LayoutTree, PaintId, Surface, TerminalSession};

const SAMPLE: &str = "\x1b[1;32mPASS\x1b[0m  parser::basic\n\
                      \x1b[1;31mFAIL\x1b[0m  parser::unicode \x1b[2m(0.02s)\x1b[0m\n\
                      \x1b[33mWARN\x1b[0m  3 tests ignored";

fn main() -> std::io::Result<()> {
    let mut input = String::new();
    if std::io::stdin().is_terminal() {
        input.push_str(SAMPLE);
    } else {
        std::io::stdin().read_to_string(&mut input)?;
    }
    let lines = parse_ansi_lines(&input);

    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));
    let mut top = 0;
    loop {
        terminal.draw(&mut surface, |_, slice| {
            let rows = slice.height() as usize;
            // Stop when the last line reaches the bottom row.
            top = top.min(lines.len().saturating_sub(rows));
            for (row, line) in lines.iter().skip(top).take(rows).enumerate() {
                slice.put_line(0, row as u16, line);
            }
        })?;
        for event in terminal.events(None)? {
            let Event::Key(key) = event else { continue };
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => top += 1,
                KeyCode::Char('k') | KeyCode::Up => top = top.saturating_sub(1),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}
