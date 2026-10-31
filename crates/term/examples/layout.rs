//! Boxes, constraints, borders, titles, and padding. Each leaf is painted by `PaintId`.
//! Run with `cargo run -p smelt-term --example layout`.
//! Any key quits.

use crossterm::event::Event;
use smelt_term::{Border, Color, Constraint, LayoutTree, PaintId, Style, Surface, TerminalSession};

const HEADER: PaintId = PaintId(1);
const SIDEBAR: PaintId = PaintId(2);
const MAIN: PaintId = PaintId(3);

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::vbox(vec![
        (Constraint::Length(1), LayoutTree::leaf(HEADER)),
        (
            Constraint::Fill,
            LayoutTree::hbox(vec![
                (
                    Constraint::Length(24),
                    LayoutTree::leaf(SIDEBAR)
                        .with_border(Border::SINGLE)
                        .with_title("Length(24)"),
                ),
                (
                    Constraint::Fill,
                    LayoutTree::leaf(MAIN)
                        .with_border(Border::ROUNDED)
                        .with_title("Fill")
                        .with_padding(1),
                ),
            ]),
        ),
    ]));
    loop {
        terminal.draw(&mut surface, |id, slice| match id {
            HEADER => {
                let style = Style::new().bg(Color::DarkGrey);
                slice.fill_row(0, style);
                slice.put_str(
                    1,
                    0,
                    "vbox: Length(1) + Fill. hbox: Length(24) + Fill.",
                    style.bold(),
                );
            }
            SIDEBAR => {
                for (row, name) in ["Cargo.toml", "README.md", "src/lib.rs"].iter().enumerate() {
                    slice.put_str(0, row as u16, name, Style::new());
                }
            }
            _ => {
                slice.put_str(0, 0, "padding insets this leaf.", Style::new());
                slice.put_str(0, 1, "Origin (0, 0) is its corner.", Style::new());
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
