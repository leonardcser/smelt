//! Clickable buttons with `HitRegistry`: paint records each button's rect, and
//! a click asks the registry which button is under the pointer.
//! Run with `cargo run -p smelt-term --example mouse`.
//! Click a button to bump its counter. `q` or `Esc` quits.

use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};
use smelt_term::{Color, HitRegistry, LayoutTree, PaintId, Rect, Style, Surface, TerminalSession};

const BUTTONS: [(&str, Color); 3] = [
    ("red", Color::Red),
    ("green", Color::Green),
    ("blue", Color::Blue),
];

fn main() -> std::io::Result<()> {
    let mut terminal = TerminalSession::enter_stdout()?;
    let mut surface = Surface::default();
    surface.set_layout(LayoutTree::leaf(PaintId(1)));
    let mut hits = HitRegistry::new();
    let mut clicks = [0u32; 3];
    loop {
        hits.clear();
        terminal.draw(&mut surface, |_, slice| {
            for (i, (label, color)) in BUTTONS.iter().enumerate() {
                let button = Rect::new(2 + i as u16 * 14, 1, 12, 1);
                let text = format!(" {label}: {} ", clicks[i]);
                let style = Style::new().fg(Color::Black).bg(*color);
                slice.put_padded(button.x, button.y, button.width, &text, style);
                hits.record_local(slice.grid_rect(), button, i);
            }
            slice.put_str(2, 3, "click a button, q to quit", Style::new().dim());
        })?;
        for event in terminal.events(None)? {
            match event {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(&i) = hits.hit(mouse.column, mouse.row) {
                        clicks[i] += 1;
                    }
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}
