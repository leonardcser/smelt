use super::*;

impl TestApp {
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Force the app into "agent turn active" state with the given
    /// `turn_id`. Subsequent `SourceEvent::Engine(_)` events flow through
    /// the active-turn dispatch path (`handle_engine_event` and
    /// `dispatch_control` with tool tracking) instead of the idle
    /// handler. No-op if a turn is already running.
    ///
    /// Used by the fuzz target to reach engine code paths a user would
    /// reach by submitting a prompt, without going through the full
    /// HTTP/auth-bearing `begin_agent_turn` flow.
    pub fn start_turn(&mut self, turn_id: u64) {
        if self.app.agent.is_some() {
            return;
        }
        self.app.context_tokens_updated_this_turn = false;
        self.app.agent = Some(crate::app::TurnState {
            turn_id,
            pending: Vec::new(),
            permissions: self.app.core.permissions.clone(),
            _perf: smelt_perf::perf::begin("test_harness:turn"),
        });
        // Production `dispatch_turn` flips `working` into `Working` phase
        // at the same point it sets `agent = Some(...)`. The harness short-
        // circuits the HTTP-bearing dispatch path; we still need to mirror
        // the working-state transition so the
        // `working.is_animating() => agent.is_some()` invariant in
        // `assert_invariants` holds in both directions.
        self.app
            .working
            .begin(smelt_core::working::TurnPhase::Working);
    }

    /// Whether an agent turn is currently active.
    pub fn agent_running(&self) -> bool {
        self.app.agent_is_running()
    }

    /// Snapshot the pending tool `call_id`s on the active turn. Empty
    /// vector when no turn is active. Used by transitional invariants
    /// that need to compare pending state before vs. after an event
    /// (e.g. asserting a `ToolFinished` actually cleared its entry).
    pub fn pending_tool_call_ids(&self) -> Vec<String> {
        self.app
            .agent
            .as_ref()
            .map(|ag| ag.pending.iter().map(|pt| pt.call_id.clone()).collect())
            .unwrap_or_default()
    }

    /// Whether streaming `text` / `thinking` / exec buffers currently hold
    /// uncommitted content. Used by post-event invariants that assert a
    /// specific event flushed the relevant buffer.
    pub fn streaming_state(&self) -> StreamingState {
        StreamingState {
            text: self.app.parser.has_active_text(),
            thinking: self.app.parser.has_active_thinking(),
            exec: self.app.parser.has_active_exec(),
        }
    }

    /// Length of `session.history`. Used by post-event invariants that
    /// assert compaction or `set_history` replaced the conversation.
    pub fn session_message_count(&self) -> usize {
        self.app.core.session.history.len()
    }

    /// `turn_id` of the active agent turn, if any. Used by fuzz ops that
    /// synthesize engine events whose dispatch is gated on a matching id
    /// (e.g. `TurnComplete`, `Messages`).
    pub fn current_turn_id(&self) -> Option<u64> {
        self.app.active_agent_turn_id()
    }

    /// Number of messages already promoted into the active turn's request
    /// queue. Used by `Steered` invariants that assert ack drain semantics.
    pub fn queued_message_count(&self) -> usize {
        self.app.queued_inputs.request_len()
    }

    /// Whether completing the active turn would immediately start a queued
    /// input that appends one user-visible history item.
    pub fn next_queued_input_writes_history(&self) -> bool {
        self.app
            .queued_inputs
            .display_texts()
            .first()
            .is_some_and(|text| !text.is_empty())
    }

    /// Side-channel: push a synthetic queued message. In production
    /// `queued_inputs` is filled by pressing Enter on the prompt while a
    /// turn is active; the harness short-circuits that flow but honors
    /// the same `MAX_QUEUED_MESSAGES` cap so the fuzz observes the real
    /// drop-on-overflow behavior instead of unbounded growth.
    pub fn push_queued_message(&mut self, text: String) {
        self.app
            .queued_inputs
            .try_push_turn(crate::app::QueuedInput::request_from_text(
                text.clone(),
                text,
            ));
    }

