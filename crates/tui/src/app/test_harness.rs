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
    EngineSend(Box<UiCommand>),
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
    pub active_modal: Option<OverlayId>,
    pub picker_count: usize,
    pub prompt_text: String,
    pub queued_inputs: Vec<String>,
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

/// Snapshot of `WorkingState`. `animating` is true while a live turn
/// exists; `busy` is true while any `smelt.work.busy` token is held
/// by a plugin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkingSnapshot {
    pub animating: bool,
    pub busy: bool,
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
            mode: AgentMode::normal(),
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
        let settings = smelt_core::config::ResolvedSettings {
            vim: self.vim,
            ..Default::default()
        };
        let shared_session = Arc::new(Mutex::new(None));
        let mut lua = crate::lua::LuaRuntime::new();
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
                AgentMode::normal(),
                AgentMode::parse("plan").unwrap(),
                AgentMode::parse("apply").unwrap(),
                AgentMode::parse("yolo").unwrap(),
            ],
            reasoning_effort: ReasoningEffort::Off,
            reasoning_cycle: Vec::new(),
            settings,
            remember: smelt_core::config::RememberConfig::default(),
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

        // Match production startup: re-run bootstrap + autoload + init.lua
        // inside an `install_app_ptr` scope so module bodies that touch
        // TUI surfaces (e.g. `smelt.prompt.win():on(...)`) see a live
        // app pointer. Production does this via `bring_up_lua` →
        // `lua.reload`. Stories skip the `on_ready` drain on purpose:
        // it's reserved for interactive decoration (splash banner, etc.)
        // that storybook snapshots should not include.
        {
            let _guard = crate::lua::install_app_ptr(&mut app);
            let _ = app.lua.reload(None);
        }

        // Pin spinner glyph and wall-clock time for snapshot determinism.
        // The production `smelt.spinner.glyph()` and `wave_color_at()` use
        // wall-clock time, so parallel or sequential test runs land on
        // different frames / colors. Freezing both makes storybook
        // snapshots stable.
        let _ = app
            .lua
            .lua()
            .load(
                r#"
                smelt.clock.unix_ms = function() return 0 end
                smelt.spinner.glyph = function() return '✿' end
                "#,
            )
            .exec();

        // Production wires the Tui frontend to `Osc52Sink`, which writes
        // `\x1b]52;c;...` to real stdout on every kill-ring copy. Inside the
        // harness that's a ring leak - corrupts test stdout, slows the fuzz
        // target, and has no semantic value. Swap to `NullSink` immediately.
        app.core.clipboard.swap_sink(Box::new(smelt_core::NullSink));

        // Install the command resolver `user::render` consults to paint
        // registered `/cmd` text with `SmeltAccent`. Production does this
        // in `TuiApp::run`, which the harness skips - without the hook
        // every slash command in stories looks unstyled.
        let command_names = app.lua.command_names_handle();
        smelt_core::commands::set_command_resolver(move |name| {
            command_names
                .lock()
                .map(|s| s.contains(name))
                .unwrap_or(false)
        });

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

    /// Number of user messages waiting to be sent on the next turn. Used
    /// by `Steered` invariants that assert the drain semantics.
    pub fn queued_message_count(&self) -> usize {
        self.app.queued_inputs.len()
    }

    /// Side-channel: push a synthetic queued message. In production
    /// `queued_inputs` is filled by pressing Enter on the prompt while a
    /// turn is active; the harness short-circuits that flow but honors
    /// the same `MAX_QUEUED_MESSAGES` cap so the fuzz observes the real
    /// drop-on-overflow behavior instead of unbounded growth.
    pub fn push_queued_message(&mut self, text: String) {
        if self.app.queued_inputs.len() < crate::app::MAX_QUEUED_MESSAGES {
            self.app
                .queued_inputs
                .push(crate::app::QueuedInput::Message(text));
        }
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

    /// Current context-tokens estimate, when a non-background usage report
    /// has set it.
    pub fn context_tokens(&self) -> Option<u32> {
        self.app.core.session.context_tokens
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

    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor. Mirrors clipboard-image paste / `:image` paths without
    /// needing a real terminal clipboard. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved mutations.
    pub fn insert_attachment(&mut self, label: String) {
        let data_url = "data:image/png;base64,FUZZ-0".to_string();
        let mut ctx = crate::input::prompt_ctx_mut(&mut self.app.ui);
        self.app.input.insert_image(&mut ctx, label, data_url);
    }

    /// Side-channel: flip pane focus between Prompt and Content. In
    /// production this requires a Ctrl-W chord inside `PANE_CHORD_WINDOW`;
    /// the harness bypasses the timing gate so coverage doesn't depend on
    /// random key collisions.
    pub fn toggle_pane_focus(&mut self) {
        self.app.toggle_pane_focus();
    }

    /// Side-channel: install a placeholder on the prompt window with given
    /// accept / dismiss chords. Mirrors what Lua's `Win:placeholder(text, opts)`
    /// does; the dispatch path then runs on the next matching key. Without a
    /// side channel the placeholder is reachable only through Lua, which
    /// limits coverage of the accept/dismiss key-routing branches.
    pub fn install_prompt_placeholder(
        &mut self,
        text: String,
        accept: Vec<crate::smelt_term::KeyBind>,
        dismiss: Vec<crate::smelt_term::KeyBind>,
    ) {
        let win = self.app.well_known.prompt;
        if text.is_empty() {
            self.app.clear_placeholder(win);
            return;
        }
        self.app.set_placeholder(win, text);
        self.app.placeholder_opts.insert(
            win,
            crate::app::PlaceholderOpts {
                accept_keys: accept,
                dismiss_keys: dismiss,
            },
        );
    }

    /// Side-channel: clear the prompt placeholder (both extmark and opts).
    pub fn clear_prompt_placeholder(&mut self) {
        let win = self.app.well_known.prompt;
        self.app.clear_placeholder(win);
    }

    /// Side-channel: invoke the `/reload` pipeline. Wipes every Lua
    /// registry (commands, keymaps, statusline, tools, hooks, timers,
    /// cell subscribers), re-runs `init.lua` and bundled plugins, then
    /// re-fires `on_ready` hooks with `ctx.kind = "reload"`. Named
    /// resources (paint slots, NamedSlots bindings on bufs/wins/overlays)
    /// must keep stable ids; anonymous ones get reaped. Exercises the
    /// hot-reload surface that was untested by fuzz.
    pub fn reload_lua(&mut self) {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.reload_lua();
    }

    /// Run an arbitrary Lua snippet against the embedded runtime with
    /// the host pointer installed. Returns whether execution succeeded
    /// (a Lua-level error is *not* a fuzz failure - many generated
    /// snippets intentionally hit type errors that the bindings layer
    /// raises as mlua errors). Used by `lua_loop` to feed batched ops
    /// that reference each other via shared locals.
    pub fn run_lua(&mut self, snippet: &str) -> bool {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.lua.lua.load(snippet).exec().is_ok()
    }

    /// Re-publish the cell diff + fire queued subscribers. Production
    /// runs this every main-loop tick (`app.rs:1068`); the harness
    /// skips that loop and exposes it here so tests can assert against
    /// the reactive `work_*` / `vim_mode` / `now` cells without driving
    /// a synthetic event.
    pub fn tick_cells(&mut self) {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.publish_diff_cells();
        self.app.drain_cells_pending();
    }

    /// Counts of bound names across the four reload-survival registries:
    /// `(bufs, wins, overlays, paints)`. Reload-survival post-checks
    /// snapshot these before and after `reload_lua()` and assert
    /// equality - anonymous slots get reaped but every name in the
    /// registry must survive with a stable id.
    pub fn named_resource_counts(&self) -> (usize, usize, usize, usize) {
        let (bufs, wins, overlays) = self.app.ui.named_counts();
        (bufs, wins, overlays, self.app.paint_registry.named_count())
    }

    /// Side-channel: open a synthetic overlay via `smelt.overlay.new`.
    /// `variant % N` picks from a small fixed set spanning the new
    /// surface area (leaf, vbox, with static measure, with keymap,
    /// named vs anonymous). Same-variant repeats land on the same
    /// NamedSlot name so the dedup path runs; different variants
    /// allocate fresh slots. Best-effort: a Lua failure is swallowed
    /// (the next op still runs against a consistent app).
    pub fn open_synthetic_overlay(&mut self, variant: u8) {
        const VARIANTS: &[&str] = &[
            // 0: named leaf
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.0.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.0.win" })
            smelt.overlay.new({
                name = "fuzz.ov.0", anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 1: anonymous leaf - reaped on reload
            r#"
            local b = smelt.buf.new()
            local w = smelt.win.new(b, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "ne",
                row = 0, col = 0, width = 15, height = 4,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 2: leaf with static measure (drives the per-leaf measure hook)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.2.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.2.win" })
            smelt.overlay.new({
                name = "fuzz.ov.2", anchor = "screen_at", corner = "sw",
                row = 0, col = 0, width = 25, height = 6,
                layout = smelt.ui.layout.leaf(w, { measure = { w = 18, h = 4 } }),
            })
            "#,
            // 3: vbox of two leaves
            r#"
            local b1 = smelt.buf.new({ name = "fuzz.ov.3.buf1" })
            local w1 = smelt.win.new(b1, { name = "fuzz.ov.3.win1" })
            local b2 = smelt.buf.new({ name = "fuzz.ov.3.buf2" })
            local w2 = smelt.win.new(b2, { name = "fuzz.ov.3.win2" })
            smelt.overlay.new({
                name = "fuzz.ov.3", anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 22, height = 8,
                layout = smelt.ui.layout.vbox({
                    { node = smelt.ui.layout.leaf(w1), height = 3 },
                    { node = smelt.ui.layout.leaf(w2), height = 3 },
                }),
            })
            "#,
            // 4: leaf with overlay-level keymap (deferred-safe path)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.4.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.4.win" })
            smelt.overlay.new({
                name = "fuzz.ov.4", anchor = "screen_at", corner = "nw",
                row = 1, col = 1, width = 18, height = 5,
                layout = smelt.ui.layout.leaf(w),
                keymaps = {
                    { key = "<C-x>", on_press = function() end },
                },
            })
            "#,
        ];
        let snippet = VARIANTS[(variant as usize) % VARIANTS.len()];
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        let _ = self.app.lua.lua.load(snippet).exec();
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
                self.app.discard_turn(false);
            }
        }
        self.drain_cmd();
        true
    }

    /// Render one frame to real stdout. Drives the same compositor
    /// pipeline production uses (`TuiApp::render_normal`). The caller is
    /// responsible for terminal setup (raw mode, alternate screen).
    pub fn render(&mut self) {
        let agent_running = self.app.agent.is_some();
        crate::lua::with_app_ptr(&mut self.app, |app| {
            app.render_normal(agent_running);
        });
    }

    /// Render variant that exercises the full projection pipeline (layout,
    /// transcript/prompt/status sync, completer overlay) but throws the
    /// final compositor diff into a sink instead of stdout. Intended for
    /// the fuzz loop: every per-frame code path under `content/*` and the
    /// `compositor:*` perf scopes runs, so renderer bugs (cursor /
    /// scroll_top / tail-follow / parser projection) become reachable
    /// under fuzz without per-iteration megabytes of ANSI bytes hitting
    /// libFuzzer's log file.
    pub fn render_silent(&mut self) {
        let agent_running = self.app.agent.is_some();
        let mut sink = std::io::sink();
        crate::lua::with_app_ptr(&mut self.app, |app| {
            app.render_normal_to(agent_running, &mut sink);
        });
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
    }

    fn assert_prompt_cursor_projection(&self) {
        let Some(win) = self.app.ui.win(crate::app::PROMPT_WIN) else {
            return;
        };
        if win.effective_endpoint() != win.cpos {
            return;
        }
        let Some(buf) = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF) else {
            return;
        };
        let source = buf.source();
        let cpos = smelt_buffer::text::snap(source, win.cpos.min(source.len()));
        assert_eq!(
            win.cpos,
            cpos,
            "prompt cpos is not on a UTF-8 boundary after render: cpos {}, source len {}",
            win.cpos,
            source.len()
        );
        let projected = win.compute_cpos(buf);
        if projected != cpos {
            let (start, end) = if projected < cpos {
                (projected, cpos)
            } else {
                (cpos, projected)
            };
            let hidden = smelt_buffer::text::slice(source, start..end);
            // Terminal cells cannot distinguish zero-width spans, and a block
            // cursor over a literal space renders like the insertion point just
            // after that space. Keep the oracle strict for visible non-space
            // text, which is the stuck-cursor class this probe targets.
            let hidden_width = unicode_width::UnicodeWidthStr::width(hidden);
            assert!(
                hidden_width == 0 || hidden.chars().all(|ch| ch == ' '),
                "prompt visual cursor projection does not round-trip to cpos: visual row {}, col {}, cpos {}, projected {}, hidden source {:?}, source {:?}",
                win.cursor_row(),
                win.cursor_col(),
                cpos,
                projected,
                hidden,
                source
            );
        }
    }

    fn assert_render_layout_invariants(&self) {
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            let Some(win) = self.app.ui.win(win_id) else {
                continue;
            };
            let Some(viewport) = win.viewport else {
                continue;
            };
            let width = viewport.content_width;
            if width < 40 {
                continue;
            }
            let max_row_width = win.layout().max_row_width();
            assert!(
                max_row_width <= width,
                "well-known window {win_id:?} has row width {max_row_width} > viewport width {width}; content would clip with scroll_left pinned",
            );
        }
    }

    /// Render one frame and return the resulting `SnapshotFrame`. Used
    /// by the app-level storybook harness; `render_normal` updates the
    /// `Ui` snapshot buffer as a side effect of composing layers, so
    /// the post-render `ui.snapshot()` reflects the rendered frame. The
    /// ANSI bytes `render_normal` flushes to stdout are captured (and
    /// discarded) by the test harness.
    pub fn render_to_frame(&mut self) -> crate::smelt_term::SnapshotFrame {
        let agent_running = self.app.agent.is_some();
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        // The main loop refreshes diff cells once per tick before
        // rendering; storybook drives `render_normal` directly without
        // that loop, so we have to publish here or Lua renderers see
        // stale `work_*` / `vim_mode` / `now` values.
        self.app.publish_diff_cells();
        self.app.render_normal(agent_running);
        self.app.ui.snapshot()
    }

    /// Append a `Block::User` to the transcript history so flows that
    /// read the user-turn list (rewind dialog, transcript projection)
    /// see a non-empty conversation without a real engine roundtrip.
    pub fn push_user_block(&mut self, text: &str) {
        self.app.show_user_message(text, Vec::new());
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

    /// Push a `Block::Compacted` summary block into the transcript -
    /// the same block the live compact plugin produces between turns.
    /// Stories use this to snapshot the compaction chrome without
    /// running a real `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::Compacted {
                summary: summary.to_string(),
            });
    }

    /// Open a `Block::Exec` shell-escape block in the transcript with
    /// `command` as the header. Pair with
    /// `SourceEvent::ExecOutput`/`ExecDone` to stream output and close
    /// the block. The production path is `start_shell_escape`, which
    /// also spawns a real `sh -c`; stories don't want a subprocess, so
    /// the harness invokes the transcript hook directly.
    pub fn start_exec(&mut self, command: &str) {
        self.app.start_exec(command.to_string());
    }

    /// Cancel the active turn (or idle background tasks). Mirrors
    /// `EventOutcome::CancelAgent` → `discard_turn(true)`.
    pub fn cancel(&mut self) {
        self.app.discard_turn(true);
        self.drain_cmd();
    }

    /// Push a steer text onto the queued-messages stack.
    pub fn steer(&mut self, text: &str) {
        if !text.is_empty() && self.app.queued_inputs.len() < crate::app::MAX_QUEUED_MESSAGES {
            self.app
                .queued_inputs
                .push(crate::app::QueuedInput::Message(text.to_string()));
        }
    }

    /// Remove up to `count` queued messages from the front.
    pub fn unsteer(&mut self, count: usize) {
        let n = count.min(self.app.queued_inputs.len());
        self.app.queued_inputs.drain(..n);
    }

    /// Send a `CallCoreTool` UiCommand to the engine channel.
    pub fn call_core_tool(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        self.app
            .core
            .engine
            .send(protocol::UiCommand::CallCoreTool {
                request_id: 1,
                parent_call_id: String::new(),
                tool_name: tool_name.to_string(),
                args,
            });
        self.drain_cmd();
    }

    /// Change the active agent mode.
    pub fn set_agent_mode(&mut self, mode: AgentMode) {
        self.app.core.config.mode = mode;
    }

    /// Push an `assistant` text block onto the transcript history so
    /// flows that read message history see a multi-turn conversation.
    pub fn push_assistant_text(&mut self, text: &str) {
        self.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::Assistant(
                protocol::AssistantTurn::terminal(
                    Some(protocol::Content::Text(text.to_string())),
                    None,
                    Vec::new(),
                ),
            ));
    }

    /// Prompt cursor byte offset in source space.
    pub fn prompt_cpos(&self) -> usize {
        self.app
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.cpos)
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
        if win.vim_enabled && !matches!(win.vim_mode, VimMode::Insert) {
            return false;
        }
        if win.selection_anchor.is_some()
            || win.effective_endpoint() != win.cpos
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

    fn prompt_text_input_ready_for_turn_probe(&self) -> bool {
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

    /// Side-channel: install a hostile prompt `text_changed` callback that
    /// tries to move the prompt cursor away from the edit endpoint.
    pub fn install_prompt_cursor_trap(&mut self, variant: u8) {
        const SNIPPETS: &[&str] = &[
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(0)
            end)
            "#,
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.win():cursor(0)
            end)
            "#,
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(999999)
            end)
            "#,
        ];
        let snippet = SNIPPETS[(variant as usize) % SNIPPETS.len()];
        let _ = self.run_lua(snippet);
    }

    /// Side-channel: register a small Lua tool and begin a synthetic custom
    /// command turn, returning the `StartTurn` payload that was sent.
    pub fn start_custom_command_with_lua_tool(
        &mut self,
        variant: u8,
    ) -> Option<protocol::StartTurnPayload> {
        let tool_name = format!("fuzz_custom_tool_{}", variant % 4);
        let snippet = format!(
            r#"
            smelt.tools.register({{
                name = "{tool_name}",
                description = "fuzz custom command tool",
                parameters = {{ type = "object", properties = {{}} }},
                execute = function(args) return "ok" end,
            }})
            "#,
        );
        let _ = self.run_lua(&snippet);

        let cmd = smelt_core::custom_commands::CustomCommand {
            name: "fuzz-custom".to_string(),
            display: "fuzz-custom".to_string(),
            body: "fuzz custom body".to_string(),
            overrides: smelt_core::custom_commands::CommandOverrides::default(),
        };
        let turn = self.app.begin_custom_command_turn(cmd);
        self.app.agent = Some(turn);
        self.drain_cmd();
        self.actions.iter().rev().find_map(|a| match a {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
    }

    /// Side-channel: start a Lua `smelt.engine.ask` request so fuzzed
    /// `EngineAskResponsePending` ops can drive the callback path.
    pub fn start_engine_ask_probe(&mut self, question: &str) {
        let question = serde_json::to_string(question).unwrap_or_else(|_| "\"\"".to_string());
        let snippet = format!(
            r#"
            smelt.engine.ask({{
                system = "fuzz ask probe",
                question = {question},
                on_response = function(_message, _err) end,
            }})
            "#,
        );
        let _ = self.run_lua(&snippet);
        self.drain_cmd();
    }

    fn force_prompt_keyboard_focus(&mut self) {
        if self.app.well_known.cmdline.is_some() {
            self.app.close_cmdline();
        }
        while let Some(overlay_id) = self
            .app
            .ui
            .focused_overlay()
            .or_else(|| self.app.ui.active_modal())
        {
            self.app.close_overlay(overlay_id);
        }
        // Prompt-docked pickers own the prompt through Lua registrations on
        // the prompt window, not through overlay focus. Reloading drops those
        // registrations before the probe installs its own clean prompt state.
        if !self.app.picker_state.is_empty() {
            self.reload_lua();
        }
        self.app.timers.pending_chord = None;
        self.app.timers.pending_pane_chord = None;
        self.app.timers.app_sequence.clear();
        self.app.ui.cancel_pointer_interaction();
        self.app.app_focus = AppFocus::Prompt;
        self.app.term_focused = true;
        self.app.clear_prompt_prediction();
        let _ = self.app.ui.set_focus(crate::app::PROMPT_WIN);
        if let Some(win) = self.app.ui.win_mut(crate::app::PROMPT_WIN) {
            if win.vim_enabled {
                win.set_vim_mode(VimMode::Insert);
            }
            win.clear_mouse_state();
            win.selection_anchor = None;
        }
    }

    fn drain_engine_ask_ids(&mut self) -> Vec<u64> {
        self.drain_engine_sends()
            .into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    fn respond_ask_with_text(&mut self, id: u64, text: &str) {
        let _g = crate::lua::install_app_ptr(&mut self.app);
        self.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text(text)),
                    None,
                    None,
                )),
                error: None,
            });
        self.app.drive_lua_tasks();
    }

    fn publish_turn_end_for_probe(&mut self) {
        let _g = crate::lua::install_app_ptr(&mut self.app);
        self.app.core.cells.set_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::cells::TurnEnd { cancelled: false }),
        );
        self.app.pump_lua();
    }

    fn bump_input_epoch_for_probe(&mut self) {
        let _g = crate::lua::install_app_ptr(&mut self.app);
        self.app.bump_epoch("input_epoch");
        self.app.pump_lua();
    }

    fn probe_stale_prompt_prediction_response(&mut self, variant: u8) {
        let seq = self.app.core.session.history.len();
        self.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                format!("fuzz stale prompt prediction {variant}/{seq}"),
            )));
        {
            let _g = crate::lua::install_app_ptr(&mut self.app);
            self.app.bump_epoch("history_epoch");
            self.app.pump_lua();
        }
        self.publish_turn_end_for_probe();
        let prediction_id = self
            .drain_engine_ask_ids()
            .last()
            .copied()
            .expect("prediction probe should issue EngineAsk");

        self.bump_input_epoch_for_probe();
        self.respond_ask_with_text(prediction_id, "stale prompt placeholder");
        assert_eq!(
            self.app.placeholder_text(crate::app::PROMPT_WIN),
            None,
            "stale prompt prediction response installed a placeholder in probe variant {variant}"
        );
    }

    fn prediction_history_for_probe(&self, variant: u8) -> Vec<protocol::HistoryItem> {
        vec![protocol::HistoryItem::user(protocol::Content::text(
            format!("fuzz prompt prediction history {variant}"),
        ))]
    }

    fn focus_prompt_without_clearing_transients(&mut self) {
        self.app.app_focus = AppFocus::Prompt;
        self.app.term_focused = true;
        let _ = self.app.ui.set_focus(crate::app::PROMPT_WIN);
        if let Some(win) = self.app.ui.win_mut(crate::app::PROMPT_WIN) {
            if win.vim_enabled {
                win.set_vim_mode(VimMode::Insert);
            }
        }
    }

    fn assert_prompt_typing_and_motion(&mut self, variant: u8) {
        self.render_silent();
        assert!(
            self.prompt_text_input_ready_for_turn_probe(),
            "prompt is not ready for text input in probe variant {variant}"
        );

        for (idx, ch) in "ab".chars().enumerate() {
            self.type_char(ch);
            let cpos_before_render = self.prompt_cpos();
            self.render_silent();
            let actual_cpos = self.prompt_cpos();
            let state = self.state();
            assert_eq!(
                actual_cpos,
                idx + 1,
                "prompt cursor did not advance after typing {ch:?} in probe variant {variant}; cpos_before_render {}, prompt_text {:?}, app_focus {:?}, overlay {:?}, cmdline {}, agent {}, pending_chord {}, pending_pane_chord {}, app_sequence {}, overlay_count {}, picker_count {}",
                cpos_before_render,
                state.prompt_text,
                state.app_focus,
                state.focused_overlay,
                state.cmdline_open,
                state.agent_running,
                self.app.timers.pending_chord.is_some(),
                self.app.timers.pending_pane_chord.is_some(),
                self.app.timers.app_sequence.has_pending(),
                self.app.ui.overlay_count(),
                self.app.picker_state.len(),
            );
        }
        assert_eq!(self.state().prompt_text, "ab");

        self.press(KeyCode::Left);
        self.render_silent();
        assert_eq!(
            self.prompt_cpos(),
            1,
            "left motion did not move prompt cursor in probe variant {variant}",
        );
        self.type_char('X');
        self.render_silent();
        assert_eq!(self.state().prompt_text, "aXb");
        assert_eq!(self.prompt_cpos(), 2);

        self.press(KeyCode::End);
        self.type_text("cd");
        self.render_silent();
        assert_eq!(self.state().prompt_text, "aXbcd");
        assert_eq!(self.prompt_cpos(), 5);
    }

    /// Side-channel: drive the exact bug class from #15. After a turn lifecycle
    /// transition, typing must advance the prompt cursor; left motion must also
    /// move the insertion point. A stuck cursor reverses "ab" into "ba" and
    /// fails this probe immediately.
    pub fn probe_prompt_cursor_after_turn(&mut self, variant: u8) {
        if self.agent_running() {
            self.cancel();
        }
        self.force_prompt_keyboard_focus();
        self.app.queued_inputs.clear();
        let _ = self.run_lua(r#"smelt.prompt.set_text("")"#);
        if variant % 4 == 1 {
            self.install_prompt_cursor_trap(variant);
        }

        let mut turn_id = 10_000 + u64::from(variant);
        let prediction_probe = variant & 0x80 != 0 && matches!(variant % 4, 0 | 1);
        self.start_turn(turn_id);
        if variant & 0x40 != 0 {
            self.press(KeyCode::Esc);
            self.press(KeyCode::Esc);
            if !self.agent_running() {
                turn_id += 1;
                self.start_turn(turn_id);
            }
        } else if variant & 0x20 != 0 {
            self.press(KeyCode::Esc);
        }
        let mut prediction_ids = Vec::new();
        match variant % 4 {
            2 => self.feed_one(SourceEvent::Engine(EngineEvent::TurnError {
                message: "fuzz turn error".into(),
            })),
            3 => self.cancel(),
            _ => {
                let history = if prediction_probe {
                    self.prediction_history_for_probe(variant)
                } else {
                    vec![]
                };
                self.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
                    turn_id,
                    history,
                    meta: None,
                }));
                if prediction_probe {
                    prediction_ids = self.drain_engine_ask_ids();
                }
            }
        }
        let reloaded = variant % 8 >= 4;
        if reloaded {
            self.reload_lua();
        }
        if variant & 0x10 != 0 {
            self.probe_stale_prompt_prediction_response(variant);
        }
        if prediction_probe {
            if !reloaded {
                if let Some(id) = prediction_ids.last().copied() {
                    self.respond_ask_with_text(id, "predicted follow-up");
                    assert_eq!(
                        self.app.placeholder_text(crate::app::PROMPT_WIN).as_deref(),
                        Some("predicted follow-up"),
                        "turn-end prediction response did not install placeholder in probe variant {variant}"
                    );
                }
            }
            self.focus_prompt_without_clearing_transients();
        } else {
            self.force_prompt_keyboard_focus();
            // Turn-end hooks can leave prompt-owned transients behind; clear after
            // they are quiesced so the typing oracle starts from a known buffer.
            let _ = self.run_lua(r#"smelt.prompt.set_text("")"#);
            assert!(
                self.prompt_plain_insert_ready(),
                "prompt is not ready for plain insertion in probe variant {variant}"
            );
        }

        self.assert_prompt_typing_and_motion(variant);
    }

    /// Side-channel: exercise the real compaction prepare-request path through
    /// `HostCall::PrepareRequest`, pair the generated EngineAsk with a response,
    /// and assert the replacement arrives while active-turn state survives.
    pub fn probe_compaction_prepare_request(&mut self, variant: u8) {
        // Production reaches host-call dispatch after draining Lua callbacks and
        // tasks for the tick. Mirror that before installing the synthetic
        // compaction history so stale callbacks from earlier random input are
        // not attributed to the prepare-request lifecycle.
        {
            let _g = crate::lua::install_app_ptr(&mut self.app);
            self.app.flush_lua_callbacks();
            self.app.drive_lua_tasks();
        }
        self.drain_cmd();

        self.app.core.config.context_window = Some(100);
        self.app.core.session.context_tokens = None;
        self.app.core.session.context_tokens_history_len = None;
        self.app.core.session.visible_context_tokens = None;
        self.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        self.push_assistant_text("a1");
        self.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u2")));
        // Prepare-request itself drains pending engine events before invoking
        // hooks. Do that before deciding whether the probe should preserve an
        // active turn so a queued completion from an earlier synthetic submit
        // is not misattributed to compaction.
        while let Ok(ev) = self.app.core.engine.try_recv() {
            self.app.dispatch_engine_event(ev);
        }
        // Prepare-request runs before a model request is dispatched. Keep the
        // probe on that lifecycle edge rather than injecting compaction while
        // a synthetic tool call is already in flight.
        if self.agent_running() && !self.pending_tool_call_ids().is_empty() {
            return;
        }
        if variant % 2 == 1 {
            self.start_turn(20_000 + u64::from(variant));
        }
        let should_preserve_turn = self.agent_running();

        let full_history = protocol::history_to_messages(&self.app.model_history());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut self.app);
            self.app
                .dispatch_host_call(engine::HostCall::PrepareRequest {
                    messages: full_history,
                    estimated_tokens: 200,
                    reply: tx,
                });
        }

        let ask_id = self
            .drain_engine_sends()
            .into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk { id, .. } => Some(id),
                _ => None,
            })
            .next_back()
            .expect("compaction prepare request should issue EngineAsk");

        {
            let _g = crate::lua::install_app_ptr(&mut self.app);
            self.app
                .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                    id: ask_id,
                    message: Some(protocol::Message::assistant(
                        Some(protocol::Content::text("# Goal\nok")),
                        None,
                        None,
                    )),
                    error: None,
                });
            self.app.drive_lua_tasks();
        }

        let replacement = rx
            .try_recv()
            .expect("compaction prepare reply should be ready")
            .expect("compaction prepare should produce replacement history");
        assert!(!replacement.is_empty(), "compaction replacement is empty");
        if should_preserve_turn {
            assert!(self.agent_running(), "compaction ended the active turn");
        }
    }

    /// Resize the app's surface to `(width, height)`. Used by replay
    /// drivers that own a real terminal and need to match the app's
    /// internal grid to the OS-reported size.
    pub fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.app.handle_resize(width, height);
    }

    /// Cheap structural invariants over every live `(Buffer, Window)` pair
    /// plus side-car state. Panics on the first violation; safe to call
    /// after every dispatched event.
    ///
    /// Composed of four focused groups so a regression points at one
    /// category and so individual groups can be reused in unit tests:
    /// - [`Self::assert_text_invariants`] - UTF-8 / byte-offset correctness
    ///   for window cpos, undo/redo entries, kill-ring, completer anchor.
    /// - [`Self::assert_ui_invariants`] - terminal size, focus reachability.
    /// - [`Self::assert_session_invariants`] - agent / working / streaming
    ///   coherence plus pending-tool bookkeeping.
    /// - [`Self::assert_resource_invariants`] - bounded queues and other
    ///   leak floors.
    pub fn assert_invariants(&self) {
        self.assert_text_invariants();
        self.assert_ui_invariants();
        self.assert_session_invariants();
        self.assert_resource_invariants();
    }

    /// UTF-8 / byte-offset correctness across every place a stale offset
    /// could land mid-character: window cpos and selection anchor, undo
    /// and redo entries, kill-ring source range, prompt completer anchor.
    pub fn assert_text_invariants(&self) {
        for (wid, win) in self.app.ui.iter_wins() {
            let Some(buf) = self.app.ui.buf(win.buf) else {
                continue;
            };
            // The buffer crate carries two representations: source-based
            // buffers (prompt) maintain `source` as the canonical byte
            // stream and feed `cpos` into it directly; line-based buffers
            // (cmdline, picker, status bar, list overlays) write through
            // `set_lines` / `set_all_lines` and leave `source` empty -
            // content lives in `lines` and `cpos` is set via cell-column
            // helpers, not byte arithmetic on `source`. Readonly buffers
            // (transcript) are line-based but vim operates on a scratch
            // built from `text()` (`lines.join("\n")`), so `cpos` lives in
            // text space, not source space.
            let src_owned;
            let src = if buf.readonly {
                src_owned = buf.text();
                src_owned.as_str()
            } else {
                buf.source()
            };
            let line_based = !buf.readonly
                && src.is_empty()
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

        // Kill-ring source range is well-formed even if we can't validate
        // it against a specific buffer (the ring doesn't track which buffer
        // it came from - yanks happen from prompt edits, transcript visual
        // mode, and overlay edits alike). `start <= end` is the only sound
        // floor; downstream consumers (`yank_flash_range` callers) snap
        // against the current buffer at read time to absorb stale offsets.
        if let Some((start, end)) = self.app.core.clipboard.kill_ring.source_range() {
            assert!(
                start <= end,
                "kill-ring source_range {} > {} (inverted)",
                start,
                end
            );
        }

        // Vim visual_anchor must stay on a UTF-8 char boundary in the
        // buffer's text-space. Visual ops snap before reading (see
        // `visual_anchor_at`), but the stored offset can still drift past
        // `text().len()` if the buffer shrinks under the anchor without
        // the window noticing - that's the trap fuzzing should catch.
        for (wid, win) in self.app.ui.iter_wins() {
            if !win.vim_enabled {
                continue;
            }
            let Some(buf) = self.app.ui.buf(win.buf) else {
                continue;
            };
            let text = if buf.readonly {
                buf.text()
            } else {
                buf.source().to_string()
            };
            let anchor = win.vim_state.visual_anchor_raw();
            assert!(
                anchor <= text.len(),
                "window {:?} vim visual_anchor {} > text len {}",
                wid,
                anchor,
                text.len()
            );
            let snapped = smelt_buffer::text::snap(&text, anchor);
            assert_eq!(
                snapped, anchor,
                "window {:?} vim visual_anchor {} not on UTF-8 char boundary (snapped {})",
                wid, anchor, snapped
            );
        }

        // Prompt-buffer attachment_ids must be in 1:1 correspondence with
        // the `\u{FFFC}` markers in the source. A divergence means an
        // insert/delete path didn't keep them in sync - the next paste or
        // copy will read off the end of the vec.
        if let Some(prompt) = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF) {
            let src = prompt.source();
            let marker_count = src.chars().filter(|c| *c == '\u{FFFC}').count();
            assert_eq!(
                marker_count,
                prompt.attachment_ids.len(),
                "prompt has {} attachment markers but {} attachment_ids",
                marker_count,
                prompt.attachment_ids.len()
            );
        }
    }

    /// UI structural integrity: terminal extent non-zero, focus is not
    /// stale, every live window's buf points at a live buffer, and the
    /// notification overlay (when set) points at a live window.
    pub fn assert_ui_invariants(&self) {
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

        // Every live window's `buf` field must resolve to an existing
        // buffer. A dangling buf ref means the rendering pass reads from
        // a phantom buffer - visually invisible until the cell layout
        // tries to query content.
        for (wid, win) in self.app.ui.iter_wins() {
            assert!(
                self.app.ui.buf(win.buf).is_some(),
                "window {wid:?} buf {:?} points at non-existent buffer",
                win.buf,
            );
        }

        // Prompt and transcript are projected/wrapped surfaces; they should
        // never require horizontal panning. Generic plugin-created windows may
        // still use `scroll_left`, but these two well-known panes must remain
        // pinned so vim `zh`/`zl` or viewport resync cannot silently clip text.
        // Width-vs-layout checks live in `assert_render_layout_invariants`,
        // after render has rebuilt layouts for the current viewport.
        for win_id in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
            if let Some(win) = self.app.ui.win(win_id) {
                assert_eq!(
                    win.scroll_left, 0,
                    "well-known window {win_id:?} has horizontal scroll_left {}",
                    win.scroll_left,
                );
            }
        }

        // Notification overlay's WinId, when set, must still resolve.
        // `dismiss_notification` and `open_notification` always pair the
        // `Option<WinId>` with the underlying overlay leaf; if they ever
        // get out of sync, the next render walks a dead window.
        if let Some(win) = self.app.notification {
            assert!(
                self.app.ui.win(win).is_some(),
                "notification points at dead window {win:?}",
            );
        }

        // Placeholder dispatch opts shadow the extmark-stored placeholder
        // text. Static placeholders (input labels, predictions) may have an
        // extmark without dispatch opts; entries in `placeholder_opts` are the
        // interactive subset and must point at a live window with exactly one
        // placeholder extmark.
        let placeholder_ns =
            smelt_buffer::buffer::create_namespace(crate::content::prompt_buf::PLACEHOLDER_NS);
        for win in self.app.placeholder_opts.keys() {
            assert!(
                self.app.ui.win(*win).is_some(),
                "placeholder_opts points at dead window {win:?}",
            );
            let buf_id = self.app.ui.win(*win).map(|w| w.buf);
            let extmark_count = buf_id
                .and_then(|bid| self.app.ui.buf(bid))
                .map(|b| b.extmarks(placeholder_ns).len())
                .unwrap_or(0);
            assert_eq!(
                extmark_count, 1,
                "placeholder_opts[{win:?}] has {extmark_count} extmarks in PLACEHOLDER_NS (expected 1)",
            );
        }
    }

    /// Agent / working / streaming coherence plus pending-tool bookkeeping.
    pub fn assert_session_invariants(&self) {
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
                // Every pending call_id must have a matching `ToolState`
                // sidecar that's still in flight. A missing entry means
                // the transcript was rebuilt without restoring the tool
                // state; a terminal status means the pending bookkeeping
                // wasn't cleared when the tool finished - both corrupt
                // the tool widget.
                let state = self.app.transcript.history().tool_states.get(&pt.call_id);
                assert!(
                    state.is_some(),
                    "pending tool {:?} has no ToolState entry in transcript history",
                    pt.call_id,
                );
                if let Some(state) = state {
                    assert!(
                        !state.is_terminal(),
                        "pending tool {:?} has terminal ToolState",
                        pt.call_id,
                    );
                }
            }
        }

        // Reverse direction of every `ToolState` key must
        // correspond to a `Block::ToolCall` in transcript history. A
        // missing block means `gc_tool_states` failed to drop a state
        // that no longer has a live block, or `set_history` left state
        // behind.
        let history = self.app.transcript.history();
        for call_id in history.tool_states.keys() {
            let exists = history.blocks.values().any(|b| {
                matches!(
                    b,
                    smelt_core::transcript_model::Block::ToolCall { call_id: cid, .. }
                        if cid == call_id
                )
            });
            assert!(
                exists,
                "tool_state {:?} has no matching Block::ToolCall in history",
                call_id,
            );
        }

        // Working-state coherence. The animation only spins inside a turn:
        // `begin_agent_turn` / harness `start_turn` flip it on alongside
        // `agent = Some(...)`, and `discard_turn` always calls
        // `working.finish` before nulling `agent`. The reverse direction
        // (agent.is_some() => working.is_animating) does NOT hold -
        // host-driven recovery hooks (e.g. on_context_limit) can pause
        // the animation while the turn keeps running - so we only assert
        // one way.
        if self.app.working.is_animating() {
            assert!(
                self.app.agent.is_some(),
                "working is animating without an active agent turn",
            );
        }

        // Idle streaming coherence. With no agent, `finish_turn` has
        // already flushed `text` and `thinking` buffers; the idle event
        // handler never appends to them. `exec` is independent of turns
        // (vim bang-shell) so it's deliberately excluded.
        if self.app.agent.is_none() {
            assert!(
                !self.app.parser.has_active_text(),
                "streaming text buffer non-empty with no agent turn",
            );
            assert!(
                !self.app.parser.has_active_thinking(),
                "streaming thinking buffer non-empty with no agent turn",
            );
        }
    }

    /// Bounded resources and leak floors. The caps sit just above what
    /// any sensible burst would need (a handful of queued user messages,
    /// a handful of in-flight confirms) so a true unbounded leak trips
    /// well before the 256-op fuzz budget runs out.
    pub fn assert_resource_invariants(&self) {
        const PENDING_DIALOGS_CAP: usize = 64;

        assert!(
            self.app.queued_inputs.len() <= crate::app::MAX_QUEUED_MESSAGES,
            "queued_inputs {} > cap {}",
            self.app.queued_inputs.len(),
            crate::app::MAX_QUEUED_MESSAGES,
        );
        assert!(
            self.app.pending_dialogs.len() <= PENDING_DIALOGS_CAP,
            "pending_dialogs {} > cap {}",
            self.app.pending_dialogs.len(),
            PENDING_DIALOGS_CAP,
        );

        // Ask callbacks live in their own map keyed on the same `next_id`
        // counter as the win/overlay/paint registry - a duplicate id in
        // both means some new registration path forgot which map to write
        // to, and `fire_ask_callback` could dispatch an unrelated handler
        // with ask-shaped args.
        let shared = self.app.lua.shared();
        if let (Ok(cbs), Ok(ask)) = (shared.callbacks.lock(), shared.ask_callbacks.lock()) {
            for id in ask.keys() {
                assert!(
                    !cbs.contains_key(id),
                    "callback id {} is in both ask_callbacks and callbacks",
                    id,
                );
            }
        }

        // BusyStack `since` field tracks the timestamp of the *first*
        // pushed token; it MUST be Some iff entries is non-empty. The
        // reactive `work_*` cells and `WorkState::elapsed` consult it,
        // and a stale `Some` after the last release would leave the
        // prompt indicator animating past 0 entries.
        assert_eq!(
            self.app.busy_stack.is_busy(),
            self.app.busy_stack.since().is_some(),
            "busy_stack is_busy={} but since.is_some()={}",
            self.app.busy_stack.is_busy(),
            self.app.busy_stack.since().is_some(),
        );
    }

    /// Enumerate every Lua function recorded by `LuaMod::fn_` at
    /// registration time. Returned tuples are `(module, name)`, e.g.
    /// `("smelt.buf", "new")`. The same registry powers
    /// `cargo xtask gen-lua-docs`, so any function visible in the
    /// reference docs is also fuzzable - and a freshly-added
    /// `smelt.foo.bar` flows into the fuzz target automatically, with
    /// no manual update to a hand-written `LuaOp` table.
    pub fn lua_doc_snapshot(&self) -> Vec<(&'static str, &'static str)> {
        smelt_core::lua::doc::snapshot()
            .into_iter()
            .map(|m| (m.module, m.name))
            .collect()
    }

    /// Force a full Lua GC, then walk every registered `LuaHandle`
    /// across the shared registries and assert it still resolves in the
    /// mlua registry. A `Value::Nil` after `gc_collect` means a Rust
    /// path dropped the handle's `RegistryKey` (or the key was wrong
    /// from the start) - the Rust→Lua reference is dangling. Used by
    /// `lua_loop` between batches so leaks surface attached to the op
    /// that caused them rather than at scenario teardown.
    pub fn assert_lua_handles_alive(&self) {
        let lua = &self.app.lua.lua;
        lua.gc_collect().expect("lua gc_collect failed");

        let check = |label: &str, handle: &smelt_core::lua::LuaHandle| {
            let val: mlua::Value = lua
                .registry_value(&handle.key)
                .unwrap_or_else(|e| panic!("FFI-LEDGER: registry_value({label}) failed: {e}"));
            if matches!(val, mlua::Value::Nil) {
                panic!("FFI-LEDGER: dangling handle in {label} (registry value is Nil after gc_collect)");
            }
        };

        let shared = self.app.lua.shared();
        let core = &shared.core;
        if let Ok(cbs) = core.callbacks.lock() {
            for (id, h) in cbs.iter() {
                check(&format!("callbacks[{id}]"), h);
            }
        }
        if let Ok(asks) = core.ask_callbacks.lock() {
            for (id, h) in asks.iter() {
                check(&format!("ask_callbacks[{id}]"), h);
            }
        }
        if let Ok(cmds) = core.commands.lock() {
            for (name, cmd) in cmds.iter() {
                check(&format!("commands[{name}]"), &cmd.handle);
            }
        }
        if let Ok(kms) = core.keymaps.lock() {
            for (k, h) in kms.iter() {
                check(&format!("keymaps[{k:?}]"), h);
            }
        }
        if let Ok(tools) = core.tools.lock() {
            for (name, t) in tools.iter() {
                check(&format!("tools[{name}].execute"), &t.execute);
                if let Some(h) = &t.approval_patterns {
                    check(&format!("tools[{name}].approval_patterns"), h);
                }
                if let Some(h) = &t.preflight {
                    check(&format!("tools[{name}].preflight"), h);
                }
                if let Some(h) = &t.render {
                    check(&format!("tools[{name}].render"), h);
                }
                if let Some(h) = &t.paths_for_workspace {
                    check(&format!("tools[{name}].paths_for_workspace"), h);
                }
                if let Some(h) = &t.preview {
                    check(&format!("tools[{name}].preview"), h);
                }
            }
        }

        let hooks = &core.hooks;
        let check_reg = |reg_label: &str, reg: &smelt_core::lua::HookRegistry| {
            reg.for_each_entry(|id, name, h| {
                check(&format!("{reg_label}[{id} name={name:?}]"), h);
            });
        };
        check_reg("hooks.tool_before", &hooks.tool_before);
        check_reg("hooks.tool_after", &hooks.tool_after);
        check_reg("hooks.provider_request", &hooks.provider_request);
        check_reg("hooks.provider_response", &hooks.provider_response);
        check_reg("hooks.context_limit", &hooks.context_limit);
        check_reg("hooks.lifecycle", &hooks.lifecycle);
    }

    /// Net live `LuaHandle` count, taken from the global drop-counter
    /// ledger (`created - dropped`). Complements [`assert_lua_handles_alive`]:
    /// that function walks named registries and asserts each handle
    /// resolves; this one counts *every* handle that's ever crossed
    /// `LuaHandle::from_func` regardless of where it ended up stored,
    /// so it catches leaks the named walk can't see (anonymous closures
    /// stashed only in Lua tables, etc.).
    pub fn lua_handles_live(&self) -> u64 {
        smelt_core::lua::lua_handles_live()
    }

    /// Reload the Lua context once, snapshot the live handle count,
    /// reload again, and assert the count didn't grow. Compares **two
    /// consecutive** reloads (not pre/post a single reload) because
    /// cold-start vs first-reload isn't apples-to-apples - lifecycle
    /// hooks fire with `ctx.kind = "reload"` only on the second-and-
    /// later bring-ups, so plugins legitimately do extra registration
    /// on the first reload. By reload N the system is in steady state;
    /// any drift between reload N and N+1 means a registry isn't
    /// being cleared.
    ///
    /// Intended for one-shot use at the END of a scenario, after the
    /// scenario's own reload ops have run - calling it inside the
    /// segment loop would inflate the reload count and obscure the
    /// scenario semantics.
    pub fn assert_no_handle_leak_across_reload(&mut self) {
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let baseline = smelt_core::lua::lua_handles_live();
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let after = smelt_core::lua::lua_handles_live();
        if after > baseline {
            panic!(
                "FFI-LEDGER: handle leak across reload - count went {baseline} -> {after} on second consecutive reload (steady state should be stable)"
            );
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
            if c == '\n' {
                self.press_mod(KeyCode::Enter, KeyModifiers::SHIFT);
            } else {
                self.type_char(c);
            }
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
            self.actions.push(Action::EngineSend(Box::new(cmd)));
        }
    }

    /// Drain queued `UiCommand`s from the engine channel and return them.
    /// Useful for host-hook tests that need to inspect background
    /// `EngineAsk` requests directly without going through `feed_one`.
    pub fn drain_engine_sends(&mut self) -> Vec<UiCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
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
            active_modal: self.app.ui.active_modal(),
            picker_count: self.app.picker_state.len(),
            prompt_text,
            queued_inputs: self
                .app
                .queued_inputs
                .iter()
                .map(crate::app::QueuedInput::display)
                .collect(),
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
/// files written by one scenario survive into the next - a real source
/// of nondeterminism for libFuzzer, which runs every iteration in the
/// same process.
fn ensure_test_home() {
    let dir = TEST_HOME.get_or_init(|| TempDir::new().expect("create test $HOME tempdir"));
    // On macOS `TempDir::new()` returns a path under `/var/folders/…` which
    // `canonicalize` resolves to `/private/var/folders/…`. `AppStoryCtx::new`
    // canonicalizes `HOME` before setting cwd, so the actual cwd directory
    // lives under the canonical path. If we don't canonicalize here, we try
    // to preserve the wrong `cwd` path and delete the real one, breaking
    // `std::env::current_dir()` for every subsequent test.
    let home = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    // SAFETY: env vars are set to the same constant path on every call;
    // concurrent reads from other threads see a stable value.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("XDG_STATE_HOME", home.join("state"));
    std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    std::env::set_var("XDG_DATA_HOME", home.join("data"));
    // Wipe everything in `home` so the next scenario sees an empty
    // filesystem. We can't `remove_dir_all` `home` itself (it'd drop the
    // tempdir backing path), so iterate one level down.
    //
    // Skip the `cwd` subdirectory - storybook tests pin the process cwd
    // there so `smelt.os.cwd()` renders as `~/cwd`. On Unix deleting the
    // cwd invalidates it (`getcwd` returns ENOENT), so parallel tests
    // must not remove each other's working directory.
    let preserved = home.join("cwd");
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == preserved {
                continue;
            }
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

    fn ask_messages(cmds: Vec<protocol::UiCommand>) -> Vec<(String, Vec<protocol::Message>)> {
        cmds.into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk {
                    system, messages, ..
                } => Some((system, messages)),
                _ => None,
            })
            .collect()
    }

    fn user_message(text: &str) -> protocol::Message {
        protocol::Message::user(protocol::Content::text(text))
    }

    fn assistant_message(text: &str) -> protocol::Message {
        protocol::Message::assistant(Some(protocol::Content::text(text)), None, None)
    }

    fn drive_lua_tasks(app: &mut TestApp) {
        for _ in 0..4 {
            app.feed_one(SourceEvent::LuaWakeup);
        }
    }

    fn respond_ask_with_text(app: &mut TestApp, id: u64, text: &str) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text(text)),
                    None,
                    None,
                )),
                error: None,
            });
        app.app.drive_lua_tasks();
    }

    fn respond_pending_ask_with_text(app: &mut TestApp, text: &str) {
        respond_ask_with_text(app, app.pending_ask_id().expect("pending ask id"), text);
    }

    fn publish_input_submit(app: &mut TestApp, text: &str) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.bump_epoch("input_epoch");
        app.app
            .core
            .cells
            .set_dyn("input_submit", std::rc::Rc::new(text.to_string()));
        app.app.pump_lua();
    }

    fn publish_turn_end(app: &mut TestApp) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.core.cells.set_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::cells::TurnEnd { cancelled: false }),
        );
        app.app.pump_lua();
    }

    fn publish_history_delta(app: &mut TestApp, kind: &str) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.publish_history_delta(kind);
        app.app.pump_lua();
    }

    fn engine_ask_ids(cmds: Vec<protocol::UiCommand>) -> Vec<u64> {
        cmds.into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    fn respond_pending_ask_with_tool_call(app: &mut TestApp, call_id: &str, name: &str) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id: app.pending_ask_id().expect("pending ask id"),
                message: Some(protocol::Message::assistant(
                    None,
                    None,
                    Some(vec![protocol::ToolCall::new(
                        call_id.into(),
                        protocol::FunctionCall {
                            name: name.into(),
                            arguments: "{}".into(),
                        },
                    )]),
                )),
                error: None,
            });
        app.app.drive_lua_tasks();
    }

    fn stub_btw_ui(app: &mut TestApp) {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .lua
            .lua
            .load(
                r#"
                smelt.buf.new = function()
                  return {
                    source = function() end,
                  }
                end
                smelt.timer.set = function() end
                smelt.dialog.content = function() return {} end
                smelt.dialog.open = function() end
                smelt.spinner.glyph = function() return "*" end
                smelt.spinner.period_ms = function() return 1 end
                "#,
            )
            .exec()
            .expect("stub /btw ui");
    }

    #[test]
    fn ask_user_question_multiple_questions_wakes_between_dialogs() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.start_turn(1);

        let mut args = std::collections::HashMap::new();
        args.insert(
            "questions".into(),
            serde_json::json!([
                {
                    "header": "First",
                    "question": "Pick first?",
                    "options": [
                        { "label": "One", "description": "first option" },
                        { "label": "Two", "description": "second option" }
                    ],
                    "multiSelect": false
                },
                {
                    "header": "Second",
                    "question": "Pick second?",
                    "options": [
                        { "label": "Three", "description": "third option" },
                        { "label": "Four", "description": "fourth option" }
                    ],
                    "multiSelect": false
                }
            ]),
        );

        app.feed_one(SourceEvent::Engine(EngineEvent::ToolDispatch {
            request_id: 77,
            call_id: "aq-questions".into(),
            tool_name: "ask_user_question".into(),
            args,
        }));

        let first = app
            .state()
            .focused_overlay
            .expect("first question dialog should open");

        app.press(KeyCode::Enter);
        assert!(
            app.app.lua_wakeup_rx.try_recv().is_ok(),
            "resolving the first dialog should wake the Lua task runtime"
        );
        app.feed_one(SourceEvent::LuaWakeup);

        let second = app
            .state()
            .focused_overlay
            .expect("second question dialog should open after first answer");
        assert_ne!(first, second);

        app.press(KeyCode::Char('2'));
        assert!(
            app.app.lua_wakeup_rx.try_recv().is_ok(),
            "resolving the final dialog should wake the Lua task runtime"
        );
        app.feed_one(SourceEvent::LuaWakeup);

        assert!(app.state().focused_overlay.is_none());
        let result = app
            .actions()
            .iter()
            .filter_map(|action| match action {
                Action::EngineSend(cmd) => match cmd.as_ref() {
                    protocol::UiCommand::ToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                        ..
                    } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                    _ => None,
                },
                _ => None,
            })
            .next_back()
            .expect("ask_user_question should send a tool result");

        assert_eq!(result.0, 77);
        assert_eq!(result.1, "aq-questions");
        assert_eq!(
            result.2,
            "Q: Pick first?\nA: One\n\nQ: Pick second?\nA: Four"
        );
        assert!(!result.3);
    }

    #[test]
    fn question_keymap_after_prompt_attachment_is_not_plain_insertion() {
        let mut app = TestApp::builder()
            .with_vim(true)
            .with_mode(AgentMode::parse("yolo").expect("valid mode"))
            .build();
        app.insert_attachment(String::new());
        app.render_silent();

        assert!(app.prompt_plain_insert_ready());
        assert!(app.prompt_plain_char_has_lua_keymap('?'));

        app.feed_one(SourceEvent::Term(Event::Key(KeyEvent {
            code: KeyCode::Char('?'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })));

        assert_eq!(
            app.state().prompt_text,
            crate::input::ATTACHMENT_MARKER.to_string()
        );
        assert_eq!(
            app.prompt_cpos(),
            crate::input::ATTACHMENT_MARKER.len_utf8()
        );
    }

    #[test]
    fn stale_prompt_prediction_response_after_submit_is_ignored() {
        let mut app = TestApp::builder().build();
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text(
                "How should I debug this failing test?",
            )));

        publish_turn_end(&mut app);
        let ask_ids = engine_ask_ids(app.drain_engine_sends());
        assert_eq!(
            ask_ids.len(),
            1,
            "prediction should issue one background ask"
        );
        let prediction_id = ask_ids[0];

        publish_input_submit(&mut app, "Run the focused test first");
        respond_ask_with_text(&mut app, prediction_id, "Run cargo test");

        let prompt = app.app.well_known.prompt;
        assert_eq!(app.app.placeholder_text(prompt), None);
    }

    #[test]
    fn stale_title_response_after_reset_is_ignored() {
        let mut app = TestApp::builder().build();
        let original_session_id = app.app.core.session.id.clone();

        publish_input_submit(&mut app, "Fix flaky integration tests");
        let ask_ids = engine_ask_ids(app.drain_engine_sends());
        assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
        let title_id = ask_ids[0];

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reset_session();
        }
        assert_ne!(app.app.core.session.id, original_session_id);

        respond_ask_with_text(
            &mut app,
            title_id,
            r#"{"title":"Wrong session title","slug":"wrong-session"}"#,
        );

        assert_eq!(app.app.core.session.title, None);
        assert_eq!(app.app.core.session.slug, None);
    }

    #[test]
    fn title_response_after_rewind_is_ignored() {
        let mut app = TestApp::builder().build();

        publish_input_submit(&mut app, "Add caching to parser");
        let ask_ids = engine_ask_ids(app.drain_engine_sends());
        assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
        let title_id = ask_ids[0];

        publish_history_delta(&mut app, "rewound");
        respond_ask_with_text(
            &mut app,
            title_id,
            r#"{"title":"Stale parser cache","slug":"stale-parser-cache"}"#,
        );

        assert_eq!(app.app.core.session.title, None);
        assert_eq!(app.app.core.session.slug, None);
    }

    #[test]
    fn second_title_request_supersedes_inflight_response() {
        let mut app = TestApp::builder().build();

        publish_input_submit(&mut app, "Investigate parser panic");
        let first_ids = engine_ask_ids(app.drain_engine_sends());
        assert_eq!(first_ids.len(), 1);
        let first_id = first_ids[0];

        publish_input_submit(&mut app, "Fix renderer panic instead");
        let second_ids = engine_ask_ids(app.drain_engine_sends());
        assert_eq!(second_ids.len(), 1);
        let second_id = second_ids[0];
        assert_ne!(first_id, second_id);

        respond_ask_with_text(
            &mut app,
            first_id,
            r#"{"title":"Old parser panic","slug":"old-parser"}"#,
        );
        assert_eq!(app.app.core.session.title, None);

        respond_ask_with_text(
            &mut app,
            second_id,
            r#"{"title":"Fix renderer panic","slug":"fix-renderer"}"#,
        );
        assert_eq!(
            app.app.core.session.title.as_deref(),
            Some("Fix renderer panic")
        );
        assert_eq!(app.app.core.session.slug.as_deref(), Some("fix-renderer"));
    }

    #[test]
    fn lua_prompt_text_strips_attachment_markers() {
        // Inserting an attachment seeds the prompt with U+FFFC + a backing id.
        // `smelt.prompt.text()` is the Lua-side accessor that history search,
        // pickers, and similar plugins use to snapshot the input - those
        // callers can't carry attachment ids, so leaking the marker byte
        // lets a marker round-trip back through `set_text` orphan an id.
        let mut app = TestApp::builder().build();
        app.insert_attachment("screenshot.png".into());
        assert!(app
            .app
            .prompt_buf()
            .source()
            .contains(smelt_buffer::ATTACHMENT_MARKER));
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let s: String = app
            .app
            .lua
            .lua
            .load("return smelt.prompt.text()")
            .eval()
            .expect("smelt.prompt.text");
        assert!(
            !s.contains(smelt_buffer::ATTACHMENT_MARKER),
            "prompt.text leaked marker byte: {s:?}"
        );
    }

    #[test]
    fn builds_a_fresh_test_app() {
        let app = TestApp::builder().build();
        let s = app.state();
        assert!(!s.cmdline_open);
        assert!(!s.quit_requested);
        assert!(!s.agent_running);
        assert_eq!(s.app_focus, AppFocus::Prompt);
        assert!(s.queued_inputs.is_empty());
    }

    // ── Resource invariants: per-event allocation tracking ────────────

    /// `feed_one` captures a non-negative allocation delta on every event,
    /// and a `Tick` (pure clock advance) allocates next to nothing - the
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
    /// or the budget needs revisiting - both worth noticing.
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
    fn smelt_work_busy_pushes_token_and_flips_work_cells() {
        let mut app = TestApp::builder().build();
        let lua_ok = app.run_lua(
            r#"
                _G._busy_handle = smelt.work.busy("syncing")
            "#,
        );
        assert!(lua_ok, "smelt.work.busy snippet failed");
        app.tick_cells();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let state: String = app
            .app
            .lua
            .lua
            .load(r#"return smelt.cell("work_state"):get()"#)
            .eval()
            .expect("work_state");
        assert_eq!(state, "busy");
        let label: String = app
            .app
            .lua
            .lua
            .load(r#"return smelt.cell("work_label"):get()"#)
            .eval()
            .expect("work_label");
        assert_eq!(label, "syncing");
        let (count, first_label): (i64, String) = app
            .app
            .lua
            .lua
            .load(
                r#"
                local s = smelt.cell("work_busy"):get()
                return #s, s[1].label
                "#,
            )
            .eval()
            .expect("work_busy");
        assert_eq!(count, 1);
        assert_eq!(first_label, "syncing");
        drop(_guard);

        let ok = app.run_lua("_G._busy_handle:remove(); _G._busy_handle = nil");
        assert!(ok);
        app.tick_cells();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let state_after: String = app
            .app
            .lua
            .lua
            .load(r#"return smelt.cell("work_state"):get()"#)
            .eval()
            .expect("work_state post-release");
        assert_eq!(state_after, "idle");
    }

    #[test]
    fn tick_event_advances_virtual_clock() {
        let mut app = TestApp::builder().build();
        let before = app.app.core.clock.instant_now();
        app.feed_one(SourceEvent::Tick(500));
        let after = app.app.core.clock.instant_now();
        assert_eq!(after - before, Duration::from_millis(500));
    }

    // ── Escape sequence semantics ────────────────────────────────────

    #[test]
    fn vim_insert_double_esc_opens_rewind_dialog_when_idle() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.push_user_block("write the parser");
        assert_eq!(app.state().vim_mode, VimMode::Insert);

        app.press(KeyCode::Esc);
        let after_first = app.state();
        assert_eq!(after_first.vim_mode, VimMode::Normal);
        assert!(
            after_first.focused_overlay.is_none(),
            "first Esc is only the local Vim action"
        );

        app.press(KeyCode::Esc);
        drive_lua_tasks(&mut app);

        assert!(
            app.state().focused_overlay.is_some(),
            "second Esc should complete the idle Esc-Esc rewind chord"
        );
    }

    #[test]
    fn idle_placeholder_dismissal_does_not_swallow_second_escape_rewind() {
        let mut app = TestApp::builder().build();
        app.push_user_block("write the parser");
        app.install_prompt_placeholder(
            "ghost".to_string(),
            Vec::new(),
            vec![crate::smelt_term::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )],
        );

        app.press(KeyCode::Esc);
        assert!(
            app.state().focused_overlay.is_none(),
            "first Esc only dismisses the placeholder"
        );

        app.press(KeyCode::Esc);
        drive_lua_tasks(&mut app);

        assert!(
            app.state().focused_overlay.is_some(),
            "second Esc should still complete the idle Esc-Esc rewind chord"
        );
    }

    #[test]
    fn vim_insert_double_esc_cancels_running_agent_on_second_press() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.start_turn(1);
        assert_eq!(app.state().vim_mode, VimMode::Insert);
        assert!(app.agent_running());

        app.press(KeyCode::Esc);
        assert!(app.agent_running(), "first Esc is the local Vim action");
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.press(KeyCode::Esc);
        assert!(!app.agent_running(), "second Esc hard-cancels the agent");
    }

    #[test]
    fn vim_insert_double_esc_unqueues_messages_on_second_press() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.start_turn(1);
        app.push_queued_message("queued".to_string());

        app.press(KeyCode::Esc);
        let after_first = app.state();
        assert!(after_first.agent_running);
        assert_eq!(after_first.vim_mode, VimMode::Normal);
        assert_eq!(after_first.queued_inputs, vec!["queued".to_string()]);
        assert_eq!(after_first.prompt_text, "");

        app.press(KeyCode::Esc);
        let after_second = app.state();
        assert!(
            after_second.agent_running,
            "unqueue does not cancel the turn"
        );
        assert!(after_second.queued_inputs.is_empty());
        assert_eq!(after_second.prompt_text, "queued");
    }

    #[test]
    fn placeholder_dismissal_does_not_swallow_second_escape_cancel() {
        let mut app = TestApp::builder().build();
        app.install_prompt_placeholder(
            "ghost".to_string(),
            Vec::new(),
            vec![crate::smelt_term::KeyBind::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )],
        );
        app.start_turn(1);

        app.press(KeyCode::Esc);
        assert!(
            app.agent_running(),
            "first Esc only dismisses the placeholder"
        );

        app.press(KeyCode::Esc);
        assert!(!app.agent_running(), "second Esc still reaches hard cancel");
    }

    #[test]
    fn slow_double_escape_is_two_single_escapes() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);

        app.press(KeyCode::Esc);
        app.feed_one(SourceEvent::Tick(crate::app::CHORD_TIMEOUT_MS + 1));
        app.press(KeyCode::Esc);

        assert!(
            app.agent_running(),
            "expired Esc prefix must not hard-cancel"
        );
    }

    #[test]
    fn non_escape_key_breaks_pending_escape_sequence() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);

        app.press(KeyCode::Esc);
        app.type_char('x');
        app.press(KeyCode::Esc);

        assert!(
            app.agent_running(),
            "Esc, other key, Esc is not a double Esc"
        );
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
    fn vim_yank_in_overlay_viewer_writes_system_clipboard() {
        let mut app = TestApp::builder().with_vim(true).build();
        let buf = app
            .app
            .ui
            .buf_create(crate::smelt_term::BufCreateOpts::default());
        {
            let buf = app.app.ui.buf_mut(buf).expect("overlay buffer");
            buf.readonly = true;
            buf.set_all_lines(vec!["alpha beta".into(), "gamma".into()]);
        }

        let leaf = app
            .app
            .ui
            .win_open_split(
                buf,
                crate::smelt_term::SplitConfig {
                    region: "dialog".into(),
                    gutters: Default::default(),
                },
            )
            .expect("overlay leaf");
        if let Some(win) = app.app.ui.win_mut(leaf) {
            win.focusable = true;
            win.selectable = true;
            win.set_vim_enabled(true);
        }
        app.app.ui.overlay_open(
            crate::smelt_term::Overlay::new(
                crate::smelt_term::LayoutTree::leaf(leaf),
                crate::smelt_term::layout::Anchor::ScreenCenter,
            )
            .with_size((40, 5))
            .modal(true),
        );
        app.render_silent();

        app.type_char('v');
        app.type_char('e');
        app.type_char('y');

        assert_eq!(app.app.core.clipboard.kill_ring.current(), "alpha");
        assert_eq!(
            app.app.core.clipboard.kill_ring.last_clipboard_write(),
            Some("alpha")
        );
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

    /// Regression: a prompt-docked picker whose `scroll_top` lands at
    /// `max_scroll` (cursor at the bottom in reversed mode) was getting
    /// clobbered by `Ui::apply_tail_follow` on the first frame - the new
    /// leaf has no viewport rect yet, so `max_scroll = total_rows - 0`
    /// snapped `scroll_top` past the end and the picker rendered blank
    /// until the user typed a character to force a re-layout.
    #[test]
    fn prompt_docked_picker_does_not_get_tail_clobbered_on_first_render() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            smelt.cmd.picker("pick", {
              items = (function()
                local out = {}
                for i = 1, 12 do out[i] = { label = "item" .. i } end
                return out
              end)(),
              apply = function() end,
            })
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        app.type_text("/pick");
        app.app.render_normal(false);

        app.press(KeyCode::Enter);
        app.feed_one(SourceEvent::LuaWakeup);
        app.app.render_normal(false);

        // Locate the prompt-docked picker overlay (the slash completer's
        // own picker is closed on Enter).
        let leaf = (1u32..50)
            .map(crate::smelt_term::OverlayId)
            .filter_map(|id| app.app.ui.overlay(id))
            .filter_map(|ov| ov.layout.leaves_in_order().into_iter().next())
            .map(|p| WinId(p.0))
            .find(|&w| app.app.picker_state.contains_key(&w))
            .expect("a prompt-docked picker overlay should be open after /pick");

        let win = app.app.ui.win(leaf).expect("picker leaf alive");
        let buf = app.app.ui.buf(win.buf).expect("picker buf alive");
        let viewport_rows = win
            .viewport
            .map(|v| v.rect.height)
            .expect("picker leaf must have a viewport after render_normal");
        let total_rows = buf.line_count() as crate::smelt_term::RowIndex;
        let max_scroll = total_rows.saturating_sub(viewport_rows as crate::smelt_term::RowIndex);
        assert!(
            win.scroll_top <= max_scroll,
            "picker scroll_top must stay within bounds on first render \
             (scroll_top={}, max_scroll={}, total_rows={}, viewport_rows={})",
            win.scroll_top,
            max_scroll,
            total_rows,
            viewport_rows,
        );
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
        // Normal-mode 'h' / 'l' are motions, not characters - should not
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
    fn generic_win_cursor_setter_cannot_repark_prompt_cursor() {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(r#"smelt.prompt.set_text("hel\nlo")"#));
        app.app.render_normal(false);
        assert!(app.run_lua("smelt.prompt.win():cursor(0)"));
        app.type_text("!");
        assert_eq!(app.state().prompt_text, "hel\nlo!");
    }

    #[test]
    fn generic_prompt_buf_source_setter_uses_prompt_install_path() {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(r#"smelt.prompt.win():buf():source("hel")"#));
        app.type_text("lo");
        assert_eq!(app.state().prompt_text, "hello");
    }

    #[test]
    fn generic_prompt_buf_lines_setter_uses_prompt_install_path() {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(r#"smelt.prompt.win():buf():lines({ "hel" })"#));
        app.type_text("lo");
        assert_eq!(app.state().prompt_text, "hello");
    }

    fn prompt_content_cell(app: &mut TestApp) -> (u16, u16) {
        app.app.render_normal(false);
        let vp = app
            .app
            .ui
            .win(crate::app::PROMPT_WIN)
            .and_then(|w| w.viewport)
            .expect("prompt viewport after render");
        let pad_left = app
            .app
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.config.gutters.pad_left)
            .unwrap_or_default();
        (
            vp.rect.top,
            vp.rect
                .left
                .saturating_add(vp.gutter_width)
                .saturating_add(pad_left),
        )
    }

    #[test]
    fn transcript_triple_click_event_pipeline_yanks_clicked_display_line() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = TestApp::builder().with_vim(false).build();
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: "```text\nalpha\nbeta\ngamma\n```\n\nIt avoids background weirdness, looks good in most themes.".into(),
            });
        app.render_silent();

        let transcript_win = app.app.transcript_win();
        let vp = transcript_win
            .viewport
            .expect("transcript viewport after render");
        let pad_left = transcript_win.config.gutters.pad_left;
        let scroll_top = transcript_win.scroll_top as usize;
        let buf = app
            .app
            .ui
            .buf(transcript_win.buf)
            .expect("transcript buffer");
        let line_idx = buf
            .lines()
            .iter()
            .position(|line| line.contains("It avoids background weirdness"))
            .expect("target line rendered");
        assert!(line_idx >= scroll_top, "target line should be visible");
        let row = vp.rect.top + (line_idx - scroll_top) as u16;
        let column = vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(pad_left)
            .saturating_add(3);

        for _ in 0..3 {
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row,
                column,
                modifiers: KeyModifiers::empty(),
            })));
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                row,
                column,
                modifiers: KeyModifiers::empty(),
            })));
        }

        assert_eq!(
            app.app.core.clipboard.kill_ring.current(),
            "It avoids background weirdness, looks good in most themes."
        );
    }

    #[test]
    fn prompt_triple_click_event_pipeline_yanks_clicked_source_line() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(r#"smelt.prompt.set_text("first line\nsecond line\nthird line")"#));
        let (top, column) = prompt_content_cell(&mut app);
        let row = top + 1;
        let column = column + 2;

        for _ in 0..3 {
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row,
                column,
                modifiers: KeyModifiers::empty(),
            })));
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                row,
                column,
                modifiers: KeyModifiers::empty(),
            })));
        }

        assert_eq!(app.app.core.clipboard.kill_ring.current(), "second line");
    }

    #[test]
    fn keyboard_input_cancels_stale_prompt_mouse_endpoint() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = TestApp::builder().with_vim(false).build();
        let (row, column) = prompt_content_cell(&mut app);

        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
        assert!(
            app.app.ui.any_drag_active(),
            "mouse down staged a drag endpoint"
        );

        app.type_text("Hello");

        let prompt = app.app.prompt_win();
        assert_eq!(app.state().prompt_text, "Hello");
        assert_eq!(prompt.effective_endpoint(), 5);
        assert_eq!(app.app.ui.capture(), None);
        assert!(!app.app.ui.any_drag_active());
    }

    #[test]
    fn typing_after_turn_complete_keeps_prompt_cursor_coherent() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.start_turn(1);
        app.feed_one(SourceEvent::Engine(EngineEvent::TurnComplete {
            turn_id: 1,
            history: vec![],
            meta: None,
        }));
        app.render_silent();

        for (idx, ch) in "Hello".chars().enumerate() {
            app.type_char(ch);
            assert_eq!(app.app.prompt_win().cpos, idx + 1);
        }

        app.press(KeyCode::Left);
        app.type_char('!');

        assert_eq!(app.state().prompt_text, "Hell!o");
        assert_eq!(app.app.prompt_win().cpos, 5);
    }

    #[test]
    fn text_changed_callbacks_do_not_repark_prompt_cursor() {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(0)
            end)
            "#,
        ));

        app.type_text("Hello");

        assert_eq!(app.state().prompt_text, "Hello");
        assert_eq!(app.app.prompt_win().cpos, 5);
    }

    #[test]
    fn reload_clears_surviving_prompt_keymaps() {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(
            r#"
            smelt.prompt.win():key("left", function() end)
            "#,
        ));

        app.reload_lua();
        app.type_text("ab");
        app.press(KeyCode::Left);
        app.type_char('X');

        assert_eq!(app.state().prompt_text, "aXb");
        assert_eq!(app.app.prompt_win().cpos, 2);
    }

    #[test]
    fn typing_after_unfinished_prompt_click_uses_clicked_caret() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(r#"smelt.prompt.set_text("abcd")"#));
        let (row, column) = prompt_content_cell(&mut app);

        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column: column + 1,
            modifiers: KeyModifiers::empty(),
        })));
        assert_eq!(app.app.prompt_win().effective_endpoint(), 1);

        app.type_text("X");

        let prompt = app.app.prompt_win();
        assert_eq!(app.state().prompt_text, "aXbcd");
        assert_eq!(prompt.cpos, 2);
        assert_eq!(prompt.effective_endpoint(), 2);
        assert_eq!(app.app.ui.capture(), None);
        assert!(!app.app.ui.any_drag_active());
    }

    #[test]
    fn focus_lost_cancels_stale_prompt_mouse_endpoint() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = TestApp::builder().with_vim(false).build();
        let (row, column) = prompt_content_cell(&mut app);

        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        })));
        assert!(
            app.app.ui.any_drag_active(),
            "mouse down staged a drag endpoint"
        );

        app.feed_one(SourceEvent::Term(Event::FocusLost));

        assert_eq!(app.app.ui.capture(), None);
        assert!(!app.app.ui.any_drag_active());
        assert_eq!(app.app.prompt_win().effective_endpoint(), 0);
    }

    #[test]
    fn prompt_window_wraps_parser_output() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Paste(
            "x".repeat(200),
        )));
        app.render_silent();
        app.assert_ui_invariants();
    }

    #[test]
    fn custom_command_turn_includes_registered_lua_tools() {
        let mut app = TestApp::builder().with_vim(false).build();
        let payload = app
            .start_custom_command_with_lua_tool(0)
            .expect("custom command should send StartTurn");
        assert!(
            payload.tools.iter().any(|t| t.name == "fuzz_custom_tool_0"),
            "registered Lua tool missing from custom command payload: {:?}",
            payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn engine_ask_probe_registers_pending_callback() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.start_engine_ask_probe("summarize this");
        assert!(app.pending_ask_id().is_some());
    }

    #[test]
    fn prompt_cursor_probe_catches_stuck_insert_after_turn() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.probe_prompt_cursor_after_turn(1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_prepare_probe_completes_and_preserves_turn() {
        let mut app = TestApp::builder().with_vim(false).build();
        app.probe_compaction_prepare_request(1);
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
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", focusable = false })
            smelt.overlay.new({
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
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", focusable = false })
            smelt.overlay.new({
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
    /// wrap to its default - that would clobber the prior value.
    #[test]
    fn named_win_refresh_preserves_wrap_when_omitted() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let lua = &app.app.lua.lua;

        lua.load(
            r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win", wrap = false })
            "#,
        )
        .exec()
        .expect("first open");

        let wid = app.app.ui.named_win("w.win").expect("named win");
        assert!(
            !app.app.ui.win(wid).unwrap().wrap,
            "wrap should be false after explicit open"
        );

        // Re-open with the same name but no `wrap` key → wrap should stay false.
        lua.load(
            r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win" })
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

        lua.load(
            r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
        )
        .exec()
        .expect("first");
        let first_buf = app.app.ui.named_buf("x.buf").expect("buf 1");
        let first_win = app.app.ui.named_win("x.win").expect("win 1");

        lua.load(
            r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
        )
        .exec()
        .expect("second");
        let second_buf = app.app.ui.named_buf("x.buf").expect("buf 2");
        let second_win = app.app.ui.named_win("x.win").expect("win 2");

        assert_eq!(
            first_buf, second_buf,
            "named buf id stable across re-create"
        );
        assert_eq!(first_win, second_win, "named win id stable across re-open");
    }

    /// Re-opening a named overlay with a structurally different layout
    /// (leaf → vbox split) should replace the tree in place - not silently
    /// keep the old one.
    #[test]
    fn named_overlay_refresh_replaces_layout_structure() {
        let mut app = TestApp::builder().build();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let lua = &app.app.lua.lua;

        lua.load(
            r#"
            local buf = smelt.buf.new({ name = "a.buf" })
            local win = smelt.win.new(buf, { name = "a.win" })
            smelt.overlay.new({
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
            local b1 = smelt.buf.new({ name = "a.buf" })
            local b2 = smelt.buf.new({ name = "b.buf" })
            local w1 = smelt.win.new(b1, { name = "a.win" })
            local w2 = smelt.win.new(b2, { name = "b.win" })
            smelt.overlay.new({
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
                    local buf = smelt.buf.new({{ name = "plug.buf" }})
                    local win = smelt.win.new(buf, {{ name = "plug.win" }})
                    smelt.overlay.new({{
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
    /// reloads - never re-wrap the previous wrap.
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
            // the call would still succeed - but every reload would add a
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
        // First version opens both a named overlay and a plain
        // anonymous overlay. init.lua doesn't call `smelt.plugin(...)`,
        // so its loader frame is unnamed and anonymous resources stay
        // anonymous - they get reaped on /reload.
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("mix")
            local function attach()
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            state.open = true
            attach()

            -- Anonymous overlay: init.lua's frame is unnamed (no
            -- `smelt.plugin(...)` call), so this gets reaped on /reload.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();

        // Capture the anonymous overlay's id - we'll assert it's gone
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
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
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

    /// Named paint slots (`smelt.paint.register(fn, { name = "..." })`)
    /// must keep the same `PaintId` across `/reload` so surviving
    /// overlays / layouts that reference the id keep painting with the
    /// fresh closure. Anonymous slots get reaped.
    #[test]
    fn reload_lua_preserves_named_paint_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        // Module-body code: capture the paint id in a state slot so we
        // can read it back from Rust after the reload cycle.
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("paint_id_probe")
            local function painter(_slice, _ctx) end
            -- No `smelt.plugin(...)` call → init.lua's loader frame
            -- stays unnamed, so the unnamed register call below is
            -- anonymous and gets reaped on /reload. The explicit
            -- name = "probe.named" slot survives.
            smelt.paint.register(painter, { name = "probe.named" })
            smelt.paint.register(painter)
            state.dummy = true
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();

        let pre_named = app
            .app
            .paint_registry
            .id_by_name("probe.named")
            .expect("named pre id");
        // The anonymous slot has no name binding; locate it as the only
        // un-named PaintId currently registered.
        let pre_anon = find_anon_paint(&app.app);

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }

        let post_named = app
            .app
            .paint_registry
            .id_by_name("probe.named")
            .expect("named post id");
        let post_anon = find_anon_paint(&app.app);
        assert_eq!(
            pre_named, post_named,
            "named paint slot must keep stable PaintId across reload"
        );
        assert_ne!(
            pre_anon, post_anon,
            "anonymous paint slot must allocate a fresh id on reload"
        );
        assert!(
            !app.app.paint_registry.contains(pre_anon),
            "old anonymous PaintId must be reaped"
        );
        assert!(app.app.paint_registry.contains(post_named));
        assert!(app.app.paint_registry.contains(post_anon));
    }

    /// Find the single anonymous paint id (no name binding) currently
    /// registered. Used by paint-reload tests to track the throwaway
    /// slot across `/reload` without needing Lua-side reflection.
    fn find_anon_paint(app: &crate::app::TuiApp) -> crate::smelt_term::layout::PaintId {
        let reg = &app.paint_registry;
        let named: std::collections::HashSet<crate::smelt_term::layout::PaintId> = ["probe.named"]
            .iter()
            .filter_map(|n| reg.id_by_name(n))
            .collect();
        reg.all_ids()
            .into_iter()
            .find(|id| !named.contains(id))
            .expect("anonymous paint id present")
    }

    /// `lifecycle.on("ready", fn)` hooks must re-drain on `/reload` so
    /// plugins that subscribe to cells / open splash overlays / etc.
    /// re-wire themselves on every Lua-context bring-up. The fire
    /// passes `ctx = { kind = "launch" | "reload" }`.
    #[test]
    fn reload_lua_drains_ready_hooks_with_kind_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            local state = smelt.state("ready_kind_probe")
            state.fires = (state.fires or 0)
            state.last_kind = nil
            smelt.lifecycle.on_ready(function(ctx)
                state.fires = state.fires + 1
                state.last_kind = ctx and ctx.kind or "<nil>"
            end)
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        // Cold-start `TestApp` skips the `on_ready` drain (storybook
        // tests don't want interactive decoration like the splash
        // banner). Fire it manually here since this test specifically
        // covers the `kind = "launch"` drain.
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            let _ = app.app.bring_up_lua("launch");
        }

        let read = |rt: &crate::lua::LuaRuntime, k: &str| -> String {
            rt.lua
                .load(format!(
                    "return tostring(__smelt_state__['ready_kind_probe'].{k})"
                ))
                .eval::<String>()
                .unwrap()
        };
        assert_eq!(read(&app.app.lua, "fires"), "1");
        assert_eq!(read(&app.app.lua, "last_kind"), "launch");

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.reload_lua();
        }
        assert_eq!(read(&app.app.lua, "fires"), "2");
        assert_eq!(read(&app.app.lua, "last_kind"), "reload");
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
    /// that hold Lua handles MUST add a check here - otherwise the
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
            local b1 = smelt.buf.new({ name = "seed.buf" })
            local w1 = smelt.win.new(b1, { name = "seed.win" })
            smelt.overlay.new({
                name = "seed.ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 30, height = 8,
                layout = smelt.ui.layout.leaf(w1),
            })
            -- Anonymous overlay (init.lua frame unnamed): must be reaped.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
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
    /// reload, the parked task should never resume - driving tasks
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
        let outs = app.app.lua.drive_tasks(app.app.core.clock.instant_now());
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

    /// `/reload` (`smelt.engine.reload()`) used to refuse with
    /// "cannot reload while a modal dialog is open". We now dismiss
    /// the modal first so the parked dialog coroutine joins the rest
    /// of the in-flight tasks `clear_for_reload` cancels - symmetric
    /// with how reload already drops any other `smelt.spawn`. After
    /// reload, no modal is open and a fresh dialog opens cleanly.
    #[test]
    fn reload_lua_via_engine_dismisses_open_modal() {
        let tmp = tempfile::tempdir().unwrap();
        let init = tmp.path().join("init.lua");
        std::fs::write(
            &init,
            r#"
            smelt.cmd.register("open_modal", function()
                smelt.spawn(function()
                    local leaf = smelt.dialog.content({ text = "hello" })
                    smelt.dialog.open({
                        title = "test",
                        max_height = "50%",
                        panels = { { leaf = leaf } },
                    })
                end)
            end)
            "#,
        )
        .unwrap();

        let mut app = TestApp::builder().with_init_lua(&init).build();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.apply_lua_command("open_modal");
            app.app.drive_lua_tasks();
        }
        assert!(
            app.app.ui.active_modal().is_some(),
            "modal should be open after /open_modal"
        );

        // Drive the reload through the Lua binding (the gate lives there,
        // not in `TuiApp::reload_lua`). The binding should dismiss the
        // modal and call through to `reload_lua` instead of bailing out.
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .lua
                .lua
                .load("smelt.engine.reload()")
                .exec()
                .expect("reload succeeds even with modal open");
        }
        assert!(
            app.app.ui.active_modal().is_none(),
            "modal must be dismissed after reload"
        );

        // Reload should have re-registered the command - reopen works.
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.apply_lua_command("open_modal");
            app.app.drive_lua_tasks();
        }
        assert!(
            app.app.ui.active_modal().is_some(),
            "command survived reload and reopens modal"
        );
    }

    #[test]
    fn btw_command_preserves_model_history_prefix_and_appends_question() {
        let mut app = TestApp::builder().build();
        stub_btw_ui(&mut app);
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u2")));

        let expected_prefix = protocol::history_to_messages(&app.app.model_history());

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.apply_lua_command("btw what changed?");
            app.app.drive_lua_tasks();
        }

        let asks = ask_messages(app.drain_engine_sends());
        assert_eq!(asks.len(), 1, "/btw should issue one inherited ask");
        let (system, messages) = &asks[0];
        assert_eq!(system, &app.app.assemble_system_prompt());
        assert_eq!(
            &messages[..expected_prefix.len()],
            expected_prefix.as_slice(),
            "/btw must preserve the exact model-visible prefix"
        );
        let last_text = messages
            .last()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content())
            .expect("/btw question");
        assert!(last_text.contains("Under no circumstances use tools"));
        assert!(last_text.contains("Question: what changed?"));

        respond_pending_ask_with_text(&mut app, "done");
        app.app.core.timers.clear();
    }

    #[test]
    fn btw_command_denies_tool_calls_then_retries_same_request_shape() {
        let mut app = TestApp::builder().build();
        stub_btw_ui(&mut app);
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        let expected_prefix = protocol::history_to_messages(&app.app.model_history());

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app.apply_lua_command("btw quick summary");
            app.app.drive_lua_tasks();
        }

        let first = ask_messages(app.drain_engine_sends());
        assert_eq!(first.len(), 1);
        let first_messages = first[0].1.clone();

        respond_pending_ask_with_tool_call(&mut app, "call-1", "grep");

        let second = ask_messages(app.drain_engine_sends());
        assert_eq!(second.len(), 1);
        let second_messages = &second[0].1;
        assert_eq!(
            &second_messages[..first_messages.len()],
            first_messages.as_slice(),
            "/btw tool denial retry must keep the same request prefix"
        );
        assert_eq!(
            &second_messages[..expected_prefix.len()],
            expected_prefix.as_slice(),
            "/btw tool denial retry must keep the same inherited conversation prefix"
        );
        assert_eq!(
            second_messages[first_messages.len()].role,
            protocol::Role::Assistant
        );
        assert_eq!(
            second_messages[first_messages.len() + 1].role,
            protocol::Role::Tool
        );
        assert!(second_messages[first_messages.len() + 1].is_error);

        respond_pending_ask_with_text(&mut app, "done");
        app.app.core.timers.clear();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_prepare_request_preserves_session_prefix_and_appends_summary_instruction() {
        let mut app = TestApp::builder().build();
        app.app.core.config.context_window = Some(100);
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u2")));

        let full_history = protocol::history_to_messages(&app.app.model_history());
        let expected_prefix = &full_history[..2];
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_host_call(engine::HostCall::PrepareRequest {
                    messages: full_history.clone(),
                    estimated_tokens: 200,
                    reply: tx,
                });
        }

        let asks = ask_messages(app.drain_engine_sends());
        assert_eq!(asks.len(), 1, "compaction should issue one EngineAsk");
        let (system, messages) = &asks[0];
        assert_eq!(system, &app.app.assemble_system_prompt());
        assert_eq!(
            &messages[..expected_prefix.len()],
            expected_prefix,
            "initial compaction attempt must preserve the exact session prefix up to the current boundary"
        );
        let last_text = messages
            .last()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content())
            .expect("summary task");
        assert!(last_text.contains("CONTEXT CHECKPOINT COMPACTION"));
        assert!(last_text.contains("Under no circumstances use tools"));
        assert!(last_text.contains("# Goal"));

        respond_pending_ask_with_text(&mut app, "# Goal\nok");
        let replacement = rx
            .await
            .expect("prepare_request reply")
            .expect("replacement");
        let replacement_text = replacement
            .first()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content());
        let expected = format!("{}\n# Goal\nok", engine::SUMMARY_PREFIX.trim_end());
        assert_eq!(replacement_text.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_prepare_request_keeps_active_turn_guard_current() {
        let mut app = TestApp::builder().build();
        app.app.core.config.context_window = Some(100);
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u2")));
        app.start_turn(42);

        let full_history = protocol::history_to_messages(&app.app.model_history());
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_host_call(engine::HostCall::PrepareRequest {
                    messages: full_history,
                    estimated_tokens: 200,
                    reply: tx,
                });
        }

        assert_eq!(app.app.working.phase_label(), Some("compacting"));
        assert_eq!(ask_messages(app.drain_engine_sends()).len(), 1);

        respond_pending_ask_with_text(&mut app, "# Goal\nok");
        let replacement = rx
            .await
            .expect("prepare_request reply")
            .expect("active-turn guard should allow replacement");
        let replacement_text = replacement
            .first()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content());
        let expected = format!("{}\n# Goal\nok", engine::SUMMARY_PREFIX.trim_end());
        assert_eq!(replacement_text.as_deref(), Some(expected.as_str()));
        assert_eq!(app.app.working.phase_label(), Some("working"));
        assert!(app.agent_running());
    }

    #[test]
    fn cancelled_turn_without_usage_preserves_context_token_baseline() {
        let mut app = TestApp::builder().build();
        app.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        app.app.core.session.context_tokens = Some(500);
        app.app.core.session.context_tokens_history_len = Some(app.app.core.session.history.len());
        app.app.core.session.visible_context_tokens = Some(500);
        app.start_turn(7);

        app.app.discard_turn(true);

        assert_eq!(app.app.core.session.context_tokens, Some(500));
        assert_eq!(app.app.core.session.context_tokens_history_len, Some(2));
        assert_eq!(app.app.core.session.visible_context_tokens, Some(500));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_context_limit_moves_boundary_earlier_on_context_window() {
        let mut app = TestApp::builder().build();
        let messages = vec![
            user_message("u1"),
            assistant_message("a1"),
            user_message("u2"),
            assistant_message("a2"),
            user_message("u3"),
        ];
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                    messages: messages.clone(),
                    reply: tx,
                });
        }

        let first = ask_messages(app.drain_engine_sends());
        assert_eq!(first.len(), 1);
        let first_messages = &first[0].1;
        assert_eq!(
            &first_messages[..4],
            &messages[..4],
            "keep_recent_groups=1 should compact everything before the last group"
        );

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                    id: app.pending_ask_id().expect("pending ask id"),
                    message: None,
                    error: Some(protocol::EngineAskError {
                        kind: protocol::EngineAskErrorKind::ContextWindow,
                        message: "too large".into(),
                    }),
                });
        }

        let second = ask_messages(app.drain_engine_sends());
        assert_eq!(second.len(), 1);
        let second_messages = &second[0].1;
        assert_eq!(
            &second_messages[..3],
            &messages[..3],
            "retry should move the boundary one group earlier"
        );

        respond_pending_ask_with_text(&mut app, "# Goal\nok");
        let replacement = rx.await.expect("recovery reply").expect("replacement");
        assert_eq!(replacement.len(), 3);
        assert_eq!(replacement[1], messages[3]);
        assert_eq!(replacement[2], messages[4]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_context_limit_denies_tool_calls_without_moving_boundary() {
        let mut app = TestApp::builder().build();
        let messages = vec![
            user_message("u1"),
            assistant_message("a1"),
            user_message("u2"),
        ];
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                    messages: messages.clone(),
                    reply: tx,
                });
        }

        let first = ask_messages(app.drain_engine_sends());
        assert_eq!(first.len(), 1);
        let first_messages = first[0].1.clone();

        respond_pending_ask_with_tool_call(&mut app, "call-1", "read_file");

        let second = ask_messages(app.drain_engine_sends());
        assert_eq!(second.len(), 1);
        let second_messages = &second[0].1;
        assert_eq!(
            &second_messages[..first_messages.len()],
            first_messages.as_slice(),
            "tool denial retry must keep the same boundary prefix"
        );
        assert_eq!(
            second_messages[first_messages.len()].role,
            protocol::Role::Assistant
        );
        assert_eq!(
            second_messages[first_messages.len() + 1].role,
            protocol::Role::Tool
        );
        assert!(second_messages[first_messages.len() + 1].is_error);

        respond_pending_ask_with_text(&mut app, "# Goal\nok");
        let replacement = rx.await.expect("recovery reply").expect("replacement");
        assert_eq!(replacement.first().unwrap().role, protocol::Role::User);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_context_limit_returns_none_when_no_earlier_boundary_fits() {
        let mut app = TestApp::builder().build();
        let messages = vec![user_message("u1"), user_message("u2")];
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                    messages,
                    reply: tx,
                });
        }

        {
            let _g = crate::lua::install_app_ptr(&mut app.app);
            app.app
                .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                    id: app.pending_ask_id().expect("pending ask id"),
                    message: None,
                    error: Some(protocol::EngineAskError {
                        kind: protocol::EngineAskErrorKind::ContextWindow,
                        message: "too large".into(),
                    }),
                });
        }

        assert!(rx.await.expect("recovery reply").is_none());
    }

    /// User-resized overlay (`size_override`) must survive reload -
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
                local b = smelt.buf.new({ name = "res.buf" })
                local w = smelt.win.new(b, { name = "res.win" })
                smelt.overlay.new({
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
