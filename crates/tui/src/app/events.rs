use crate::app::{
    AppSequenceAction, CommandAction, EventOutcome, InputOutcome, QueuedInput, TuiApp,
};

use crate::input::Action;
use crate::keymap::{self, KeyAction};
use crate::smelt_term::UiHost;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

const APP_ESC: &[&str] = &["<Esc>"];
const APP_ESC_ESC: &[&str] = &["<Esc>", "<Esc>"];
const APP_SEQUENCE_BINDINGS: &[smelt_core::keymap::SequenceBinding<'static, AppSequenceAction>] = &[
    smelt_core::keymap::SequenceBinding {
        sequence: APP_ESC_ESC,
        action: AppSequenceAction::HardEsc,
        ambiguity: smelt_core::keymap::AmbiguityBehavior::RunAndClose,
        priority: 100,
    },
    smelt_core::keymap::SequenceBinding {
        sequence: APP_ESC,
        action: AppSequenceAction::LocalEsc,
        ambiguity: smelt_core::keymap::AmbiguityBehavior::RunAndKeepPrefix,
        priority: 0,
    },
];

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Returns `true` if the app should quit.
    pub(crate) fn dispatch_terminal_event(&mut self, ev: Event) -> bool {
        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            if self.term_focused != focused {
                self.term_focused = focused;
            }
            if !focused {
                self.ui.cancel_pointer_interaction();
            }
            return false;
        }

        if matches!(ev, Event::Key(_) | Event::Paste(_)) {
            self.ui.finish_pointer_interaction_for_keyboard();
        }

        if let Event::Key(k) = ev {
            if let Some(outcome) = self.dispatch_app_key_sequence(k) {
                self.emit_prompt_text_changed_if_dirty();
                return self.apply_event_outcome(outcome);
            }
        }

        // Global chords fire before focus-specific routing so no handler can swallow them.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = &ev
        {
            // Skip when an overlay or cmdline is focused - they get first dibs.
            if self.ui.focused_overlay().is_none() && self.well_known.cmdline.is_none() {
                let pctx = crate::input::prompt_ctx_ref(&self.ui);
                let ctx = self.input.key_context(pctx, self.agent.is_some());
                match keymap::lookup(*code, *modifiers, &ctx) {
                    Some(KeyAction::ToggleMode) => {
                        self.lua.cycle_mode();
                        return false;
                    }
                    Some(KeyAction::CycleReasoning) => {
                        self.lua.cycle_reasoning();
                        return false;
                    }
                    Some(KeyAction::Redraw) => {
                        self.ui.force_redraw();
                        return false;
                    }
                    _ => {}
                }
            }
        }

        // Overlay/modal focus: route keys through the focused leaf's keymap registry.
        // Mouse events fall through so wheel/scrollbar logic runs over the overlay rect.
        if self.ui.focused_overlay().is_some() || self.ui.active_modal().is_some() {
            if let Event::Resize(w, h) = ev {
                self.handle_resize(w, h);
                return false;
            }
            if let Event::Key(k) = ev {
                // Cmdline owns its keystrokes end-to-end: text edit,
                // history nav, completer cycling, and command exec
                // all need `&mut TuiApp`, so the overlay leaf has no
                // recipe and `cmdline_handle_key` runs every key
                // before the generic compositor dispatch. Returns
                // `Some(true)` only when the run command resolved to
                // Quit (propagated as the loop's quit signal).
                if self.cmdline_is_focused() {
                    if let Some(quit) = self.cmdline_handle_key(k) {
                        return quit;
                    }
                    // Swallow unclaimed keys so split keymaps don't fire over an open cmdline.
                    return false;
                }
                if matches!(self.run_key_cascade(k), crate::smelt_term::Status::Consumed) {
                    self.flush_lua_callbacks();
                    return false;
                }
                self.flush_lua_callbacks();
                return false;
            }
            if !matches!(ev, Event::Mouse(_)) {
                return false;
            }
            // Mouse events fall through so wheel + scrollbar drag work on overlays.
        }

        // Ctrl+C kills a running exec process.
        if self.exec.is_some()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            if let Some(handle) = self.exec.take() {
                handle.kill.notify_one();
            }
            return false;
        }

        let prompt_insert_check = if self.prompt_trace_enabled() {
            self.prompt_insert_check_for_event(&ev)
        } else {
            None
        };
        if prompt_insert_check.is_some() {
            self.trace_prompt_event(
                "prompt_printable_before",
                serde_json::json!({ "event": format!("{:?}", ev) }),
            );
        }

        let outcome = if self.agent.is_some() || self.busy_stack.is_busy() {
            self.handle_event_running(ev)
        } else {
            self.handle_event_idle(ev)
        };

        if prompt_insert_check.is_some() {
            self.trace_prompt_event("prompt_printable_after_input", serde_json::json!({}));
        }

        // Notify Lua subscribers if the prompt buffer changed (drives filter-as-you-type pickers).
        self.emit_prompt_text_changed_if_dirty();

        if let Some(check) = &prompt_insert_check {
            self.check_prompt_insert_after_event(check);
        }

        self.apply_event_outcome(outcome)
    }

    fn apply_event_outcome(&mut self, outcome: EventOutcome) -> bool {
        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                self.discard_turn(true);
                true
            }
            EventOutcome::CancelAgent => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel",
                    }),
                );
                self.discard_turn(true);
                false
            }
            EventOutcome::InterruptWithQueued => {
                // Save queued messages before cancel - the cancel path dumps them into the input buffer.
                let remaining = std::mem::take(&mut self.queued_inputs);
                self.discard_turn(true);
                self.queued_inputs = remaining;
                false
            }
            EventOutcome::Exec(handle) => {
                self.exec = Some(handle);
                false
            }
            EventOutcome::Submit {
                mut content,
                mut display,
            } => {
                self.clear_prompt_prediction();
                self.redact_user_submission(&mut content, &mut display);
                // Queue while a background plugin (compaction, etc.) has
                // taken a `smelt.work.busy` token so messages run
                // against the post-busy state.
                if self.busy_stack.is_busy() {
                    let text = content.text_content();
                    if !text.is_empty()
                        && self.queued_inputs.len() < crate::app::MAX_QUEUED_MESSAGES
                    {
                        self.queued_inputs
                            .push(QueuedInput::Message(text.into_owned()));
                    }
                } else {
                    let text = content.text_content();
                    let has_images = content.image_count() > 0;
                    if !text.is_empty() || has_images {
                        let outcome = if has_images && text.trim().is_empty() {
                            InputOutcome::StartAgent
                        } else {
                            self.process_input(&text)
                        };
                        self.apply_input_outcome(outcome, content, &display);
                    } else if !self.queued_inputs.is_empty() {
                        // Empty submit: send the oldest queued message immediately.
                        let queued = self.queued_inputs.remove(0);
                        self.start_queued_input(queued);
                    }
                }
                // Don't restore stash if a dialog opened - it restores on close.
                if self.ui.focused_overlay().is_none() {
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    self.input.restore_stash(&mut pctx);
                }
                false
            }
        }
    }

    fn dispatch_app_key_sequence(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        if self.agent.is_none() && !self.busy_stack.is_busy() {
            self.timers.app_sequence.clear();
            return None;
        }

        if !self.timers.app_sequence.has_pending() && !matches!(key.code, KeyCode::Esc) {
            return None;
        }

        let token = match crate::lua::chord_string(key) {
            Some(token) => token,
            None => {
                self.timers.app_sequence.clear();
                return None;
            }
        };
        let now = self.core.clock.instant_now();
        match self.timers.app_sequence.step(
            token,
            APP_SEQUENCE_BINDINGS,
            now,
            crate::app::CHORD_TIMEOUT_MS,
        ) {
            smelt_core::keymap::SequenceStep::Run(AppSequenceAction::HardEsc) => {
                Some(self.handle_hard_escape_sequence())
            }
            smelt_core::keymap::SequenceStep::Run(AppSequenceAction::LocalEsc)
            | smelt_core::keymap::SequenceStep::Pending
            | smelt_core::keymap::SequenceStep::NoMatch => None,
        }
    }

    fn handle_hard_escape_sequence(&mut self) -> EventOutcome {
        self.timers.pending_chord = None;
        if !self.queued_inputs.is_empty() {
            self.drain_queued_inputs_into_prompt();
            return EventOutcome::Noop;
        }
        EventOutcome::CancelAgent
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared preamble for idle and agent-running paths.
    ///
    /// Returns `Some(outcome)` when consumed; `None` to continue with path-specific logic.
    ///
    /// Dispatch priority: resize/mouse → Lua keymaps → pane chords → cmdline `:` → content focus → overlay keys.
    fn dispatch_common(&mut self, ev: &Event) -> Option<EventOutcome> {
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        // Buffer-local Lua keymaps win over global (nvim priority). Skipped when an overlay
        // owns focus - overlay-leaf dispatch happens upstream.
        if let Event::Key(k) = *ev {
            if self.ui.focused_overlay().is_none() {
                let lua = &self.lua;
                let mut lua_invoke =
                    |handle: crate::smelt_term::LuaHandle,
                     win: crate::smelt_term::WinId,
                     payload: &crate::smelt_term::Payload| {
                        lua.queue_invocation(handle, win, payload);
                    };
                let result = self
                    .ui
                    .dispatch_event(crate::smelt_term::Event::Key(k), &mut lua_invoke);
                if matches!(result, crate::smelt_term::Status::Consumed) {
                    self.flush_lua_callbacks();
                    return Some(EventOutcome::Noop);
                }
            }
        }

        // Global Lua keymaps. Handlers may return `false` to fall through.
        // Multi-key sequences accumulate in `self.timers.pending_chord` until exact-matched or timed out.
        if let Event::Key(k) = *ev {
            if let Some(token) = crate::lua::chord_string(k) {
                let vim_mode = self.current_vim_mode_label();
                use smelt_core::lua::runtime::KeymapResult;

                // Single-key first - allocation-free common case.
                match self.lua.run_keymap(&token, vim_mode.as_deref(), None) {
                    KeymapResult::Consumed => {
                        self.timers.pending_chord = None;
                        self.flush_lua_callbacks();
                        return Some(EventOutcome::Noop);
                    }
                    KeymapResult::PassThrough | KeymapResult::NoBinding => {
                        self.flush_lua_callbacks();
                    }
                }

                // Multi-key chord: drop stale pending sequence, then append and match.
                let now = self.core.clock.instant_now();
                if let Some(pending) = &self.timers.pending_chord {
                    if smelt_core::keymap::chord_expired(
                        pending.started,
                        now,
                        crate::app::CHORD_TIMEOUT_MS,
                    ) {
                        self.timers.pending_chord = None;
                    }
                }
                if self.timers.pending_chord.is_none() {
                    self.timers.pending_chord = Some(crate::app::PendingChord {
                        tokens: Vec::new(),
                        started: now,
                        vim_mode_at_start: if self.input.vim_enabled(self.prompt_win()) {
                            Some(self.prompt_win().vim_mode)
                        } else {
                            None
                        },
                    });
                }
                let (mut tokens, started, vim_mode_at_start) = {
                    let p = self.timers.pending_chord.take().unwrap();
                    (p.tokens, p.started, p.vim_mode_at_start)
                };
                tokens.push(token);

                let mut oracle = LuaChordOracle {
                    lua: &self.lua,
                    vim_mode: vim_mode.as_deref(),
                    vim_mode_at_start,
                };
                let outcome = smelt_core::keymap::match_chord(tokens, &mut oracle);
                self.flush_lua_callbacks();
                match outcome {
                    smelt_core::keymap::ChordOutcome::Consumed => {
                        return Some(EventOutcome::Noop);
                    }
                    smelt_core::keymap::ChordOutcome::Pending { tokens } => {
                        if tokens.is_empty() {
                            self.timers.pending_chord = None;
                        } else {
                            self.timers.pending_chord = Some(crate::app::PendingChord {
                                tokens,
                                started,
                                vim_mode_at_start,
                            });
                        }
                    }
                }
            }
        }
        if let Some(outcome) = self.handle_pane_chord(ev) {
            return Some(outcome);
        }
        // `:` opens the cmdline unless in insert mode.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char(':'),
            modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
            ..
        }) = ev
        {
            let in_insert = match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    !self.input.vim_enabled(self.prompt_win())
                        || self.prompt_win().vim_mode == crate::smelt_term::VimMode::Insert
                }
                crate::app::AppFocus::Content => false,
            };
            if !in_insert {
                self.open_cmdline();
                return Some(EventOutcome::Noop);
            }
        }
        if self.app_focus == crate::app::AppFocus::Content {
            if matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Esc,
                    ..
                })
            ) {
                if let Some(outcome) = self.dismiss_notification_for_esc() {
                    return Some(outcome);
                }
            }
            return Some(self.handle_event_app_history(ev));
        }
        if let Some(outcome) = self.handle_overlay_keys(ev) {
            return Some(outcome);
        }
        None
    }

    fn handle_event_idle(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev) {
            return outcome;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            if matches!(code, KeyCode::Esc) {
                return self.handle_idle_prompt_esc(ev, modifiers);
            }

            // Placeholder routing: when the prompt is empty and a placeholder is set,
            // matching `accept_keys` accept the text into the buffer; matching
            // `dismiss_keys` clear it. Both fire the corresponding win event.
            // Typing past those chords leaves the placeholder intact (the buffer
            // becoming non-empty just hides it visually - undoing back to empty
            // restores it).
            if let Some(outcome) =
                self.dispatch_placeholder_key(self.well_known.prompt, code, modifiers)
            {
                return outcome;
            }

            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.input.key_context(pctx_ref, false);

            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::Quit => {
                        return EventOutcome::Quit;
                    }
                    KeyAction::ClearBuffer => {
                        self.timers.last_ctrlc = Some(self.core.clock.instant_now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.clear(&mut pctx);
                        return EventOutcome::Redraw;
                    }
                    _ => {}
                }
            }
        }

        let now = self.core.clock.instant_now();
        if self.prompt_trace_enabled() {
            self.trace_prompt_event(
                "prompt_input_before",
                serde_json::json!({ "path": "idle", "event": format!("{:?}", ev) }),
            );
        }
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
            &mut self.core.clipboard,
            now,
        );
        if self.prompt_trace_enabled() {
            self.trace_prompt_event(
                "prompt_input_after",
                serde_json::json!({ "path": "idle", "action": input_action_label(&action) }),
            );
        }
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev) {
            return outcome;
        }

        // Record last keypress for deferred permission dialogs.
        if matches!(ev, Event::Key(_)) {
            self.timers.last_keypress = Some(self.core.clock.instant_now());
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.input.key_context(pctx_ref, true);
            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::CancelAgent => {
                        self.queued_inputs.clear();
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        self.timers.last_ctrlc = Some(self.core.clock.instant_now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.clear(&mut pctx);
                        self.queued_inputs.clear();
                        return EventOutcome::Noop;
                    }
                    _ => {}
                }
            }
        }

        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            return self.handle_running_prompt_esc(ev);
        }

        let now = self.core.clock.instant_now();
        if self.prompt_trace_enabled() {
            self.trace_prompt_event(
                "prompt_input_before",
                serde_json::json!({ "path": "running", "event": format!("{:?}", ev) }),
            );
        }
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let input_action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
            &mut self.core.clipboard,
            now,
        );
        if self.prompt_trace_enabled() {
            self.trace_prompt_event(
                "prompt_input_after",
                serde_json::json!({ "path": "running", "action": input_action_label(&input_action) }),
            );
        }
        match input_action {
            Action::Submit {
                mut content,
                mut display,
            } => {
                self.clear_prompt_prediction();
                self.redact_user_submission(&mut content, &mut display);
                let text = content.text_content();
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                if !text.is_empty() && self.queued_inputs.len() < crate::app::MAX_QUEUED_MESSAGES {
                    self.queued_inputs
                        .push(QueuedInput::Message(text.into_owned()));
                }
            }
            Action::SubmitEmpty => {
                if !self.queued_inputs.is_empty() {
                    self.clear_prompt_prediction();
                    return EventOutcome::InterruptWithQueued;
                }
            }
            Action::Redraw => {}
            Action::EditInEditor => {
                self.edit_in_editor();
            }
            Action::CenterScroll => {
                self.prompt_win_mut().pending_recenter = true;
            }
            Action::PanColumns(d) => {
                self.pan_prompt_columns(d);
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
            }
            Action::Noop => {}
        }
        EventOutcome::Noop
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn handle_idle_prompt_esc(&mut self, ev: Event, modifiers: KeyModifiers) -> EventOutcome {
        if self.input.vim_enabled(self.prompt_win())
            && matches!(
                self.prompt_win().vim_mode,
                crate::smelt_term::VimMode::Insert
                    | crate::smelt_term::VimMode::Visual
                    | crate::smelt_term::VimMode::VisualLine
            )
        {
            let now = self.core.clock.instant_now();
            let action = {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.handle_event(
                    &mut pctx,
                    ev,
                    Some(&mut self.input_history),
                    &mut self.core.clipboard,
                    now,
                )
            };
            return self.dispatch_input_action(action);
        }

        if let Some(outcome) =
            self.dispatch_placeholder_key(self.well_known.prompt, KeyCode::Esc, modifiers)
        {
            return outcome;
        }

        if let Some(outcome) = self.dismiss_notification_for_esc() {
            return outcome;
        }

        EventOutcome::Noop
    }

    fn handle_running_prompt_esc(&mut self, ev: Event) -> EventOutcome {
        let cur_mode = if self.input.vim_enabled(self.prompt_win()) {
            Some(self.prompt_win().vim_mode)
        } else {
            None
        };
        let now = self.core.clock.instant_now();

        if cur_mode == Some(crate::smelt_term::VimMode::Insert) {
            self.apply_prompt_escape_to_input(ev, now);
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.dispatch_placeholder_key(
            self.well_known.prompt,
            KeyCode::Esc,
            KeyModifiers::empty(),
        ) {
            return outcome;
        }

        if !self.queued_inputs.is_empty() {
            self.drain_queued_inputs_into_prompt();
            self.timers.app_sequence.clear();
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.dismiss_notification_for_esc() {
            return outcome;
        }

        if matches!(
            cur_mode,
            Some(crate::smelt_term::VimMode::Visual | crate::smelt_term::VimMode::VisualLine)
        ) {
            self.apply_prompt_escape_to_input(ev, now);
        }

        EventOutcome::Noop
    }

    fn apply_prompt_escape_to_input(&mut self, ev: Event, now: std::time::Instant) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input
            .handle_event(&mut pctx, ev, None, &mut self.core.clipboard, now);
    }

    fn dismiss_notification_for_esc(&mut self) -> Option<EventOutcome> {
        if self.notification.is_some() {
            self.dismiss_notification();
            return Some(EventOutcome::Redraw);
        }
        None
    }

    fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit { content, display } => EventOutcome::Submit { content, display },
            Action::SubmitEmpty => EventOutcome::Noop,
            Action::EditInEditor => {
                self.edit_in_editor();
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.prompt_win_mut().pending_recenter = true;
                EventOutcome::Noop
            }
            Action::PanColumns(d) => {
                self.pan_prompt_columns(d);
                EventOutcome::Redraw
            }
            Action::Redraw => EventOutcome::Redraw,
            Action::NotifyError(msg) => {
                self.notify_error(msg);
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    fn pan_prompt_columns(&mut self, delta: isize) {
        let win_id = crate::app::PROMPT_WIN;
        if let Some(win) = self.ui.win_mut(win_id) {
            let viewport_cols = win.viewport.map(|v| v.content_width).unwrap_or(0);
            win.pan_by_columns(delta, viewport_cols);
        }
    }

    fn edit_in_editor(&mut self) {
        let req = match crate::input::editor::prepare(self.prompt_buf().source()) {
            Ok(req) => req,
            Err(e) => {
                self.notify_error(format!("editor: {e}"));
                return;
            }
        };
        let spawn = || {
            std::process::Command::new(&req.program)
                .args(&req.args)
                .status()
        };
        let status = match self.terminal.as_ref() {
            Some(t) => t.suspended(spawn),
            None => {
                self.notify_error("editor: unavailable without an attached terminal".to_string());
                self.ui.force_redraw();
                return;
            }
        };
        // Vim et al re-show the hardware cursor and scribble over the alt
        // screen; force a full repaint so the diff baseline is rebuilt.
        self.ui.force_redraw();
        match crate::input::editor::finalize(req, status) {
            Ok(text) => {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.replace_text(&mut pctx, text);
            }
            Err(msg) => self.notify_error(msg),
        }
    }

    pub(crate) fn handle_resize(&mut self, w: u16, h: u16) {
        let width_changed = w != self.last_width;
        self.last_width = w;
        self.last_height = h;
        let _ = self
            .ui
            .dispatch_event(crate::smelt_term::Event::Resize(w, h), &mut |_, _, _| {});
        if width_changed {
            self.invalidate_for_width(w);
        }
    }

    fn handle_overlay_keys(&mut self, ev: &Event) -> Option<EventOutcome> {
        if matches!(ev, Event::Key(k) if k.code != KeyCode::Esc) && self.notification.is_some() {
            self.dismiss_notification();
        }

        None
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    pub(crate) fn process_input(&mut self, input: &str) -> InputOutcome {
        if input.is_empty() {
            return InputOutcome::Continue;
        }

        let trimmed = input.trim();
        self.input_history.push(input.to_string());

        let is_from_paste = self.input.skip_shell_escape();

        // `:` is a vim-style alias for `/` - normalize before command lookup.
        let dispatch_input = if let Some(rest) = trimmed.strip_prefix(':') {
            format!("/{rest}")
        } else {
            trimmed.to_string()
        };

        match crate::commands::run_command(self, &dispatch_input) {
            CommandAction::Exec(handle) => return InputOutcome::Exec(handle),
            CommandAction::Continue => {}
        }
        if dispatch_input.starts_with('/') && smelt_core::commands::is_command(&dispatch_input) {
            return InputOutcome::Continue;
        }
        // Shell escapes (`!cmd`) skip agent start, but pasted content starting with `!` does not.
        if trimmed.starts_with('!') && !is_from_paste {
            return InputOutcome::Continue;
        }

        self.bump_epoch("input_epoch");
        self.core
            .cells
            .set_dyn("input_submit", std::rc::Rc::new(trimmed.to_string()));
        self.pump_lua();

        InputOutcome::StartAgent
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    /// Viewport rows for the content pane. Uses the prompt's previous-frame rendered height
    /// so multi-line prompts don't cause scroll math to overshoot.
    pub(crate) fn viewport_rows_estimate(&self) -> u16 {
        self.layout.viewport_rows().max(1)
    }

    /// Close an overlay leaf and clean up picker/Lua-callback registrations.
    /// `Ui::win_close` cascades to overlay close when the leaf belongs to one.
    pub(crate) fn close_overlay_leaf(&mut self, win_id: crate::smelt_term::WinId) {
        crate::picker::forget(self, win_id);
        self.placeholder_opts.remove(&win_id);
        for id in self.win_close(win_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close an overlay by id without assuming its first layout leaf is a window.
    pub(crate) fn close_overlay(&mut self, overlay_id: crate::smelt_term::OverlayId) {
        for id in self.ui.overlay_close_tree(overlay_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close the focused overlay if it doesn't block the agent.
    /// Fires `WinEvent::Dismiss` so callbacks can flush pending state before close.
    pub(crate) fn close_focused_non_blocking_overlay(&mut self) {
        let Some(overlay_id) = self.ui.focused_overlay() else {
            return;
        };
        let Some(overlay) = self.ui.overlay(overlay_id) else {
            return;
        };
        if overlay.blocks_agent {
            return;
        }
        let Some(root) = overlay
            .layout
            .leaves_in_order()
            .into_iter()
            .next()
            .map(|p| crate::smelt_term::WinId(p.0))
        else {
            return;
        };
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_term::LuaHandle,
                              win: crate::smelt_term::WinId,
                              payload: &crate::smelt_term::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            root,
            crate::smelt_term::WinEvent::Dismiss,
            crate::smelt_term::Payload::None,
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// True when the focused overlay blocks engine-event drain (Confirm/Question/Lua dialogs).
    pub(crate) fn focused_overlay_blocks_agent(&self) -> bool {
        self.ui
            .focused_overlay()
            .and_then(|id| self.ui.overlay(id))
            .is_some_and(|o| o.blocks_agent)
    }

    /// Snap the transcript cursor to the nearest selectable cell, skipping gutters and padding.
    pub(crate) fn snap_transcript_cursor(&mut self) {
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let cpos = self.transcript_win().cpos;
        let show_thinking = self.core.config.settings.show_thinking;
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        let snapped = self.snap_cpos_to_selectable(&rows, cpos, show_thinking);
        if snapped != cpos {
            self.transcript_win_mut().cpos = snapped;
            let viewport = self.viewport_rows_estimate();
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            win.expect("transcript window")
                .resync(buf.expect("transcript buffer"), viewport);
        }
    }

    /// Orchestrates the per-key dispatch cascade when an overlay or modal is
    /// focused. Tiers fire in order; the first to return `Consumed` wins:
    ///
    /// - Tier 1: specific keymap on the focused leaf (`win_set_keymap`).
    /// - Tier 1b: overlay-scoped keymap (`overlay_set_keymap`) on the overlay
    ///   containing the focused leaf.
    /// - Tier 2: global Lua keymap (`smelt.keymap.set("", chord, fn)`).
    /// - Tier 3: per-window catch-all fallback (`win_set_key_fallback`).
    /// - Tier 4: vim viewer keys on the focused leaf.
    /// - Tier 5: modal dismiss for bare Esc / Ctrl-C.
    ///
    /// Putting global keymaps between tiers 1b and 3 lets a site-wide chord
    /// like `?` -> /help win over a dialog input's blanket printable-char
    /// fallback, without each leaf needing a bespoke carve-out. Overlay-scoped
    /// keymaps sit above global so an open dialog/picker's local intent
    /// (e.g. `Tab` cycles items) beats a site-wide rebinding of the same chord.
    pub(crate) fn run_key_cascade(&mut self, k: KeyEvent) -> crate::smelt_term::Status {
        use crate::smelt_term::Status;

        // Tier 1: specific keymap on the focused leaf.
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_term::LuaHandle,
                 win: crate::smelt_term::WinId,
                 payload: &crate::smelt_term::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui.dispatch_key(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 1b: overlay-scoped keymap on the overlay containing the
        // focused leaf. Owned by the overlay, so any leaf inside it sees
        // the same bindings without per-leaf re-registration.
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_term::LuaHandle,
                 win: crate::smelt_term::WinId,
                 payload: &crate::smelt_term::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui
                    .dispatch_overlay_key(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 2: global Lua keymap (single-chord lookup only - overlays
        // don't participate in the chord-buffering path).
        if let Some(token) = crate::lua::chord_string(k) {
            let vim_mode = self.current_vim_mode_label();
            use smelt_core::lua::runtime::KeymapResult;
            match self.lua.run_keymap(&token, vim_mode.as_deref(), None) {
                KeymapResult::Consumed => {
                    self.flush_lua_callbacks();
                    return Status::Consumed;
                }
                KeymapResult::PassThrough | KeymapResult::NoBinding => {
                    self.flush_lua_callbacks();
                }
            }
        }

        // Tier 3: vim viewer keys for vim-enabled read-only overlay leaves.
        // Run before per-window catch-alls so generic dialog/list fallbacks do
        // not swallow Normal/Visual-mode motions and yanks.
        if self.dispatch_overlay_viewer_key(k) {
            return Status::Consumed;
        }

        // Tier 4: per-window catch-all fallback (dialog inputs, etc.).
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_term::LuaHandle,
                 win: crate::smelt_term::WinId,
                 payload: &crate::smelt_term::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui
                    .dispatch_key_fallback(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 5: bare Esc / Ctrl-C dismisses the active modal.
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_term::LuaHandle,
                              win: crate::smelt_term::WinId,
                              payload: &crate::smelt_term::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui
            .try_dismiss_modal_for_chord(k.code, k.modifiers, &mut lua_invoke)
    }

    /// Overlay-focus key cascade tier 4. Wraps the shared
    /// [`Self::dispatch_window_viewer_key`] with two overlay-specific gates:
    ///   * Insert-mode skip - typing inside an editable overlay leaf must
    ///     not bubble nav keys here.
    ///   * Esc-in-idle-Normal falls through so the modal-dismiss tier (5)
    ///     can close the overlay; Visual / pending-sequence Esc stays with vim.
    pub(crate) fn dispatch_overlay_viewer_key(&mut self, k: KeyEvent) -> bool {
        let win = match self.ui.focus() {
            Some(w) => w,
            None => return false,
        };
        let (vim_enabled, vim_mode, vim_idle) = match self.ui.win(win) {
            Some(w) => (w.vim_enabled, w.vim_mode, w.vim_state.is_idle()),
            None => return false,
        };
        let in_insert = vim_enabled && vim_mode == crate::smelt_term::VimMode::Insert;
        if in_insert {
            return false;
        }
        if k.code == KeyCode::Esc
            && vim_enabled
            && vim_mode == crate::smelt_term::VimMode::Normal
            && vim_idle
        {
            return false;
        }
        matches!(
            self.dispatch_window_viewer_key(win, k),
            crate::smelt_term::Status::Consumed
        )
    }

    /// Unified viewer-key dispatcher shared between transcript, overlay leaves,
    /// and any future scrollable window. Resolution order:
    ///   1. Vim engine (when `vim_enabled`) - handles motions, operators,
    ///      and yanks; falls through with `Passthrough` for chords vim
    ///      doesn't claim (e.g. Shift+Arrow selection-extend).
    ///   2. Shared keymap dispatch via [`Self::dispatch_buffer_action`].
    ///      The keymap uses the window's actual `vim_enabled`/`vim_mode`
    ///      context so vim-Normal-only chords (Ctrl-U/D, Ctrl-B/F page
    ///      motion, Ctrl-Y/E line scroll) and emacs chords (Ctrl-V/Alt-V,
    ///      Alt-</>, Ctrl-P/N) route correctly.
    ///
    /// Editing actions (kill, delete, yank, etc.) are silently dropped on
    /// `buf.readonly` buffers - the same dispatcher serves the read-only
    /// transcript and a future read-write Lua-created buffer without
    /// branching the call site.
    pub(crate) fn dispatch_window_viewer_key(
        &mut self,
        win_id: crate::smelt_term::WinId,
        k: KeyEvent,
    ) -> crate::smelt_term::Status {
        use crate::smelt_term::Status;
        let (vim_enabled, buf_id, viewport_rows) = match self.ui.win(win_id) {
            Some(w) => (
                w.vim_enabled,
                w.buf,
                w.viewport.map(|v| v.rect.height).unwrap_or(0),
            ),
            None => return Status::Ignored,
        };
        if viewport_rows == 0 {
            return Status::Ignored;
        }
        let buf_empty = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().is_empty())
            .unwrap_or(true);
        if buf_empty {
            return Status::Ignored;
        }

        if vim_enabled {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            let now = self.core.clock.instant_now();
            let status = win.handle_key(buf, k, &mut self.core.clipboard, now);
            win.sync_follow_tail(buf, viewport_rows);
            if matches!(status, Status::Consumed) {
                return Status::Consumed;
            }
            // Vim Passthrough (Shift+Arrows, etc.) falls through so the
            // shared keymap layer can claim selection-extend chords -
            // matches the prompt's behaviour.
        }

        self.dispatch_buffer_action(win_id, buf_id, k, viewport_rows)
    }

    /// Keymap-driven dispatcher: looks up the binding under the window's
    /// real vim context, executes the resolved [`KeyAction`] against the
    /// (win, buf) pair, and honours `buf.readonly` for editing actions.
    /// Mirrors the prompt's `PromptState::execute_key_action` for the
    /// motion + selection + copy + page-motion subset; non-motion actions
    /// (editing, history) are handled elsewhere (the prompt) or dropped
    /// (read-only buffers).
    fn dispatch_buffer_action(
        &mut self,
        win_id: crate::smelt_term::WinId,
        buf_id: crate::smelt_term::BufId,
        k: KeyEvent,
        viewport_rows: u16,
    ) -> crate::smelt_term::Status {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crate::smelt_term::{Status, VimMode};

        let (vim_enabled, vim_mode, readonly, buf_empty) =
            match (self.ui.win(win_id), self.ui.buf(buf_id)) {
                (Some(w), Some(b)) => (w.vim_enabled, w.vim_mode, b.readonly, b.text().is_empty()),
                _ => return Status::Ignored,
            };
        let ctx = KeyContext {
            buf_empty,
            vim_non_insert: vim_enabled
                && matches!(
                    vim_mode,
                    VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                ),
            vim_enabled,
            agent_running: false,
        };
        let Some(action) = lookup(k.code, k.modifiers, &ctx) else {
            return Status::Ignored;
        };

        let extending = matches!(
            action,
            KeyAction::SelectLeft
                | KeyAction::SelectRight
                | KeyAction::SelectUp
                | KeyAction::SelectDown
                | KeyAction::SelectWordForward
                | KeyAction::SelectWordBackward
                | KeyAction::SelectStartOfLine
                | KeyAction::SelectEndOfLine
        );
        let is_motion = matches!(
            action,
            KeyAction::MoveLeft
                | KeyAction::MoveRight
                | KeyAction::MoveUp
                | KeyAction::MoveDown
                | KeyAction::MoveStartOfLine
                | KeyAction::MoveEndOfLine
                | KeyAction::MoveWordForward
                | KeyAction::MoveWordBackward
                | KeyAction::MoveStartOfBuffer
                | KeyAction::MoveEndOfBuffer
                | KeyAction::PageUp
                | KeyAction::PageDown
                | KeyAction::HalfPageUp
                | KeyAction::HalfPageDown
                | KeyAction::ScrollLineUp
                | KeyAction::ScrollLineDown
        );

        let cpos_before = self.ui.win(win_id).expect("window").cpos;
        let win = self.ui.win_mut(win_id).expect("window");
        if is_motion {
            win.selection_anchor = None;
        } else if extending {
            win.extend_selection(cpos_before);
        }

        // Line-delta motions resolve through the window's display-line API,
        // which respects soft-wrap and gutter math.
        let line_delta: Option<isize> = match action {
            KeyAction::MoveUp | KeyAction::SelectUp => Some(-1),
            KeyAction::MoveDown | KeyAction::SelectDown => Some(1),
            KeyAction::PageUp => Some(-(viewport_rows as isize)),
            KeyAction::PageDown => Some(viewport_rows as isize),
            KeyAction::HalfPageUp => Some(-((viewport_rows as isize) / 2).max(1)),
            KeyAction::HalfPageDown => Some(((viewport_rows as isize) / 2).max(1)),
            KeyAction::ScrollLineUp => Some(-1),
            KeyAction::ScrollLineDown => Some(1),
            _ => None,
        };
        if let Some(d) = line_delta {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.move_cursor_by_lines(buf, d, viewport_rows);
            win.sync_follow_tail(buf, viewport_rows);
            return Status::Consumed;
        }

        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Status::Ignored,
        };
        let text = buf.text().to_string();
        let cpos = self.ui.win(win_id).expect("window").cpos;
        let mv: Option<usize> = match action {
            KeyAction::MoveLeft | KeyAction::SelectLeft => {
                Some(crate::smelt_term::text::prev_char_boundary(&text, cpos))
            }
            KeyAction::MoveRight | KeyAction::SelectRight => {
                Some(crate::smelt_term::text::next_char_boundary(&text, cpos))
            }
            KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                Some(crate::smelt_term::text::line_start(&text, cpos))
            }
            KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                Some(crate::smelt_term::text::line_end(&text, cpos))
            }
            KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                Some(crate::smelt_term::text::word_forward_pos(
                    &text,
                    cpos,
                    crate::smelt_term::text::CharClass::Word,
                ))
            }
            KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                Some(crate::smelt_term::text::word_backward_pos(
                    &text,
                    cpos,
                    crate::smelt_term::text::CharClass::Word,
                ))
            }
            KeyAction::MoveStartOfBuffer => Some(0),
            KeyAction::MoveEndOfBuffer => Some(text.len()),
            KeyAction::CopySelection => {
                let win = self.ui.win(win_id).expect("window");
                if let Some((s, e)) = win.selection_range(buf) {
                    let s = crate::smelt_term::text::snap(&text, s);
                    let e = crate::smelt_term::text::snap(&text, e);
                    if s < e {
                        let out = buf.copy_range(s..e);
                        if !out.clipboard.is_empty() {
                            let _ = self.core.clipboard.write(&out.clipboard);
                        }
                    }
                }
                return Status::Consumed;
            }
            // Read-only buffers silently drop editing actions; the prompt
            // path is the only consumer for these.
            _ if readonly => return Status::Ignored,
            _ => None,
        };
        drop(text);
        if let Some(new_cpos) = mv {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.cpos = new_cpos;
            // A shift-motion that resolved to no movement (e.g. Shift+End at
            // EOL) leaves `selection_anchor == cpos` - a degenerate, empty
            // selection that downstream code treats as "no selection" but
            // whose anchor still points into the buffer. Clear it so a
            // follow-up source-shrinking edit can't orphan it.
            if win.selection_anchor == Some(win.cpos) {
                win.selection_anchor = None;
            }
            win.resync(buf, viewport_rows);
            return Status::Consumed;
        }
        Status::Ignored
    }
}

fn input_action_label(action: &Action) -> &'static str {
    match action {
        Action::Redraw => "redraw",
        Action::Submit { .. } => "submit",
        Action::SubmitEmpty => "submit_empty",
        Action::EditInEditor => "edit_in_editor",
        Action::CenterScroll => "center_scroll",
        Action::PanColumns(_) => "pan_columns",
        Action::NotifyError(_) => "notify_error",
        Action::Noop => "noop",
    }
}

/// Adapter that lets the pure [`smelt_core::keymap::match_chord`] loop call
/// into the live Lua keymap registry. Carries the `vim_mode_at_chord_start`
/// context pair that handlers see on multi-key matches.
struct LuaChordOracle<'a> {
    lua: &'a crate::lua::LuaRuntime,
    vim_mode: Option<&'a str>,
    vim_mode_at_start: Option<crate::smelt_term::VimMode>,
}

impl smelt_core::keymap::ChordOracle for LuaChordOracle<'_> {
    fn has_longer(&self, seq: &str) -> bool {
        self.lua.chord_has_longer(seq, self.vim_mode)
    }
    fn try_keymap(&mut self, seq: &str) -> smelt_core::lua::runtime::KeymapResult {
        let ctx: Vec<(&str, String)> = vec![(
            "vim_mode_at_chord_start",
            self.vim_mode_at_start
                .map(|m| crate::lua::LuaVimMode::from(m).label().to_string())
                .unwrap_or_default(),
        )];
        self.lua
            .run_keymap(seq, self.vim_mode, Some(ctx.as_slice()))
    }
}
