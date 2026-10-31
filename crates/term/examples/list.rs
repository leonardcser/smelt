//! Scrollable, selectable list.
//! Run with `cargo run -p smelt-term --example list`.
//! Keys: `j` / `k` or arrows move, `q` or `Esc` quits.

use crossterm::event::{Event, KeyCode};
use smelt_term::{LayoutTree, PaintId, Style, Surface, TerminalSession};

fn main() -> std::io::Result<()> {
    let items: Vec<String> = (1..=100).map(|n| format!("item {n}")).collect();
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));
    let mut selected: usize = 0;
    let mut top = 0;
    loop {
        terminal.draw(&mut surface, |_, slice| {
            let rows = (slice.height() as usize).max(1);
            // Scroll only as far as needed to keep the selection visible.
            top = top.clamp((selected + 1).saturating_sub(rows), selected);
            for (row, item) in items.iter().enumerate().skip(top).take(rows) {
                let style = if row == selected {
                    Style::new().reverse()
                } else {
                    Style::new()
                };
                slice.put_padded(0, (row - top) as u16, slice.width(), item, style);
            }
        })?;
        for event in terminal.events(None)? {
            let Event::Key(key) = event else { continue };
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    selected = (selected + 1).min(items.len() - 1)
                }
                KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}
