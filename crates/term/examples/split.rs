//! Resizable panes: drag the divider with the mouse.
//! Run with `cargo run -p smelt-term --example split`.
//! `q` or `Esc` quits.

use crossterm::event::{Event, KeyCode};
use smelt_term::{
    Axis, Color, DividerStyles, LayoutStyle, LayoutTree, PaintId, Split, SplitOptions, Style,
    Surface, TerminalSession,
};

const LEFT: PaintId = PaintId(1);
const RIGHT: PaintId = PaintId(2);

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::split(
        Split::new(Axis::Horizontal, SplitOptions::default()),
        LayoutTree::leaf(LEFT),
        LayoutTree::leaf(RIGHT),
    ));
    surface.set_layout_style(LayoutStyle {
        dividers: DividerStyles {
            normal: Style::new().fg(Color::DarkGrey),
            active: Style::new().fg(Color::Cyan),
        },
        ..LayoutStyle::default()
    });
    loop {
        terminal.draw(&mut surface, |id, slice| {
            let text = if id == LEFT { "left" } else { "right" };
            slice.put_str(1, 1, text, Style::new().bold());
            slice.put_str(1, 3, "drag the divider, q to quit", Style::new().dim());
        })?;
        for event in terminal.events(None)? {
            match event {
                Event::Mouse(mouse) => {
                    surface.handle_mouse(mouse);
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}
