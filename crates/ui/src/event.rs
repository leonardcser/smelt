//! Terminal event surface and dispatch outcome shared by `Ui` and
//! `Window`.
//!
//! The `Event` enum mirrors `crossterm::event::Event` but lives in
//! `ui` so the dispatch surface doesn't leak the backend type. Hosts
//! convert at the App boundary via `Event::from(crossterm_event)`.
//!
//! `Status` is the single dispatch outcome. It replaces the earlier
//! `DispatchOutcome` (key/mouse pre-flight at `Ui::dispatch_event`)
//! and `MouseAction` (`Window::handle_mouse` return). `Capture`
//! signals an in-flight gesture grab the host folds into
//! `Ui::set_capture`.

use crate::id::WinId;

/// Terminal event in `ui`'s vocabulary. Variants carry crossterm
/// payloads — the conversion is a thin re-wrap, so no information
/// is lost. Consuming code matches on `ui::Event` and pulls the
/// crossterm payload out of the variant when it needs the typed
/// fields.
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
/// - `Consumed` — handled end-to-end; the host has nothing further to
///   route. For mouse this is also the "fall-through to host's own
///   paint refresh" signal.
/// - `Capture` — gesture grab. Returned from `Window::handle_mouse`
///   on a `Down(Left)` that lands on content; the host folds it into
///   `Ui::set_capture` so subsequent `Drag` / `Up` reach this target
///   even when the pointer leaves the window's rect.
/// - `Ignored` — handler had nothing to say. The host may continue
///   routing through its own paths (App-level chords, prompt /
///   transcript mouse, paste side effects, terminal focus tracking).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Consumed,
    Capture,
    Ignored,
}

/// Semantic keyboard-focus target. Currently single-variant —
/// `FocusTarget::Window(WinId)` — but exists as its own type so
/// consumers don't have to choose between "focused window id" and
/// "hit-tested geometric target" using a bare `WinId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Window(WinId),
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

    #[test]
    fn focus_target_carries_win_id() {
        let target = FocusTarget::Window(WinId(7));
        assert_eq!(target, FocusTarget::Window(WinId(7)));
    }
}
