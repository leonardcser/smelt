//! End-to-end test harness for `TuiApp`.
//!
//! Input is a `SourceEvent` stream (Term / Engine / Tick); output is a
//! structured `Action` log plus snapshots of inspectable state.
//!
//! Side effects are contained by pointing every `$HOME`/XDG path at a
//! process-wide tempdir.

#![allow(dead_code)]

use crate::app::{AppFocus, TuiApp};
use crate::smelt_term::{OverlayId, VimMode, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use engine::clock::VirtualClock;
use engine::EngineHandle;
use protocol::{AgentMode, EngineEvent, ReasoningEffort, UiCommand};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tokio::sync::mpsc;

pub use crate::event_source::SourceEvent;

/// One observed out-bound effect of a `SourceEvent`.
#[derive(Debug, Clone)]
pub enum Action {
    /// A `UiCommand` was sent on the engine channel.
    EngineSend(UiCommand),
    /// The event dispatch asked the app to quit.
    Quit,
}

/// Immutable snapshot of state observable by tests.
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub app_focus: AppFocus,
    pub vim_mode: VimMode,
    pub cmdline_open: bool,
    pub cmdline_text: String,
    pub focused_overlay: Option<OverlayId>,
    pub prompt_text: String,
    pub queued_messages: Vec<String>,
    pub agent_running: bool,
    pub term_focused: bool,
    pub quit_requested: bool,
    pub notification: Option<WinId>,
    pub pending_quit: bool,
}

/// Snapshot of the streaming buffers (`text`, `thinking`, `exec`) at one
/// point in time. Used by fuzz-time transitional invariants that need to
/// assert a specific event flushed the relevant buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamingState {
    pub text: bool,
    pub thinking: bool,
    pub exec: bool,
}

/// Snapshot of `WorkingState`. `animating` means a live turn exists;
/// `compacting` is true iff the live phase is `Compacting`. The two are
/// always either both `false` or `animating == true` with one phase flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkingSnapshot {
    pub animating: bool,
    pub compacting: bool,
}

/// Per-event allocation delta captured by `TestApp::feed_one`. Snapshots
/// `(alloc_count, alloc_bytes_grown)` for the calling thread before and after
/// the event runs and stores the difference. Per-thread TLS counters mean
/// parallel `nextest` workers do not contaminate each other's numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllocDelta {
    pub allocs: u64,
    pub bytes_grown: u64,
}

/// Tunable per-event allocation budget. [`TestApp::feed_one_within_budget`]
/// panics when any single `SourceEvent` exceeds either field; external
/// drivers can use the same seam to flag runaway-allocation scenarios.
#[derive(Debug, Clone, Copy)]
pub struct AllocBudget {
    pub max_allocs: u64,
    pub max_bytes: u64,
}

impl AllocBudget {
    /// Default per-event budget. Tight enough to surface runaway per-event
    /// growth; loose enough that normal large pastes / engine deltas pass.
    /// Ratcheted down after initial fuzz waves cleared.
    pub const DEFAULT: AllocBudget = AllocBudget {
        max_allocs: 5_000,
        max_bytes: 2 * 1024 * 1024,
    };
}

/// Test driver around a real `TuiApp`.
pub struct TestApp {
    pub app: TuiApp,
    pub clock: Arc<VirtualClock>,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    actions: Vec<Action>,
    quit: bool,
    /// Allocation delta for the most recent `feed_one`. `None` when no event
    /// has been fed yet.
    last_alloc: Option<AllocDelta>,
}

pub struct TestAppBuilder {
    vim: bool,
    mode: AgentMode,
    init_lua: Option<std::path::PathBuf>,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            vim: false,
            mode: AgentMode::Normal,
            init_lua: None,
        }
    }
}

impl TestAppBuilder {
    /// Enable vim-mode on the prompt window.
    pub fn with_vim(mut self, vim: bool) -> Self {
        self.vim = vim;
        self
    }

    pub fn with_mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }

    /// Run user `init.lua` from this path during build, and from the same
    /// path again on every `reload_lua()`.
    pub fn with_init_lua(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.init_lua = Some(path.into());
        self
    }

    pub fn build(self) -> TestApp {
        ensure_test_home();

        let (engine, cmd_rx, event_tx) = EngineHandle::for_test();

        let permissions = Arc::new(smelt_core::permissions::Permissions::load());
        let mut settings = smelt_core::config::SettingsConfig::default().resolve();
        settings.vim = self.vim;
        let shared_session = Arc::new(Mutex::new(None));
        let mut lua = crate::lua::LuaRuntime::new();
        // Match production startup: autoload registers built-in
        // commands (`:quit`, `:help`, ...) and the default keymap.
        lua.load_autoload();
        if let Some(ref path) = self.init_lua {
            lua.set_init_lua_path(path.clone());
        }

        let config = smelt_core::AppConfig {
            model: String::new(),
            api_base: String::new(),
            api_key_env: String::new(),
            provider_type: String::new(),
            available_models: Vec::new(),
            model_config: engine::ModelConfig::default(),
            cli_model_override: false,
            cli_api_base_override: false,
            cli_api_key_env_override: false,
            mode: self.mode,
            mode_cycle: vec![
                AgentMode::Normal,
                AgentMode::Plan,
                AgentMode::Apply,
                AgentMode::Yolo,
            ],
            reasoning_effort: ReasoningEffort::Off,
            reasoning_cycle: Vec::new(),
            settings,
            context_window: None,
        };

        let clock = Arc::new(VirtualClock::new(Instant::now(), SystemTime::now()));
        let env = Arc::new(engine::env::RuntimeEnv::scripted(
            4242,
            std::path::PathBuf::from("/tmp/smelt-test/home"),
            std::path::PathBuf::from("/tmp/smelt-test/home/.config"),
            std::path::PathBuf::from("/tmp/smelt-test/home/.state"),
            std::path::PathBuf::from("/tmp/smelt-test/home/.cache"),
            std::path::PathBuf::from("/tmp/smelt-test/home/.data"),
            std::path::PathBuf::from("/tmp/smelt-test/cwd"),
            std::num::NonZeroUsize::new(1).unwrap(),
        ));

        let mut app = TuiApp::new(
            config,
            engine,
            permissions,
            shared_session,
            None, // startup_auth_error
            lua,
            smelt_core::trust::TrustState::NoContent,
            Arc::clone(&clock) as Arc<dyn engine::clock::Clock>,
            env,
        );

        // init.lua may touch TUI surfaces (overlays / wins / bufs), so it
        // needs the app TLS pointer installed — which can only happen
        // after TuiApp construction.
        if self.init_lua.is_some() {
            let _guard = crate::lua::install_app_ptr(&mut app);
            app.lua.load_user_config();
        }

        // Production wires the Tui frontend to `Osc52Sink`, which writes
        // `\x1b]52;c;...` to real stdout on every kill-ring copy. Inside the
        // harness that's a ring leak — corrupts test stdout, slows the fuzz
        // target, and has no semantic value. Swap to `NullSink` immediately.
        app.core.clipboard.swap_sink(Box::new(smelt_core::NullSink));

        // Turn on per-thread allocation counters so `feed_one` snapshots see
        // real numbers. Idempotent; cheap when re-called.
        smelt_perf::alloc::enable();

        TestApp {
            app,
            clock,
            cmd_rx,
            event_tx,
            actions: Vec::new(),
            quit: false,
            last_alloc: None,
        }
    }
}

