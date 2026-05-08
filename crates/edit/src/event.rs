//! Terminal event type and dispatch outcome.

/// Terminal event mirroring `crossterm::event::Event`. Hosts convert at the App boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize(u16, u16),
    FocusGained,
    FocusLost,
    Paste(String),
}

impl From<crossterm::event::Event> for Event {
    fn from(ev: crossterm::event::Event) -> Self {
        use crossterm::event::Event as Ce;
        match ev {
            Ce::Key(k) => Event::Key(k),
            Ce::Mouse(m) => Event::Mouse(m),
            Ce::Resize(w, h) => Event::Resize(w, h),
            Ce::FocusGained => Event::FocusGained,
            Ce::FocusLost => Event::FocusLost,
            Ce::Paste(s) => Event::Paste(s),
        }
    }
}

/// Dispatch outcome for `Ui::dispatch_event` and `Window::handle_mouse`.
///
/// `Capture` is returned on `Down(Left)` landing on content; the host folds it
/// into `Ui::set_capture` so subsequent `Drag`/`Up` reach the same target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Consumed,
    Capture,
    Ignored,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        Event as Ce, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    #[test]
    fn from_crossterm_round_trips_each_variant() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(Event::from(Ce::Key(key)), Event::Key(key));

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(Event::from(Ce::Mouse(mouse)), Event::Mouse(mouse));

        assert_eq!(Event::from(Ce::Resize(40, 20)), Event::Resize(40, 20));
        assert_eq!(Event::from(Ce::FocusGained), Event::FocusGained);
        assert_eq!(Event::from(Ce::FocusLost), Event::FocusLost);
        assert_eq!(
            Event::from(Ce::Paste("hi".into())),
            Event::Paste("hi".into())
        );
    }
}
