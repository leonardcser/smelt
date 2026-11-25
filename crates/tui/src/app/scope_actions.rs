//! Running the actions of native keymap scopes (`crate::keymap`) once a key
//! resolved to one.

use crate::app::lua_bridge::NativeCall;
use crate::app::{EventOutcome, TuiApp};
use crate::keymap::{NativeAction, TextAction};
use crossterm::event::KeyEvent;

/// A Ctrl+C this soon after the one that cleared the prompt is a repeated
/// press: it must not also quit or cancel the turn.
pub(crate) const CTRL_C_REPEAT_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

impl TuiApp {
    /// Run native action `call` outside key dispatch, such as for
    /// `smelt.keymap.run`.
    pub(crate) fn run_native_action_now(&mut self, call: NativeCall) {
        let outcome = self.run_native_action(call, None);
        self.finish_outside_dispatch(outcome);
    }

    /// Run native action `call`, which `key` resolved to.
    pub(crate) fn run_native_action(
        &mut self,
        call: NativeCall,
        key: Option<KeyEvent>,
    ) -> EventOutcome {
        let win = call.win;
        let prompt_action = matches!(
            call.action,
            NativeAction::Prompt(_) | NativeAction::Interrupt | NativeAction::EditInEditor
        );
        // Placeholder accept and dismiss keys come before the prompt's keys.
        if let Some(key) = key.filter(|_| prompt_action) {
            if let Some(outcome) = self.dispatch_placeholder_key(win, key.code, key.modifiers) {
                return outcome;
            }
        }
        let outcome = match call.action {
            NativeAction::Prompt(action) => self.run_prompt_action(action),
            NativeAction::Interrupt => self.interrupt_prompt(),
            NativeAction::EditInEditor => {
                self.edit_in_editor();
                EventOutcome::Noop
            }
            NativeAction::View(action) => {
                let moved = matches!(
                    self.run_viewer_action(win, action),
                    crate::smelt_edit::Status::Consumed
                );
                if moved && win == self.well_known.transcript {
                    self.snap_transcript_cursor();
                }
                EventOutcome::Redraw
            }
            NativeAction::LeaveTranscript => {
                self.app_focus = crate::app::AppFocus::Prompt;
                EventOutcome::Redraw
            }
            NativeAction::Fold(fold) => self.fold_transcript(fold),
            NativeAction::ListStep(rows) => {
                crate::lua::ui_ops::move_cursor(self, win, rows);
                EventOutcome::Noop
            }
            NativeAction::ListFirst => {
                crate::lua::ui_ops::set_cursor_row(self, win, 0);
                EventOutcome::Noop
            }
            NativeAction::ListLast => {
                crate::lua::ui_ops::set_cursor_row(self, win, crate::smelt_edit::RowIndex::MAX);
                EventOutcome::Noop
            }
            NativeAction::ListSubmit => {
                crate::lua::ui_ops::submit_list(self, win);
                EventOutcome::Noop
            }
            NativeAction::Edit(command) => {
                crate::lua::ui_ops::edit_input(self, win, command);
                EventOutcome::Noop
            }
            NativeAction::InputSubmit => {
                crate::lua::ui_ops::submit_input(self, win);
                EventOutcome::Noop
            }
        };
        self.flush_lua_callbacks();
        outcome
    }

    fn run_prompt_action(&mut self, action: TextAction) -> EventOutcome {
        if action == TextAction::SubmitToRequestQueue && self.prompt_is_empty() {
            return EventOutcome::Noop;
        }
        let now = self.core.clock.instant_now();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let result = self
            .prompt
            .run_text_action(&mut pctx, action, &mut self.core.clipboard, now);
        if self.prompt_work_state().is_busy() {
            self.dispatch_running_input_action(result)
        } else {
            self.dispatch_input_action(result)
        }
    }

    /// Ctrl+C on the prompt: clear the prompt, else cancel the work in
    /// progress, else quit.
    fn interrupt_prompt(&mut self) -> EventOutcome {
        if !self.prompt_is_empty() {
            self.timers.last_prompt_clear = Some(self.core.clock.instant_now());
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            self.prompt.clear_with_undo(&mut pctx);
            return EventOutcome::Redraw;
        }
        let repeats_clear = self.timers.last_prompt_clear.is_some_and(|cleared| {
            self.core
                .clock
                .instant_now()
                .saturating_duration_since(cleared)
                < CTRL_C_REPEAT_GRACE
        });
        if repeats_clear {
            return EventOutcome::Noop;
        }
        // A paused continuation has no active work to cancel; Ctrl-C quits.
        let cancellable = self.prompt_work_state().is_busy()
            && (self.turn_input_is_active() || self.busy_stack.is_busy());
        if cancellable {
            EventOutcome::CancelAgent
        } else {
            EventOutcome::Quit
        }
    }

    fn prompt_is_empty(&self) -> bool {
        let pctx = crate::input::prompt_ctx_ref(&self.ui);
        pctx.buf.source().is_empty() && pctx.buf.attachments.is_empty()
    }
}