    /// Side-channel: seed prompt history without submitting through the engine.
    /// Storybook uses this to open reverse-history UI deterministically.
    pub fn push_history_entry(&mut self, text: String) {
        self.app.input_history.push(text);
    }

    /// Snapshot of the working-status bar's live state. Used by fuzz
    /// invariants that assert phase transitions (e.g. compaction ends with
    /// `animating == false`, `Retrying` event leaves `animating == true`).
    pub fn working_state(&self) -> WorkingSnapshot {
        WorkingSnapshot {
            animating: self.app.working.is_animating(),
            busy: self.app.busy_stack.is_busy(),
        }
    }

    /// Accumulated session cost in USD. Used by `TokenUsage` invariants
    /// asserting cost is monotonically non-decreasing.
    pub fn session_cost_usd(&self) -> f64 {
        self.app.core.session.session_cost_usd
    }

    /// Current authoritative context-token snapshot for the live history, when
    /// a non-background usage report has set one.
    pub fn context_tokens(&self) -> Option<u32> {
        self.app.core.session.current_context_tokens()
    }

    /// Active context checkpoint prefix length, if compaction installed one.
    /// `TuiApp::set_history` preserves this prefix and merges incoming model
    /// history after it.
    pub fn checkpoint_first_live_index(&self) -> Option<usize> {
        self.app
            .core
            .session
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index)
    }

    /// Number of deferred model-history notes that will be committed when the
    /// active turn finishes successfully.
    pub fn pending_history_append_count(&self) -> usize {
        self.app.pending_history_appends.len()
    }

    /// Set the configured context window size used by the prompt bar's
    /// percentage display.
    pub fn set_context_window(&mut self, context_window: Option<u32>) {
        self.app.core.config.context_window = context_window;
    }

    /// Number of transcript blocks. Used by event invariants that assert
    /// a block was pushed (e.g. `ProcessCompleted`).
    pub fn transcript_block_count(&self) -> usize {
        self.app.transcript.history().len()
    }

    /// Number of confirm dialogs currently registered with the core. Used
    /// by `RequestPermission` invariants to assert the dispatch either
    /// auto-approved (no confirm registered) or registered exactly one.
    pub fn pending_confirm_count(&self) -> usize {
        self.app.core.confirms.len()
    }

    /// Lowest-numbered pending confirm handle. Used by the resolve
    /// side-channel to pick a victim deterministically.
    pub fn first_pending_confirm(&self) -> Option<u64> {
        self.app.core.confirms.first_handle()
    }

    /// Number of dialogs deferred because a recent keystroke gated them.
    /// They get replayed on the next idle tick. Used to confirm the
    /// `should_queue` branch in `dispatch_control` was actually hit.
    pub fn pending_deferred_dialog_count(&self) -> usize {
        self.app.pending_dialogs.len()
    }

    /// Slice of `Action`s appended since `idx`. PostChecks use this to
    /// count or inspect specific `UiCommand` variants without each one
    /// growing a dedicated Snapshot field per variant. Pair with
    /// `Snapshot::action_count`: capture the index before dispatch, then
    /// call `actions_since(pre.action_count)` after.
    pub fn actions_since(&self, idx: usize) -> &[Action] {
        let len = self.actions.len();
        let start = idx.min(len);
        &self.actions[start..len]
    }

    /// Side-channel: resolve the first pending confirm with `Yes` or
    /// `No`. Mirrors what the Lua dialog calls into via
    /// `lua_handlers::handle_dialog_decision` without going through the
    /// Lua layer. Returns `true` when a confirm was consumed.
    pub fn resolve_first_confirm(&mut self, approve: bool, message: Option<String>) -> bool {
        let Some(handle_id) = self.first_pending_confirm() else {
            return false;
        };
        let Some(entry) = self.app.core.confirms.take(handle_id) else {
            return false;
        };
        let req = entry.req;
        let choice = if approve {
            smelt_core::transcript_model::ConfirmChoice::Yes
        } else {
            smelt_core::transcript_model::ConfirmChoice::No
        };
        {
            let _guard = crate::lua::install_app_ptr(&mut self.app);
            let cancel = self.app.resolve_confirm(
                (choice, message),
                &req.call_id,
                req.request_id,
                &req.tool_name,
            );
            if cancel {
                self.app.discard_turn(crate::app::TurnEnd::Complete);
            }
        }
        self.drain_cmd();
        true
    }

    /// Smallest pending `smelt.engine.ask` callback id, if any.
    /// Stories that drive `/btw` or other ask flows use this to
    /// pair their synthesised `EngineAskResponse` to the right id.
    pub fn pending_ask_id(&self) -> Option<u64> {
        let shared = self.app.lua.shared().core_arc();
        let cbs = shared.ask_callbacks.lock().ok()?;
        cbs.keys().min().copied()
    }

    /// Working directory string the live app uses (captured at
    /// construction). Stories that seed persisted-session fixtures
    /// match this value into `meta.json` so the resume dialog's
    /// workspace filter keeps the seeded entries.
    pub fn cwd_str(&self) -> &str {
        &self.app.cwd
    }

    /// Prompt cursor byte offset in source space.
    pub fn prompt_cpos(&self) -> usize {
        self.app
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.cpos())
            .unwrap_or(0)
    }

    fn prompt_input_ready_base(&self, allow_pending_chord: bool) -> bool {
        let state = self.state();
        if !matches!(state.app_focus, AppFocus::Prompt)
            || state.agent_running
            || state.cmdline_open
            || state.focused_overlay.is_some()
            || state.active_modal.is_some()
            || state.picker_count > 0
            || !state.term_focused
        {
            return false;
        }
        let Some(win) = self.app.ui.win(crate::app::PROMPT_WIN) else {
            return false;
        };
        if win.vim_enabled() && !matches!(win.vim_mode(), VimMode::Insert) {
            return false;
        }
        if win.selection_anchor().is_some()
            || win.effective_endpoint() != win.cpos()
            || (!allow_pending_chord && self.app.timers.pending_chord.is_some())
            || self.app.timers.pending_pane_chord.is_some()
        {
            return false;
        }
        !self.app.ui.any_drag_active()
    }

    /// Whether plain text input should edit the prompt. Unlike
    /// `prompt_plain_insert_ready`, prediction placeholders are allowed: typing
    /// an ordinary printable key or paste while a placeholder is visible should
    /// still insert into the empty prompt; only placeholder accept/dismiss
    /// chords are special.
    pub fn prompt_text_input_ready(&self) -> bool {
        self.prompt_input_ready_base(false)
    }

    pub(super) fn prompt_text_input_ready_for_turn_probe(&self) -> bool {
        // The turn-end probe intentionally preserves a stale Lua chord prefix,
        // such as the first Esc of the global Esc-Esc binding. The next
        // ordinary text key must decay that prefix and still edit the prompt,
        // so do not pre-filter those cases out of the probe.
        self.prompt_input_ready_base(true)
    }

    /// Whether a plain printable key should insert at the prompt cursor with
    /// no overlay/cmdline/selection/key-capture semantics in front of it.
    pub fn prompt_plain_insert_ready(&self) -> bool {
        self.prompt_input_ready_base(false)
            && !self
                .app
                .placeholder_opts
                .contains_key(&crate::app::PROMPT_WIN)
    }

    pub fn prompt_plain_char_has_lua_keymap(&self, ch: char) -> bool {
        let chord = ch.to_string();
        let mode = self.app.current_vim_mode_label();
        self.app.lua.chord_has_binding(&chord, mode.as_deref())
    }
}