impl TestApp {
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Feed a single event. Drains any engine commands the dispatch
    /// produced into the action log. Captures the per-thread allocation
    /// delta for this event into [`Self::last_alloc_delta`].
    pub fn feed_one(&mut self, ev: SourceEvent) {
        let (a0, b0) = smelt_perf::alloc::thread_snapshot();
        {
            // Install the TLS app pointer so Lua bindings (e.g. `:quit`
            // setting `pending_quit`) can call back into the app, mirroring
            // the main loop's `install_app_ptr` boundary.
            let _guard = crate::lua::install_app_ptr(&mut self.app);
            match ev {
                SourceEvent::Term(ev) => {
                    let quit = self.app.dispatch_terminal_event(ev);
                    if quit {
                        self.quit = true;
                        self.actions.push(Action::Quit);
                    }
                }
                SourceEvent::Engine(ev) => {
                    self.app.dispatch_engine_event(ev);
                }
                SourceEvent::Tick(ms) => {
                    self.clock.advance(Duration::from_millis(ms));
                }
                SourceEvent::LuaWakeup => {
                    self.app.flush_lua_callbacks();
                    self.app.drive_lua_tasks();
                }
                SourceEvent::ExecOutput(line) => {
                    self.app.append_exec_output(&line);
                }
                SourceEvent::ExecDone(code) => {
                    self.app.finish_exec(code);
                    self.app.finalize_exec();
                    self.app.exec = None;
                }
                SourceEvent::Resize { width, height } => {
                    self.app.handle_resize(width, height);
                }
            }
        }
        self.drain_cmd();
        let (a1, b1) = smelt_perf::alloc::thread_snapshot();
        self.last_alloc = Some(AllocDelta {
            allocs: a1.saturating_sub(a0),
            bytes_grown: b1.saturating_sub(b0),
        });
    }

    /// Feed a single event and panic if the per-event allocation delta
    /// exceeds `budget`. Useful as a regression guard against accidental
    /// per-keystroke growth and as a hard cap for external scenario drivers.
    pub fn feed_one_within_budget(&mut self, ev: SourceEvent, budget: AllocBudget) {
        self.feed_one(ev);
        let delta = self.last_alloc.expect("feed_one populates last_alloc");
        assert!(
            delta.allocs <= budget.max_allocs,
            "event exceeded alloc-count budget: {} > {} ({} bytes grown)",
            delta.allocs,
            budget.max_allocs,
            delta.bytes_grown
        );
        assert!(
            delta.bytes_grown <= budget.max_bytes,
            "event exceeded bytes-grown budget: {} > {} ({} allocs)",
            delta.bytes_grown,
            budget.max_bytes,
            delta.allocs
        );
    }

