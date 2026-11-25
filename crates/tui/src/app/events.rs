use crate::app::{
    EventOutcome, InputOutcome, PendingChordPolicy, PromptWorkState, QueueStage, QueuedInput,
    TuiApp,
};

use crate::app::lua_bridge::{KeymapChain, KeymapReach};
use crate::input::{Action, PromptSubmission};
use crate::smelt_edit::UiHost;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const ESC_TOKEN: &str = "<Esc>";
/// How long a second Ctrl+C after a prompt clear counts as the same press.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlobalLuaKeymapRoute {
    Dispatch,
    ObservePrefix,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatefulViewerKeyDispatch {
    NotApplicable,
    Consumed,
    Passthrough {
        vim_dispatched_for: crate::smelt_edit::WinId,
        global_keymap_checked: bool,
    },
}

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Dispatch `ev`, one of a batch of terminal events handled before the
    /// next frame. An earlier event of the batch may have left a transcript
    /// projection to a target pending, such as the cursor move of `G`; it is
    /// resolved first, so `ev` reads the cursor and view it produced.
    /// A projection that waits on hydration stays pending, and navigation
    /// continues from its target instead. Returns `true` if the app should quit.
    pub(crate) fn dispatch_batched_terminal_event<W: std::io::Write>(
        &mut self,
        ev: Event,
        out: &mut W,
    ) -> bool {
        if self
            .conversation
            .transcript()
            .has_pending_target_projection()
        {
            self.dispatch_ui_window_events(false);
            self.render_normal_to(out);
        }
        self.dispatch_terminal_event(ev)
    }

    /// Returns `true` if the app should quit.
    pub(crate) fn dispatch_terminal_event(&mut self, ev: Event) -> bool {
        if matches!(&ev, Event::Key(key) if key.kind == KeyEventKind::Release) {
            return false;
        }

        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            self.platform.set_terminal_focus(focused);
            if focused {
                // tmux drops graphics passthrough from hidden panes, so images uploaded
                // while unfocused may never have reached the terminal. Repaint and resend.
                self.ui.force_redraw();
            } else {
                self.ui.cancel_pointer_interaction();
            }
            // Paints read focus from `ctx.focused`.
            self.request_urgent_render();
            return false;
        }

        if matches!(ev, Event::Key(_) | Event::Paste(_)) {
            self.ui.finish_pointer_interaction_for_keyboard();
        }

        // A pending pane chord owns its follow-up before local shortcuts or Vim.
        if self.timers.pending_pane_chord.is_some() {
            if let Some(outcome) = self.handle_pane_chord(&ev) {
                return self.apply_event_outcome(outcome);
            }
        }

        // Ctrl+C kills a focused shell-output command before modal dismiss sees it.
        if self.shell_panel_is_focused()
            && self
                .overlays
                .execution_uses_sink(crate::commands::ShellSink::Overlay)
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            self.overlays.cancel_execution();
            return false;
        }

        if self.shell_panel_is_focused()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Esc | KeyCode::Char('q'),
                    modifiers: KeyModifiers::NONE,
                    ..
                })
            )
        {
            self.close_shell_panel_and_stop_job();
            return false;
        }

        // Overlay/modal focus: route keys through the focused leaf's keymap registry.
        // Mouse events fall through so wheel/scrollbar logic runs over the overlay rect.
        if self.ui.focused_overlay().is_some() || self.ui.active_modal().is_some() {
            if let Event::Resize(w, h) = ev {
                self.handle_resize(w, h);
                return false;
            }
            if matches!(&ev, Event::Key(_) | Event::Paste(_)) {
                // Cmdline owns its input end-to-end: text edit,
                // history nav, completer cycling, and command exec
                // all need `&mut TuiApp`, so the overlay leaf has no
                // recipe and `cmdline_handle_event` runs every key/paste
                // before the generic compositor dispatch. Returns
                // `Some(true)` only when the run command resolved to
                // Quit (propagated as the loop's quit signal).
                if self.cmdline_is_focused() {
                    if let Some(quit) = self.cmdline_handle_event(ev) {
                        return quit;
                    }
                    // Swallow unclaimed input so split keymaps don't fire over an open cmdline.
                    return false;
                }
                if let Event::Paste(data) = ev {
                    if matches!(
                        self.run_paste_fallback(data),
                        crate::smelt_edit::Status::Consumed
                    ) {
                        self.flush_lua_callbacks();
                    }
                    return false;
                }
                let Event::Key(k) = ev else { unreachable!() };
                if self.handle_focused_search_key(k) {
                    return false;
                }
                if self.handle_search_open_before_window_dispatch(k) {
                    return false;
                }
                if self.try_open_cmdline_for_key(k) {
                    return false;
                }
                if matches!(self.run_key_cascade(k), crate::smelt_edit::Status::Consumed) {
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
        if self.overlays.execution_is_running()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            self.overlays.cancel_execution();
            return false;
        }

        let outcome = if self.prompt_work_state().is_busy() {
            self.handle_event_running(ev)
        } else {
            self.handle_event_idle(ev)
        };

        // Notify Lua subscribers if the prompt buffer changed (drives filter-as-you-type pickers).
        self.emit_prompt_text_changed_if_dirty();
        self.emit_split_resizes();

        let redraw = self.apply_event_outcome(outcome);
        self.save_deferred_session_batch_if_ready();
        redraw
    }

    pub(super) fn apply_event_outcome(&mut self, outcome: EventOutcome) -> bool {
        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                self.discard_turn(crate::app::TurnEnd::Cancelled);
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
                self.discard_turn(crate::app::TurnEnd::Cancelled);
                false
            }
            EventOutcome::InterruptWithQueued => {
                self.interrupt_with_next_queued();
                false
            }
            EventOutcome::ContinueTurn => {
                let sent_at_ms = engine::clock::unix_time_ms(self.core.clock.as_ref());
                let turn = self.begin_agent_turn("", protocol::Content::text(""), sent_at_ms);
                self.conversation.set_active(turn);
                false
            }
            EventOutcome::Exec(handle) => {
                self.overlays.install_execution(handle);
                false
            }
            EventOutcome::Submit {
                mut submission,
                cut,
            } => {
                let sent_at_ms = engine::clock::unix_time_ms(self.core.clock.as_ref());
                self.clear_prompt_prediction();
                let accepted = match self.prompt_work_state() {
                    PromptWorkState::TurnActive
                    | PromptWorkState::BackgroundBusy
                    | PromptWorkState::Paused => {
                        // Queue while an active turn or background plugin owns the
                        // input lifecycle so messages run against the next stable state.
                        if submission.content.is_empty() {
                            false
                        } else {
                            let accepted = self.queue_explicit_submission(
                                QueuedInput::from_prompt(submission, sent_at_ms),
                                QueueStage::Turn,
                            );
                            if accepted {
                                self.commit_prompt_submission(cut);
                            }
                            accepted
                        }
                    }
                    PromptWorkState::Idle => {
                        let text = submission.content.text_content().into_owned();
                        let has_images = submission.content.image_count() > 0;
                        if !text.is_empty() || has_images {
                            let outcome = if has_images && text.trim().is_empty() {
                                InputOutcome::StartAgent
                            } else {
                                self.process_input(&text)
                            };
                            let accepted = match outcome {
                                InputOutcome::StartAgent => {
                                    let placement = submission
                                        .replay
                                        .as_ref()
                                        .map(crate::input::PromptReplay::image_placement);
                                    let replay = submission.replay.take().unwrap_or_else(|| {
                                        crate::input::PromptReplay {
                                            source: submission.display.clone(),
                                            attachments: Vec::new(),
                                            image_indices: Vec::new(),
                                        }
                                    });
                                    match self.begin_agent_turn_with_placement(
                                        &submission.display,
                                        submission.content,
                                        sent_at_ms,
                                        placement,
                                    ) {
                                        Some(turn) => {
                                            self.commit_prompt_submission(cut);
                                            self.conversation.set_active(Some(turn));
                                            true
                                        }
                                        None if self.turn_submission_is_pending() => {
                                            let recovery = self.prompt.prepare_submission_recovery(
                                                crate::input::prompt_ctx_ref(&self.ui),
                                                &cut,
                                                replay,
                                            );
                                            self.commit_prompt_submission(cut);
                                            self.preserve_pending_prompt_submission(recovery);
                                            true
                                        }
                                        None => false,
                                    }
                                }
                                outcome => {
                                    self.commit_prompt_submission(cut);
                                    self.apply_input_outcome(
                                        outcome,
                                        submission.content,
                                        &submission.display,
                                        sent_at_ms,
                                    );
                                    false
                                }
                            };
                            if self.pending_quit {
                                return true;
                            }
                            accepted
                        } else {
                            let outcome = self.handle_empty_submit();
                            return self.apply_event_outcome(outcome);
                        }
                    }
                };
                if accepted {
                    self.restore_stash_after_submit();
                }
                false
            }
        }
    }

    /// Submits the prompt for Lua as Enter (`Turn`) or steer (`Request`) does,
    /// such as from a completer or a rebound key. `text` replaces the prompt's
    /// text first. Steering an empty prompt does nothing.
    pub(crate) fn submit_prompt(&mut self, text: Option<String>, stage: QueueStage) {
        let action = {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            if let Some(text) = text {
                self.prompt.replace_text(&mut pctx, text);
            }
            self.prompt.submit_action(&mut pctx, stage)
        };
        let outcome = if self.prompt_work_state().is_busy() {
            self.dispatch_running_input_action(action)
        } else {
            self.dispatch_input_action(action)
        };
        self.finish_outside_dispatch(outcome);
    }

    /// Settle `outcome` of work Lua started outside key dispatch, as the
    /// event loop does after a key.
    pub(super) fn finish_outside_dispatch(&mut self, outcome: EventOutcome) {
        self.emit_prompt_text_changed_if_dirty();
        self.emit_split_resizes();
        if matches!(outcome, EventOutcome::Quit) {
            self.pending_quit = true;
        }
        self.apply_event_outcome(outcome);
    }

    fn commit_prompt_submission(&mut self, cut: crate::input::PromptCut) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt.apply_cut(&mut pctx, cut);
        // Publish the edit before the submission runs, so a picker it opens
        // does not take the cleared prompt for its query.
        let _ = self.queue_prompt_text_changed_if_dirty();
    }

    /// Brings the stashed draft back once a sent message empties the prompt,
    /// unless the submission opened a dialog that owns the prompt.
    fn restore_stash_after_submit(&mut self) {
        if self.ui.active_modal().is_some() {
            return;
        }
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        if pctx.buf.source().is_empty() && pctx.buf.attachments.is_empty() {
            self.prompt.restore_stash(&mut pctx);
        }
    }

    fn handle_focused_search_key(&mut self, k: KeyEvent) -> bool {
        let Some(win) = self.ui.focus() else {
            return false;
        };
        self.handle_search_key_for_target(win, k)
    }

    fn handle_search_open_before_window_dispatch(&mut self, k: KeyEvent) -> bool {
        self.try_open_search_for_key(k)
    }

    fn focused_transient_owns_printable_keys(&self) -> bool {
        if self.ui.focused_overlay().is_none() && self.ui.focused_modal().is_none() {
            return false;
        }
        let Some(win) = self.ui.focus().and_then(|win| self.ui.win(win)) else {
            return true;
        };
        !win.surface().is_readonly_text()
            || win.vim_enabled() && win.vim_mode() == crate::smelt_edit::VimMode::Insert
    }

    pub(crate) fn try_open_cmdline_for_key(&mut self, k: KeyEvent) -> bool {
        if !matches!(
            (k.code, k.modifiers),
            (KeyCode::Char(':'), KeyModifiers::NONE | KeyModifiers::SHIFT)
        ) || self.focused_transient_owns_printable_keys()
        {
            return false;
        }

        self.open_cmdline();
        true
    }

    pub(crate) fn try_open_search_for_key(&mut self, k: KeyEvent) -> bool {
        let direction = match (k.code, k.modifiers) {
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                crate::app::search::SearchDirection::Forward
            }
            (KeyCode::Char('?'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                crate::app::search::SearchDirection::Backward
            }
            _ => return false,
        };

        if self.focused_transient_owns_printable_keys() {
            return false;
        }

        self.open_search_input(direction)
    }

    fn dispatch_window_lua_keymap(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        if self.ui.focused_overlay().is_some() {
            return None;
        }

        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        let result = self
            .ui
            .dispatch_event(crate::smelt_edit::Event::Key(key), &mut lua_invoke);
        if matches!(result, crate::smelt_edit::Status::Consumed) {
            self.flush_lua_callbacks();
            return Some(EventOutcome::Noop);
        }
        None
    }

    fn global_lua_keymap_route(&self, key: KeyEvent) -> GlobalLuaKeymapRoute {
        if self.timers.pending_chord.is_some() {
            return GlobalLuaKeymapRoute::Dispatch;
        }

        let plain_esc = matches!(key.code, KeyCode::Esc) && key.modifiers == KeyModifiers::NONE;
        if !plain_esc {
            return GlobalLuaKeymapRoute::Dispatch;
        }

        if self.app_focus == crate::app::AppFocus::Prompt
            && self.ui.focused_overlay().is_none()
            && self.prompt_escape_owned_by_vim()
        {
            return GlobalLuaKeymapRoute::ObservePrefix;
        }

        if self.prompt_escape_owned_by_vim()
            || matches!(
                self.focused_vim_mode(),
                Some(
                    crate::smelt_edit::VimMode::Insert
                        | crate::smelt_edit::VimMode::Visual
                        | crate::smelt_edit::VimMode::VisualLine
                )
            )
        {
            return GlobalLuaKeymapRoute::Skip;
        }

        GlobalLuaKeymapRoute::Dispatch
    }

    fn pending_lua_keymap_cancelled_by(
        &self,
        key: KeyEvent,
        token: &str,
        chain: &KeymapChain,
    ) -> bool {
        let Some(pending) = &self.timers.pending_chord else {
            return false;
        };
        let cancel_key = matches!(key.code, KeyCode::Esc) && key.modifiers == KeyModifiers::NONE
            || matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                && key.modifiers == KeyModifiers::CONTROL;
        if !cancel_key {
            return false;
        }

        let mut candidate = pending.tokens.concat();
        candidate.push_str(token);
        let frames = chain.frames();
        !self.lua.chord_has_binding(&frames, &candidate)
            && !self.lua.chord_has_longer(&frames, &candidate)
    }

    /// Run the binding `seq` resolves to around focus. A native action comes
    /// back for the caller to run.
    fn run_lua_keymap(
        &mut self,
        chain: &KeymapChain,
        seq: &str,
    ) -> (
        smelt_core::lua::runtime::KeymapResult,
        Option<crate::app::lua_bridge::NativeCall>,
    ) {
        let lua = self.lua.execution();
        let frames = chain.frames();
        let mut native = None;
        let result = crate::lua::scope_app(self, || {
            lua.run_keymap(&frames, seq, &[], &mut |frame, name| {
                resolve_action(chain, frame, name, &mut native)
            })
        });
        (result, native)
    }

    /// Run a single-key binding of the frames `reach` covers, without
    /// buffering a chord.
    fn dispatch_single_lua_keymap(&mut self, key: KeyEvent, reach: KeymapReach) -> bool {
        if self.global_lua_keymap_route(key) != GlobalLuaKeymapRoute::Dispatch {
            return false;
        }
        let Some(token) = crate::lua::chord_string(key) else {
            return false;
        };
        let chain = self.keymap_chain().reach(reach);
        use smelt_core::lua::runtime::KeymapResult;
        let (result, native) = self.run_lua_keymap(&chain, &token);
        self.flush_lua_callbacks();
        if let Some(call) = native {
            self.run_native_action(call, Some(key));
        }
        result == KeymapResult::Consumed
    }

    fn pending_lua_chord_policy(
        &self,
        first_token: &str,
        now: std::time::Instant,
    ) -> PendingChordPolicy {
        match first_token {
            ESC_TOKEN => PendingChordPolicy::Timed {
                expires_at: now
                    + std::time::Duration::from_millis(crate::app::ESC_CHORD_TIMEOUT_MS),
            },
            _ => PendingChordPolicy::Sticky,
        }
    }

    /// Dispatch a key to the bindings of the frames `reach` covers, buffering
    /// multi-key chords.
    fn dispatch_lua_keymap(&mut self, key: KeyEvent, reach: KeymapReach) -> Option<EventOutcome> {
        self.expire_pending_keymap_chord();
        let route = self.global_lua_keymap_route(key);
        if route == GlobalLuaKeymapRoute::Skip {
            return None;
        }
        let observe_prefix = route == GlobalLuaKeymapRoute::ObservePrefix;

        let Some(token) = crate::lua::chord_string(key) else {
            return self.timers.pending_chord.take().map(|_| EventOutcome::Noop);
        };

        let had_pending = self.timers.pending_chord.is_some();
        let chain = self.keymap_chain().reach(reach);
        if self.pending_lua_keymap_cancelled_by(key, &token, &chain) {
            self.timers.pending_chord = None;
            return Some(EventOutcome::Noop);
        }
        use smelt_core::lua::runtime::KeymapResult;

        // If no sequence is pending, try exact single-key bindings first. Once a
        // prefix is pending, the next key belongs to that sequence before any
        // single-key binding gets a chance, matching Vim's modal mapping feel.
        // Observe-only keys seed prefixes without stealing the local key action.
        if !had_pending && !observe_prefix {
            let (result, native) = self.run_lua_keymap(&chain, &token);
            self.flush_lua_callbacks();
            if let Some(call) = native {
                self.timers.pending_chord = None;
                return Some(self.run_native_action(call, Some(key)));
            }
            if result == KeymapResult::Consumed {
                self.timers.pending_chord = None;
                return Some(EventOutcome::Noop);
            }
        }

        let now = self.core.clock.instant_now();
        if self.timers.pending_chord.is_none() {
            self.timers.pending_chord = Some(crate::app::PendingChord {
                tokens: Vec::new(),
                vim_mode_at_start: if self.prompt.vim_enabled(self.prompt_win()) {
                    Some(self.prompt_win().vim_mode())
                } else {
                    None
                },
                policy: self.pending_lua_chord_policy(&token, now),
            });
        }
        let (mut tokens, vim_mode_at_start, policy, first_token_at_start) = {
            let p = self.timers.pending_chord.take().unwrap();
            let first = p.tokens.first().cloned();
            (p.tokens, p.vim_mode_at_start, p.policy, first)
        };
        tokens.push(token);

        let lua = self.lua.execution();
        let mut oracle = LuaChordOracle {
            lua: &lua,
            chain: &chain,
            frames: chain.frames(),
            vim_mode_at_start,
            native: None,
        };
        let outcome = crate::lua::scope_app(self, || {
            smelt_core::keymap::match_chord(tokens, &mut oracle)
        });
        let native = oracle.native.take();
        self.flush_lua_callbacks();
        match outcome {
            smelt_core::keymap::ChordOutcome::Consumed => Some(match native {
                Some(call) => self.run_native_action(call, Some(key)),
                None => EventOutcome::Noop,
            }),
            smelt_core::keymap::ChordOutcome::Pending { tokens } => {
                if tokens.is_empty() {
                    self.timers.pending_chord = None;
                } else {
                    let next_policy = if tokens.first() == first_token_at_start.as_ref() {
                        policy
                    } else {
                        tokens
                            .first()
                            .map(|first| self.pending_lua_chord_policy(first, now))
                            .unwrap_or(PendingChordPolicy::Sticky)
                    };
                    self.timers.pending_chord = Some(crate::app::PendingChord {
                        tokens,
                        vim_mode_at_start,
                        policy: next_policy,
                    });
                    if !observe_prefix {
                        return Some(EventOutcome::Noop);
                    }
                }
                None
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared preamble for idle and agent-running paths.
    ///
    /// Returns `Some(outcome)` when consumed; `None` to continue with path-specific logic.
    ///
    /// Dispatch priority: resize/mouse → notification dismissal → pending
    /// prompt or transcript Vim input → content search opener → window keys
    /// (`win:key`) → keymap scopes, the native `prompt` or `transcript` scope
    /// included, then global bindings → pane chords → cmdline `:` → content
    /// focus.
    fn dispatch_common(&mut self, ev: &Event, running: bool) -> Option<EventOutcome> {
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        if let Event::Key(k) = *ev {
            if let Some(outcome) = self.handle_notification_key(k) {
                return Some(outcome);
            }
            if self.vim_pending_input_owns_key(k) {
                return Some(match self.app_focus {
                    crate::app::AppFocus::Prompt => {
                        self.dispatch_prompt_event_to_input(Event::Key(k), running)
                    }
                    crate::app::AppFocus::Content => self.handle_event_app_history(&Event::Key(k)),
                });
            }
            if self.handle_focused_search_key(k) {
                return Some(EventOutcome::Noop);
            }
            if self.handle_search_open_before_window_dispatch(k) {
                return Some(EventOutcome::Noop);
            }
            if let Some(outcome) = self.dispatch_window_lua_keymap(k) {
                return Some(outcome);
            }
            if let Some(outcome) = self.dispatch_lua_keymap(k, KeymapReach::All) {
                return Some(outcome);
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
            let focused = match self.app_focus {
                crate::app::AppFocus::Prompt => self.prompt_win(),
                crate::app::AppFocus::Content => self.transcript_win(),
            };
            if focused.plain_keys_are_commands() {
                self.open_cmdline();
                return Some(EventOutcome::Noop);
            }
        }
        if self.app_focus == crate::app::AppFocus::Content {
            return Some(self.handle_event_app_history(ev));
        }
        None
    }

    fn handle_event_idle(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, false) {
            return outcome;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            if matches!(code, KeyCode::Esc) {
                return if self.prompt_work_state() == PromptWorkState::Paused {
                    self.handle_running_prompt_esc(ev)
                } else {
                    self.handle_idle_prompt_esc(ev, modifiers)
                };
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
        }

        let now = self.core.clock.instant_now();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let action = self
            .prompt
            .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now);
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event) -> EventOutcome {
        // Record last keypress for deferred permission dialogs.
        if matches!(ev, Event::Key(_)) {
            self.timers.last_keypress = Some(self.core.clock.instant_now());
        }

        if let Some(outcome) = self.dispatch_common(&ev, true) {
            return outcome;
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
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let input_action =
            self.prompt
                .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now);
        self.dispatch_running_input_action(input_action)
    }

    pub(super) fn dispatch_running_input_action(&mut self, input_action: Action) -> EventOutcome {
        match input_action {
            Action::Submit {
                submission,
                cut,
                stage,
            } => {
                return self.handle_running_submit(submission, cut, stage);
            }
            Action::SubmitEmpty => {
                return self.handle_empty_submit();
            }
            Action::Redraw => {}
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

    fn handle_running_submit(
        &mut self,
        submission: PromptSubmission,
        cut: crate::input::PromptCut,
        target: QueueStage,
    ) -> EventOutcome {
        let sent_at_ms = engine::clock::unix_time_ms(self.core.clock.as_ref());
        self.clear_prompt_prediction();
        let text = submission.content.text_content().into_owned();
        if self.prompt_work_state() == PromptWorkState::Paused
            && submission.content.image_count() == 0
        {
            if let InputOutcome::Command(line) = self.process_input(&text) {
                self.commit_prompt_submission(cut);
                self.apply_input_outcome(
                    InputOutcome::Command(line),
                    submission.content,
                    &submission.display,
                    sent_at_ms,
                );
                if self.pending_quit {
                    return EventOutcome::Quit;
                }
                return EventOutcome::Noop;
            }
        } else if submission.content.image_count() == 0 {
            if let Some(outcome) = self.try_command_while_running(
                smelt_buffer::text::trim_whitespace(&text),
                target,
                sent_at_ms,
            ) {
                self.commit_prompt_submission(cut);
                return outcome;
            }
        }
        if submission.content.is_empty() {
            return EventOutcome::Noop;
        }
        if self.queue_explicit_submission(QueuedInput::from_prompt(submission, sent_at_ms), target)
        {
            self.commit_prompt_submission(cut);
            self.restore_stash_after_submit();
        }
        EventOutcome::Noop
    }

    fn handle_empty_submit(&mut self) -> EventOutcome {
        if !self.prompt.queue_is_empty() {
            self.clear_cancelled_pause();
        }
        match self.prompt_work_state() {
            PromptWorkState::TurnActive => {
                self.clear_prompt_prediction();
                if self.prompt.has_queued_request() {
                    return EventOutcome::InterruptWithQueued;
                }
                if self.prompt.front_turn_can_be_request() {
                    self.promote_next_queued_turn_to_request();
                }
                return EventOutcome::Noop;
            }
            PromptWorkState::BackgroundBusy => {
                self.clear_prompt_prediction();
                if self.only_native_work_is_busy() {
                    self.start_next_queued_input();
                }
                return EventOutcome::Noop;
            }
            PromptWorkState::Paused => {
                self.resume_paused_turn(None);
                return EventOutcome::Noop;
            }
            PromptWorkState::Idle => {}
        }

        if !self.prompt.queue_is_empty() {
            self.start_next_queued_input_if_idle();
            return EventOutcome::Noop;
        }

        if self.can_continue_turn() {
            EventOutcome::ContinueTurn
        } else {
            EventOutcome::Noop
        }
    }

    fn promote_next_queued_turn_to_request(&mut self) {
        if self.prompt.promote_turn_to_request() {
            self.steer_queued_requests();
        }
    }

    pub(super) fn interrupt_with_next_queued(&mut self) {
        if self.defer_queued_compaction_handoff() {
            return;
        }
        // The cancel ends every steer of the turn, which the next turns take up.
        let interrupted = self.prompt.suspend_for_interrupt();
        self.discard_turn(crate::app::TurnEnd::Cancelled);
        if let Some(queued) = self.prompt.restore_after_interrupt(interrupted) {
            self.conversation.set_turn_pause(None);
            if let Err(queued) = self.start_queued_input(queued) {
                self.prompt.queue_front(QueueStage::Turn, queued);
            }
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn handle_idle_prompt_esc(&mut self, ev: Event, modifiers: KeyModifiers) -> EventOutcome {
        if self.prompt_escape_owned_by_vim() {
            return self.dispatch_prompt_event_to_input(ev, false);
        }

        if let Some(outcome) =
            self.dispatch_placeholder_key(self.well_known.prompt, KeyCode::Esc, modifiers)
        {
            return outcome;
        }

        self.drain_queued_inputs_into_prompt();
        EventOutcome::Noop
    }

    fn handle_running_prompt_esc(&mut self, ev: Event) -> EventOutcome {
        let now = self.core.clock.instant_now();

        if self.prompt_escape_owned_by_vim() {
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

        if self.drain_queued_inputs_into_prompt() {
            return EventOutcome::Noop;
        }
        if self.conversation.turn_pause().is_some() {
            self.conversation.clear_continuation();
        }

        EventOutcome::Noop
    }

    /// A count or operator the focused pane's Vim has pending takes the next
    /// plain key before keymap scopes.
    fn vim_pending_input_owns_key(&self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        match self.app_focus {
            crate::app::AppFocus::Prompt => self.prompt_vim_has_pending_input(),
            crate::app::AppFocus::Content => {
                let win = self.transcript_win();
                win.vim_enabled() && !win.vim_state().is_idle()
            }
        }
    }

    fn prompt_vim_has_pending_input(&self) -> bool {
        self.prompt_win().vim_has_pending_input()
    }

    fn dispatch_prompt_event_to_input(&mut self, ev: Event, running: bool) -> EventOutcome {
        let now = self.core.clock.instant_now();
        let action = {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            self.prompt
                .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now)
        };
        if running {
            self.dispatch_running_input_action(action)
        } else {
            self.dispatch_input_action(action)
        }
    }

    fn prompt_escape_owned_by_vim(&self) -> bool {
        if !self.prompt.vim_enabled(self.prompt_win()) {
            return false;
        }
        match self.prompt_win().vim_mode() {
            crate::smelt_edit::VimMode::Insert
            | crate::smelt_edit::VimMode::Visual
            | crate::smelt_edit::VimMode::VisualLine => true,
            crate::smelt_edit::VimMode::Normal => self.prompt_vim_has_pending_input(),
        }
    }

    fn apply_prompt_escape_to_input(&mut self, ev: Event, now: std::time::Instant) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt
            .handle_event(&mut pctx, ev, false, &mut self.core.clipboard, now);
    }

    fn handle_notification_key(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        let sticky = self.overlays.notification_is_sticky()?;
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            if matches!(self.app_focus, crate::app::AppFocus::Prompt)
                && self.prompt_escape_owned_by_vim()
            {
                return None;
            }
            self.dismiss_notification();
            return Some(EventOutcome::Redraw);
        }
        if !sticky {
            self.dismiss_notification();
        }
        None
    }

    pub(super) fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit {
                submission,
                cut,
                stage,
            } if self.prompt_work_state() == PromptWorkState::Paused => {
                self.handle_running_submit(submission, cut, stage)
            }
            Action::Submit {
                submission, cut, ..
            } => EventOutcome::Submit { submission, cut },
            Action::SubmitEmpty => self.handle_empty_submit(),
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

    pub(super) fn edit_in_editor(&mut self) {
        let text = self
            .prompt
            .labelled_text(crate::input::prompt_ctx_ref(&self.ui));
        let req = match crate::input::editor::prepare(&text, self.core.env.runtime_dir()) {
            Ok(req) => req,
            Err(e) => {
                self.notify_error(format!("editor: {e}"));
                return;
            }
        };
        let status = self.run_editor(&req.program, &req.args);
        match crate::input::editor::finalize(req, status) {
            Ok(text) => {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.prompt.replace_labelled_text(&mut pctx, &text);
            }
            Err(msg) => self.notify_error(msg),
        }
    }

    pub(crate) fn handle_resize(&mut self, w: u16, h: u16) {
        smelt_view::display_layout::update_kitty_geometry();
        let width_changed = w != self.last_width;
        self.last_width = w;
        self.last_height = h;
        let _ = self
            .ui
            .dispatch_event(crate::smelt_edit::Event::Resize(w, h), &mut |_, _, _| {});
        self.refresh_main_layout();
        if width_changed {
            self.ui.cancel_pointer_interaction();
        }
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    pub(crate) fn process_input(&mut self, input: &str) -> InputOutcome {
        if input.is_empty() {
            return InputOutcome::Continue;
        }

        let trimmed = smelt_buffer::text::trim_whitespace(input);
        self.prompt.push_history(input.to_string());

        let is_from_paste = self.prompt.skip_shell_escape();
        let parsed = crate::commands::parse_command_line(trimmed);

        if let crate::commands::ParsedCommand::Slash { name, .. } = &parsed {
            return if self.has_command_name(name) {
                InputOutcome::Command(trimmed.to_string())
            } else {
                InputOutcome::StartAgent
            };
        }
        if crate::commands::prompt_quit_alias(trimmed)
            || (trimmed.starts_with('!') && !is_from_paste)
        {
            return InputOutcome::Command(trimmed.to_string());
        }

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
    pub(crate) fn close_overlay_leaf(&mut self, win_id: crate::smelt_edit::WinId) {
        crate::picker::forget(self, win_id);
        self.prompt.clear_placeholder(win_id);
        for id in self.win_close(win_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close an overlay by id without assuming its first layout leaf is a window.
    pub(crate) fn close_overlay(&mut self, overlay_id: crate::smelt_edit::OverlayId) {
        for id in self.ui.overlay_close_tree(overlay_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close a window-owned decoration and clean up leaf Lua callbacks.
    pub(crate) fn close_decoration(&mut self, decoration_id: crate::smelt_edit::DecorationId) {
        for id in self.ui.decoration_close_tree(decoration_id) {
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
            .map(|p| crate::smelt_edit::WinId(p.0))
        else {
            return;
        };
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            root,
            crate::smelt_edit::WinEvent::Dismiss,
            crate::smelt_edit::Payload::None,
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// True when the active modal blocks engine-event drain.
    pub(crate) fn modal_blocks_agent(&self) -> bool {
        self.ui.active_modal_blocks_agent()
    }

    /// Snap the transcript cursor to the nearest selectable cell, skipping gutters and padding.
    pub(crate) fn snap_transcript_cursor(&mut self) {
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        // Row-backed windows manage their own cursor; snapping the local cpos would
        // overwrite the document cursor via sync_from_cpos in resync.
        if self
            .ui
            .win(win_id)
            .is_some_and(|w| w.has_materialized_rows())
        {
            return;
        }
        let cpos = self.transcript_win().cpos();
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        let snapped = self.snap_cpos_to_selectable(&rows, cpos);
        if snapped != cpos {
            self.transcript_win_mut().set_cpos(snapped);
            let viewport = self.viewport_rows_estimate();
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            win.expect("transcript window")
                .resync(buf.expect("transcript buffer"), viewport);
        }
    }

    /// Orchestrates the per-key dispatch cascade when an overlay or modal is
    /// focused. Tiers fire in order; the first to return `Consumed` wins:
    ///
    /// - Tier 1: keymap on the focused leaf (`win:key`).
    /// - Tier 1b: stateful Vim input on the focused transient viewer.
    /// - Tier 2: Lua bindings of the focused window's, modal's and overlay's
    ///   keymap scopes, innermost first. Multi-key chords buffer here. Lists
    ///   navigate through their `list` scope here.
    /// - Tier 2b: global Lua bindings, single keys only, so a global `Esc Esc`
    ///   never holds back the `Esc` that dismisses the surface.
    /// - Tier 3: vim viewer keys on the focused leaf.
    /// - Tier 4: per-window catch-all fallback (`win_set_key_fallback`), such
    ///   as the line editing of inputs.
    /// - Tier 5: modal dismiss for bare Esc / Ctrl-C.
    ///
    /// Putting Lua bindings above tier 4 lets a site-wide chord like `?` ->
    /// /help win over a dialog input's blanket printable-char fallback.
    /// Scoped bindings sit above global ones, so an open dialog's local intent
    /// (e.g. `Tab` cycles items) beats a site-wide binding of the same chord.
    pub(crate) fn run_key_cascade(&mut self, k: KeyEvent) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;

        // Tier 1: keymap on the focused leaf.
        if self.dispatch_leaf_key(k) {
            return Status::Consumed;
        }

        // Tier 1b: explicit Visual mappings can override viewer defaults;
        // otherwise Visual/VisualLine mode and pending Normal-mode sequences
        // belong to Vim before container shortcuts.
        let stateful_dispatch = self.dispatch_stateful_transient_viewer_key(k);
        let (vim_dispatched_for, global_keymap_checked) = match stateful_dispatch {
            StatefulViewerKeyDispatch::Consumed => return Status::Consumed,
            StatefulViewerKeyDispatch::Passthrough {
                vim_dispatched_for,
                global_keymap_checked,
            } => (Some(vim_dispatched_for), global_keymap_checked),
            StatefulViewerKeyDispatch::NotApplicable => (None, false),
        };

        // Tier 2: Lua bindings of the scopes around focus, with multi-key
        // chords.
        if self.dispatch_lua_keymap(k, KeymapReach::Scoped).is_some() {
            return Status::Consumed;
        }

        // Tier 2b: global Lua bindings. Visual mode checked them before Vim
        // so explicit remaps can override viewer defaults.
        if !global_keymap_checked && self.dispatch_single_lua_keymap(k, KeymapReach::Global) {
            return Status::Consumed;
        }

        // Shared pane chords remain below explicit leaf/container/global bindings.
        if self.handle_pane_chord(&Event::Key(k)).is_some() {
            return Status::Consumed;
        }

        // Tier 3: viewer fallbacks for the focused transient leaf. A key that
        // already passed through stateful Vim skips the Vim engine here so each
        // terminal key is evaluated by Vim at most once.
        let viewer_consumed = if vim_dispatched_for.is_some_and(|win| self.ui.focus() == Some(win))
        {
            self.dispatch_transient_viewer_fallback_key(k)
        } else {
            self.dispatch_transient_viewer_key(k)
        };
        if viewer_consumed {
            return Status::Consumed;
        }

        // Tier 4: per-window catch-all fallback (dialog inputs, etc.).
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
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
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui
            .try_dismiss_modal_for_chord(k.code, k.modifiers, &mut lua_invoke)
    }

    /// Run the focused leaf's keymap for `k`.
    fn dispatch_leaf_key(&mut self, k: KeyEvent) -> bool {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        matches!(
            self.ui.dispatch_key(k.code, k.modifiers, &mut lua_invoke),
            crate::smelt_edit::Status::Consumed
        )
    }

    pub(crate) fn run_paste_fallback(&mut self, content: String) -> crate::smelt_edit::Status {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.dispatch_paste_fallback(content, &mut lua_invoke)
    }

    fn dispatch_stateful_transient_viewer_key(&mut self, k: KeyEvent) -> StatefulViewerKeyDispatch {
        let Some(win_id) = self.ui.focus() else {
            return StatefulViewerKeyDispatch::NotApplicable;
        };
        if self.ui.overlay_for_leaf(win_id).is_none() && self.ui.focused_modal().is_none() {
            return StatefulViewerKeyDispatch::NotApplicable;
        }
        let Some(win) = self.ui.win(win_id) else {
            return StatefulViewerKeyDispatch::NotApplicable;
        };
        if !win.vim_enabled() || !win.surface().is_readonly_text() {
            return StatefulViewerKeyDispatch::NotApplicable;
        }
        let visual = match win.vim_mode() {
            crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => true,
            crate::smelt_edit::VimMode::Normal if !win.vim_state().is_idle() => false,
            crate::smelt_edit::VimMode::Normal | crate::smelt_edit::VimMode::Insert => {
                return StatefulViewerKeyDispatch::NotApplicable;
            }
        };

        // Only global bindings beat Vim here; scoped keys sit below it.
        if visual && self.dispatch_single_lua_keymap(k, KeymapReach::Global) {
            return StatefulViewerKeyDispatch::Consumed;
        }
        if matches!(
            self.dispatch_viewer_vim_key(win_id, k),
            crate::smelt_edit::Status::Consumed
        ) {
            StatefulViewerKeyDispatch::Consumed
        } else {
            StatefulViewerKeyDispatch::Passthrough {
                vim_dispatched_for: win_id,
                global_keymap_checked: visual,
            }
        }
    }

    /// Transient-focus key cascade tier 3. Wraps the shared
    /// [`Self::dispatch_window_viewer_key`] with two gates:
    ///   * Insert-mode skip - typing inside an editable leaf must not bubble
    ///     navigation keys here.
    ///   * Esc-in-idle-Normal falls through so the modal-dismiss tier (5)
    ///     can close the surface; Visual / pending-sequence Esc stays with vim.
    pub(crate) fn dispatch_transient_viewer_key(&mut self, k: KeyEvent) -> bool {
        let win = match self.ui.focus() {
            Some(w) => w,
            None => return false,
        };
        let (vim_enabled, vim_mode, vim_idle) = match self.ui.win(win) {
            Some(w) => (w.vim_enabled(), w.vim_mode(), w.vim_state().is_idle()),
            None => return false,
        };
        let in_insert = vim_enabled && vim_mode == crate::smelt_edit::VimMode::Insert;
        let insert_escape =
            in_insert && k.code == KeyCode::Esc && k.modifiers == KeyModifiers::NONE;
        if in_insert && !insert_escape {
            return false;
        }
        if k.code == KeyCode::Esc
            && vim_enabled
            && vim_mode == crate::smelt_edit::VimMode::Normal
            && vim_idle
        {
            return false;
        }
        matches!(
            self.dispatch_window_viewer_key(win, k),
            crate::smelt_edit::Status::Consumed
        )
    }

    fn dispatch_transient_viewer_fallback_key(&mut self, k: KeyEvent) -> bool {
        let Some(win) = self.ui.focus() else {
            return false;
        };
        matches!(
            self.dispatch_window_viewer_fallback_key(win, k),
            crate::smelt_edit::Status::Consumed
        )
    }

    fn viewer_key_context(
        &self,
        win_id: crate::smelt_edit::WinId,
    ) -> Option<(crate::smelt_edit::BufId, u16, bool)> {
        let win = self.ui.win(win_id)?;
        let viewport_rows = win.viewport.map(|v| v.rect.height).unwrap_or(0);
        if viewport_rows == 0 {
            return None;
        }
        let buf_id = win.buf;
        if self.ui.buf(buf_id)?.lines().is_empty() {
            return None;
        }
        Some((
            buf_id,
            viewport_rows,
            win.vim_enabled() && win.surface().is_readonly_text(),
        ))
    }

    fn dispatch_viewer_vim_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;
        let Some((buf_id, viewport_rows, uses_vim)) = self.viewer_key_context(win_id) else {
            return Status::Ignored;
        };
        if !uses_vim {
            return Status::Ignored;
        }

        match self
            .ui
            .win_mut(win_id)
            .expect("viewer window")
            .handle_viewer_key(k)
        {
            crate::smelt_edit::DocumentKeyResult::Command(command) => {
                self.execute_viewer_command(win_id, buf_id, command, viewport_rows)
            }
            crate::smelt_edit::DocumentKeyResult::Consumed => Status::Consumed,
            crate::smelt_edit::DocumentKeyResult::Passthrough => Status::Ignored,
        }
    }

    fn dispatch_window_viewer_fallback_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        if self.handle_search_key_for_target(win_id, k) {
            return crate::smelt_edit::Status::Consumed;
        }
        crate::smelt_edit::Status::Ignored
    }

    /// Viewer keys of the transcript and overlay leaves: search navigation,
    /// then the viewer Vim engine for `readonly_text` surfaces, which handles
    /// motions, counts, visual selection and yanks as viewer commands. Plain
    /// motion keys belong to the `viewer` keymap scope.
    pub(crate) fn dispatch_window_viewer_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;
        if self.handle_search_key_for_target(win_id, k) {
            return Status::Consumed;
        }
        self.dispatch_viewer_vim_key(win_id, k)
    }

    /// Walk the jump list of the viewer window `win_id` as `Ctrl-O`, `Ctrl-I`
    /// or `''` would. Returns whether the cursor moved.
    pub(crate) fn jump_viewer(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        jump: crate::smelt_edit::JumpMove,
    ) -> bool {
        use crate::smelt_edit::DocumentCommand;

        let Some((buf_id, viewport_rows, _)) = self.viewer_key_context(win_id) else {
            return false;
        };
        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
        let (Some(win), Some(buf)) = (win, buf) else {
            return false;
        };
        let Some(command) = win.resolve_viewer_jump(buf, DocumentCommand::Jump(jump)) else {
            return false;
        };
        self.execute_viewer_command(win_id, buf_id, command, viewport_rows);
        true
    }

    /// Leave the cursor of the viewer window `win_id` in its jump list, before
    /// a host-driven move such as a reveal.
    pub(crate) fn record_viewer_jump(&mut self, win_id: crate::smelt_edit::WinId) {
        let Some(win) = self.ui.win(win_id) else {
            return;
        };
        let Some(buf) = self.ui.buf(win.buf) else {
            return;
        };
        let Some(from) = win.viewer_doc_cursor(buf) else {
            return;
        };
        if let Some(win) = self.ui.win_mut(win_id) {
            win.record_jump(from);
        }
    }

    /// Move the focused transcript cursor as the vim block motion `[`/`]` plus
    /// kind key would. Returns whether the transcript took the motion.
    pub(crate) fn move_transcript_block(
        &mut self,
        motion: crate::smelt_edit::BlockMotion,
        count: usize,
    ) -> bool {
        use crate::smelt_edit::{DocumentCommand, Motion};

        let win_id = crate::app::TRANSCRIPT_WIN;
        if self.app_focus != crate::app::AppFocus::Content {
            return false;
        }
        let Some((buf_id, viewport_rows, _)) = self.viewer_key_context(win_id) else {
            return false;
        };
        self.execute_viewer_command(
            win_id,
            buf_id,
            DocumentCommand::Move {
                motion: Motion::Block(motion),
                count: Some(count),
            },
            viewport_rows,
        );
        true
    }

    fn execute_viewer_command(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        command: crate::smelt_edit::DocumentCommand,
        viewport_rows: u16,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::{DocumentCommand, Status};

        if matches!(command, DocumentCommand::OpenAction) {
            let pos = {
                let Some(win) = self.ui.win(win_id) else {
                    return Status::Ignored;
                };
                let Some(buf) = self.ui.buf(buf_id) else {
                    return Status::Ignored;
                };
                win.viewer_doc_cursor(buf)
            };
            let action = pos.and_then(|pos| self.document_action_at(win_id, pos));
            if let Some(action) = action {
                self.dispatch_span_action(action);
            } else {
                self.record_notice(
                    smelt_core::messages::MessageKind::Info,
                    "actions".into(),
                    "no action under cursor".into(),
                );
            }
            return Status::Consumed;
        }

        let (command, register) = {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let (Some(win), Some(buf)) = (win, buf) else {
                return Status::Ignored;
            };
            let register = win.take_viewer_register();
            match win.resolve_viewer_jump(buf, command) {
                Some(command) => (command, register),
                None => return Status::Consumed,
            }
        };
        let now = self.core.clock.instant_now();
        let copied = if self.win_uses_document_view(win_id) {
            self.execute_document_view_command_for_win(win_id, command, viewport_rows, now)
                .map(crate::smelt_edit::DocumentCopy::Rows)
        } else {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.execute_viewer_command(buf, command, viewport_rows, now)
        };
        self.copy_viewer_selection(win_id, buf_id, copied, register, now);
        Status::Consumed
    }

    fn copy_viewer_selection(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        copied: Option<crate::smelt_edit::DocumentCopy>,
        register: Option<char>,
        now: std::time::Instant,
    ) {
        match copied {
            Some(crate::smelt_edit::DocumentCopy::Rows(range)) => {
                if let Some(out) = self.copy_document_rows(win_id, range) {
                    self.yank_to_register(out, register);
                }
            }
            Some(crate::smelt_edit::DocumentCopy::Bytes(range)) => {
                let copied = self.ui.buf(buf_id).map(|buf| buf.copy_range(range.clone()));
                if let Some(win) = self.ui.win_mut(win_id) {
                    win.set_byte_yank_flash(range, now);
                }
                if let Some(out) = copied {
                    self.yank_to_register(out, register);
                }
            }
            None => {}
        }
    }

    /// Put a viewer yank in the register `"` named: a named register keeps
    /// the system clipboard as it was, `"_` drops the text, and any other
    /// register goes to the clipboard.
    fn yank_to_register(&mut self, out: crate::smelt_edit::CopyOutput, register: Option<char>) {
        match register {
            Some('_') => {}
            Some(name) if name.is_ascii_alphabetic() => {
                let ring = &mut self.core.clipboard.kill_ring;
                ring.store(name, out.kill_ring, false);
                let (text, linewise) = ring
                    .register(name)
                    .map(|(text, linewise)| (text.clone(), linewise))
                    .unwrap_or_default();
                ring.set_with_linewise(text, linewise);
            }
            _ => {
                self.core
                    .clipboard
                    .kill_ring
                    .store('0', out.kill_ring.clone(), false);
                self.yank_to_clipboard(out);
            }
        }
    }

    pub(crate) fn dispatch_span_action(&mut self, action: smelt_core::buffer::SpanAction) {
        use smelt_core::buffer::SpanAction;
        use smelt_core::messages::MessageKind;

        let (result, fallback_text) = match action {
            SpanAction::OpenUrl(url) => {
                let result = engine::opener::open_url_if_available_in(&url, &self.core.env.cwd());
                (result, url)
            }
            SpanAction::OpenFile { path, line, col } => {
                let target = engine::opener::FileOpenTarget::new(path, line, col);
                let fallback_text = target.display_location();
                let result =
                    engine::opener::open_file_if_available_in(&target, &self.core.env.cwd());
                if result.opened() {
                    self.record_notice(
                        MessageKind::Info,
                        "actions".into(),
                        format!("opened {fallback_text}"),
                    );
                }
                (result, fallback_text)
            }
        };

        match result {
            engine::opener::OpenResult::Opened => {}
            engine::opener::OpenResult::Unavailable(reason) => {
                self.record_action_open_fallback(reason, &fallback_text);
            }
            engine::opener::OpenResult::Failed(err) => {
                self.record_action_open_fallback(&err, &fallback_text);
            }
        }
    }

    fn record_action_open_fallback(&mut self, reason: &str, text: &str) {
        use smelt_core::messages::MessageKind;

        let body = match self.core.clipboard.write(text) {
            Ok(()) => {
                self.core
                    .clipboard
                    .kill_ring
                    .record_clipboard_write(text.to_string());
                format!("{reason}: copied {text} to clipboard")
            }
            Err(err) => format!("{reason}: could not copy ({err}); open {text} manually"),
        };
        self.record_notice(MessageKind::Warning, "actions".into(), body);
    }

    /// Run text action `action` of the `viewer` scope on window `win_id`:
    /// motion, selection, page motion and copy. Editing actions do nothing.
    pub(super) fn run_viewer_action(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        action: crate::keymap::TextAction,
    ) -> crate::smelt_edit::Status {
        use crate::keymap::TextAction;
        use crate::smelt_edit::Status;

        let Some((buf_id, viewport_rows, uses_vim)) = self.viewer_key_context(win_id) else {
            return Status::Ignored;
        };
        let (readonly, readonly_text) = match (self.ui.win(win_id), self.ui.buf(buf_id)) {
            (Some(w), Some(b)) => (b.readonly, w.surface().is_readonly_text()),
            _ => return Status::Ignored,
        };

        if readonly_text
            || self
                .ui
                .win(win_id)
                .is_some_and(|win| win.has_materialized_rows())
        {
            let extending = matches!(
                action,
                TextAction::SelectUp
                    | TextAction::SelectDown
                    | TextAction::SelectStartOfLine
                    | TextAction::SelectEndOfLine
                    | TextAction::SelectWordForward
                    | TextAction::SelectWordBackward
            );
            use crate::smelt_edit::text::CharClass;
            use crate::smelt_edit::{Motion, WordTo};
            let move_command = |motion| crate::smelt_edit::DocumentCommand::Move {
                motion,
                count: None,
            };
            let command = match action {
                TextAction::MoveUp | TextAction::SelectUp => {
                    Some(crate::smelt_edit::DocumentCommand::MoveRows(-1))
                }
                TextAction::MoveDown | TextAction::SelectDown => {
                    Some(crate::smelt_edit::DocumentCommand::MoveRows(1))
                }
                TextAction::MoveWordForward | TextAction::SelectWordForward => {
                    Some(move_command(Motion::Word(WordTo::Next, CharClass::Word)))
                }
                TextAction::MoveWordBackward | TextAction::SelectWordBackward => {
                    Some(move_command(Motion::Word(WordTo::Prev, CharClass::Word)))
                }
                TextAction::PageUp => Some(crate::smelt_edit::DocumentCommand::PageRows(-1)),
                TextAction::PageDown => Some(crate::smelt_edit::DocumentCommand::PageRows(1)),
                TextAction::HalfPageUp => {
                    Some(crate::smelt_edit::DocumentCommand::HalfPageRows(-1))
                }
                TextAction::HalfPageDown => {
                    Some(crate::smelt_edit::DocumentCommand::HalfPageRows(1))
                }
                TextAction::ScrollLineUp => {
                    Some(crate::smelt_edit::DocumentCommand::ScrollRows(-1))
                }
                TextAction::ScrollLineDown => {
                    Some(crate::smelt_edit::DocumentCommand::ScrollRows(1))
                }
                TextAction::MoveStartOfBuffer => Some(move_command(Motion::BufferStart)),
                TextAction::MoveEndOfBuffer => Some(move_command(Motion::BufferEnd)),
                TextAction::MoveStartOfLine | TextAction::SelectStartOfLine => {
                    Some(move_command(Motion::LineStart))
                }
                TextAction::MoveEndOfLine | TextAction::SelectEndOfLine => {
                    Some(move_command(Motion::LineEnd))
                }
                TextAction::CopySelection => {
                    Some(crate::smelt_edit::DocumentCommand::YankSelection)
                }
                _ => None,
            };
            if let Some(command) = command {
                // Under Vim, motions run as the Vim engine runs them.
                if uses_vim && !extending {
                    return self.execute_viewer_command(win_id, buf_id, command, viewport_rows);
                }
                let now = self.core.clock.instant_now();
                let anchor_active = self
                    .ui
                    .win(win_id)
                    .is_some_and(|win| win.row_selection_anchor_active());
                if extending && !anchor_active {
                    self.execute_document_view_command_for_win(
                        win_id,
                        crate::smelt_edit::DocumentCommand::StartVisual,
                        viewport_rows,
                        now,
                    );
                } else if !extending
                    && !matches!(command, crate::smelt_edit::DocumentCommand::YankSelection)
                {
                    self.execute_document_view_command_for_win(
                        win_id,
                        crate::smelt_edit::DocumentCommand::ClearSelection,
                        viewport_rows,
                        now,
                    );
                }
                let copied =
                    self.execute_document_view_command_for_win(win_id, command, viewport_rows, now);
                if let Some(range) = copied {
                    if let Some(out) = self.copy_document_rows(win_id, range) {
                        self.yank_to_clipboard(out);
                    }
                }
                return Status::Consumed;
            }
        }

        let extending = matches!(
            action,
            TextAction::SelectLeft
                | TextAction::SelectRight
                | TextAction::SelectUp
                | TextAction::SelectDown
                | TextAction::SelectWordForward
                | TextAction::SelectWordBackward
                | TextAction::SelectStartOfLine
                | TextAction::SelectEndOfLine
        );
        let is_motion = matches!(
            action,
            TextAction::MoveLeft
                | TextAction::MoveRight
                | TextAction::MoveUp
                | TextAction::MoveDown
                | TextAction::MoveStartOfLine
                | TextAction::MoveEndOfLine
                | TextAction::MoveWordForward
                | TextAction::MoveWordBackward
                | TextAction::MoveStartOfBuffer
                | TextAction::MoveEndOfBuffer
                | TextAction::PageUp
                | TextAction::PageDown
                | TextAction::HalfPageUp
                | TextAction::HalfPageDown
                | TextAction::ScrollLineUp
                | TextAction::ScrollLineDown
        );

        let cpos_before = self.ui.win(win_id).expect("window").cpos();
        let win = self.ui.win_mut(win_id).expect("window");
        if is_motion {
            win.clear_selection_anchor();
        } else if extending {
            win.extend_selection(cpos_before);
        }

        // Line-delta motions resolve through the window's display-line API,
        // which respects soft-wrap and gutter math.
        let line_delta: Option<isize> = match action {
            TextAction::MoveUp | TextAction::SelectUp => Some(-1),
            TextAction::MoveDown | TextAction::SelectDown => Some(1),
            TextAction::PageUp => Some(-(viewport_rows as isize)),
            TextAction::PageDown => Some(viewport_rows as isize),
            TextAction::HalfPageUp => Some(-((viewport_rows as isize) / 2).max(1)),
            TextAction::HalfPageDown => Some(((viewport_rows as isize) / 2).max(1)),
            TextAction::ScrollLineUp => Some(-1),
            TextAction::ScrollLineDown => Some(1),
            _ => None,
        };
        if let Some(d) = line_delta {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.move_cursor_by_lines(buf, d, viewport_rows);
            win.update_tail_state(buf, viewport_rows);
            return Status::Consumed;
        }

        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Status::Ignored,
        };
        let text = buf.text().to_string();
        let cpos = self.ui.win(win_id).expect("window").cpos();
        let mv: Option<usize> = match action {
            TextAction::MoveLeft | TextAction::SelectLeft => {
                Some(crate::smelt_edit::text::prev_grapheme_boundary(&text, cpos))
            }
            TextAction::MoveRight | TextAction::SelectRight => {
                Some(crate::smelt_edit::text::next_grapheme_boundary(&text, cpos))
            }
            TextAction::MoveStartOfLine | TextAction::SelectStartOfLine => {
                Some(crate::smelt_edit::text::line_start(&text, cpos))
            }
            TextAction::MoveEndOfLine | TextAction::SelectEndOfLine => {
                Some(crate::smelt_edit::text::line_end(&text, cpos))
            }
            TextAction::MoveWordForward | TextAction::SelectWordForward => {
                Some(crate::smelt_edit::text::word_forward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            TextAction::MoveWordBackward | TextAction::SelectWordBackward => {
                Some(crate::smelt_edit::text::word_backward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            TextAction::MoveStartOfBuffer => Some(0),
            TextAction::MoveEndOfBuffer => Some(text.len()),
            TextAction::CopySelection => {
                let win = self.ui.win(win_id).expect("window");
                if let Some((s, e)) = win.selection_range(buf) {
                    let s = crate::smelt_edit::text::snap_grapheme(&text, s);
                    let e = crate::smelt_edit::text::snap_grapheme(&text, e);
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
            win.set_cpos(new_cpos);
            win.set_curswant(None);
            // A shift-motion that resolved to no movement (e.g. Shift+End at
            // EOL) leaves `selection_anchor == cpos` - a degenerate, empty
            // selection that downstream code treats as "no selection" but
            // whose anchor still points into the buffer. Clear it so a
            // follow-up source-shrinking edit can't orphan it.
            if win.selection_anchor() == Some(win.cpos()) {
                win.clear_selection_anchor();
            }
            win.resync(buf, viewport_rows);
            return Status::Consumed;
        }
        Status::Ignored
    }
}

/// Adapter that lets the pure [`smelt_core::keymap::match_chord`] loop call
/// into the live Lua keymap registry. Carries the `vim_mode_at_chord_start`
/// context pair that handlers see on multi-key matches.
struct LuaChordOracle<'a> {
    lua: &'a crate::lua::LuaExecution,
    chain: &'a KeymapChain,
    frames: Vec<smelt_core::lua::keymaps::Frame<'a>>,
    vim_mode_at_start: Option<crate::smelt_edit::VimMode>,
    /// The native action a matched chord resolved to.
    native: Option<crate::app::lua_bridge::NativeCall>,
}

/// Resolve action `name` of the scope at `frame`, keeping a native one in
/// `native` for the app to run once dispatch returns.
fn resolve_action(
    chain: &KeymapChain,
    frame: usize,
    name: &str,
    native: &mut Option<crate::app::lua_bridge::NativeCall>,
) -> Option<smelt_core::lua::runtime::ScopeActionImpl> {
    use smelt_core::lua::runtime::ScopeActionImpl;
    match chain.action(frame, name)? {
        crate::app::lua_bridge::ScopeAction::Lua(id) => Some(ScopeActionImpl::Lua(id)),
        crate::app::lua_bridge::ScopeAction::Native(call) => {
            *native = Some(call);
            Some(ScopeActionImpl::Native)
        }
    }
}

impl smelt_core::keymap::ChordOracle for LuaChordOracle<'_> {
    fn has_longer(&self, seq: &str) -> bool {
        self.lua.chord_has_longer(&self.frames, seq)
    }
    fn try_keymap(&mut self, seq: &str) -> smelt_core::lua::runtime::KeymapResult {
        let ctx = [(
            "vim_mode_at_chord_start",
            self.vim_mode_at_start
                .map(|m| crate::lua::LuaVimMode::from(m).label().to_string())
                .unwrap_or_default(),
        )];
        let chain = self.chain;
        let native = &mut self.native;
        self.lua
            .run_keymap(&self.frames, seq, &ctx, &mut |frame, name| {
                resolve_action(chain, frame, name, native)
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::app::test_harness::{Action, TestApp};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use protocol::AgentMode;
    use smelt_core::working::TurnPhase;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    #[test]
    fn key_releases_are_ignored_without_disabling_key_repeat() {
        let mut app = TestApp::builder().build();
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            app.tui
                .dispatch_terminal_event(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    kind,
                )));
        }

        assert_eq!(app.state().prompt_text, "aa");
    }

    #[test]
    fn running_esc_esc_cancels_but_single_esc_does_not() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);

        app.press(KeyCode::Esc);
        assert!(
            app.agent_running(),
            "first Esc should only arm the Lua Esc Esc sequence"
        );

        app.press(KeyCode::Esc);
        assert!(
            !app.agent_running(),
            "second Esc should resolve the Lua cancel sequence"
        );
    }

    #[test]
    fn running_esc_esc_drains_queued_input_before_canceling() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("queued steer".to_string());

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);

        let state = app.state();
        assert!(
            state.agent_running,
            "Esc Esc with queued input should preserve the active turn"
        );
        assert!(
            state.queued_inputs.is_empty(),
            "queued input should be drained instead of left pending"
        );
        assert_eq!(state.prompt_text, "queued steer");
    }

    fn queue_stages(app: &TestApp) -> Vec<String> {
        app.tui.prompt.queued_kinds()
    }

    fn insert_prompt_image(app: &mut TestApp, label: &str) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut app.tui.ui);
        app.tui.prompt.insert_image_for_harness(
            &mut pctx,
            label.to_string(),
            format!("data:image/png;base64,{label}"),
        );
    }

    fn lua_command_queue_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ctrl_c_clear_is_undoable_and_redoable() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "");

        app.press_mod(KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(KeyCode::Char('_'), KeyModifiers::ALT);
        assert_eq!(app.state().prompt_text, "");
    }

    #[test]
    fn ctrl_c_right_after_a_clear_neither_quits_nor_cancels() {
        let ctrl_c = |app: &mut TestApp| app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);

        let mut app = TestApp::builder().build();
        app.type_text("draft");
        ctrl_c(&mut app);
        ctrl_c(&mut app);
        assert!(!app.quit_requested());
        app.clock
            .advance(crate::app::scope_actions::CTRL_C_REPEAT_GRACE);
        ctrl_c(&mut app);
        assert!(app.quit_requested());

        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("draft");
        ctrl_c(&mut app);
        ctrl_c(&mut app);
        assert!(app.agent_running());
        app.clock
            .advance(crate::app::scope_actions::CTRL_C_REPEAT_GRACE);
        ctrl_c(&mut app);
        assert!(!app.agent_running());
    }

    #[test]
    fn cmd_z_and_cmd_shift_z_undo_redo_prompt_edits() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.press_mod(KeyCode::Char('z'), KeyModifiers::SUPER);
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(
            KeyCode::Char('z'),
            KeyModifiers::SUPER.union(KeyModifiers::SHIFT),
        );
        assert_eq!(app.state().prompt_text, "");
    }

    #[test]
    fn empty_enter_promotes_turn_queue_to_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("check this first");
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "check this first")
        )));
    }

    #[test]
    fn empty_enter_promotes_turn_queue_during_compacting() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        let _dispatch = app
            .tui
            .conversation
            .begin_dispatch()
            .expect("test turn enters dispatch");
        app.tui.working.begin(TurnPhase::Compacting);

        app.type_text("check this first");
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "check this first")
        )));
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
    }

    #[test]
    fn enter_promotes_queued_image_prompt_to_current_request() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["see [pic.png]".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                if input.provider_content().image_count() == 1)
        )));
    }

    #[test]
    fn queued_image_starts_next_turn_with_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("inspect ");
        insert_prompt_image(&mut app, "pic.png");
        app.press(KeyCode::Enter);

        app.clear_actions();
        assert!(app.finish_turn());
        let _ = app.flush_persist();
        app.start_next_queued_input_if_idle();
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));

        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().image_count() == 1)
        )));
    }

    #[test]
    fn request_queue_with_image_steers_and_unqueues_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(app.state().queued_inputs, vec!["see [pic.png]".to_string()]);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert_eq!(app.state().prompt_text, "");
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                if matches!(input, protocol::StartTurnInput::User {
                    content,
                    display: Some(display),
                    image_placement: Some(placement),
                    command: false,
                    ..
                } if content.image_count() == 1
                    && display == "see [pic.png]"
                    && placement.source == format!("see {}", crate::input::ATTACHMENT_MARKER)
                    && placement.image_indices == [0]))
        )));

        app.clear_actions();
        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);

        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Withdraw { ids } if ids.len() == 1)
        )));
        assert_eq!(
            app.state().queued_inputs,
            vec!["see [pic.png]".to_string()],
            "the steer stays queued until the engine gives it back"
        );
        assert_eq!(app.state().prompt_text, "");

        app.answer_withdraw();

        assert!(app.agent_running());
        assert!(app.state().queued_inputs.is_empty());
        assert_eq!(
            app.state().prompt_text,
            format!("see {}", crate::input::ATTACHMENT_MARKER)
        );
        assert_eq!(
            app.tui
                .prompt
                .render_input()
                .build_content(app.tui.prompt_buf())
                .image_count(),
            1
        );
    }

    #[test]
    fn unqueue_preserves_image_order_and_existing_prompt() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("first ");
        insert_prompt_image(&mut app, "one.png");
        app.press(KeyCode::Enter);
        app.type_text("second ");
        insert_prompt_image(&mut app, "two.png");
        app.press(KeyCode::Enter);
        app.type_text("draft");
        insert_prompt_image(&mut app, "draft.png");

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);

        let marker = crate::input::ATTACHMENT_MARKER;
        assert_eq!(
            app.state().prompt_text,
            format!("first {marker}\nsecond {marker}\ndraft{marker}")
        );
        let pctx = crate::input::prompt_ctx_ref(&app.tui.ui);
        let labels: Vec<_> = pctx
            .buf
            .attachments
            .iter()
            .map(|attachment| attachment.display_label())
            .collect();
        assert_eq!(labels, ["[one.png]", "[two.png]", "[draft.png]"]);
    }

    #[test]
    fn image_only_prompt_survives_queue_unqueue_and_requeue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        insert_prompt_image(&mut app, "pic.png");
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        assert_eq!(
            app.state().prompt_text,
            crate::input::ATTACHMENT_MARKER.to_string()
        );

        app.clear_actions();
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                if input.provider_content().image_count() == 1)
        )));
    }

    #[test]
    fn image_request_without_replay_is_rejected() {
        let image = protocol::Content::with_images(
            "see ".into(),
            vec![("pic.png".into(), "data:image/png;base64,pic".into())],
        );
        assert!(std::panic::catch_unwind(|| {
            crate::app::queue::QueuedInput::request("see [pic.png]", image, 0)
        })
        .is_err());
    }

    #[test]
    fn repeated_image_and_literal_label_rewind_to_exact_positions() {
        let mut app = TestApp::builder().build();
        app.type_text("original");
        app.press(KeyCode::Enter);
        let _ = app.flush_persist();
        app.type_text("literal [pic.png] before ");
        insert_prompt_image(&mut app, "pic.png");
        let id = app.tui.prompt_buf().attachments[0].clone();
        app.type_text(" between ");
        {
            let pctx = crate::input::prompt_ctx_mut(&mut app.tui.ui);
            let pos = pctx.buf.source().len();
            pctx.buf.text_mut().insert_marker(pos, id);
            pctx.win.set_cpos(pctx.buf.source().len());
        }
        app.type_text(" after");
        let expected = app.state().prompt_text;
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);

        assert_eq!(app.state().prompt_text, expected);
        let pctx = crate::input::prompt_ctx_ref(&app.tui.ui);
        assert_eq!(pctx.buf.attachments.len(), 2);
        assert_eq!(pctx.buf.attachments[0], pctx.buf.attachments[1]);
        assert_eq!(
            app.tui
                .prompt
                .render_input()
                .build_content(pctx.buf)
                .image_count(),
            1
        );
    }

    #[test]
    fn promoted_image_interrupts_into_new_message_and_can_rewind() {
        let mut app = TestApp::builder().build();
        app.type_text("original");
        app.press(KeyCode::Enter);
        let _ = app.flush_persist();
        app.type_text("inspect ");
        insert_prompt_image(&mut app, "first.png");
        app.type_text(" then ");
        insert_prompt_image(&mut app, "second.png");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().image_count() == 2)
        )));
        assert!(app.state().queued_inputs.is_empty());
        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        app.wait_for_rewind();
        assert_eq!(
            app.state().prompt_text,
            format!("inspect {0} then {0}", crate::input::ATTACHMENT_MARKER)
        );
        let content = app
            .tui
            .prompt
            .render_input()
            .build_content(app.tui.prompt_buf());
        assert_eq!(content.image_count(), 2);
        assert_eq!(content.image_labels(), vec!["[first.png]", "[second.png]"]);
    }

    #[test]
    fn paused_turn_resumes_then_sends_queued_image_and_rewinds_it() {
        let mut app = TestApp::builder().build();
        app.type_text("original");
        app.press(KeyCode::Enter);
        let _ = app.flush_persist();
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::TurnError {
                message: "connection failed".into(),
                kind: Some(protocol::EngineAskErrorKind::Network),
                retry_at_ms: None,
            },
        ));
        app.wait_for_turn_dispatch();
        assert_eq!(
            app.tui.prompt_work_state(),
            crate::app::PromptWorkState::Paused
        );
        app.type_text("retry with ");
        insert_prompt_image(&mut app, "pic.png");
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);
        assert!(app.agent_running() || app.tui.turn_submission_is_pending());
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().image_count() == 1)
        )));

        app.wait_for_turn_dispatch();
        assert!(app.finish_turn());
        app.wait_for_turn_dispatch();
        app.start_next_queued_input_if_idle();
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().image_count() == 1)
        )));
        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        app.wait_for_rewind();
        assert_eq!(
            app.state().prompt_text,
            format!("retry with {}", crate::input::ATTACHMENT_MARKER)
        );
        assert_eq!(
            app.tui
                .prompt
                .render_input()
                .build_content(app.tui.prompt_buf())
                .image_count(),
            1
        );
    }

    #[test]
    fn full_queue_keeps_image_in_prompt() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        for _ in 0..crate::app::queue::MAX_QUEUED_MESSAGES {
            app.push_queued_message("waiting".to_string());
        }
        insert_prompt_image(&mut app, "pic.png");
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            app.tui.prompt.queued_len(),
            crate::app::queue::MAX_QUEUED_MESSAGES
        );
        assert_eq!(
            app.state().prompt_text,
            crate::input::ATTACHMENT_MARKER.to_string()
        );
        assert_eq!(
            app.tui
                .prompt
                .render_input()
                .build_content(app.tui.prompt_buf())
                .image_count(),
            1
        );
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn ctrl_enter_queues_prompt_to_current_request() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("next turn".to_string());
        app.type_text("right now");

        app.clear_actions();
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["right now".to_string(), "next turn".to_string()]
        );
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "right now")
        )));
    }

    #[test]
    fn ctrl_enter_slash_command_queues_expansion_to_current_request() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("request-body", function(arg)
                    smelt.engine.submit_command(
                        "request-body",
                        "expanded " .. (arg or ""),
                        nil,
                        "request-body " .. (arg or "")
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.push_queued_message("next turn".to_string());
        app.type_text("/request-body now");

        app.clear_actions();
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            app.state().queued_inputs,
            vec!["/request-body now".to_string(), "next turn".to_string()]
        );
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "expanded now"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn empty_enter_interrupts_promoted_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("run now");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));

        assert!(app.agent_running());
        assert!(app.state().queued_inputs.is_empty());
        assert!(
            !app.actions().iter().any(|action| matches!(
                action,
                Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Withdraw { .. })
            )),
            "the cancel ends the turn's steers, so none is withdrawn"
        );
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel)
        )));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().text_content() == "run now")
        )));
    }

    #[test]
    fn turn_dispatches_while_the_session_writer_is_busy() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        assert!(app.finish_turn());
        let _ = app.flush_persist();

        let release_persistence = app.pause_persistence();
        app.type_text("next request");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();

        assert!(app.agent_running());
        assert!(!app.tui.turn_submission_is_pending());
        assert!(!app.session_is_read_only());
        assert_eq!(app.state().prompt_text, "");
        release_persistence
            .send(())
            .expect("release session writer");
        assert!(matches!(
            app.flush_persist(),
            crate::persist::PersistenceFlushOutcome::Durable
        ));
        assert!(!app.session_is_read_only());
    }

    #[test]
    fn queued_process_status_dispatches_while_the_session_writer_is_busy() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        assert!(app.finish_turn());
        let _ = app.flush_persist();

        let release_persistence = app.pause_persistence();
        let queued = crate::app::QueuedInput::Delivery(protocol::HistoryNote::notice(
            "background process finished",
        ));
        assert!(app.tui.start_queued_input(queued).is_ok());
        app.wait_for_turn_dispatch();
        assert!(app.agent_running());
        assert!(!app.tui.turn_submission_is_pending());
        release_persistence
            .send(())
            .expect("release session writer");
    }

    #[test]
    fn input_after_a_stopped_turn_starts_a_new_turn() {
        for cancelled in [false, true] {
            let mut app = TestApp::builder().build();
            app.start_submitted_turn("initial request");
            app.flush_persist();
            let release = app.pause_persistence();
            app.discard_turn(if cancelled {
                crate::app::TurnEnd::Cancelled
            } else {
                crate::app::TurnEnd::Complete
            });
            assert!(!app.agent_running());
            app.drain_engine_sends();
            app.type_text("follow-up");
            app.press(KeyCode::Enter);
            app.wait_for_turn_dispatch();
            assert!(queue_stages(&app).is_empty());
            assert!(app.agent_running());
            assert!(app.state().queued_inputs.is_empty());
            assert!(app.actions().iter().all(|action| !matches!(
                action,
                Action::EngineSend(command)
                    if matches!(command.as_ref(), protocol::UiCommand::Steer { .. } | protocol::UiCommand::Cancel)
            )));
            release.send(()).unwrap();
        }
    }

    #[test]
    fn ctrl_c_can_quit_while_a_stopped_turn_is_being_persisted() {
        for cancelled in [false, true] {
            let mut app = TestApp::builder().build();
            app.start_submitted_turn("initial request");
            app.flush_persist();
            let release = app.pause_persistence();
            app.discard_turn(if cancelled {
                crate::app::TurnEnd::Cancelled
            } else {
                crate::app::TurnEnd::Complete
            });
            assert!(app.tui.dispatch_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))));
            release.send(()).unwrap();
            app.flush_persist();
        }
    }

    #[test]
    fn explicit_queue_acceptance_only_clears_cancelled_pauses() {
        use crate::app::agent::TurnPause;
        use crate::app::{QueueStage, QueuedInput};
        use protocol::EngineAskErrorKind;

        for kind in [
            Some(EngineAskErrorKind::Cancelled),
            Some(EngineAskErrorKind::Quota),
            Some(EngineAskErrorKind::RateLimited),
            Some(EngineAskErrorKind::Network),
            None,
        ] {
            for target in [QueueStage::Turn, QueueStage::Request] {
                for full in [false, true] {
                    let mut app = TestApp::builder().build();
                    let pause = TurnPause {
                        kind,
                        retry_at_ms: Some(123_000),
                    };
                    app.tui.conversation.set_turn_pause(Some(pause));
                    if full {
                        for _ in 0..crate::app::queue::MAX_QUEUED_MESSAGES {
                            assert!(app.tui.prompt.try_queue_turn(QueuedInput::request(
                                "queued",
                                protocol::Content::text("queued"),
                                0,
                            )));
                        }
                    }
                    assert_eq!(
                        app.tui.queue_explicit_submission(
                            QueuedInput::request(
                                "follow-up",
                                protocol::Content::text("follow-up"),
                                0,
                            ),
                            target,
                        ),
                        !full,
                    );
                    assert_eq!(
                        app.tui.conversation.turn_pause(),
                        if !full && kind == Some(EngineAskErrorKind::Cancelled) {
                            None
                        } else {
                            Some(pause)
                        },
                    );
                    if !full {
                        let expected = if kind == Some(EngineAskErrorKind::Cancelled) {
                            QueueStage::Turn
                        } else {
                            target
                        };
                        assert_eq!(queue_stages(&app), vec![expected.as_str()]);
                    }
                }
            }
        }
    }

    #[test]
    fn command_after_cancellation_dispatches_without_an_extra_enter() {
        let _guard = lua_command_queue_guard();
        for (busy, modifiers) in [
            ("queue_command", KeyModifiers::NONE),
            ("queue_command", KeyModifiers::CONTROL),
            ("queue_request", KeyModifiers::NONE),
            ("queue_request", KeyModifiers::CONTROL),
        ] {
            let mut app = TestApp::builder().build();
            assert!(app.run_lua(&format!(
                r#"smelt.cmd.register("follow-up", function()
                    smelt.engine.submit_command("follow-up", "next request", nil, "follow-up")
                end, {{ busy = "{busy}" }})"#
            )));
            app.start_submitted_turn("initial request");
            app.flush_persist();
            let release = app.pause_persistence();
            app.discard_turn(crate::app::TurnEnd::Cancelled);
            app.type_text("/follow-up");
            app.press_mod(KeyCode::Enter, modifiers);
            app.wait_for_turn_dispatch();
            assert!(queue_stages(&app).is_empty());
            assert!(app.agent_running(), "command stayed paused: {busy}");
            assert!(app.state().queued_inputs.is_empty());
            release.send(()).unwrap();
        }
    }

    #[test]
    fn interrupt_starts_queued_request_while_the_writer_is_paused() {
        let mut app = TestApp::builder().build();
        app.type_text("first turn");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        assert!(app.agent_running());
        let _ = app.flush_persist();
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::ToolStarted {
                invocation_id: protocol::InvocationId::new(1),
                call_id: "call-1".into(),
                tool_name: "read_file".into(),
                args: std::collections::HashMap::new(),
                called_at_ms: 1,
            },
        ));

        app.type_text("run now");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        let release_persistence = app.pause_persistence();

        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        assert!(app.agent_running(), "queued request did not start");
        assert!(app.state().queued_inputs.is_empty());

        release_persistence
            .send(())
            .expect("release terminal transition");
        let flush = app.flush_persist();
        assert!(
            matches!(flush, crate::persist::PersistenceFlushOutcome::Durable),
            "terminal transition did not become durable: {flush:?}"
        );

        assert!(
            !app.session_is_read_only(),
            "queued request made the session read-only: {}",
            app.tui.conversation.read_only_reason()
        );
        assert!(app.session_snapshot().history.iter().any(|item| {
            matches!(item, protocol::HistoryItem::User { content, .. }
                if content.text_content() == "run now")
        }));

        app.set_session_title(
            "Writable after interrupt".into(),
            "writable-after-interrupt".into(),
            None,
        );
        let _ = app.flush_persist();
        assert_eq!(
            app.session_snapshot().title.as_deref(),
            Some("Writable after interrupt")
        );
    }

    #[test]
    fn empty_enter_promotes_queued_custom_command_expansion() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        assert!(app.run_lua(
            r#"smelt.engine.submit_command("queued-custom", "custom body", nil, "queued-custom")"#
        ));
        assert_eq!(
            app.state().queued_inputs,
            vec!["/queued-custom".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["/queued-custom".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "custom body"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn queue_request_busy_command_enqueues_expanded_request() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("qbody", function(arg)
                    smelt.engine.submit_command(
                        "qbody",
                        "expanded " .. (arg or ""),
                        nil,
                        "qbody " .. (arg or "")
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/qbody arg");
        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["/qbody arg".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "expanded arg"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn queue_request_busy_command_overrides_survive_interrupt_start() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("qoverride", function(_)
                    smelt.engine.submit_command(
                        "qoverride",
                        "override body",
                        { reasoning_effort = "high" },
                        "qoverride"
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/qoverride");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));

        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().text_content() == "override body"
                    && payload.reasoning_effort == protocol::ReasoningEffort::High)
        )));
    }

    #[test]
    fn invalid_busy_callback_rejects_prompt_submission_without_running_or_queueing() {
        for body in ["error('policy failed')", "return 'invalid'", "return {}"] {
            for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
                let mut app = TestApp::builder().build();
                assert!(app.run_lua(&format!(
                    r#"
                    smelt.cmd.register("probe", function() _G.probe_ran = 1 end, {{
                        busy = "run",
                        busy_fn = function(arg)
                            assert(smelt.engine.has_active_turn())
                            assert(arg == "some arguments")
                            {body}
                        end,
                    }})
                "#
                )));
                app.start_turn(1);
                app.type_text("/probe some arguments");
                app.press_mod(KeyCode::Enter, modifiers);
                assert!(app.state().queued_inputs.is_empty());
                assert_eq!(app.lua_int_global("probe_ran"), None);
                assert!(app.lua_messages_contain("cannot resolve /probe busy behavior"));
                assert_eq!(app.current_turn_id(), Some(1));
                assert!(app.agent_running());
            }
        }
    }

    #[test]
    fn blocked_slash_command_does_not_enqueue_while_running() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("blocked", function(_)
                    _G.blocked_ran = 1
                    smelt.engine.submit_command("blocked", "should not run", nil, "blocked")
                end, { busy = "reject" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/blocked arg");
        app.press(KeyCode::Enter);

        assert!(app.state().queued_inputs.is_empty());
        assert_eq!(app.lua_int_global("blocked_ran"), None);
    }

    #[test]
    fn empty_enter_does_not_interrupt_for_turn_only_status_item() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        let note = protocol::HistoryNote::notice("Background process 42 exited.");
        app.tui
            .prompt
            .try_queue_turn(crate::app::QueuedInput::Delivery(note));

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["Background process 42 exited.".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel | protocol::UiCommand::Steer { .. } | protocol::UiCommand::StartTurn(_))
        )));
    }

    #[test]
    fn empty_enter_busy_without_agent_does_not_promote_to_request() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua("_G._busy_handle = smelt.work.busy('busy')"));
        app.type_text("wait for busy");
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(!app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["wait for busy".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn steered_ack_drains_only_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("request now".to_string());
        app.push_queued_message("next turn".to_string());
        app.press(KeyCode::Enter);
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );

        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::Steered {
                turn_id: 1,
                ids: vec![app.sent_steer_id("request now")],
                delta: protocol::CanonicalHistoryDelta::new(
                    app.session_message_count(),
                    vec![
                        protocol::HistoryItem::user(protocol::Content::text("request now"))
                            .with_sent_at_ms(1_742_567_823_000),
                    ],
                ),
            },
        ));

        assert_eq!(app.state().queued_inputs, vec!["next turn".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["next turn".to_string()]);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input, .. }
                    if input.provider_content().text_content() == "next turn")
        )));
    }

    #[test]
    fn interrupt_starts_request_queue_before_turn_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("run first".to_string());
        app.push_queued_message("run second".to_string());
        app.press(KeyCode::Enter);
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );

        app.clear_actions();
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));

        assert!(app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["run second".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().text_content() == "run first")
        )));
    }

    #[test]
    fn empty_enter_continue_does_not_add_user_block() {
        let mut app = TestApp::builder().build();
        app.type_text("before");
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        let first_turn_id = app.current_turn_id().expect("first turn starts");
        app.push_assistant_text("done");
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::TurnComplete {
                turn_id: first_turn_id,
                history: None,
                meta: None,
            },
        ));
        let user_blocks_before = app
            .tui
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.tui.conversation.transcript().history().block(*id))
            .filter(|block| matches!(block, smelt_core::Block::User { .. }))
            .count();
        app.clear_actions();

        app.wait_for_turn_dispatch();
        app.press(KeyCode::Enter);
        app.wait_for_turn_dispatch();
        app.feed_one(crate::app::test_harness::SourceEvent::Tick(0));

        assert!(app.agent_running());
        let history = app.tui.conversation.transcript().history();
        let user_blocks_after = history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter(|block| matches!(block, smelt_core::Block::User { .. }))
            .count();
        assert_eq!(user_blocks_after, user_blocks_before);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().is_empty())
        )));
    }

    #[test]
    fn vim_insert_esc_bypasses_global_esc_prefix_on_focused_surface() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set("", "<Esc><Esc>", function()
                    _G.esc_esc_hit = (_G.esc_esc_hit or 0) + 1
                end)
            "#
        ));
        app.tui.handle_resize(80, 16);
        app.tui
            .push_block(smelt_transcript::transcript_model::Block::Text {
                content: "transcript row".to_string().into(),
            });
        app.render_silent();
        app.tui.app_focus = crate::app::AppFocus::Content;
        app.tui.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app.tui.transcript_win_mut().set_vim_enabled(true);
        app.tui
            .transcript_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Insert);

        app.press(KeyCode::Esc);

        assert_eq!(
            app.tui.transcript_win().vim_mode(),
            crate::smelt_edit::VimMode::Normal
        );
        assert!(app.tui.timers.pending_chord.is_none());
        assert_eq!(app.lua_int_global("esc_esc_hit"), None);
    }

    #[test]
    fn lua_keymap_prefix_enters_pending_state_until_match() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set_leader("<space>")
                smelt.keymap.set("n", "<leader>r", function()
                    _G.leader_hit = (_G.leader_hit or 0) + 1
                end)
            "#
        ));
        app.tui
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));

        assert_eq!(app.state().prompt_text, "");
        assert!(app.tui.timers.pending_chord.is_some());
        app.tui.publish_diff_signals();
        assert_eq!(
            app.tui
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("<space>")
        );

        app.press(KeyCode::Char('r'));

        assert_eq!(app.lua_int_global("leader_hit"), Some(1));
        assert!(app.tui.timers.pending_chord.is_none());
    }

    #[test]
    fn lua_keymap_prefix_clears_on_escape_cancel() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set_leader("<space>")
                smelt.keymap.set("n", "<leader>r", function()
                    _G.leader_hit = (_G.leader_hit or 0) + 1
                end)
            "#
        ));
        app.tui
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));
        app.press(KeyCode::Esc);
        app.tui.publish_diff_signals();

        assert!(app.tui.timers.pending_chord.is_none());
        assert_eq!(
            app.tui
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("")
        );

        app.press(KeyCode::Char('r'));

        assert_eq!(app.lua_int_global("leader_hit"), None);
    }

    #[test]
    fn lua_keymap_prefix_mismatch_passes_current_prompt_key() {
        let mut app = TestApp::builder().build();

        app.press(KeyCode::Esc);

        assert!(app.tui.timers.pending_chord.is_some());

        app.press(KeyCode::Char('a'));

        assert!(app.tui.timers.pending_chord.is_none());
        assert_eq!(app.state().prompt_text, "a");
        assert_eq!(app.prompt_cpos(), 1);
    }

    #[test]
    fn vim_insert_plain_less_than_inserts_despite_named_key_prefixes() {
        let mut app = TestApp::builder()
            .with_vim(true)
            .with_mode(AgentMode::parse("write").unwrap())
            .build();

        app.press(KeyCode::Char('<'));

        assert!(app.tui.timers.pending_chord.is_none());
        assert_eq!(app.state().prompt_text, "<");
        assert_eq!(app.prompt_cpos(), 1);
    }

    #[test]
    fn transient_escape_prefix_expires() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set("", "<Esc><Esc>", function()
                    _G.esc_esc_hit = (_G.esc_esc_hit or 0) + 1
                end)
            "#
        ));

        app.press(KeyCode::Esc);
        assert!(app.tui.timers.pending_chord.is_some());

        app.feed_one(crate::app::test_harness::SourceEvent::Tick(
            crate::app::ESC_CHORD_TIMEOUT_MS + 1,
        ));
        app.tui.publish_diff_signals();

        assert!(app.tui.timers.pending_chord.is_none());
        assert_eq!(
            app.tui
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("")
        );

        app.press(KeyCode::Esc);

        assert_eq!(app.lua_int_global("esc_esc_hit"), None);
        assert!(app.tui.timers.pending_chord.is_some());
    }

    #[test]
    fn global_mode_toggle_wins_before_prompt_placeholder_accept_key() {
        let mut app = TestApp::builder().build();
        app.install_prompt_placeholder(
            "draft".to_string(),
            vec![crate::smelt_edit::KeyBind {
                code: KeyCode::BackTab,
                mods: KeyModifiers::NONE,
            }],
            Vec::new(),
        );
        assert_eq!(app.tui.core.config.mode, AgentMode::read());

        app.press(KeyCode::BackTab);

        assert_eq!(app.tui.core.config.mode, AgentMode::parse("plan").unwrap());
        assert_eq!(
            app.state().prompt_text,
            "",
            "prompt placeholder must not accept a globally-handled chord"
        );
    }

    #[test]
    fn modal_overlay_swallows_cmdline_open_key() {
        let mut app = TestApp::builder().build();
        let buf = app
            .tui
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        let win = app
            .tui
            .ui
            .win_open_split(
                buf,
                crate::smelt_edit::SplitConfig {
                    gutters: crate::smelt_edit::Gutters::default(),
                },
            )
            .expect("overlay test window opens");
        app.tui.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(win),
                crate::smelt_edit::Anchor::ScreenCenter,
            )
            .modal(true),
        );
        assert!(app.state().active_modal.is_some());

        app.press(KeyCode::Char(':'));

        let state = app.state();
        assert!(state.active_modal.is_some());
        assert!(
            !state.cmdline_open,
            "':' must stay inside overlay routing while a modal overlay is active"
        );
        assert_eq!(state.prompt_text, "");
    }

    #[test]
    fn busy_submit_queues_without_starting_agent_turn() {
        let mut app = TestApp::builder().build();
        app.tui.busy_stack.push("background".to_string());
        app.type_text("run later");

        app.press(KeyCode::Enter);

        let state = app.state();
        assert!(
            !state.agent_running,
            "busy-only submit should not start an agent turn immediately"
        );
        assert_eq!(state.queued_inputs, vec!["run later".to_string()]);
        assert_eq!(state.prompt_text, "");
    }
}
