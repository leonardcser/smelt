//! End-to-end test harness for `TuiApp`.
//!
//! Input is a `SourceEvent` stream (Term / Engine / Tick); output is a
//! structured `Action` log plus snapshots of inspectable state.
//!
//! Side effects are contained by pointing every `$HOME`/XDG path at a
//! process-wide tempdir.

#![allow(dead_code)]

use crate::app::{AppFocus, TuiApp};
use crate::smelt_edit::{OverlayId, VimMode, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use engine::clock::VirtualClock;
use engine::EngineHandle;
use protocol::{AgentMode, EngineEvent, ReasoningEffort, UiCommand};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tokio::sync::mpsc;

pub use crate::event_source::SourceEvent;

mod driver;
mod invariants;
mod lua;
mod probes;
mod state;
mod synthetic;

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
    mode_cycle: Option<Vec<AgentMode>>,
    init_lua: Option<std::path::PathBuf>,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            vim: false,
            mode: AgentMode::normal(),
            mode_cycle: None,
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

    pub fn with_mode_cycle(mut self, modes: Vec<AgentMode>) -> Self {
        self.mode_cycle = Some(modes);
        self
    }

    /// Run user `init.lua` from this path during build, and from the same
    /// path again on every `reload_lua()`.
    pub fn with_init_lua(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.init_lua = Some(path.into());
        self
    }

    pub fn build(self) -> TestApp {
        let _home_guard = test_home_guard();
        reset_test_home();
        self.build_after_test_home_setup()
    }

    pub(crate) fn build_with_test_home_guard(self, _guard: &MutexGuard<'static, ()>) -> TestApp {
        reset_test_home();
        self.build_after_test_home_setup()
    }

    pub(crate) fn build_without_test_home_reset(self, _guard: &MutexGuard<'static, ()>) -> TestApp {
        self.build_after_test_home_setup()
    }

    fn build_after_test_home_setup(self) -> TestApp {
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

        let mode_cycle = self.mode_cycle.unwrap_or_else(|| {
            vec![
                AgentMode::normal(),
                AgentMode::parse("plan").unwrap(),
                AgentMode::parse("apply").unwrap(),
                AgentMode::parse("yolo").unwrap(),
            ]
        });

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
            cli_mode_cycle_override: false,
            mode: self.mode,
            mode_cycle,
            reasoning_effort: ReasoningEffort::Off,
            reasoning_cycle: Vec::new(),
            settings,
            remember: smelt_core::config::RememberConfig::default(),
            context_window: None,
        };

        let clock = Arc::new(VirtualClock::new(Instant::now(), SystemTime::now()));
        let home = engine::paths::home_dir();
        let env = Arc::new(engine::env::RuntimeEnv::scripted(
            4242,
            home.clone(),
            home.join("config"),
            home.join("state"),
            home.join("cache"),
            home.join("data"),
            home.join("cwd"),
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
            None,
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
            app.pending_history_appends.clear();
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
    line.get(1..).unwrap_or(&line).to_string()
}

// ── Process-wide tempdir for $HOME and XDG vars ─────────────────────

static TEST_HOME: OnceLock<TempDir> = OnceLock::new();
static TEST_HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn test_home_guard() -> MutexGuard<'static, ()> {
    TEST_HOME_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Initialize `$HOME` + XDG env vars on first call, then wipe the
/// directory's contents on every call so each `TestApp::build` starts
/// against an empty filesystem. Without this, session / history / state
/// files written by one scenario survive into the next - a real source
/// of nondeterminism for libFuzzer, which runs every iteration in the
/// same process.
fn reset_test_home() {
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
    // Keep a stable `cwd` directory under HOME. The scripted RuntimeEnv
    // uses it as the app cwd so story snapshots render `~/cwd`.
    let preserved = home.join("cwd");
    let _ = std::fs::create_dir_all(&preserved);
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
