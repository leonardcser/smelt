use crate::app::{CommandAction, EventOutcome, InputOutcome, Timers, TuiApp};

use crate::input::{resolve_agent_esc, Action, EscAction};
use crate::keymap::{self, KeyAction};
use crate::smelt_term::UiHost;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEvent},
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use protocol::Content;
use std::io;
use std::time::{Duration, Instant};

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Returns `true` if the app should quit.
    pub(crate) fn dispatch_terminal_event(&mut self, ev: Event, t: &mut Timers) -> bool {
        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            if self.term_focused != focused {
                self.term_focused = focused;
            }
            return false;
        }

        // Global chords fire before focus-specific routing so no handler can swallow them.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = &ev
        {
            // Skip when an overlay or cmdline is focused — they get first dibs.
            if self.ui.focused_overlay().is_none() && self.well_known.cmdline.is_none() {
                let ctx = self
                    .input
                    .key_context(self.agent.is_some(), false, self.vim_mode);
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
                let lua = &self.lua;
                let mut lua_invoke =
                    |handle: crate::smelt_term::LuaHandle,
                     win: crate::smelt_term::WinId,
                     payload: &crate::smelt_term::Payload| {
                        lua.queue_invocation(handle, win, payload);
                    };
                let status = self
                    .ui
                    .dispatch_event(crate::smelt_term::Event::Key(k), &mut lua_invoke);
                if matches!(status, crate::smelt_term::Status::Ignored) {
                    // Global Lua keymaps get the next shot, before the vim viewer fallthrough,
                    // so unbound chords aren't swallowed by Insert-mode passthrough on read-only viewers.
                    let mut consumed = false;
                    if let Some(chord) = crate::lua::chord_string(k) {
                        let vim_mode = self.current_vim_mode_label();
                        use smelt_core::lua::runtime::KeymapResult;
                        if matches!(
                            self.lua.run_keymap(&chord, vim_mode.as_deref(), None),
                            KeymapResult::Consumed
                        ) {
                            consumed = true;
                        }
                    }
                    // Vim-enabled overlay leaves share the same `Window::handle_key` path
                    // as transcript/prompt — gives viewer panels (help, stats, plugins)
                    // vim navigation + selection + yank without per-panel keymap wiring.
                    if !consumed {
                        let _ = self.dispatch_overlay_viewer_key(k);
                    }
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

        let outcome = if self.agent.is_some() {
            self.handle_event_running(ev, t)
        } else {
            self.handle_event_idle(ev, t)
        };

        // Notify Lua subscribers if the prompt buffer changed (drives filter-as-you-type pickers).
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
                // Save queued messages before cancel — the cancel path dumps them into the input buffer.
                let remaining = std::mem::take(&mut self.queued_messages);
                self.discard_turn(true);
                self.queued_messages = remaining;
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
                self.redact_user_submission(&mut content, &mut display);
                // Queue during compaction so messages run against the compacted history.
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
                        self.apply_input_outcome(outcome, content, &display);
                    } else if !self.queued_messages.is_empty() {
                        // Empty submit: send the oldest queued message immediately.
                        let queued = self.queued_messages.remove(0);
                        let outcome = self.process_input(&queued);
                        let content = Content::text(queued.clone());
                        self.apply_input_outcome(outcome, content, &queued);
                    }
                }
                // Don't restore stash if a dialog opened — it restores on close.
                if self.ui.focused_overlay().is_none() {
                    self.input.restore_stash();
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared preamble for idle and agent-running paths.
    ///
    /// Returns `Some(outcome)` when consumed; `None` to continue with path-specific logic.
    ///
    /// Dispatch priority: resize/mouse → Lua keymaps → pane chords → cmdline `:` → content focus → overlay keys.
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
        // Buffer-local Lua keymaps win over global (nvim priority). Skipped when an overlay
        // owns focus — overlay-leaf dispatch happens upstream.
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
        // Multi-key sequences accumulate in `t.pending_chord` until exact-matched or timed out.
        if let Event::Key(k) = *ev {
            if let Some(token) = crate::lua::chord_string(k) {
                let vim_mode = self.current_vim_mode_label();
                use smelt_core::lua::runtime::KeymapResult;

                // Single-key first — allocation-free common case.
                match self.lua.run_keymap(&token, vim_mode.as_deref(), None) {
                    KeymapResult::Consumed => {
                        t.pending_chord = None;
                        self.flush_lua_callbacks();
                        return Some(EventOutcome::Noop);
                    }
                    KeymapResult::PassThrough | KeymapResult::NoBinding => {
                        self.flush_lua_callbacks();
                    }
                }

                // Multi-key chord: drop stale pending sequence, then append and match.
                let now = Instant::now();
                if let Some(pending) = &t.pending_chord {
                    if now.duration_since(pending.started)
                        >= Duration::from_millis(crate::app::CHORD_TIMEOUT_MS)
                    {
                        t.pending_chord = None;
                    }
                }
                if t.pending_chord.is_none() {
                    t.pending_chord = Some(crate::app::PendingChord {
                        tokens: Vec::new(),
                        started: now,
                        vim_mode_at_start: if self.input.vim_enabled() {
                            Some(self.vim_mode)
                        } else {
                            None
                        },
                    });
                }
                let (mut tokens, started, vim_mode_at_start) = {
                    let p = t.pending_chord.take().unwrap();
                    (p.tokens, p.started, p.vim_mode_at_start)
                };
                tokens.push(token);

                let outcome = loop {
                    let seq: String = tokens.concat();
                    let has_longer = self.lua.chord_has_longer(&seq, vim_mode.as_deref());
                    if tokens.len() > 1 {
                        let ctx_pairs: Vec<(&str, String)> = vec![(
                            "vim_mode_at_chord_start",
                            vim_mode_at_start
                                .map(|m| format!("{m:?}"))
                                .unwrap_or_default(),
                        )];
                        let res = self.lua.run_keymap(
                            &seq,
                            vim_mode.as_deref(),
                            Some(ctx_pairs.as_slice()),
                        );
                        match res {
                            KeymapResult::Consumed => {
                                self.flush_lua_callbacks();
                                return Some(EventOutcome::Noop);
                            }
                            KeymapResult::PassThrough => {
                                self.flush_lua_callbacks();
                                break has_longer;
                            }
                            KeymapResult::NoBinding => {}
                        }
                    }
                    if has_longer {
                        break true;
                    }
                    if tokens.is_empty() {
                        break false;
                    }
                    tokens.remove(0);
                    if tokens.is_empty() {
                        break false;
                    }
                };
                if outcome && !tokens.is_empty() {
                    t.pending_chord = Some(crate::app::PendingChord {
                        tokens,
                        started,
                        vim_mode_at_start,
                    });
                }
            }
        }
        if let Some(outcome) = self.handle_pane_chord(ev, t) {
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
                    !self.input.vim_enabled() || self.vim_mode == crate::smelt_term::VimMode::Insert
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

        // Single Esc: vim falls through to PromptState::handle_event; non-vim is a no-op.
        // Esc-Esc lives in the chord registry (esc_chord.lua).
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) && !self.input.vim_enabled()
        {
            return EventOutcome::Noop;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ghost_text = self.prompt_completer_text();
            let ghost = ghost_text.is_some() && self.input.win.text.is_empty();
            let ctx = self.input.key_context(false, ghost, self.vim_mode);

            // Editing keys dismiss ghost text; transparent actions (mode toggles, redraw) preserve it.
            if ghost {
                match keymap::lookup(code, modifiers, &ctx) {
                    Some(KeyAction::AcceptGhostText) => {
                        let full = self.take_prompt_completer().unwrap();
                        let line = full.lines().next().unwrap_or(&full).to_string();
                        let __mode = self.vim_mode;
                        self.input.replace_text(line, None, __mode);
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
                match action {
                    KeyAction::Quit => {
                        return EventOutcome::Quit;
                    }
                    KeyAction::ClearBuffer => {
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Redraw;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        return EventOutcome::Redraw;
                    }
                    _ => {}
                }
            }
        }

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

        // Record last keypress for deferred permission dialogs.
        if matches!(ev, Event::Key(_)) {
            t.last_keypress = Some(Instant::now());
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ctx = self.input.key_context(true, false, self.vim_mode);
            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::CancelAgent => {
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Noop;
                        }
                        self.queued_messages.clear();
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        if self.input.completer.is_some() {
                            self.input.close_completer();
                            return EventOutcome::Noop;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        self.queued_messages.clear();
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
                    if !self.input.win.text.is_empty() {
                        combined.push('\n');
                        combined.push_str(&self.input.win.text);
                    }
                    let mode = self.vim_mode;
                    self.input.replace_text(combined, None, mode);
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
            Action::Redraw => {}
            Action::EditInEditor => {
                self.edit_in_editor();
            }
            Action::CenterScroll => {
                self.input.win.pending_recenter = true;
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
            }
            Action::Noop => {}
        }
        EventOutcome::Noop
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit { content, display } => EventOutcome::Submit { content, display },
            Action::SubmitEmpty => EventOutcome::Noop,
            Action::EditInEditor => {
                self.edit_in_editor();
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.input.win.pending_recenter = true;
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
        if let Err(e) = std::fs::write(tmp.path(), &self.input.win.text) {
            self.notify_error(format!("write tmp: {e}"));
            return;
        }

        // Suspend TUI for the editor.
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(EnableLineWrap);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        terminal::disable_raw_mode().ok();

        let status = std::process::Command::new(&editor).arg(tmp.path()).status();

        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnterAlternateScreen);
        let _ = io::stdout().execute(DisableLineWrap);
        let _ = io::stdout().execute(EnableMouseCapture);

        match status {
            Ok(s) if s.success() => match std::fs::read_to_string(tmp.path()) {
                Ok(new) => {
                    let __mode = self.vim_mode;
                    self.input.replace_text(new, None, __mode);
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
            .dispatch_event(crate::smelt_term::Event::Resize(w, h), &mut |_, _, _| {});
        if width_changed {
            self.invalidate_for_width(w);
        }
    }

    fn handle_overlay_keys(&mut self, ev: &Event) -> Option<EventOutcome> {
        if matches!(ev, Event::Key(_)) && self.notification.is_some() {
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

        // `:` is a vim-style alias for `/` — normalize before command lookup.
        let dispatch_input = if let Some(rest) = trimmed.strip_prefix(':') {
            format!("/{rest}")
        } else {
            trimmed.to_string()
        };

        match crate::commands::run_command(self, &dispatch_input) {
            CommandAction::Exec(handle) => return InputOutcome::Exec(handle),
            CommandAction::Continue => {}
        }
        if dispatch_input.starts_with('/')
            && crate::completer::Completer::is_command(&dispatch_input)
        {
            return InputOutcome::Continue;
        }
        // Shell escapes (`!cmd`) skip agent start, but pasted content starting with `!` does not.
        if trimmed.starts_with('!') && !is_from_paste {
            return InputOutcome::Continue;
        }

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
        for id in self.win_close(win_id) {
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

    /// Forward a key to the focused overlay-leaf Window when it's vim-enabled and no keymap
    /// absorbed the key. Forces Normal mode — Insert would passthrough `j`/`k` without moving.
    /// Returns `true` if the Window claimed the event.
    pub(crate) fn dispatch_overlay_viewer_key(&mut self, k: KeyEvent) -> bool {
        let win = match self.ui.focus() {
            Some(w) => w,
            None => return false,
        };
        let (vim_enabled, buf_id, viewport_rows) = match self.ui.win(win) {
            Some(w) if w.vim_enabled => {
                (true, w.buf, w.viewport.map(|v| v.rect.height).unwrap_or(0))
            }
            _ => return false,
        };
        if !vim_enabled || viewport_rows == 0 {
            return false;
        }
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        if rows.is_empty() {
            return false;
        }
        if self.vim_mode == crate::smelt_term::VimMode::Insert {
            self.vim_mode = crate::smelt_term::VimMode::Normal;
        }
        let win_mut = match self.ui.win_mut(win) {
            Some(w) => w,
            None => return false,
        };
        let status = win_mut.handle_key(
            k,
            &rows,
            viewport_rows,
            &mut self.vim_mode,
            &mut self.core.clipboard,
        );
        matches!(status, crate::smelt_term::Status::Consumed)
    }
}
