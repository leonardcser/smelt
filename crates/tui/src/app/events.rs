use crate::app::{
    AppSequenceAction, CommandAction, EventOutcome, InputOutcome, QueueStage, QueuedInput, TuiApp,
};

use crate::input::Action;
use crate::keymap::{self, KeyAction};
use crate::smelt_edit::UiHost;
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
                if self.handle_focused_search_key(k) {
                    return false;
                }
                if self.handle_search_open_before_window_dispatch(k) {
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

        let outcome = if self.agent.is_some() || self.busy_stack.is_busy() {
            self.handle_event_running(ev)
        } else {
            self.handle_event_idle(ev)
        };

        // Notify Lua subscribers if the prompt buffer changed (drives filter-as-you-type pickers).
        self.emit_prompt_text_changed_if_dirty();

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
                self.interrupt_with_next_queued();
                false
            }
            EventOutcome::ContinueTurn => {
                let turn = self.begin_agent_turn("", protocol::Content::text(""));
                self.agent = Some(turn);
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
                    if !content.is_empty() {
                        self.queued_inputs
                            .try_push_turn(QueuedInput::request(display.clone(), content));
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
                    } else {
                        let outcome = self.handle_empty_submit();
                        return self.apply_event_outcome(outcome);
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
            crate::app::APP_SEQUENCE_TIMEOUT_MS,
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

    fn handle_focused_search_key(&mut self, k: KeyEvent) -> bool {
        let Some(win) = self.ui.focus() else {
            return false;
        };
        self.handle_search_key_for_target(win, k)
    }

    fn handle_search_open_before_window_dispatch(&mut self, k: KeyEvent) -> bool {
        self.try_open_search_for_key(k)
    }

    pub(crate) fn try_open_search_for_key(&mut self, k: KeyEvent) -> bool {
        match (k.code, k.modifiers) {
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                self.open_search_input(crate::app::search::SearchDirection::Forward)
            }
            (KeyCode::Char('?'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.open_search_input(crate::app::search::SearchDirection::Backward)
            }
            _ => false,
        }
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

    fn global_lua_keymaps_can_handle(&self, key: KeyEvent) -> bool {
        if matches!(key.code, KeyCode::Esc)
            && key.modifiers == KeyModifiers::NONE
            && self.timers.pending_chord.is_none()
        {
            if self.prompt_escape_owned_by_vim() {
                return false;
            }
            if matches!(
                self.focused_vim_mode(),
                Some(
                    crate::smelt_edit::VimMode::Insert
                        | crate::smelt_edit::VimMode::Visual
                        | crate::smelt_edit::VimMode::VisualLine
                )
            ) {
                return false;
            }
        }
        true
    }

    fn pending_lua_keymap_cancelled_by(
        &self,
        key: KeyEvent,
        token: &str,
        vim_mode: Option<&str>,
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
        !self.lua.chord_has_binding(&candidate, vim_mode)
            && !self.lua.chord_has_longer(&candidate, vim_mode)
    }

    fn dispatch_single_global_lua_keymap(&mut self, key: KeyEvent) -> bool {
        if !self.global_lua_keymaps_can_handle(key) {
            return false;
        }
        let Some(token) = crate::lua::chord_string(key) else {
            return false;
        };
        let vim_mode = self.current_vim_mode_label();
        use smelt_core::lua::runtime::KeymapResult;
        match self.lua.run_keymap(&token, vim_mode.as_deref(), None) {
            KeymapResult::Consumed => {
                self.flush_lua_callbacks();
                true
            }
            KeymapResult::PassThrough | KeymapResult::NoBinding => {
                self.flush_lua_callbacks();
                false
            }
        }
    }

    fn dispatch_global_lua_keymap(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        if !self.global_lua_keymaps_can_handle(key) {
            return None;
        }

        let Some(token) = crate::lua::chord_string(key) else {
            return self.timers.pending_chord.take().map(|_| EventOutcome::Noop);
        };

        let had_pending = self.timers.pending_chord.is_some();
        let vim_mode = self.current_vim_mode_label();
        if self.pending_lua_keymap_cancelled_by(key, &token, vim_mode.as_deref()) {
            self.timers.pending_chord = None;
            return Some(EventOutcome::Noop);
        }
        use smelt_core::lua::runtime::KeymapResult;

        // If no sequence is pending, try exact single-key bindings first. Once a
        // prefix is pending, the next key belongs to that sequence before any
        // single-key binding gets a chance, matching Vim's modal mapping feel.
        if !had_pending {
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
        }

        if self.timers.pending_chord.is_none() {
            self.timers.pending_chord = Some(crate::app::PendingChord {
                tokens: Vec::new(),
                vim_mode_at_start: if self.input.vim_enabled(self.prompt_win()) {
                    Some(self.prompt_win().vim_mode())
                } else {
                    None
                },
            });
        }
        let (mut tokens, vim_mode_at_start) = {
            let p = self.timers.pending_chord.take().unwrap();
            (p.tokens, p.vim_mode_at_start)
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
            smelt_core::keymap::ChordOutcome::Consumed => Some(EventOutcome::Noop),
            smelt_core::keymap::ChordOutcome::Pending { tokens } => {
                if tokens.is_empty() {
                    self.timers.pending_chord = None;
                } else {
                    self.timers.pending_chord = Some(crate::app::PendingChord {
                        tokens,
                        vim_mode_at_start,
                    });
                    return Some(EventOutcome::Noop);
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
    /// Dispatch priority: resize/mouse → content search opener → window-local Lua
    /// keymaps → global Lua keymaps → pane chords → cmdline `:` → content focus → overlay keys.
    fn dispatch_common(&mut self, ev: &Event) -> Option<EventOutcome> {
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        if let Event::Key(k) = *ev {
            if matches!(self.app_focus, crate::app::AppFocus::Content)
                && self.handle_search_open_before_window_dispatch(k)
            {
                return Some(EventOutcome::Noop);
            }
            if let Some(outcome) = self.dispatch_window_lua_keymap(k) {
                return Some(outcome);
            }
            if let Some(outcome) = self.dispatch_global_lua_keymap(k) {
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
            let in_insert = match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    !self.input.vim_enabled(self.prompt_win())
                        || self.prompt_win().vim_mode() == crate::smelt_edit::VimMode::Insert
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
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
            &mut self.core.clipboard,
            now,
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
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        self.timers.last_ctrlc = Some(self.core.clock.instant_now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.input.clear(&mut pctx);
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
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let input_action = self.input.handle_event(
            &mut pctx,
            ev,
            Some(&mut self.input_history),
            &mut self.core.clipboard,
            now,
        );
        match input_action {
            Action::Submit { content, display } => {
                return self.handle_running_submit(content, display, QueueStage::Turn);
            }
            Action::SubmitToRequestQueue { content, display } => {
                return self.handle_running_submit(content, display, QueueStage::Request);
            }
            Action::SubmitEmpty => {
                return self.handle_empty_submit();
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

    fn handle_running_submit(
        &mut self,
        mut content: protocol::Content,
        mut display: String,
        target: QueueStage,
    ) -> EventOutcome {
        self.clear_prompt_prediction();
        self.redact_user_submission(&mut content, &mut display);
        let text = content.text_content();
        if content.image_count() == 0 {
            if let Some(outcome) = self.try_command_while_running(text.trim(), target) {
                return outcome;
            }
        }
        if content.is_empty() {
            return EventOutcome::Noop;
        }
        if target == QueueStage::Request && content.image_count() > 0 {
            self.restore_submission_to_prompt(display, &content);
            self.notify_error(
                "cannot use image attachments to steer the current response; prompt restored"
                    .into(),
            );
            return EventOutcome::Noop;
        }
        let queued = QueuedInput::request(display, content);
        match target {
            QueueStage::Turn => {
                self.queued_inputs.try_push_turn(queued);
            }
            QueueStage::Request => {
                self.queue_input_for_request(queued);
            }
        }
        EventOutcome::Noop
    }

    fn restore_submission_to_prompt(&mut self, display: String, content: &protocol::Content) {
        let images = match content {
            protocol::Content::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    protocol::ContentPart::ImageUrl { url, label } => Some((
                        label.clone().unwrap_or_else(|| "image".to_string()),
                        url.clone(),
                    )),
                    protocol::ContentPart::Text { .. } => None,
                })
                .collect(),
            protocol::Content::Text(_) => Vec::new(),
        };
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        if images.is_empty() {
            self.input.replace_text(&mut pctx, display);
        } else {
            self.input.restore_from_rewind(&mut pctx, display, images);
        }
    }

    fn handle_empty_submit(&mut self) -> EventOutcome {
        if self.agent.is_some() {
            self.clear_prompt_prediction();
            if self.queued_inputs.has_request() {
                return EventOutcome::InterruptWithQueued;
            }
            if self.queued_inputs.front_turn_is_request() {
                self.promote_next_queued_turn_to_request();
            }
            return EventOutcome::Noop;
        }

        if self.busy_stack.is_busy() {
            self.clear_prompt_prediction();
            return EventOutcome::Noop;
        }

        if !self.queued_inputs.is_empty() {
            if let Some(queued) = self.queued_inputs.pop_next_for_turn() {
                self.start_queued_input(queued);
            }
            return EventOutcome::Noop;
        }

        if self.can_continue_turn() {
            EventOutcome::ContinueTurn
        } else {
            EventOutcome::Noop
        }
    }

    fn promote_next_queued_turn_to_request(&mut self) {
        let Some(queued) = self.queued_inputs.promote_turn_to_request() else {
            return;
        };
        let Some(text) = queued.request_text().map(str::to_string) else {
            return;
        };
        if !text.is_empty() {
            self.core.engine.send(protocol::UiCommand::Steer { text });
        }
    }

    fn interrupt_with_next_queued(&mut self) {
        let (unsteer_count, next, remaining) = self.queued_inputs.take_for_interrupt();
        if unsteer_count > 0 {
            self.core.engine.send(protocol::UiCommand::Unsteer {
                count: unsteer_count,
            });
        }
        self.discard_turn(true);
        self.queued_inputs = remaining;
        if let Some(queued) = next {
            self.start_queued_input(queued);
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn handle_idle_prompt_esc(&mut self, ev: Event, modifiers: KeyModifiers) -> EventOutcome {
        if self.prompt_escape_owned_by_vim() {
            let vim_mode_at_start = self
                .prompt_win()
                .vim_enabled()
                .then(|| self.prompt_win().vim_mode());
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
            self.arm_prompt_esc_lua_prefix(vim_mode_at_start);
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
        let now = self.core.clock.instant_now();

        if self.prompt_escape_owned_by_vim() {
            let vim_mode_at_start = self
                .prompt_win()
                .vim_enabled()
                .then(|| self.prompt_win().vim_mode());
            self.apply_prompt_escape_to_input(ev, now);
            self.arm_prompt_esc_lua_prefix(vim_mode_at_start);
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

        EventOutcome::Noop
    }

    fn prompt_escape_owned_by_vim(&self) -> bool {
        if !self.input.vim_enabled(self.prompt_win()) {
            return false;
        }
        match self.prompt_win().vim_mode() {
            crate::smelt_edit::VimMode::Insert
            | crate::smelt_edit::VimMode::Visual
            | crate::smelt_edit::VimMode::VisualLine => true,
            crate::smelt_edit::VimMode::Normal => !self.prompt_win().vim_state().is_idle(),
        }
    }

    fn apply_prompt_escape_to_input(&mut self, ev: Event, now: std::time::Instant) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.input
            .handle_event(&mut pctx, ev, None, &mut self.core.clipboard, now);
    }

    fn arm_prompt_esc_lua_prefix(&mut self, vim_mode_at_start: Option<crate::smelt_edit::VimMode>) {
        if self.timers.pending_chord.is_some() {
            return;
        }
        let vim_mode = self.current_vim_mode_label();
        if !self.lua.chord_has_longer("<Esc>", vim_mode.as_deref()) {
            return;
        }
        self.timers.pending_chord = Some(crate::app::PendingChord {
            tokens: vec!["<Esc>".to_string()],
            vim_mode_at_start,
        });
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
            Action::Submit { content, display }
            | Action::SubmitToRequestQueue { content, display } => {
                EventOutcome::Submit { content, display }
            }
            Action::SubmitEmpty => self.handle_empty_submit(),
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
            .dispatch_event(crate::smelt_edit::Event::Resize(w, h), &mut |_, _, _| {});
        if width_changed {
            self.ui.cancel_pointer_interaction();
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
        if smelt_core::commands::registered_command_token(&dispatch_input).is_some() {
            return InputOutcome::Continue;
        }
        // Shell escapes (`!cmd`) skip agent start, but pasted content starting with `!` does not.
        if trimmed.starts_with('!') && !is_from_paste {
            return InputOutcome::Continue;
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
        self.placeholder_opts.remove(&win_id);
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
        let show_thinking = self.core.config.settings.show_thinking;
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        let snapped = self.snap_cpos_to_selectable(&rows, cpos, show_thinking);
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
    pub(crate) fn run_key_cascade(&mut self, k: KeyEvent) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;

        // Tier 1: specific keymap on the focused leaf.
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
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
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
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
        if self.dispatch_single_global_lua_keymap(k) {
            return Status::Consumed;
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

    fn sync_viewer_clipboard_from_kill_ring(
        &mut self,
        buf_id: crate::smelt_edit::BufId,
        yank_tick_before: u64,
    ) {
        if self.core.clipboard.kill_ring.yank_tick() == yank_tick_before {
            return;
        }
        if let Some(buf) = self.ui.buf(buf_id) {
            buf.sync_clipboard_from_kill_ring(&mut self.core.clipboard);
        }
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
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;
        if self.handle_search_key_for_target(win_id, k) {
            return Status::Consumed;
        }
        let (vim_enabled, buf_id, viewport_rows) = match self.ui.win(win_id) {
            Some(w) => (
                w.vim_enabled(),
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
            let has_materialized_rows = self
                .ui
                .win(win_id)
                .is_some_and(|win| win.has_materialized_rows());
            if has_materialized_rows {
                let command = {
                    let win = self.ui.win_mut(win_id).expect("window");
                    win.handle_row_viewer_key(k)
                };
                if let Some(command) = command {
                    let command = self
                        .resolve_row_viewer_command(win_id, command)
                        .unwrap_or(command);
                    let now = self.core.clock.instant_now();
                    let copied = {
                        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                        let win = win.expect("window");
                        let buf = buf.expect("buffer");
                        win.execute_row_viewer_command(buf, command, viewport_rows, now)
                    };
                    if let Some(range) = copied {
                        if let Some(out) =
                            crate::smelt_edit::UiHost::copy_document_range(self, win_id, range)
                        {
                            self.yank_to_clipboard(out);
                        }
                    }
                    return Status::Consumed;
                }
            }

            let yank_tick_before = self.core.clipboard.kill_ring.yank_tick();
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            let now = self.core.clock.instant_now();
            let status = win.handle_key(buf, k, &mut self.core.clipboard, now);
            win.update_tail_state(buf, viewport_rows);
            if matches!(status, Status::Consumed) {
                self.sync_viewer_clipboard_from_kill_ring(buf_id, yank_tick_before);
                return Status::Consumed;
            }
            // Vim Passthrough (Shift+Arrows, etc.) falls through so the
            // shared keymap layer can claim selection-extend chords -
            // matches the prompt's behaviour.
        }

        self.dispatch_buffer_action(win_id, buf_id, k, viewport_rows)
    }

    fn resolve_row_viewer_command(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        command: crate::smelt_edit::ViewerCommand,
    ) -> Option<crate::smelt_edit::ViewerCommand> {
        use crate::smelt_edit::{text, DocPosition, UiHost, ViewerCommand};

        #[derive(Clone, Copy)]
        enum WordKind {
            Forward,
            Backward,
            End,
        }

        let (kind, count) = match command {
            ViewerCommand::WordForward(count) => (WordKind::Forward, count.max(1)),
            ViewerCommand::WordBackward(count) => (WordKind::Backward, count.max(1)),
            ViewerCommand::WordEnd(count) => (WordKind::End, count.max(1)),
            _ => return Some(command),
        };
        let total_rows = UiHost::document_total_rows(self, win_id)?;
        if total_rows == 0 {
            return None;
        }
        let mut pos = self.ui.win(win_id)?.row_cursor()?;
        pos.row = pos.row.min(total_rows.saturating_sub(1));

        for _ in 0..count {
            match kind {
                WordKind::Forward => {
                    let line = self.row_line(win_id, pos.row)?;
                    let col = text::word_forward_pos(&line, pos.byte_col, text::CharClass::Word);
                    if col < line.len() || pos.row + 1 >= total_rows {
                        pos.byte_col = col;
                    } else {
                        pos.row = pos.row.saturating_add(1).min(total_rows.saturating_sub(1));
                        pos.byte_col = 0;
                    }
                }
                WordKind::Backward => {
                    let line = self.row_line(win_id, pos.row)?;
                    let col = text::word_backward_pos(&line, pos.byte_col, text::CharClass::Word);
                    if col > 0 || pos.row == 0 {
                        pos.byte_col = col;
                    } else {
                        pos.row = pos.row.saturating_sub(1);
                        pos.byte_col = self.row_line(win_id, pos.row)?.len();
                    }
                }
                WordKind::End => {
                    let line = self.row_line(win_id, pos.row)?;
                    let col = text::word_end_pos(&line, pos.byte_col, text::CharClass::Word);
                    if col > pos.byte_col || pos.row + 1 >= total_rows {
                        pos.byte_col = col;
                    } else {
                        pos.row = pos.row.saturating_add(1).min(total_rows.saturating_sub(1));
                        pos.byte_col = 0;
                    }
                }
            }
        }

        Some(ViewerCommand::GotoPosition(DocPosition {
            row: pos.row,
            byte_col: pos.byte_col,
        }))
    }

    fn row_line(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<String> {
        crate::smelt_edit::UiHost::display_rows_for_range(self, win_id, row, 1)?
            .into_text_rows()
            .into_iter()
            .next()
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
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        k: KeyEvent,
        viewport_rows: u16,
    ) -> crate::smelt_edit::Status {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crate::smelt_edit::{Status, VimMode};

        let (vim_enabled, vim_mode, readonly, buf_empty) =
            match (self.ui.win(win_id), self.ui.buf(buf_id)) {
                (Some(w), Some(b)) => (
                    w.vim_enabled(),
                    w.vim_mode(),
                    b.readonly,
                    b.text().is_empty(),
                ),
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

        if self
            .ui
            .win(win_id)
            .is_some_and(|win| win.has_materialized_rows())
        {
            let extending = matches!(
                action,
                KeyAction::SelectUp
                    | KeyAction::SelectDown
                    | KeyAction::SelectStartOfLine
                    | KeyAction::SelectEndOfLine
                    | KeyAction::SelectWordForward
                    | KeyAction::SelectWordBackward
            );
            let command = match action {
                KeyAction::MoveUp | KeyAction::SelectUp => {
                    Some(crate::smelt_edit::ViewerCommand::MoveRows(-1))
                }
                KeyAction::MoveDown | KeyAction::SelectDown => {
                    Some(crate::smelt_edit::ViewerCommand::MoveRows(1))
                }
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(crate::smelt_edit::ViewerCommand::WordForward(1))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(crate::smelt_edit::ViewerCommand::WordBackward(1))
                }
                KeyAction::PageUp => Some(crate::smelt_edit::ViewerCommand::PageRows(-1)),
                KeyAction::PageDown => Some(crate::smelt_edit::ViewerCommand::PageRows(1)),
                KeyAction::HalfPageUp => Some(crate::smelt_edit::ViewerCommand::MoveRows(
                    -((viewport_rows as isize) / 2).max(1),
                )),
                KeyAction::HalfPageDown => Some(crate::smelt_edit::ViewerCommand::MoveRows(
                    ((viewport_rows as isize) / 2).max(1),
                )),
                KeyAction::ScrollLineUp => Some(crate::smelt_edit::ViewerCommand::ScrollRows(-1)),
                KeyAction::ScrollLineDown => Some(crate::smelt_edit::ViewerCommand::ScrollRows(1)),
                KeyAction::MoveStartOfBuffer => Some(crate::smelt_edit::ViewerCommand::BufferStart),
                KeyAction::MoveEndOfBuffer => Some(crate::smelt_edit::ViewerCommand::BufferEnd),
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                    Some(crate::smelt_edit::ViewerCommand::LineStart)
                }
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                    Some(crate::smelt_edit::ViewerCommand::LineEnd)
                }
                KeyAction::CopySelection => Some(crate::smelt_edit::ViewerCommand::YankSelection),
                _ => None,
            };
            if let Some(command) = command {
                let command = self
                    .resolve_row_viewer_command(win_id, command)
                    .unwrap_or(command);
                let now = self.core.clock.instant_now();
                let copied = {
                    let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                    let win = win.expect("window");
                    let buf = buf.expect("buffer");
                    if extending && !win.row_selection_anchor_active() {
                        win.execute_row_viewer_command(
                            buf,
                            crate::smelt_edit::ViewerCommand::StartVisual,
                            viewport_rows,
                            now,
                        );
                    } else if !extending
                        && !matches!(command, crate::smelt_edit::ViewerCommand::YankSelection)
                    {
                        win.execute_row_viewer_command(
                            buf,
                            crate::smelt_edit::ViewerCommand::ClearSelection,
                            viewport_rows,
                            now,
                        );
                    }
                    win.execute_row_viewer_command(buf, command, viewport_rows, now)
                };
                if let Some(range) = copied {
                    if let Some(out) =
                        crate::smelt_edit::UiHost::copy_document_range(self, win_id, range)
                    {
                        self.yank_to_clipboard(out);
                    }
                }
                return Status::Consumed;
            }
        }

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
            KeyAction::MoveLeft | KeyAction::SelectLeft => {
                Some(crate::smelt_edit::text::prev_char_boundary(&text, cpos))
            }
            KeyAction::MoveRight | KeyAction::SelectRight => {
                Some(crate::smelt_edit::text::next_char_boundary(&text, cpos))
            }
            KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                Some(crate::smelt_edit::text::line_start(&text, cpos))
            }
            KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                Some(crate::smelt_edit::text::line_end(&text, cpos))
            }
            KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                Some(crate::smelt_edit::text::word_forward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                Some(crate::smelt_edit::text::word_backward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            KeyAction::MoveStartOfBuffer => Some(0),
            KeyAction::MoveEndOfBuffer => Some(text.len()),
            KeyAction::CopySelection => {
                let win = self.ui.win(win_id).expect("window");
                if let Some((s, e)) = win.selection_range(buf) {
                    let s = crate::smelt_edit::text::snap(&text, s);
                    let e = crate::smelt_edit::text::snap(&text, e);
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
    lua: &'a crate::lua::LuaRuntime,
    vim_mode: Option<&'a str>,
    vim_mode_at_start: Option<crate::smelt_edit::VimMode>,
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

#[cfg(test)]
mod tests {
    use crate::app::test_harness::{Action, TestApp};
    use crossterm::event::{KeyCode, KeyModifiers};
    use protocol::AgentMode;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    #[test]
    fn running_esc_esc_cancels_but_single_esc_does_not() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);

        app.press(KeyCode::Esc);
        assert!(
            app.agent_running(),
            "first Esc should only arm the app-level Esc Esc sequence"
        );

        app.press(KeyCode::Esc);
        assert!(
            !app.agent_running(),
            "second Esc should resolve the app-level hard-cancel sequence"
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
        app.app.queued_inputs.display_kinds()
    }

    fn insert_prompt_image(app: &mut TestApp, label: &str) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut app.app.ui);
        app.app.input.insert_image(
            &mut pctx,
            label.to_string(),
            "data:image/png;base64,AAAA".to_string(),
        );
    }

    fn lua_command_queue_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "check this first")
        )));
    }

    #[test]
    fn enter_queues_image_prompt_for_next_turn_without_dropping_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["see [pic.png]".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn request_queue_with_image_restores_prompt_instead_of_dropping_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        let state = app.state();
        assert!(state.queued_inputs.is_empty());
        assert_eq!(
            state.prompt_text,
            format!("see {}", crate::input::ATTACHMENT_MARKER)
        );
        assert!(app.app.notification.is_some());
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "right now")
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
                end, { while_busy = false, queue_when_busy = true })
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "expanded now")
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

        assert!(app.agent_running());
        assert!(app.state().queued_inputs.is_empty());
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Unsteer { count } if *count == 1)
        )));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel)
        )));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.content.text_content() == "run now")
        )));
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "custom body")
        )));
    }

    #[test]
    fn queue_when_busy_slash_command_enqueues_expanded_request() {
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
                end, { while_busy = false, queue_when_busy = true })
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "expanded arg")
        )));
    }

    #[test]
    fn queue_when_busy_command_overrides_survive_interrupt_start() {
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
                end, { while_busy = false, queue_when_busy = true })
            "#
        ));
        app.start_turn(1);
        app.type_text("/qoverride");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.content.text_content() == "override body"
                    && payload.reasoning_effort == protocol::ReasoningEffort::High)
        )));
    }

    #[test]
    fn blocked_slash_command_does_not_enqueue_while_running() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("blocked", function(_)
                    _G.blocked_ran = 1
                    smelt.engine.submit_command("blocked", "should not run", nil, "blocked")
                end, { while_busy = false })
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
        let note = "Background process 42 exited.".to_string();
        app.app
            .queued_inputs
            .try_push_turn(crate::app::QueuedInput::ProcessStatus(note.clone()));

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(app.state().queued_inputs, vec![note]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel | protocol::UiCommand::Steer { .. } | protocol::UiCommand::StartTurn(_))
        )));
    }

    #[test]
    fn empty_enter_while_busy_without_agent_does_not_promote_to_request() {
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

        app.feed_one(crate::app::test_harness::SourceEvent::Engine(
            protocol::EngineEvent::Steered {
                text: "request now".to_string(),
                count: 1,
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
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { text } if text == "next turn")
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

        assert!(app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["run second".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.content.text_content() == "run first")
        )));
    }

    #[test]
    fn empty_enter_continue_does_not_add_user_block() {
        let mut app = TestApp::builder().build();
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                "before",
            )));
        app.push_assistant_text("done");

        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        let history = app.app.transcript.history();
        let users = history
            .order
            .iter()
            .filter_map(|id| history.blocks.get(id))
            .filter_map(|block| match block {
                smelt_core::Block::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(users.is_empty());
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.content.is_empty())
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
        app.app.handle_resize(80, 16);
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: "transcript row".to_string(),
            });
        app.render_silent();
        app.app.app_focus = crate::app::AppFocus::Content;
        app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app.app.transcript_win_mut().set_vim_enabled(true);
        app.app
            .transcript_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Insert);

        app.press(KeyCode::Esc);

        assert_eq!(
            app.app.transcript_win().vim_mode(),
            crate::smelt_edit::VimMode::Normal
        );
        assert!(app.app.timers.pending_chord.is_none());
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
        app.app
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));

        assert_eq!(app.state().prompt_text, "");
        assert!(app.app.timers.pending_chord.is_some());
        app.app.publish_diff_cells();
        assert_eq!(
            app.app
                .core
                .cells
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("<space>")
        );

        app.press(KeyCode::Char('r'));

        assert_eq!(app.lua_int_global("leader_hit"), Some(1));
        assert!(app.app.timers.pending_chord.is_none());
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
        app.app
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));
        app.press(KeyCode::Esc);
        app.app.publish_diff_cells();

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(
            app.app
                .core
                .cells
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

        assert!(app.app.timers.pending_chord.is_some());

        app.press(KeyCode::Char('a'));

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(app.state().prompt_text, "a");
        assert_eq!(app.prompt_cpos(), 1);
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
        assert_eq!(app.app.core.config.mode, AgentMode::normal());

        app.press(KeyCode::BackTab);

        assert_eq!(app.app.core.config.mode, AgentMode::parse("plan").unwrap());
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
            .app
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        let win = app
            .app
            .ui
            .win_open_split(
                buf,
                crate::smelt_edit::SplitConfig {
                    region: "event-test-overlay".into(),
                    gutters: crate::smelt_edit::Gutters::default(),
                },
            )
            .expect("overlay test window opens");
        app.app.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(win),
                crate::smelt_edit::layout::Anchor::ScreenCenter,
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
        app.app.busy_stack.push("background".to_string());
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
