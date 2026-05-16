use crate::app::{CommandAction, EventOutcome, InputOutcome, TuiApp};

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
use std::time::Instant;

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Returns `true` if the app should quit.
    pub(crate) fn dispatch_terminal_event(&mut self, ev: Event) -> bool {
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
                let pctx = crate::input::prompt_ctx_ref(&self.ui);
                let ctx = self.input.key_context(pctx, self.agent.is_some(), false);
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

        let outcome = if self.agent.is_some() {
            self.handle_event_running(ev)
        } else {
            self.handle_event_idle(ev)
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
                        self.queued_messages.push(text.into_owned());
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
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    self.input.restore_stash(&mut pctx);
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
    fn dispatch_common(&mut self, ev: &Event) -> Option<EventOutcome> {
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
        // Multi-key sequences accumulate in `self.timers.pending_chord` until exact-matched or timed out.
        if let Event::Key(k) = *ev {
            if let Some(token) = crate::lua::chord_string(k) {
                let vim_mode = self.current_vim_mode_label();
                use smelt_core::lua::runtime::KeymapResult;

                // Single-key first — allocation-free common case.
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
                let now = Instant::now();
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

        // Single Esc: vim falls through to PromptState::handle_event; non-vim is a no-op.
        // Esc-Esc lives in the chord registry (esc_chord.lua).
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) && !self.input.vim_enabled(self.prompt_win())
        {
            return EventOutcome::Noop;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ghost_text = self.prompt_completer_text();
            let ghost = ghost_text.is_some() && self.prompt_buf().source().is_empty();
            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.input.key_context(pctx_ref, false, ghost);

            // Editing keys dismiss ghost text; transparent actions (mode toggles, redraw) preserve it.
            if ghost {
                match keymap::lookup(code, modifiers, &ctx) {
                    Some(KeyAction::AcceptGhostText) => {
                        let full = self.take_prompt_completer().unwrap();
                        let line = full.lines().next().unwrap_or(&full).to_string();
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.replace_text(&mut pctx, line, None);
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
                        self.timers.last_ctrlc = Some(Instant::now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.clear(&mut pctx);
                        return EventOutcome::Redraw;
                    }
                    _ => {}
                }
            }
        }

        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
            &mut self.core.clipboard,
        );
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev) {
            return outcome;
        }

        // Record last keypress for deferred permission dialogs.
        if matches!(ev, Event::Key(_)) {
            self.timers.last_keypress = Some(Instant::now());
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.input.key_context(pctx_ref, true, false);
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
                        self.timers.last_ctrlc = Some(Instant::now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.clear(&mut pctx);
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
            let cur_mode = if self.input.vim_enabled(self.prompt_win()) {
                Some(self.prompt_win().vim_mode)
            } else {
                None
            };
            match resolve_agent_esc(
                cur_mode,
                !self.queued_messages.is_empty(),
                &mut self.timers.last_esc,
                &mut self.timers.esc_vim_mode,
            ) {
                EscAction::VimToNormal => {
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    self.input
                        .handle_event(&mut pctx, ev, None, &mut self.core.clipboard);
                }
                EscAction::Unqueue => {
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    let mut combined = self.queued_messages.join("\n");
                    if !pctx.buf.source().is_empty() {
                        combined.push('\n');
                        combined.push_str(pctx.buf.source());
                    }
                    self.input.replace_text(&mut pctx, combined, None);
                    self.queued_messages.clear();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        let win = self
                            .ui
                            .win_mut(crate::app::PROMPT_WIN)
                            .expect("prompt window");
                        self.input.set_vim_mode(win, mode);
                    }
                    return EventOutcome::CancelAgent;
                }
                EscAction::StartTimer => {}
            }
            return EventOutcome::Noop;
        }

        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let input_action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
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
                    self.queued_messages.push(text.into_owned());
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
                self.prompt_win_mut().pending_recenter = true;
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
                self.prompt_win_mut().pending_recenter = true;
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
        if let Err(e) = std::fs::write(tmp.path(), self.prompt_buf().source()) {
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
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    self.input.replace_text(&mut pctx, new, None);
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
        if dispatch_input.starts_with('/') && smelt_core::commands::is_command(&dispatch_input) {
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
    ///   1. Specific keymap on the focused leaf (`win_set_keymap`)
    ///   2. Global Lua keymap (`smelt.keymap.set("", chord, fn)`)
    ///   3. Per-window catch-all fallback (`win_set_key_fallback`)
    ///   4. Vim viewer keys on the focused leaf
    ///   5. Modal dismiss for bare Esc / Ctrl-C
    ///
    /// Putting global keymaps between tiers 1 and 3 lets a site-wide chord
    /// like `?` → /help win over a dialog input's blanket printable-char
    /// fallback, without each leaf needing a bespoke carve-out.
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

        // Tier 2: global Lua keymap (single-chord lookup only — overlays
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

        // Tier 3: per-window catch-all fallback (dialog inputs, etc.).
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

        // Tier 4: vim viewer keys for vim-enabled read-only overlay leaves.
        if self.dispatch_overlay_viewer_key(k) {
            return Status::Consumed;
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

    /// Forward a key to the focused leaf Window. Two distinct cases:
    ///   1. PageUp / PageDown — work on any focused window with a viewport,
    ///      vim or not. Lets non-vim users still scroll a code/help/diff view.
    ///   2. Everything else — only routes when the focused window is
    ///      `vim_enabled`. Esc in idle Normal mode falls through so the
    ///      modal-dismiss tier can close the overlay; vim's half/full-page
    ///      Ctrl- scroll is wired here so the host owns viewport arithmetic.
    pub(crate) fn dispatch_overlay_viewer_key(&mut self, k: KeyEvent) -> bool {
        let win = match self.ui.focus() {
            Some(w) => w,
            None => return false,
        };
        let (vim_enabled, buf_id, viewport_rows) = match self.ui.win(win) {
            Some(w) => (
                w.vim_enabled,
                w.buf,
                w.viewport.map(|v| v.rect.height).unwrap_or(0),
            ),
            None => return false,
        };
        if viewport_rows == 0 {
            return false;
        }
        let (win_mut, buf) = self.ui.win_and_buf_mut(win, buf_id);
        let win_mut = match win_mut {
            Some(w) => w,
            None => return false,
        };
        let buf = match buf {
            Some(b) => b,
            None => return false,
        };
        if buf.lines().is_empty() {
            return false;
        }
        // (1) Page motion: always available. For vim windows, only fire
        // outside Insert mode so typing a literal PageUp inside an editable
        // input still inserts/edits — overlay viewers are read-only so this
        // is a no-op for them, but the same path serves future editable
        // vim leaves too. The `Ctrl-` variants must stay gated on Normal
        // because vim Insert handlers may want them later.
        let in_insert = vim_enabled && win_mut.vim_mode == crate::smelt_term::VimMode::Insert;
        if !in_insert {
            let is_ctrl_motion = k
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL);
            let allow = !is_ctrl_motion
                || !vim_enabled
                || win_mut.vim_mode == crate::smelt_term::VimMode::Normal;
            if allow {
                if let Some(d) = crate::smelt_term::vim::page_motion_delta(k, viewport_rows) {
                    win_mut.move_cursor_by_lines(buf, d, viewport_rows);
                    let max_scroll = (buf.lines().len() as u16).saturating_sub(viewport_rows);
                    win_mut.follow_tail = win_mut.scroll_top >= max_scroll;
                    return true;
                }
            }
        }
        if !vim_enabled {
            return false;
        }
        // Esc in idle Normal mode is a no-op for the viewer — let it fall
        // through so the modal-dismiss tier can close the overlay. When
        // Visual mode is active or a pending sequence is buffered, Esc still
        // belongs to vim (exit Visual / cancel pending) and stays consumed.
        if k.code == KeyCode::Esc
            && win_mut.vim_mode == crate::smelt_term::VimMode::Normal
            && win_mut.vim_state.is_idle()
        {
            return false;
        }
        let status = win_mut.handle_key(buf, k, &mut self.core.clipboard);
        let max_scroll = (buf.lines().len() as u16).saturating_sub(viewport_rows);
        win_mut.follow_tail = win_mut.scroll_top >= max_scroll;
        matches!(status, crate::smelt_term::Status::Consumed)
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
