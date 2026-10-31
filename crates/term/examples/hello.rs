//! Hello world: one leaf with styled text.
//! Run with `cargo run -p smelt-term --example hello`.
//! Any key quits.

use crossterm::event::Event;
use smelt_term::{Color, LayoutTree, PaintId, Style, Surface, TerminalSession};

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));
    loop {
        terminal.draw(&mut surface, |_, slice| {
            let title = Style::new().fg(Color::Cyan).bold();
            slice.put_str(2, 1, "hello, smelt-term", title);
            slice.put_str(2, 3, "press any key to quit", Style::new().dim());
        })?;
        if terminal
            .events(None)?
            .iter()
            .any(|e| matches!(e, Event::Key(_)))
        {
            return Ok(());
        }
    }
}
