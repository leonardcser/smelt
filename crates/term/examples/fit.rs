//! `Constraint::Fit` sizes a pane to its content: the sidebar is as wide as its
//! longest entry plus its border, and `Fill` takes the rest.
//! Run with `cargo run -p smelt-term --example fit`.
//! Resize the terminal to watch it. Any key quits.

use crossterm::event::Event;
use smelt_term::{
    display_width, Border, Constraint, LayoutTree, PaintId, Style, Surface, TerminalSession,
};

const SIDEBAR: PaintId = PaintId(1);
const MAIN: PaintId = PaintId(2);

fn main() -> std::io::Result<()> {
    let entries = ["inbox", "sent", "drafts and templates"];
    let widest = entries.iter().map(|e| display_width(e)).max().unwrap_or(0);

    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::hbox(vec![
        (
            Constraint::Fit,
            LayoutTree::leaf(SIDEBAR)
                .with_natural_size(widest, entries.len() as u16)
                .with_border(Border::SINGLE)
                .with_title("Fit"),
        ),
        (
            Constraint::Fill,
            LayoutTree::leaf(MAIN)
                .with_border(Border::SINGLE)
                .with_title("Fill"),
        ),
    ]));
    loop {
        terminal.draw(&mut surface, |id, slice| {
            if id == SIDEBAR {
                for (row, entry) in entries.iter().enumerate() {
                    slice.put_str(0, row as u16, entry, Style::new());
                }
            } else {
                slice.put_str(0, 0, "takes the remaining width", Style::new());
            }
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