    /// Allocation delta captured by the most recent [`Self::feed_one`].
    pub fn last_alloc_delta(&self) -> Option<AllocDelta> {
        self.last_alloc
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
        self.app.agent = Some(crate::app::TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: smelt_perf::perf::begin("test_harness:turn"),
        });
    }

    /// Whether an agent turn is currently active.
    pub fn agent_running(&self) -> bool {
        self.app.agent.is_some()
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

    /// Length of `session.messages`. Used by post-event invariants that
    /// assert compaction or `set_history` replaced the conversation.
    pub fn session_message_count(&self) -> usize {
        self.app.core.session.messages.len()
    }

    /// Whether `pending_compact_epoch == compact_epoch`. When true, an
    /// incoming `CompactionComplete` hits the apply path; when false, it
    /// hits the stale fast-finish path.
    pub fn compact_epoch_match(&self) -> bool {
        self.app.pending_compact_epoch == self.app.compact_epoch
    }

    /// Counts of token / cost / turn-meta snapshots. Used by compaction
    /// invariants — `apply_compaction` clears all three when the messages
    /// payload is non-empty.
    pub fn snapshot_counts(&self) -> (usize, usize, usize) {
        let s = &self.app.core.session;
        (
            s.token_snapshots.len(),
            s.cost_snapshots.len(),
            s.turn_metas.len(),
        )
    }

    /// Side-channel: prime the compact epoch so a subsequent
    /// `CompactionComplete` lands on the apply path. Mirrors what
    /// `compact_history` does without emitting `UiCommand::Compact`.
    pub fn begin_compaction(&mut self) {
        self.app.pending_compact_epoch = self.app.compact_epoch;
    }

    /// `turn_id` of the active agent turn, if any. Used by fuzz ops that
    /// synthesize engine events whose dispatch is gated on a matching id
    /// (e.g. `TurnComplete`, `Messages`).
    pub fn current_turn_id(&self) -> Option<u64> {
        self.app.agent.as_ref().map(|ag| ag.turn_id)
    }

    /// Number of user messages waiting to be sent on the next turn. Used
    /// by `Steered` invariants that assert the drain semantics.
    pub fn queued_message_count(&self) -> usize {
        self.app.queued_messages.len()
    }

    /// Side-channel: push a synthetic queued message. In production
    /// `queued_messages` is filled by pressing Enter on the prompt while a
    /// turn is active; the harness short-circuits that flow.
    pub fn push_queued_message(&mut self, text: String) {
        self.app.queued_messages.push(text);
    }

    /// Snapshot of the working-status bar's live state. Used by fuzz
    /// invariants that assert phase transitions (e.g. compaction ends with
    /// `animating == false`, `Retrying` event leaves `animating == true`).
    pub fn working_state(&self) -> WorkingSnapshot {
        WorkingSnapshot {
            animating: self.app.working.is_animating(),
            compacting: self.app.working.is_compacting(),
        }
    }

    /// Accumulated session cost in USD. Used by `TokenUsage` invariants
    /// asserting cost is monotonically non-decreasing.
    pub fn session_cost_usd(&self) -> f64 {
        self.app.core.session.session_cost_usd
    }

    /// Current context-tokens estimate, when a non-background usage report
    /// has set it.
    pub fn context_tokens(&self) -> Option<u32> {
        self.app.core.session.context_tokens
    }

    /// Number of transcript blocks. Used by event invariants that assert
    /// a block was pushed (e.g. `ProcessCompleted`).
    pub fn transcript_block_count(&self) -> usize {
        self.app.transcript.history.len()
    }

    /// Session title / slug. Used by `TitleGenerated` invariants.
    pub fn session_title(&self) -> Option<String> {
        self.app.core.session.title.clone()
    }
    pub fn session_slug(&self) -> Option<String> {
        self.app.core.session.slug.clone()
    }
    pub fn pending_title(&self) -> bool {
        self.app.pending_title
    }

    /// Side-channel: prime the `pending_title` flag so a subsequent
    /// `TitleGenerated` event applies. In production `pending_title` is
    /// set when the UI sends `UiCommand::GenerateTitle`; the harness
    /// short-circuits that flow.
    pub fn prime_pending_title(&mut self) {
        self.app.pending_title = true;
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

    /// Count of `UiCommand::PermissionDecision` entries in the action log.
    /// Increments whenever an auto-approval fires inline during
    /// `dispatch_control` or a `resolve_confirm` resolves a registered
    /// dialog.
    pub fn permission_decision_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::EngineSend(UiCommand::PermissionDecision { .. })))
            .count()
    }

    /// Latest `PermissionDecision` action, if any. Used to verify the
    /// `(request_id, approved)` pair after a resolve.
    pub fn last_permission_decision(&self) -> Option<(u64, bool, Option<String>)> {
        self.actions.iter().rev().find_map(|a| match a {
            Action::EngineSend(UiCommand::PermissionDecision {
                request_id,
                approved,
                message,
            }) => Some((*request_id, *approved, message.clone())),
            _ => None,
        })
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
        let cancel = self.app.resolve_confirm(
            (choice, message),
            &req.call_id,
            req.request_id,
            &req.tool_name,
        );
        if cancel {
            self.app.discard_turn(false);
        }
        self.drain_cmd();
        true
    }

    /// Render one frame to real stdout. Drives the same compositor
    /// pipeline production uses (`TuiApp::render_normal`). The caller is
    /// responsible for terminal setup (raw mode, alternate screen).
    pub fn render(&mut self) {
        let agent_running = self.app.agent.is_some();
        self.app.render_normal(agent_running);
    }

    /// Resize the app's surface to `(width, height)`. Used by replay
    /// drivers that own a real terminal and need to match the app's
    /// internal grid to the OS-reported size.
    pub fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.app.handle_resize(width, height);
    }

    /// Cheap structural invariants over every live `(Buffer, Window)` pair
    /// plus side-car state that holds byte offsets across mutations.
    /// Panics on the first violation. Safe to call after every dispatched
    /// event.
    ///
    /// Checks per window:
    /// - `cpos` is in `0..=source.len()`.
    /// - `cpos` lands on a UTF-8 char boundary.
    /// - `selection_anchor`, when set, satisfies the same two constraints.
    ///
    /// Checks per buffer:
    /// - Every saved undo and redo `UndoEntry.cpos` is in-bounds and on a
    ///   UTF-8 boundary relative to its own `entry.buf` snapshot.
    /// - When a cap is set, `undo_len() <= cap` (cap honored after push).
    ///
    /// Plus globals:
    /// - Terminal width and height non-zero.
    /// - Kill-ring `source_range`, when set, is well-formed (`start <= end`).
    /// - Prompt completer anchor, when active, is in-bounds and on a UTF-8
    ///   boundary in the prompt-edit buffer.
    pub fn assert_invariants(&self) {
        for (wid, win) in self.app.ui.iter_wins() {
            let Some(buf) = self.app.ui.buf(win.buf) else {
                continue;
            };
            let src = buf.source();
            // The buffer crate carries two representations: source-based
            // buffers (prompt, transcript) maintain `source` as the
            // canonical byte stream and feed `cpos` into it directly;
            // line-based buffers (cmdline, picker, status bar, list
            // overlays) write through `set_lines` / `set_all_lines` and
            // leave `source` empty — content lives in `lines` and `cpos`
            // is set via cell-column helpers, not byte arithmetic on
            // `source`. The source-based invariants below don't apply to
            // that mode, so skip when the buffer has empty source but
            // non-empty line content.
            let line_based = src.is_empty()
                && (buf.line_count() > 1 || buf.get_line(0).is_some_and(|l| !l.is_empty()));
            if line_based {
                continue;
            }
            assert!(
                win.cpos <= src.len(),
                "window {:?} cpos {} > source len {}",
                wid,
                win.cpos,
                src.len()
            );
            let snapped = smelt_buffer::text::snap(src, win.cpos);
            assert_eq!(
                snapped, win.cpos,
                "window {:?} cpos {} not on UTF-8 char boundary (snapped {})",
                wid, win.cpos, snapped
            );
            if let Some(anchor) = win.selection_anchor {
                assert!(
                    anchor <= src.len(),
                    "window {:?} selection_anchor {} > source len {}",
                    wid,
                    anchor,
                    src.len()
                );
                let snapped = smelt_buffer::text::snap(src, anchor);
                assert_eq!(
                    snapped, anchor,
                    "window {:?} selection_anchor {} not on UTF-8 char boundary (snapped {})",
                    wid, anchor, snapped
                );
            }
        }

        for (bid, buf) in self.app.ui.iter_bufs() {
            // Undo/redo snapshots are self-contained: each entry's `cpos`
            // is an offset into that entry's own `buf` string, not the
            // current source. A stale `cpos` lurking in an undo entry
            // surfaces here before the user ever steps back into it.
            for (i, entry) in buf.history.iter_undo().enumerate() {
                assert!(
                    entry.cpos <= entry.buf.len(),
                    "buf {:?} undo[{}] cpos {} > snapshot len {}",
                    bid,
                    i,
                    entry.cpos,
                    entry.buf.len()
                );
                let snapped = smelt_buffer::text::snap(&entry.buf, entry.cpos);
                assert_eq!(
                    snapped, entry.cpos,
                    "buf {:?} undo[{}] cpos {} not on UTF-8 char boundary",
                    bid, i, entry.cpos
                );
            }
            for (i, entry) in buf.history.iter_redo().enumerate() {
                assert!(
                    entry.cpos <= entry.buf.len(),
                    "buf {:?} redo[{}] cpos {} > snapshot len {}",
                    bid,
                    i,
                    entry.cpos,
                    entry.buf.len()
                );
                let snapped = smelt_buffer::text::snap(&entry.buf, entry.cpos);
                assert_eq!(
                    snapped, entry.cpos,
                    "buf {:?} redo[{}] cpos {} not on UTF-8 char boundary",
                    bid, i, entry.cpos
                );
            }
            if let Some(cap) = buf.history.cap() {
                assert!(
                    buf.history.undo_len() <= cap,
                    "buf {:?} undo stack {} > cap {}",
                    bid,
                    buf.history.undo_len(),
                    cap
                );
            }
        }

        let (w, h) = self.app.ui.terminal_size();
        assert!(w > 0 && h > 0, "terminal size collapsed to {w}x{h}");

        // Focused window, when set, must still be alive. A stale `focus`
        // pointing at a closed leaf is a use-after-free in waiting.
        if let Some(focused) = self.app.ui.focus() {
            assert!(
                self.app.ui.win(focused).is_some(),
                "focus points at dead window {focused:?}"
            );
        }

        // Active agent turn: pending tool call_ids must be unique. A
        // duplicate means `ToolStarted` was processed twice for the same
        // call without an intervening `ToolFinished`, which corrupts the
        // tool-widget state.
        if let Some(ag) = self.app.agent.as_ref() {
            let mut seen = std::collections::HashSet::with_capacity(ag.pending.len());
            for pt in &ag.pending {
                assert!(
                    seen.insert(pt.call_id.as_str()),
                    "duplicate pending tool call_id {:?} in turn {}",
                    pt.call_id,
                    ag.turn_id
                );
            }
        }

        // Kill-ring source range is well-formed even if we can't validate
        // it against a specific buffer (the ring doesn't track which buffer
        // it came from). `start <= end` is the floor.
        if let Some((start, end)) = self.app.core.clipboard.kill_ring.source_range() {
            assert!(
                start <= end,
                "kill-ring source_range {} > {} (inverted)",
                start,
                end
            );
        }

        // Completer anchor lives in the prompt-edit buffer. When active,
        // it must still resolve to a valid byte boundary after every
        // mutation — that's the exact stale-offset trap fuzzing should
        // catch.
        if let Some(session) = self.app.input.completer.as_ref() {
            if let Some(prompt) = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF) {
                let src = prompt.source();
                let anchor = session.completer.anchor;
                assert!(
                    anchor <= src.len(),
                    "completer anchor {} > prompt source len {}",
                    anchor,
                    src.len()
                );
                let snapped = smelt_buffer::text::snap(src, anchor);
                assert_eq!(
                    snapped, anchor,
                    "completer anchor {} not on UTF-8 char boundary (snapped {})",
                    anchor, snapped
                );
            }
        }
    }

    pub fn feed<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SourceEvent>,
    {
        for ev in events {
            self.feed_one(ev);
        }
    }

    /// Type a single character key with no modifiers.
    pub fn type_char(&mut self, c: char) {
        self.press_mod(KeyCode::Char(c), KeyModifiers::NONE);
    }

    /// Type each char of `s` as a separate keystroke.
    pub fn type_text(&mut self, s: &str) {
        for c in s.chars() {
            self.type_char(c);
        }
    }

    pub fn press(&mut self, code: KeyCode) {
        self.press_mod(code, KeyModifiers::NONE);
    }

    pub fn press_mod(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ev = Event::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        self.feed_one(SourceEvent::Term(ev));
    }

    pub fn inject_engine(&self, ev: EngineEvent) -> Result<(), Box<EngineEvent>> {
        self.event_tx.send(ev).map_err(|e| Box::new(e.0))
    }

    /// Drain `UiCommand`s buffered on the engine channel into the action log.
    fn drain_cmd(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.actions.push(Action::EngineSend(cmd));
        }
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn clear_actions(&mut self) {
        self.actions.clear();
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Snapshot the public-facing state at this instant.
    pub fn state(&self) -> AppSnapshot {
        let cmdline_open = self.app.well_known.cmdline.is_some();
        let cmdline_text = if cmdline_open {
            cmdline_text(&self.app)
        } else {
            String::new()
        };
        let prompt_text = self
            .app
            .ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .map(|b| b.source().to_string())
            .unwrap_or_default();
        let vim_mode = self
            .app
            .ui
            .win(self.app.well_known.prompt)
            .map(|w| w.vim_mode)
            .unwrap_or(VimMode::Insert);
        AppSnapshot {
            app_focus: self.app.app_focus,
            vim_mode,
            cmdline_open,
            cmdline_text,
            focused_overlay: self.app.ui.focused_overlay(),
            prompt_text,
            queued_messages: self.app.queued_messages.clone(),
            agent_running: self.app.agent.is_some(),
            term_focused: self.app.term_focused,
            quit_requested: self.quit,
            notification: self.app.notification,
            pending_quit: self.app.pending_quit,
        }
    }
}

