//! Per-dialog state owned by `App`. Each file implements one builtin
//! float (resume, permissions, ps, rewind, export, help) as a struct
//! that carries its domain state and implements `DialogState`.
//!
//! Dispatch: `events.rs` keeps `App::float_states: HashMap<WinId,
//! Box<dyn DialogState>>`. On every intercepted key / select / dismiss
//! we take the state out of the map, call its method with `&mut App`,
//! then put it back if the dialog is still open.

pub mod agents;
pub mod export;
pub mod help;
pub mod permissions;
pub mod ps;
pub mod resume;
pub mod rewind;

use super::{App, TurnState};
use crossterm::event::{KeyCode, KeyModifiers};

pub trait DialogState {
    /// Whether this dialog pauses the engine-event drain while open.
    /// True for Confirm / Question (permission prompts that block a
    /// tool call); false for everything else. Default: `false`.
    fn blocks_agent(&self) -> bool {
        false
    }

    /// Intercept a key before the Dialog's default handler runs. Return
    /// `Some` to short-circuit, `None` to let the default (nav, Enter,
    /// Esc) take over. Default: no interception.
    fn handle_key(
        &mut self,
        _app: &mut App,
        _win: ui::WinId,
        _code: KeyCode,
        _mods: KeyModifiers,
    ) -> Option<ui::KeyResult> {
        None
    }

    /// Called when the default handler produces `Action("select:N")`.
    /// The dialog is closed by the caller immediately after.
    fn on_select(
        &mut self,
        _app: &mut App,
        _win: ui::WinId,
        _idx: usize,
        _agent: &mut Option<TurnState>,
    ) {
    }

    /// Called when the default handler produces `Action("dismiss")`.
    /// The dialog is closed by the caller immediately after.
    fn on_dismiss(&mut self, _app: &mut App, _win: ui::WinId) {}

    /// Called once per event-loop tick on the focused float. Use for
    /// dialogs that need to refresh their buffers from live external
    /// state (subagent snapshots, process registry, etc.). Default:
    /// no-op.
    fn tick(&mut self, _app: &mut App, _win: ui::WinId) {}
}
