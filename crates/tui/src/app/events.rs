use super::*;

use crate::keymap::{self, KeyAction};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event},
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::time::{Duration, Instant};
use ui::UiHost;

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Handle a single terminal event, potentially starting/stopping agents.
    /// Returns `true` if the app should quit.
    pub(super) fn dispatch_terminal_event(&mut self, ev: Event, t: &mut Timers) -> bool {
        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            if self.term_focused != focused {
                self.term_focused = focused;
            }
            return false;
        }

        // Global chord layer: these keys fire in every focus context
        // (prompt, content, or any overlay leaf). Intercepted before
        // focus-specific routing so no handler below can swallow them.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = &ev
        {
            // Global shortcuts only fire when no overlay is focused
            // and no cmdline overlay is open — otherwise the dialog's
            // keymap (e.g. Confirm's BackTab handler) or the cmdline's
            // text-edit recipe gets first dibs.
            if self.ui.focused_overlay().is_none() && self.well_known.cmdline.is_none() {
                let ctx = self
                    .input
                    .key_context(self.agent.is_some(), false, self.vim_mode);
                match keymap::lookup(*code, *modifiers, &ctx) {
                    Some(KeyAction::ToggleMode) => {
                        self.toggle_mode();
                        return false;
                    }
                    Some(KeyAction::CycleReasoning) => {
                        self.cycle_reasoning();
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

        // Overlay focus: when an overlay leaf is focused (or a modal
        // overlay is active), route keys through the focused leaf's
        // keymap registry. Per-window keymaps fire from `Callbacks`;
        // any Rust callbacks queue `AppOp`s drained below. Mouse
        // events fall through so the regular `handle_mouse` path can
        // run wheel/scrollbar logic over the overlay's rect.
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
                    // Cmdline didn't claim the key — swallow it so
                    // unrelated split keymaps don't fire on top of an
                    // open cmdline.
                    return false;
                }
                let lua = &self.core.lua;
                let mut lua_invoke =
                    |handle: ui::LuaHandle, win: ui::WinId, payload: &ui::Payload| {
                        lua.queue_invocation(handle, win, payload);
                    };
                let _ = self.ui.dispatch_event(ui::Event::Key(k), &mut lua_invoke);
                self.flush_lua_callbacks();
                return false;
            }
            if !matches!(ev, Event::Mouse(_)) {
                return false;
            }
            // Fallthrough: mouse events go to the regular dispatch
            // below so wheel + scrollbar drag work on overlays.
        }

        // Ctrl+C while exec is running → kill it.
        if self.exec_kill.is_some()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            if let Some(kill) = self.exec_kill.take() {
                kill.notify_one();
            }
            return false;
        }

        let outcome = if self.agent.is_some() {
            self.handle_event_running(ev, t)
        } else {
            self.handle_event_idle(ev, t)
        };

        // Fire `WinEvent::TextChanged` on the prompt window if the buffer
        // changed during this event. Lua plugins use this to drive
        // filter-as-you-type (e.g. `smelt.prompt.open_picker`).
        self.emit_prompt_text_changed_if_dirty();

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
                // Cancel the running turn and let the queued messages
                // auto-start via the main loop once TurnComplete arrives.
                // We must save the queued messages before finish_turn
                // because the cancel path dumps them into the input buffer.
                let remaining = std::mem::take(&mut self.queued_messages);
                self.discard_turn(true);
                self.queued_messages = remaining;
                false
            }
            EventOutcome::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
                false
            }
            EventOutcome::Submit {
                mut content,
                mut display,
            } => {
                // Ingress: scrub secrets from user submissions before they
                // reach the screen, the queue, or the engine.
                self.redact_user_submission(&mut content, &mut display);
                // Queue messages while compaction is in progress so they
                // are sent against the compacted history, not the old one.
                if self.is_compacting() {
                    let text = content.text_content();
                    if !text.is_empty() {
                        self.queued_messages.push(text);
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
                        if self.apply_input_outcome(outcome, content, &display) {
                            return true;
                        }
                    } else if !self.queued_messages.is_empty() {
                        // Empty submit with queued messages: pop and send the
                        // oldest one immediately.
                        let queued = self.queued_messages.remove(0);
                        if let Some(cmd) = crate::custom_commands::resolve(queued.trim()) {
                            let turn = self.begin_custom_command_turn(cmd);
                            self.agent = Some(turn);
                        } else {
                            let outcome = self.process_input(&queued);
                            let content = Content::text(queued.clone());
                            if self.apply_input_outcome(outcome, content, &queued) {
                                return true;
                            }
                        }
                    }
                }
                // Restore stash unless a dialog was opened (it will restore on close).
                if self.ui.focused_overlay().is_none() {
                    self.input.restore_stash();
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared event-routing preamble for both the idle and
    /// agent-running paths. Handles the routes that behave identically
    /// regardless of whether the agent is streaming: paste (drops the
    /// prompt prediction), resize, mouse (wheel, click, drag-select,
    /// scrollbar), `Ctrl-W` pane chord, transcript-window key
    /// routing when `Content` has focus, and dialog/overlay keys.
    ///
    /// Shared key/event preamble for both idle and running paths.
    ///
    /// Returns `Some(outcome)` when the event was fully handled;
    /// `None` when the caller should continue with path-specific
    /// logic (esc handling, keymap lookups, `PromptState`).
    ///
    /// Dispatch priority (first match wins):
    ///  1. Resize / mouse — structural events, always handled
    ///  2. Lua keymaps    — user-registered chords (`vim.keymap.set`)
    ///  3. Pane chords    — Ctrl-W window management
    ///  4. Cmdline `:`    — opens nvim-style command line (normal mode only)
    ///  5. Content focus  — transcript navigation when content pane is focused
    ///  6. Overlay keys   — notifications, btw block dismiss
    fn dispatch_common(&mut self, ev: &Event, t: &mut Timers) -> Option<EventOutcome> {
        if matches!(ev, Event::Paste(_)) {
            self.clear_prompt_completer();
        }
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        // Split-scoped ("buffer-local") Lua keymaps win over global
        // ones — matches nvim's buffer-local > global priority. The
        // focused split (prompt_input / transcript) is resolved inside
        // `Ui::dispatch_event` via the registered split layer-id map.
        // Used by `smelt.prompt.open_picker` to capture Enter / Esc /
        // arrows while a picker is active. Skipped when an overlay
        // owns focus — overlay-leaf dispatch happens upstream.
        if let Event::Key(k) = *ev {
            if self.ui.focused_overlay().is_none() {
                let lua = &self.core.lua;
                let mut lua_invoke =
                    |handle: ui::LuaHandle, win: ui::WinId, payload: &ui::Payload| {
                        lua.queue_invocation(handle, win, payload);
                    };
                let result = self.ui.dispatch_event(ui::Event::Key(k), &mut lua_invoke);
                if matches!(result, ui::Status::Consumed) {
                    self.flush_lua_callbacks();
                    return Some(EventOutcome::Noop);
                }
            }
        }

        // Lua-registered keymaps get first crack at key events, matching
        // nvim's `vim.keymap.set` priority. Unbound chords fall through
        // to the built-in keymap dispatcher.
        if let Event::Key(k) = *ev {
            if let Some(chord) = crate::lua::chord_string(k) {
                let vim_mode = self.current_vim_mode_label();
                let handled = self.core.lua.run_keymap(&chord, vim_mode.as_deref());
                if handled {
                    self.flush_lua_callbacks();
                    return Some(EventOutcome::Noop);
                }
            }
        }
        if let Some(outcome) = self.handle_pane_chord(ev, t) {
            return Some(outcome);
        }
        // `:` opens the cmdline from any window, unless in insert mode.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char(':'),
            modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
            ..
        }) = ev
        {
            let in_insert = match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    !self.input.vim_enabled() || self.vim_mode == ui::VimMode::Insert
                }
                crate::app::AppFocus::Content => false,
            };
            if !in_insert {
                self.open_cmdline();
                return Some(EventOutcome::Noop);
            }
        }
        if self.app_focus == crate::app::AppFocus::Content {
            return Some(self.handle_event_app_history(ev));
        }
        if let Some(outcome) = self.handle_overlay_keys(ev) {
            return Some(outcome);
        }
        None
    }

    fn handle_event_idle(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, t) {
            return outcome;
        }

        // Esc / double-Esc
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            let in_normal = !self.input.vim_enabled() || self.vim_mode != ui::VimMode::Insert;
            if in_normal {
                let double = t
                    .last_esc
                    .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
                if double {
                    t.last_esc = None;
                    let restore_mode = t.esc_vim_mode.take();

                    // Cancel in-flight compaction on double-Esc.
                    if self.working.is_compacting() {
                        self.compact_epoch += 1;
                        {
                            self.working.finish(TurnOutcome::Interrupted);
                        };
                        self.notify("compaction cancelled".into());
                        if restore_mode == Some(ui::VimMode::Insert) {
                            self.input
                                .set_vim_mode(&mut self.vim_mode, ui::VimMode::Insert);
                        }
                        return EventOutcome::Noop;
                    }

                    if self.user_turns().is_empty() {
                        return EventOutcome::Noop;
                    }
                    let line = if restore_mode == Some(ui::VimMode::Insert) {
                        "/rewind insert"
                    } else {
                        "/rewind"
                    };
                    super::commands::run_command(self, line);
                    return EventOutcome::Redraw;
                }
                // Single Esc in normal mode — start timer.
                t.last_esc = Some(Instant::now());
                t.esc_vim_mode = if self.input.vim_enabled() {
                    Some(self.vim_mode)
                } else {
                    None
                };
                if !self.input.vim_enabled() {
                    return EventOutcome::Noop;
                }
                // Vim normal mode — fall through to handle_event (resets pending op).
            } else {
                // Vim insert mode — start double-Esc timer, fall through so
                // handle_event processes the Esc and switches vim to normal.
                t.esc_vim_mode = Some(ui::VimMode::Insert);
                t.last_esc = Some(Instant::now());
            }
        } else {
            t.last_esc = None;
        }

        // Keymap lookup for app-level actions (before delegating to PromptState).
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ghost_text = self.prompt_completer_text();
            let ghost = ghost_text.is_some() && self.input.buf.is_empty();
            let ctx = self.input.key_context(false, ghost, self.vim_mode);

            // Dismiss ghost text on keys that affect input content.
            // Transparent actions (mode toggles, redraw, etc.) preserve it.
            if ghost {
                match keymap::lookup(code, modifiers, &ctx) {
                    Some(KeyAction::AcceptGhostText) => {
                        let full = self.take_prompt_completer().unwrap();
                        let line = full.lines().next().unwrap_or(&full).to_string();
                        let __mode = self.vim_mode;
                        crate::api::buf::replace(&mut self.input, line, None, __mode);
                        return EventOutcome::Redraw;
                    }
                    Some(
                        KeyAction::ToggleMode
                        | KeyAction::CycleReasoning
                        | KeyAction::Redraw
                        | KeyAction::ToggleStash,
                    ) => {}
                    _ => {
                        self.clear_prompt_completer();
                    }
                }
            }

            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                // Handle actions that need app-level context.
                match action {
                    KeyAction::Quit => {
                        return EventOutcome::Quit;
                    }
                    KeyAction::ClearBuffer => {
                        // Dismiss completer first, then clear buffer.
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Redraw;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        return EventOutcome::Redraw;
                    }
                    KeyAction::OpenHelp => {
                        super::commands::run_command(self, "/help");
                        return EventOutcome::Redraw;
                    }
                    _ => {
                        // Delegate to PromptState for editing/navigation actions.
                    }
                }
            }
        }

        // Delegate to PromptState::handle_event (menu, completer, vim, editing).
        let action = self.input.handle_event(
            ev,
            Some(&mut self.input_history),
            &mut self.vim_mode,
            &mut self.core.clipboard,
        );
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, t) {
            return outcome;
        }

        // Track last keypress for deferring permission dialogs.
        if matches!(ev, Event::Key(_)) {
            t.last_keypress = Some(Instant::now());
        }

        // Keymap lookup for Ctrl+C (agent-running variant).
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ctx = self.input.key_context(true, false, self.vim_mode);
            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::CancelAgent => {
                        // Dismiss completer first, then cancel.
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Noop;
                        }
                        self.queued_messages.clear();
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        // Dismiss completer first, then clear.
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Noop;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        self.queued_messages.clear();
                        return EventOutcome::Noop;
                    }
                    _ => {
                        // Other keymap actions — continue to Esc / input handling.
                    }
                }
            }
        }

        // Esc: dismiss any open picker completer first, then run agent-mode logic.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            let cur_mode = if self.input.vim_enabled() {
                Some(self.vim_mode)
            } else {
                None
            };
            match resolve_agent_esc(
                cur_mode,
                !self.queued_messages.is_empty(),
                &mut t.last_esc,
                &mut t.esc_vim_mode,
            ) {
                EscAction::VimToNormal => {
                    self.input
                        .handle_event(ev, None, &mut self.vim_mode, &mut self.core.clipboard);
                }
                EscAction::Unqueue => {
                    let mut combined = self.queued_messages.join("\n");
                    if !self.input.buf.is_empty() {
                        combined.push('\n');
                        combined.push_str(&self.input.buf);
                    }
                    let mode = self.vim_mode;
                    crate::api::buf::replace(&mut self.input, combined, None, mode);
                    self.queued_messages.clear();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        self.input.set_vim_mode(&mut self.vim_mode, mode);
                    }
                    return EventOutcome::CancelAgent;
                }
                EscAction::StartTimer => {}
            }
            return EventOutcome::Noop;
        }

        // Everything else → PromptState::handle_event (type-ahead with history).
        let input_action = self.input.handle_event(
            ev,
            Some(&mut self.input_history),
            &mut self.vim_mode,
            &mut self.core.clipboard,
        );
        match input_action {
            Action::Submit {
                mut content,
                mut display,
            } => {
                // Ingress: scrub secrets before queueing or running commands.
                self.redact_user_submission(&mut content, &mut display);
                let text = content.text_content();
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
            }
            Action::SubmitEmpty => {
                if !self.queued_messages.is_empty() {
                    return EventOutcome::InterruptWithQueued;
                }
            }
            Action::ToggleMode => {
                self.toggle_mode();
            }
            Action::Redraw => {}
            Action::CycleReasoning => {
                self.cycle_reasoning();
            }
            Action::EditInEditor => {
                self.edit_in_editor();
            }
            Action::CenterScroll => {
                self.input.win.pending_recenter = true;
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
            }
            Action::Noop | Action::Resize { .. } => {}
        }
        EventOutcome::Noop
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    /// Map an `input::Action` into an `EventOutcome`.
    fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit { content, display } => EventOutcome::Submit { content, display },
            Action::SubmitEmpty => EventOutcome::Noop,
            Action::ToggleMode => {
                self.toggle_mode();
                EventOutcome::Redraw
            }
            Action::CycleReasoning => {
                self.cycle_reasoning();
                EventOutcome::Redraw
            }
            Action::EditInEditor => {
                self.edit_in_editor();
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.input.win.pending_recenter = true;
                EventOutcome::Noop
            }
            Action::Resize {
                width: w,
                height: h,
            } => {
                self.handle_resize(w as u16, h as u16);
                EventOutcome::Noop
            }
            Action::Redraw => EventOutcome::Redraw,
            Action::NotifyError(msg) => {
                self.notify_error(msg);
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    fn edit_in_editor(&mut self) {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());

        let tmp = match tempfile::Builder::new().suffix(".md").tempfile() {
            Ok(f) => f,
            Err(e) => {
                self.notify_error(format!("tmpfile: {e}"));
                return;
            }
        };
        if let Err(e) = std::fs::write(tmp.path(), &self.input.buf) {
            self.notify_error(format!("write tmp: {e}"));
            return;
        }

        // Suspend TUI so the editor gets a normal terminal.
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(EnableLineWrap);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        terminal::disable_raw_mode().ok();

        let status = std::process::Command::new(&editor).arg(tmp.path()).status();

        // Resume TUI.
        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnterAlternateScreen);
        let _ = io::stdout().execute(DisableLineWrap);
        let _ = io::stdout().execute(EnableMouseCapture);

        match status {
            Ok(s) if s.success() => match std::fs::read_to_string(tmp.path()) {
                Ok(new) => {
                    let __mode = self.vim_mode;
                    crate::api::buf::replace(&mut self.input, new, None, __mode);
                }
                Err(e) => self.notify_error(format!("read tmp: {e}")),
            },
            Ok(s) => {
                self.notify_error(format!("{editor} exited with {s}"));
            }
            Err(e) => {
                self.notify_error(format!("{editor}: {e}"));
            }
        }
    }

    fn handle_resize(&mut self, w: u16, h: u16) {
        if w == self.last_width && h == self.last_height {
            return;
        }
        let width_changed = w != self.last_width;
        self.last_width = w;
        self.last_height = h;
        let _ = self
            .ui
            .dispatch_event(ui::Event::Resize(w, h), &mut |_, _, _| {});
        if width_changed {
            self.invalidate_for_width(w);
        }
    }

    /// Handle overlay keys (notification dismiss).
    /// Returns `Some(EventOutcome)` if the event was consumed.
    fn handle_overlay_keys(&mut self, ev: &Event) -> Option<EventOutcome> {
        if matches!(ev, Event::Key(_)) && self.notification.is_some() {
            self.dismiss_notification();
        }

        None
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    pub(super) fn process_input(&mut self, input: &str) -> InputOutcome {
        if input.is_empty() {
            return InputOutcome::Continue;
        }

        let trimmed = input.trim();
        self.input_history.push(input.to_string());

        // Skip shell escape for pasted content
        let is_from_paste = self.input.skip_shell_escape();
        match super::commands::run_command(self, trimmed) {
            CommandAction::Exec(rx, kill) => return InputOutcome::Exec(rx, kill),
            CommandAction::Continue => {}
        }
        if trimmed.starts_with('/') {
            if let Some(cmd) = crate::custom_commands::resolve(trimmed) {
                return InputOutcome::CustomCommand(Box::new(cmd));
            }
            if crate::completer::Completer::is_command(trimmed) {
                return InputOutcome::Continue;
            }
        }
        // Skip starting agent for shell escapes, but NOT for pasted content
        if trimmed.starts_with('!') && !is_from_paste {
            return InputOutcome::Continue;
        }

        // Publish input_submit so Lua plugins can observe/log.
        self.core
            .cells
            .set_dyn("input_submit", std::rc::Rc::new(trimmed.to_string()));
        self.drain_cells_pending();
        self.flush_lua_callbacks();

        InputOutcome::StartAgent
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    /// Viewport rows available for the content pane. Uses the prompt's
    /// actual rendered height from the previous frame plus the 1-row
    /// gap, so multi-line prompts (and completion menus) don't cause
    /// the scroll math to overshoot.
    pub(super) fn viewport_rows_estimate(&self) -> u16 {
        self.layout.viewport_rows().max(1)
    }

    /// Close an overlay leaf and clean up its picker / Lua-callback
    /// registrations. The leaf id is whatever was returned from the
    /// overlay's open path (picker / cmdline / notification / dialog);
    /// `Ui::win_close` cascades to overlay close when the leaf belongs
    /// to one.
    pub(crate) fn close_overlay_leaf(&mut self, win_id: ui::WinId) {
        crate::picker::forget(self, win_id);
        for id in self.win_close(win_id) {
            self.core.lua.remove_callback(id);
        }
    }

    /// Close the focused overlay if it doesn't block the agent (e.g.
    /// Ps, Permissions, Resume). Used before opening a blocking
    /// dialog so only one is visible at a time. Fires
    /// `WinEvent::Dismiss` on the overlay's root leaf so the dialog's
    /// callbacks can flush any pending state (e.g. Permissions syncs
    /// its edits before close).
    pub(super) fn close_focused_non_blocking_overlay(&mut self) {
        let Some(overlay_id) = self.ui.focused_overlay() else {
            return;
        };
        let Some(overlay) = self.ui.overlay(overlay_id) else {
            return;
        };
        if overlay.blocks_agent {
            return;
        }
        let Some(root) = overlay.layout.leaves_in_order().into_iter().next() else {
            return;
        };
        let lua = &self.core.lua;
        let mut lua_invoke = |handle: ui::LuaHandle, win: ui::WinId, payload: &ui::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            root,
            ui::WinEvent::Dismiss,
            ui::Payload::None,
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// True when the focused overlay pauses engine-event drain
    /// (Confirm / Question / Lua dialogs gating a pending tool call).
    pub(super) fn focused_overlay_blocks_agent(&self) -> bool {
        self.ui
            .focused_overlay()
            .and_then(|id| self.ui.overlay(id))
            .is_some_and(|o| o.blocks_agent)
    }

    /// Snap the transcript cursor to the nearest selectable cell.
    /// Called after every cursor motion to skip non-selectable gutters
    /// and padding now that the cursor operates in display-text space.
    pub(super) fn snap_transcript_cursor(&mut self) {
        let rows = self.full_transcript_display_text(self.core.config.settings.show_thinking);
        let snapped = self.snap_cpos_to_selectable(
            &rows,
            self.transcript_window.cpos,
            self.core.config.settings.show_thinking,
        );
        if snapped != self.transcript_window.cpos {
            self.transcript_window.cpos = snapped;
            let viewport = self.viewport_rows_estimate();
            self.transcript_window.resync(&rows, viewport);
        }
    }
}