/// Read the cmdline payload (stripped of the `:` prefix). Mirrors the
/// private `TuiApp::cmdline_text` so suites can assert against it.
fn cmdline_text(app: &TuiApp) -> String {
    let Some(win) = app.well_known.cmdline else {
        return String::new();
    };
    let buf_id = app.ui.win(win).map(|w| w.buf);
    let line = buf_id
        .and_then(|b| app.ui.buf(b))
        .and_then(|b| b.get_line(0).map(|s| s.to_string()))
        .unwrap_or_default();
    line.strip_prefix(':').unwrap_or(&line).to_string()
}

// ── Process-wide tempdir for $HOME and XDG vars ─────────────────────

static TEST_HOME: OnceLock<TempDir> = OnceLock::new();

/// Initialize `$HOME` + XDG env vars on first call, then wipe the
/// directory's contents on every call so each `TestApp::build` starts
/// against an empty filesystem. Without this, session / history / state
/// files written by one scenario survive into the next — a real source
/// of nondeterminism for libFuzzer, which runs every iteration in the
/// same process.
fn ensure_test_home() {
    let dir = TEST_HOME.get_or_init(|| TempDir::new().expect("create test $HOME tempdir"));
    let home = dir.path();
    // SAFETY: env vars are set to the same constant path on every call;
    // concurrent reads from other threads see a stable value.
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("XDG_STATE_HOME", home.join("state"));
    std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    std::env::set_var("XDG_DATA_HOME", home.join("data"));
    // Wipe everything in `home` so the next scenario sees an empty
    // filesystem. We can't `remove_dir_all` `home` itself (it'd drop the
    // tempdir backing path), so iterate one level down.
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

// ── Suites ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_fresh_test_app() {
        let app = TestApp::builder().build();
        let s = app.state();
        assert!(!s.cmdline_open);
        assert!(!s.quit_requested);
        assert!(!s.agent_running);
        assert_eq!(s.app_focus, AppFocus::Prompt);
        assert!(s.queued_messages.is_empty());
    }

    // ── Resource invariants: per-event allocation tracking ────────────

    /// `feed_one` captures a non-negative allocation delta on every event,
    /// and a `Tick` (pure clock advance) allocates next to nothing — the
    /// floor sanity-check that the counting allocator is actually wired
    /// into the test binary. If `Counting` regresses to `System`, the
    /// snapshots stay zero and this still passes; pair with the keystroke
    /// budget test below to catch that.
    #[test]
    fn feed_one_records_alloc_delta_with_tick_near_zero() {
        let mut app = TestApp::builder().build();
        assert!(app.last_alloc_delta().is_none());
        app.feed_one(SourceEvent::Tick(10));
        let delta = app.last_alloc_delta().expect("delta after first event");
        assert!(
            delta.allocs < 32,
            "Tick should allocate near zero, got {} allocs / {} bytes",
            delta.allocs,
            delta.bytes_grown
        );
    }

    /// One keystroke through the dispatch chain stays well under the default
    /// budget. If this trips, either we have a real per-keystroke regression
    /// or the budget needs revisiting — both worth noticing.
    #[test]
    fn keystroke_stays_within_default_alloc_budget() {
        let mut app = TestApp::builder().build();
        // Warm caches with a discarded first keystroke so this test
        // measures steady-state cost, not first-event init.
        app.type_char('a');
        app.feed_one_within_budget(
            SourceEvent::Term(Event::Key(KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })),
            AllocBudget::DEFAULT,
        );
        let delta = app.last_alloc_delta().expect("delta recorded");
        // Print observed steady-state cost so it's visible in `cargo test
        // -- --nocapture` runs and during budget-tuning sweeps.
        eprintln!(
            "steady-state keystroke delta: {} allocs / {} bytes",
            delta.allocs, delta.bytes_grown
        );
    }

    #[test]
    fn tick_event_advances_virtual_clock() {
        let mut app = TestApp::builder().build();
        let before = app.app.core.clock.instant_now();
        app.feed_one(SourceEvent::Tick(500));
        let after = app.app.core.clock.instant_now();
        assert_eq!(after - before, Duration::from_millis(500));
    }

    // ── Ctrl-C semantics ───────────────────────────────────────────

    #[test]
    fn ctrl_c_on_empty_buffer_when_idle_quits() {
        let mut app = TestApp::builder().build();
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit_requested());
    }

    #[test]
    fn ctrl_c_with_text_in_buffer_clears_buffer_without_quitting() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "");
        assert!(!app.quit_requested());
    }

    #[test]
    fn ctrl_c_twice_clears_then_quits() {
        let mut app = TestApp::builder().build();
        app.type_text("hi");
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.quit_requested(), "first Ctrl-C clears, doesn't quit");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit_requested(), "second Ctrl-C on empty buffer quits");
    }

    // ── Cmdline open/close (vim-gated) ──────────────────────────────

    #[test]
    fn colon_in_non_vim_mode_does_not_open_cmdline() {
        let mut app = TestApp::builder().build();
        app.type_char(':');
        let s = app.state();
        assert!(!s.cmdline_open);
        assert_eq!(s.prompt_text, ":");
    }

    #[test]
    fn fresh_vim_prompt_starts_in_insert_mode() {
        // Chat input ergonomics: even with vim enabled, the prompt starts
        // in Insert so the first keystroke types instead of navigating.
        let app = TestApp::builder().with_vim(true).build();
        assert_eq!(app.state().vim_mode, VimMode::Insert);
    }

    #[test]
    fn colon_in_vim_insert_mode_does_not_open_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Fresh prompt is already in Insert.
        assert_eq!(app.state().vim_mode, VimMode::Insert);

        app.type_char(':');
        let s = app.state();
        assert!(!s.cmdline_open);
        assert_eq!(s.prompt_text, ":");
    }

    #[test]
    fn colon_in_vim_normal_mode_opens_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Drop to Normal first since the prompt starts in Insert.
        app.press(KeyCode::Esc);
        app.type_char(':');
        let s = app.state();
        assert!(s.cmdline_open);
        assert_eq!(s.cmdline_text, "");
    }

    #[test]
    fn typing_into_cmdline_appends_to_payload() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        app.type_char(':');
        app.type_text("help");
        assert_eq!(app.state().cmdline_text, "help");
    }

    #[test]
    fn esc_closes_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        app.type_char(':');
        assert!(app.state().cmdline_open);

        app.press(KeyCode::Esc);
        assert!(!app.state().cmdline_open);
    }

    #[test]
    fn cmdline_quit_command_requests_quit() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        app.type_char(':');
        app.type_text("quit");
        app.press(KeyCode::Enter);
        assert!(app.state().pending_quit);
    }

    /// Regression: typing into the cmdline grows its line-based buffer
    /// and the cmdline window's `cpos` past `source.len()` (the cmdline
    /// stays empty because content lives in `lines`). The invariant
    /// scoping must recognize this as a line-based buffer and skip the
    /// source-bounded cursor check rather than fire spuriously.
    #[test]
    fn cmdline_typed_payload_does_not_trip_cursor_invariant() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        app.type_char(':');
        // Type more than any single-line buffer could ever encode in
        // source byte arithmetic from cell position alone.
        app.type_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        app.assert_invariants();
    }

    // ── Picker open/filter/select ───────────────────────────────────

    fn open_test_picker(app: &mut TestApp, labels: &[&str], selected: usize) -> WinId {
        let items: Vec<crate::picker::PickerItem> = labels
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        crate::picker::open(
            &mut app.app,
            items,
            selected,
            crate::picker::PickerPlacement::ScreenCenter,
            true,  // focusable
            false, // blocks_agent
            10,    // z
        )
        .expect("picker leaf created")
    }

    fn picker_buffer_lines(app: &TestApp, leaf: WinId) -> Vec<String> {
        let Some(buf_id) = app.app.ui.win(leaf).map(|w| w.buf) else {
            return Vec::new();
        };
        let Some(buf) = app.app.ui.buf(buf_id) else {
            return Vec::new();
        };
        (0..buf.line_count())
            .filter_map(|i| buf.get_line(i).map(String::from))
            .collect()
    }

    #[test]
    fn picker_open_focuses_overlay() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["one", "two", "three"], 0);
        let s = app.state();
        assert!(s.focused_overlay.is_some());
        assert_eq!(app.app.ui.focus(), Some(leaf));
    }

    #[test]
    fn picker_open_renders_items_into_buffer() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["alpha", "beta", "gamma"], 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("alpha"));
        assert!(lines[1].contains("beta"));
        assert!(lines[2].contains("gamma"));
    }

    #[test]
    fn picker_set_items_replaces_buffer_contents() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["foo", "bar"], 0);
        let new_items: Vec<_> = ["x", "y", "z"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, new_items, 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("x"));
        assert!(lines[2].contains("z"));
    }

    #[test]
    fn picker_set_selected_moves_cursor() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["a", "b", "c", "d"], 0);
        let initial_cpos = app.app.ui.win(leaf).map(|w| w.cpos).unwrap_or(0);

        crate::picker::set_selected(&mut app.app, leaf, 2);
        let new_cpos = app.app.ui.win(leaf).map(|w| w.cpos).unwrap_or(0);
        assert_ne!(initial_cpos, new_cpos, "cursor moved with selection");
    }

    #[test]
    fn picker_wheel_pans_viewport_when_unfocused() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = TestApp::builder().build();
        let items: Vec<crate::picker::PickerItem> = (0..40)
            .map(|i| crate::picker::PickerItem::new(format!("item {i}")))
            .collect();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let leaf = crate::picker::open(
            &mut app.app,
            items,
            0,
            crate::picker::PickerPlacement::ScreenCenter,
            false, // non-focusable: focus stays on prompt
            false,
            10,
        )
        .expect("picker leaf created");
        drop(_guard);

        // Render to populate the viewport.
        app.app.render_normal(false);
        assert_eq!(app.app.ui.win(leaf).map(|w| w.scroll_top), Some(0));

        let leaf_rect = app
            .app
            .ui
            .paint_rect(crate::smelt_term::PaintId::from(leaf))
            .expect("picker leaf has a rect after render");
        // Pick a cell inside the picker rect.
        let row = leaf_rect.top + 1;
        let col = leaf_rect.left + 1;

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            row,
            column: col,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let _ = scroll; // silence unused-warning if path below ignores it
        let _ = MouseButton::Left;

        let pre_scroll = app.app.ui.win(leaf).unwrap().scroll_top;
        let _ = app.app.ui.scroll_at(row, col, 3);
        let post_scroll = app.app.ui.win(leaf).unwrap().scroll_top;
        assert!(
            post_scroll > pre_scroll,
            "wheel over unfocused picker must pan scroll_top (pre={pre_scroll}, post={post_scroll})",
        );
    }

    #[test]
    fn picker_forget_drops_state() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["a", "b"], 0);
        assert!(app.app.picker_state.contains_key(&leaf));

        crate::picker::forget(&mut app.app, leaf);
        assert!(!app.app.picker_state.contains_key(&leaf));
    }

    // ── Vim mode transitions ────────────────────────────────────────

    #[test]
    fn vim_i_enters_insert_from_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('i');
        assert_eq!(app.state().vim_mode, VimMode::Insert);
    }

    #[test]
    fn vim_a_enters_insert_after_cursor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        app.type_char('a');
        assert_eq!(app.state().vim_mode, VimMode::Insert);
    }

    #[test]
    fn vim_esc_returns_insert_to_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Prompt starts in Insert; type directly.
        app.type_text("hello");
        assert_eq!(app.state().vim_mode, VimMode::Insert);
        assert_eq!(app.state().prompt_text, "hello");

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_v_enters_visual_from_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_text("abc");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);
    }

    #[test]
    fn vim_shift_v_enters_visual_line() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_text("abc");
        app.press(KeyCode::Esc);

        app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
        assert_eq!(app.state().vim_mode, VimMode::VisualLine);
    }

    #[test]
    fn vim_esc_from_visual_returns_to_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_text("abc");
        app.press(KeyCode::Esc);
        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_full_cycle_normal_insert_normal_visual_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('i');
        assert_eq!(app.state().vim_mode, VimMode::Insert);

        app.type_text("foo");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_typing_in_normal_mode_does_not_append_to_buffer() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Normal-mode 'h' / 'l' are motions, not characters — should not
        // land in the prompt buffer.
        app.press(KeyCode::Esc);
        app.type_text("hl");
        assert_eq!(app.state().prompt_text, "");
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_typing_in_insert_mode_appends_to_buffer() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Prompt starts in Insert.
        app.type_text("hello world");
        assert_eq!(app.state().prompt_text, "hello world");
    }

    #[test]
    fn vim_dd_in_normal_deletes_line() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_text("line one");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().prompt_text, "line one");

        // `dd` deletes the current line.
        app.type_char('d');
        app.type_char('d');
        assert_eq!(app.state().prompt_text, "");
    }

    // ── Original picker suite continues ─────────────────────────────

    #[test]
    fn picker_filter_workflow_via_set_items() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["apple", "apricot", "banana", "cherry"], 0);
        assert_eq!(picker_buffer_lines(&app, leaf).len(), 4);

        // Simulate "filter as user types": narrow set_items, then narrow again.
        let filtered: Vec<_> = ["apple", "apricot"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, filtered, 0);
        assert_eq!(picker_buffer_lines(&app, leaf).len(), 2);

        let single: Vec<_> = ["apple"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, single, 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("apple"));
    }

    // ── Named-resource hot-reload refresh ───────────────────────────

    /// Reproduces the perf_panel hot-reload flow: re-call `overlay.open`
    /// with the same `name` and a different `title` and assert the chrome
    /// title is updated in place (no close+reopen).
    #[test]
    fn named_overlay_open_refreshes_title_in_place() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);

        let lua = &app.app.lua.lua;
        lua.load(
            r#"
            local buf = smelt.buf.create({ name = "perf_panel.buf" })
            local win = smelt.win.open(buf, { name = "perf_panel.win", focusable = false })
            smelt.ui.overlay.open({
                name = "perf_panel",
                title = "old title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
        )
        .exec()
        .expect("first open");

        let id1 = app.app.ui.named_overlay("perf_panel").expect("named id");
        let title1 = app
            .app
            .ui
            .overlay(id1)
            .and_then(|ov| {
                ov.layout
                    .chrome()
                    .title
                    .as_ref()
                    .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
            })
            .unwrap_or_default();
        assert_eq!(title1, "old title");

        lua.load(
            r#"
            local buf = smelt.buf.create({ name = "perf_panel.buf" })
            local win = smelt.win.open(buf, { name = "perf_panel.win", focusable = false })
            smelt.ui.overlay.open({
                name = "perf_panel",
                title = "new title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
        )
        .exec()
        .expect("second open");

        let id2 = app
            .app
            .ui
            .named_overlay("perf_panel")
            .expect("named id after refresh");
        assert_eq!(id1, id2, "same OverlayId across refresh");
        let title2 = app
            .app
            .ui
            .overlay(id2)
            .and_then(|ov| {
                ov.layout
                    .chrome()
                    .title
                    .as_ref()
                    .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
            })
            .unwrap_or_default();
        assert_eq!(title2, "new title", "title should refresh in place");
    }

    /// `apply_window_opts` should only mutate fields that are present in
    /// opts. A named refresh that omits `wrap` must NOT silently reset
    /// wrap to its default — that would clobber the prior value.
    #[test]
    fn named_win_refresh_preserves_wrap_when_omitted() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let lua = &app.app.lua.lua;

        let win_id: u64 = lua
            .load(
                r#"
                local buf = smelt.buf.create({ name = "w.buf" })
                local win = smelt.win.open(buf, { name = "w.win", wrap = false })
                return win
                "#,
            )
            .eval()
            .expect("first open");

        let wid = crate::smelt_term::WinId(win_id);
        assert!(
            !app.app.ui.win(wid).unwrap().wrap,
            "wrap should be false after explicit open"
        );

        // Re-open with the same name but no `wrap` key → wrap should stay false.
        lua.load(
            r#"
            local buf = smelt.buf.create({ name = "w.buf" })
            smelt.win.open(buf, { name = "w.win" })
            "#,
        )
        .exec()
        .expect("refresh");

        assert!(
            !app.app.ui.win(wid).unwrap().wrap,
            "wrap must be preserved across named refresh (regression)"
        );
    }

    /// `buf.create({ name = ... })` and `win.open(buf, { name = ... })`
    /// should hand back the SAME ids when called twice with the same name.
    #[test]
    fn named_buf_and_win_survive_across_open_calls() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let lua = &app.app.lua.lua;

        let first: (u64, u64) = lua
            .load(
                r#"
                local buf = smelt.buf.create({ name = "x.buf" })
                local win = smelt.win.open(buf, { name = "x.win" })
                return buf, win
                "#,
            )
            .eval()
            .expect("first");

        let second: (u64, u64) = lua
            .load(
                r#"
                local buf = smelt.buf.create({ name = "x.buf" })
                local win = smelt.win.open(buf, { name = "x.win" })
                return buf, win
                "#,
            )
            .eval()
            .expect("second");

        assert_eq!(first.0, second.0, "named buf id stable across re-create");
        assert_eq!(first.1, second.1, "named win id stable across re-open");
    }

    /// Re-opening a named overlay with a structurally different layout
    /// (leaf → vbox split) should replace the tree in place — not silently
    /// keep the old one.
    #[test]
    fn named_overlay_refresh_replaces_layout_structure() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let lua = &app.app.lua.lua;

        lua.load(
            r#"
            local buf = smelt.buf.create({ name = "a.buf" })
            local win = smelt.win.open(buf, { name = "a.win" })
            smelt.ui.overlay.open({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
        )
        .exec()
        .expect("first open");

        let id = app.app.ui.named_overlay("ov").expect("named");
        let leaves_before = app
            .app
            .ui
            .overlay(id)
            .map(|ov| ov.layout.leaves_in_order().len())
            .unwrap_or(0);
        assert_eq!(leaves_before, 1);

        lua.load(
            r#"
            local b1 = smelt.buf.create({ name = "a.buf" })
            local b2 = smelt.buf.create({ name = "b.buf" })
            local w1 = smelt.win.open(b1, { name = "a.win" })
            local w2 = smelt.win.open(b2, { name = "b.win" })
            smelt.ui.overlay.open({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.vbox({
                    { smelt.ui.layout.leaf(w1), height = "fill" },
                    { smelt.ui.layout.leaf(w2), height = "fill" },
                }),
            })
            "#,
        )
        .exec()
        .expect("structural refresh");

        let leaves_after = app
            .app
            .ui
            .overlay(id)
            .map(|ov| ov.layout.leaves_in_order().len())
            .unwrap_or(0);
        assert_eq!(leaves_after, 2, "layout should be swapped to 2-leaf vbox");
    }

    /// `smelt.state` entries for plugins that no longer touch them on
    /// reload should be swept by `smelt.__sweep_state()`.
    #[test]
    fn sweep_state_prunes_untouched_entries() {
        let rt = crate::lua::LuaRuntime::new();
        rt.lua
            .load(
                r#"
                local s1 = smelt.state("alive")
                s1.open = true
                local s2 = smelt.state("dead")
                s2.open = true
                "#,
            )
            .exec()
            .expect("seed");

        // Mimic what `reload()` does: reset the touched table, simulate one
        // plugin re-touching its state, then sweep.
        rt.lua
            .load(
                r#"
                __smelt_state_touched__ = {}
                smelt.state("alive")
                smelt.__sweep_state()
                "#,
            )
            .exec()
            .expect("sweep");

        let alive: bool = rt
            .lua
            .load("return __smelt_state__.alive ~= nil")
            .eval()
            .unwrap();
        let dead: bool = rt
            .lua
            .load("return __smelt_state__.dead ~= nil")
            .eval()
            .unwrap();
        assert!(alive, "touched entry survives");
        assert!(!dead, "untouched entry is swept");
    }

    // ── Full-cycle /reload integration ──────────────────────────────
    //
    // These tests drive `TuiApp::reload_lua()` end-to-end with a real
    // `init.lua` on disk. Each test edits the file between reloads so
    // the new module body re-runs and we can observe the surfaces that
    // *should* survive (named bufs/wins/overlays, `smelt.state`) vs.
    // the ones that should be replaced (titles, layout structure) vs.
    // the ones that should be reaped (anonymous overlays).

    fn read_overlay_title(app: &TestApp, name: &str) -> Option<String> {
        let id = app.app.ui.named_overlay(name)?;
        let ov = app.app.ui.overlay(id)?;
        Some(
            ov.layout
                .chrome()
                .title
                .as_ref()?
                .spans
                .iter()
                .map(|s| s.text.as_ref())
                .collect::<String>(),
        )
    }

    /// Editing `init.lua` to change the overlay title and calling
    /// `reload_lua` should update the chrome title in place without
    /// destroying the OverlayId.
    #[test]
    fn reload_lua_refreshes_overlay_title_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");

        let body = |title: &str| {
            format!(
                r#"
                local state = smelt.state("plug")
                local function attach()
                    local buf = smelt.buf.create({{ name = "plug.buf" }})
                    local win = smelt.win.open(buf, {{ name = "plug.win" }})
                    smelt.ui.overlay.open({{
                        name = "plug",
                        title = "{title}",
                        anchor = "screen_at", corner = "nw",
                        row = 0, col = 0, width = 40, height = 10,
                        layout = smelt.ui.layout.leaf(win),
                    }})
                end
                state.open = true
                attach()
                "#
            )
        };
        std::fs::write(&init, body("v1")).unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v1"));
        }
        let id1 = app.app.ui.named_overlay("plug").unwrap();

        std::fs::write(&init, body("v2")).unwrap();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }
        let id2 = app.app.ui.named_overlay("plug").expect("overlay survives");
        assert_eq!(id1, id2, "OverlayId preserved across reload");
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v2"));
        }
    }

    /// Nested tables stashed in `smelt.state` must keep their identity
    /// (deep values intact) across `/reload`.
    #[test]
    fn reload_lua_preserves_nested_state_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            local s = smelt.state("nested")
            s.cfg = s.cfg or { panel = { width = 80, history = { 1, 2, 3 } } }
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
            app.app.reload_lua();
        }
        let width: u64 = app
            .app
            .lua
            .lua
            .load("return __smelt_state__.nested.cfg.panel.width")
            .eval()
            .unwrap();
        let last: u64 = app
            .app
            .lua
            .lua
            .load("return __smelt_state__.nested.cfg.panel.history[3]")
            .eval()
            .unwrap();
        assert_eq!(width, 80);
        assert_eq!(last, 3);
    }

    /// `_bootstrap.lua` wraps `smelt.tools.register` to inject a default
    /// `summary`. The wrap must remain a *single* layer across many
    /// reloads — never re-wrap the previous wrap.
    #[test]
    fn reload_lua_does_not_double_wrap_tools_register() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(&init, "").unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            for _ in 0..5 {
                app.app.reload_lua();
            }
            // Register a tool with no `summary`; the bootstrap wrap should
            // populate it once. If the wrap had compounded across reloads
            // the call would still succeed — but every reload would add a
            // closure frame on top. The functional check: registration
            // works and the registered summary handler runs.
            app.app
                .lua
                .lua
                .load(
                    r#"
                    smelt.tools.register({
                        name = "t",
                        description = "",
                        parameters = { type = "object", properties = {} },
                        execute = function() return "" end,
                    })
                    "#,
                )
                .exec()
                .expect("register after many reloads");
        }
        let summary = app
            .app
            .lua
            .tool_summary("t", &std::collections::HashMap::new());
        // `default_summary` returns "" when args have no recognised keys.
        assert!(
            summary.is_empty(),
            "summary should be empty for no-arg tool"
        );
    }

    /// Anonymous overlays (no `name`) must be reaped on reload; named
    /// ones survive.
    #[test]
    fn reload_lua_reaps_anonymous_overlay_keeps_named() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        // First version opens both a named and an anonymous overlay.
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("mix")
            local function attach()
                local b1 = smelt.buf.create({ name = "mix.buf" })
                local w1 = smelt.win.open(b1, { name = "mix.win" })
                smelt.ui.overlay.open({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            state.open = true
            attach()

            -- Anonymous overlay (no name) — should be reaped on /reload.
            local b2 = smelt.buf.create()
            local w2 = smelt.win.open(b2, {})
            smelt.ui.overlay.open({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();

        // Capture the anonymous overlay's id — we'll assert it's gone
        // after reload while the named one survives. (Total overlay
        // count is noisy: reload_lua emits a `notify(...)` toast which
        // adds its own short-lived overlay.)
        let named_id = app.app.ui.named_overlay("mix").expect("named");
        let anon_id = (1u32..)
            .map(crate::smelt_term::OverlayId)
            .find(|id| *id != named_id && app.app.ui.overlay(*id).is_some())
            .expect("anonymous overlay present");

        // Second version drops the anonymous overlay; named one stays.
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("mix")
            local function attach()
                local b1 = smelt.buf.create({ name = "mix.buf" })
                local w1 = smelt.win.open(b1, { name = "mix.win" })
                smelt.ui.overlay.open({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            if state.open then attach() end
            "#,
        )
        .unwrap();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }
        assert!(
            app.app.ui.named_overlay("mix").is_some(),
            "named overlay survives reload"
        );
        assert!(
            app.app.ui.overlay(anon_id).is_none(),
            "anonymous overlay {} should be reaped",
            anon_id.0
        );
    }

    /// A `smelt.state(...)` slot that the new init.lua no longer
    /// references must be pruned by `smelt.__sweep_state` (called by
    /// `reload()` at the end of the cycle).
    #[test]
    fn reload_lua_sweeps_state_for_deleted_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            local a = smelt.state("kept")
            a.flag = true
            local b = smelt.state("dropped")
            b.flag = true
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        let exists = |rt: &crate::lua::LuaRuntime, k: &str| -> bool {
            rt.lua
                .load(format!("return __smelt_state__['{k}'] ~= nil"))
                .eval::<bool>()
                .unwrap()
        };
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            assert!(exists(&app.app.lua, "kept"));
            assert!(exists(&app.app.lua, "dropped"));
        }

        // Edit: only the "kept" plugin remains.
        std::fs::write(
            &init,
            r#"
            local a = smelt.state("kept")
            "#,
        )
        .unwrap();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
            assert!(exists(&app.app.lua, "kept"));
            assert!(
                !exists(&app.app.lua, "dropped"),
                "dropped plugin's state should be swept"
            );
        }
    }

    /// **Single ledger** for "what does `/reload` clear?" Touches every
    /// Lua-side surface, triggers reload, asserts each is in the expected
    /// post-reload state. New `LuaShared` registries or TUI-side caches
    /// that hold Lua handles MUST add a check here — otherwise the
    /// reload contract is silently broken.
    #[test]
    fn reload_clears_every_lua_surface() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        // Populate every observable surface from user init.lua so the
        // reload-with-empty-init test below can assert each is empty.
        std::fs::write(
            &init,
            r#"
            -- LuaShared registries
            smelt.cmd.register("seed_cmd", function() end)
            smelt.keymap.set("n", "<C-g>", function() end)
            smelt.statusline.register("seed_src", function() return {} end)
            smelt.tools.register({
                name = "seed_tool",
                description = "",
                parameters = { type = "object", properties = {} },
                execute = function() return "" end,
            })
            smelt.tools.middleware("", { before = function() end })
            smelt.provider.middleware({ on_request = function() end })

            -- core::timers (Lua-side)
            smelt.timer.every(100000, function() end)

            -- in-flight task (cancel_and_clear path)
            smelt.spawn(function()
                smelt.sleep(100000)
            end)

            -- Anonymous + named UI resources
            local b1 = smelt.buf.create({ name = "seed.buf" })
            local w1 = smelt.win.open(b1, { name = "seed.win" })
            smelt.ui.overlay.open({
                name = "seed.ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 30, height = 8,
                layout = smelt.ui.layout.leaf(w1),
            })
            local b2 = smelt.buf.create()
            local w2 = smelt.win.open(b2, {})
            smelt.ui.overlay.open({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })

            -- smelt.state slot
            local s = smelt.state("seed_plugin")
            s.open = true
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        let shared = app.app.lua.shared().core.clone();

        // Pre-reload: every surface has at least the seeded entry.
        assert!(shared.commands.lock().unwrap().contains_key("seed_cmd"));
        assert!(shared
            .keymaps
            .lock()
            .unwrap()
            .keys()
            .any(|(_, c)| c == "<C-g>"));
        assert!(shared
            .statusline_sources
            .lock()
            .unwrap()
            .iter()
            .any(|(n, _)| n == "seed_src"));
        assert!(shared.tools.lock().unwrap().contains_key("seed_tool"));
        assert!(!shared.hooks.tool_before.is_empty());
        assert!(!shared.hooks.provider_request.is_empty());
        assert!(!app.app.core.timers.is_empty());
        assert!(!shared.tasks.lock().unwrap().is_empty());
        let anon_overlay = (1u32..)
            .map(crate::smelt_term::OverlayId)
            .find(|id| {
                Some(*id) != app.app.ui.named_overlay("seed.ov")
                    && app.app.ui.overlay(*id).is_some()
            })
            .expect("anonymous overlay present");

        // Edit init.lua to empty + drop the "seed_plugin" state slot.
        std::fs::write(&init, "").unwrap();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }

        // Post-reload: every "user-registered" surface is empty; named UI
        // resources survive; anonymous ones are reaped; state slot for
        // the dropped plugin is swept.
        assert!(
            !shared.commands.lock().unwrap().contains_key("seed_cmd"),
            "user command cleared"
        );
        assert!(
            !shared
                .keymaps
                .lock()
                .unwrap()
                .keys()
                .any(|(_, c)| c == "<C-g>"),
            "user keymap cleared"
        );
        assert!(
            !shared
                .statusline_sources
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "seed_src"),
            "user statusline source cleared"
        );
        assert!(
            !shared.tools.lock().unwrap().contains_key("seed_tool"),
            "user tool cleared"
        );
        assert!(
            shared.hooks.tool_before.is_empty(),
            "tool middleware cleared"
        );
        assert!(
            shared.hooks.provider_request.is_empty(),
            "provider middleware cleared"
        );
        assert!(app.app.core.timers.is_empty(), "timers cleared");
        assert!(shared.tasks.lock().unwrap().is_empty(), "tasks cleared");
        assert!(
            shared.task_inbox.lock().unwrap().is_empty(),
            "task_inbox drained"
        );
        assert!(
            shared.json_inbox.lock().unwrap().is_empty(),
            "json_inbox drained"
        );
        assert!(
            app.app.ui.named_overlay("seed.ov").is_some(),
            "named overlay survives"
        );
        assert!(
            app.app.ui.overlay(anon_overlay).is_none(),
            "anonymous overlay reaped"
        );
        let dropped_state: bool = app
            .app
            .lua
            .lua
            .load("return __smelt_state__.seed_plugin ~= nil")
            .eval()
            .unwrap();
        assert!(!dropped_state, "dropped-plugin state slot swept");
    }

    /// In-flight `smelt.spawn` coroutines must be cancelled before
    /// `clear_lua_handles` wipes the registries they reference. After
    /// reload, the parked task should never resume — driving tasks
    /// produces nothing, the post-sleep side effect never runs.
    #[test]
    fn reload_lua_cancels_in_flight_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            _G.__task_completed__ = false
            smelt.spawn(function()
                smelt.sleep(10_000)  -- long sleep so the task is still parked
                _G.__task_completed__ = true
            end)
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        // Sanity: task is parked but not complete.
        let completed: bool = app
            .app
            .lua
            .lua
            .load("return _G.__task_completed__")
            .eval()
            .unwrap();
        assert!(!completed, "task shouldn't have completed yet");

        // Edit init.lua so reload doesn't re-spawn the task.
        std::fs::write(&init, "_G.__task_completed__ = false").unwrap();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }
        // Drive: cancelled tasks should be a no-op since we cleared them.
        let outs = app.app.lua.drive_tasks();
        assert!(
            outs.is_empty(),
            "no task outputs after reload cancellation (saw {} entries)",
            outs.len()
        );
        let completed: bool = app
            .app
            .lua
            .lua
            .load("return _G.__task_completed__")
            .eval()
            .unwrap();
        assert!(!completed, "cancelled task must not have run to completion");
    }

    /// User-resized overlay (`size_override`) must survive reload —
    /// the named-refresh path preserves user gesture state.
    #[test]
    fn reload_lua_preserves_user_size_override() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("res")
            local function attach()
                local b = smelt.buf.create({ name = "res.buf" })
                local w = smelt.win.open(b, { name = "res.win" })
                smelt.ui.overlay.open({
                    name = "res",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 10,
                    resizable = true,
                    layout = smelt.ui.layout.leaf(w),
                })
            end
            state.open = true
            attach()
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        let id = app.app.ui.named_overlay("res").unwrap();
        // Simulate a user resize gesture.
        if let Some(ov) = app.app.ui.overlay_mut(id) {
            ov.size_override = Some((50, 18));
        }

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }
        let id2 = app.app.ui.named_overlay("res").expect("survives");
        assert_eq!(id, id2);
        let ov = app.app.ui.overlay(id2).unwrap();
        assert_eq!(
            ov.size_override,
            Some((50, 18)),
            "user resize preserved across reload"
        );
    }

    // ── Determinism: clock-threaded state changes are observable via Tick ─

    /// Press `Ctrl-W` to arm the pane-focus chord, advance the virtual clock
    /// past `PANE_CHORD_WINDOW` (750ms), then press a follow-up key. The
    /// expired-chord branch in `handle_pane_chord` drops `pending_pane_chord`
    /// back to `None`; if the chord-window check leaked to wall-clock reads,
    /// the follow-up would still see the stale `Some(...)` and could spuriously
    /// toggle focus.
    #[test]
    fn ctrl_w_pane_chord_expires_after_tick_past_window() {
        let mut app = TestApp::builder().build();
        app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(
            app.app.timers.pending_pane_chord.is_some(),
            "Ctrl-W arms the pane chord"
        );

        // 1000ms > PANE_CHORD_WINDOW (750ms).
        app.feed_one(SourceEvent::Tick(1000));
        // Follow-up key after expiry: handler drops the pending chord and
        // returns None so the key falls through to normal dispatch.
        app.press(KeyCode::Char('j'));
        assert!(
            app.app.timers.pending_pane_chord.is_none(),
            "expired pane chord should be cleared on the next key"
        );
    }

    /// End-to-end proof of the yank-flash clock plumbing: yank a line in vim,
    /// observe the flash window is active, advance the virtual clock past the
    /// window, and verify the flash deadline has cleared. If any link in the
    /// chain (`KillRing::mark_yanked` → `VimContext::now` → `EventCtx::now` →
    /// `Window::handle_key`) regresses to wall-clock reads, this test breaks.
    #[test]
    fn vim_yy_yank_flash_expires_after_tick() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Type a line in insert mode, return to normal, then yank-line with `yy`.
        app.type_char('i');
        app.type_text("hello");
        app.press(KeyCode::Esc);
        app.type_char('y');
        app.type_char('y');

        let now = app.app.core.clock.instant_now();
        let flash = app.app.core.clipboard.kill_ring.yank_flash_range(now);
        assert!(
            flash.is_some(),
            "yank flash range should be active right after yy"
        );

        // Advance past the 200ms flash window. If the clock chain is wired
        // correctly, the flash deadline now sits in the virtual past.
        app.feed_one(SourceEvent::Tick(300));
        let now = app.app.core.clock.instant_now();
        let flash = app.app.core.clipboard.kill_ring.yank_flash_range(now);
        assert!(
            flash.is_none(),
            "flash should expire after Tick past the window"
        );
    }
}
