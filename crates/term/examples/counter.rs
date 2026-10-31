//! Event loop: redraw after every batch of events; the compositor diffs for you.
//! Run with `cargo run -p smelt-term --example counter`.
//! Keys: `+` / `-` change the count, `q` or `Esc` quits.

use crossterm::event::{Event, KeyCode};
use smelt_term::{LayoutTree, PaintId, Style, Surface, TerminalSession};

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));
    let mut count: i32 = 0;
    loop {
        terminal.draw(&mut surface, |_, slice| {
            slice.put_str(2, 1, &format!("count: {count}"), Style::new().bold());
            slice.put_str(2, 3, "+ / - to change, q to quit", Style::new().dim());
        })?;
        for event in terminal.events(None)? {
            let Event::Key(key) = event else { continue };
            match key.code {
                KeyCode::Char('+') => count += 1,
                KeyCode::Char('-') => count -= 1,
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}
