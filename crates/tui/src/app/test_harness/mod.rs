//! End-to-end test harness for `TuiApp`.
//!
//! Input is a `SourceEvent` stream (Term / Engine / Tick); output is a
//! structured `Action` log plus snapshots of inspectable state.
//!
//! Side effects are contained by pointing every `$HOME`/XDG path at a
//! managed per-process directory under `/tmp/smelt-fuzz-harness`.

#![allow(dead_code)]

use crate::app::{AppFocus, TuiApp};
use crate::smelt_edit::{ModalId, OverlayId, VimMode, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use engine::clock::VirtualClock;
use engine::EngineHandle;
use protocol::{AgentMode, EngineEvent, ReasoningEffort, UiCommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

pub use crate::event_source::SourceEvent;

mod driver;
mod invariants;
mod lua;
mod probes;
mod state;
mod synthetic;
mod transcript_scroll;

pub use transcript_scroll::{TranscriptScrollProbeCommand, TranscriptScrollProbeEdge};

/// One observed out-bound effect of a `SourceEvent`.
#[derive(Debug, Clone)]
pub enum Action {
    /// A `UiCommand` was sent on the engine channel.
    EngineSend(Box<UiCommand>),
    /// The event dispatch asked the app to quit.
    Quit,
}

/// Render phase that produced a scripted-loop terminal snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderLoopFrameKind {
    Normal,
    Transient,
}

/// One compositor frame captured while driving a scripted event source.
#[derive(Debug, Clone)]
pub struct RenderLoopFrame {
    pub kind: RenderLoopFrameKind,
    pub snapshot: crate::smelt_edit::SnapshotFrame,
}

/// Immutable snapshot of state observable by tests.
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub app_focus: AppFocus,
    pub vim_mode: VimMode,
    pub cmdline_open: bool,
    pub cmdline_text: String,
    pub focused_overlay: Option<OverlayId>,
    pub active_modal: Option<ModalId>,
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
    transcript_scroll_probe: transcript_scroll::TranscriptScrollProbeState,
}

pub struct TestAppBuilder {
    vim: bool,
    mode: AgentMode,
    mode_cycle: Option<Vec<AgentMode>>,
    init_lua: Option<std::path::PathBuf>,
    lua_config_dir: Option<std::path::PathBuf>,
    lua_runtime_override: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    ephemeral: bool,
    model_available: bool,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            vim: false,
            mode: AgentMode::normal(),
            mode_cycle: None,
            init_lua: None,
            lua_config_dir: None,
            lua_runtime_override: None,
            cwd: None,
            ephemeral: false,
            model_available: true,
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

    pub(crate) fn with_lua_load_paths(
        mut self,
        config_dir: impl Into<std::path::PathBuf>,
        runtime_override: Option<std::path::PathBuf>,
    ) -> Self {
        self.lua_config_dir = Some(config_dir.into());
        self.lua_runtime_override = runtime_override;
        self
    }

    pub(crate) fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    pub fn without_model(mut self) -> Self {
        self.model_available = false;
        self
    }

    pub fn build(self) -> TestApp {
        let guard = test_environment_guard();
        self.build_with_test_environment_guard(&guard)
    }

    pub(crate) fn build_with_test_home_guard(self, _guard: &MutexGuard<'static, ()>) -> TestApp {
        let _cwd_guard = crate::test_support::ProcessCwdGuard::capture();
        reset_test_home();
        self.build_after_test_home_setup()
    }

    pub(crate) fn build_with_test_environment_guard(
        self,
        _guard: &TestEnvironmentGuard,
    ) -> TestApp {
        reset_test_home();
        self.build_after_test_home_setup()
    }

    pub(crate) fn build_without_test_home_reset(self, _guard: &MutexGuard<'static, ()>) -> TestApp {
        let _cwd_guard = crate::test_support::ProcessCwdGuard::capture();
        self.build_after_test_home_setup()
    }

    fn build_after_test_home_setup(self) -> TestApp {
        let (engine, cmd_rx, event_tx) = EngineHandle::for_test();

        let permissions = smelt_core::permissions::PermissionsHandle::new(
            smelt_core::permissions::Permissions::load(),
        );
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
        let providers = self
            .model_available
            .then(|| smelt_core::config::ProviderConfig {
                name: Some("test".into()),
                provider_type: Some("openai-compatible".into()),
                api_base: Some("https://example.invalid/v1".into()),
                api_key_env: Some(String::new()),
                models: vec![protocol::ModelConfig {
                    name: Some("test-model".into()),
                    ..Default::default()
                }],
            })
            .into_iter()
            .collect();
        let desired = smelt_core::config::Config {
            providers,
            settings,
            ..Default::default()
        };
        let available_models = desired.resolve_models();
        let startup_overrides = smelt_core::StartupOverrides::default();
        let selections = smelt_core::RuntimeSelections {
            mode: Some(self.mode),
            reasoning_effort: Some(ReasoningEffort::Off),
            ..Default::default()
        };
        let config = smelt_core::resolve_runtime(smelt_core::RuntimeInputs {
            config: &desired,
            startup: &startup_overrides,
            available_models: &available_models,
            registered_modes: &mode_cycle,
            selections: &selections,
            previous: None,
            headless: false,
        })
        .unwrap();

        let clock = Arc::new(VirtualClock::new(Instant::now(), SystemTime::now()));
        let home = engine::paths::home_dir();
        let cwd = self.cwd.unwrap_or_else(|| home.join("cwd"));
        let lua_config_dir = self
            .lua_config_dir
            .unwrap_or_else(smelt_core::config::config_dir);
        lua.set_load_paths_for_harness(
            lua_config_dir,
            self.lua_runtime_override,
            Some(cwd.clone()),
        );
        let env = Arc::new(engine::env::RuntimeEnv::scripted(
            4242,
            home.clone(),
            home.join("config"),
            home.join("state"),
            home.join("cache"),
            home.join("data"),
            cwd,
            std::num::NonZeroUsize::new(1).unwrap(),
        ));

        let session_persistence = if self.ephemeral {
            crate::app::SessionPersistence::ephemeral().expect("ephemeral session directory")
        } else {
            crate::app::SessionPersistence::persistent()
        };
        let mut app = TuiApp::new(
            config,
            startup_overrides,
            engine,
            permissions,
            shared_session,
            lua,
            smelt_core::trust::TrustState::NoContent,
            Arc::clone(&clock) as Arc<dyn engine::clock::Clock>,
            env,
            crate::app::TuiAppOptions {
                session_persistence,
                ..Default::default()
            },
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
            let _ = app.lua.load_initial_for_harness(None);
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
                smelt.time.now_ms = function() return 0 end
                smelt.spinner.glyph = function() return '✿' end
                "#,
            )
            .exec();

        // Production wires the Tui frontend to `Osc52Sink`, which writes
        // `\x1b]52;c;...` to real stdout on every kill-ring copy. Inside the
        // harness that's a ring leak - corrupts test stdout, slows the fuzz
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
            transcript_scroll_probe: transcript_scroll::TranscriptScrollProbeState::default(),
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

// ── Process-wide managed directories for $HOME and XDG vars ─────────────

static TEST_HOME: OnceLock<PathBuf> = OnceLock::new();
static TEST_HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) struct TestEnvironmentGuard {
    _cwd: crate::test_support::ProcessCwdGuard,
    _home: MutexGuard<'static, ()>,
}

pub(crate) fn test_environment_guard() -> TestEnvironmentGuard {
    let home = test_home_guard();
    let cwd = crate::test_support::ProcessCwdGuard::capture();
    TestEnvironmentGuard {
        _cwd: cwd,
        _home: home,
    }
}

pub(crate) fn test_home_guard() -> MutexGuard<'static, ()> {
    TEST_HOME_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn initialized_test_home_guard() -> MutexGuard<'static, ()> {
    let guard = test_home_guard();
    reset_test_home();
    guard
}

pub(super) fn managed_harness_dir(kind: &str) -> PathBuf {
    let dir = harness_root(kind).join(process_dir_name());
    cleanup_stale_harness_dirs(kind, &dir);
    std::fs::create_dir_all(&dir).expect("create managed harness dir");
    dir
}

/// Initialize `$HOME` + XDG env vars on first call, then wipe the
/// directory's contents on every call so each `TestApp::build` starts
/// against an empty filesystem. Without this, session / history / state
/// files written by one scenario survive into the next - a real source
/// of nondeterminism for libFuzzer, which runs every iteration in the
/// same process.
fn reset_test_home() {
    let dir = TEST_HOME.get_or_init(|| managed_harness_dir("home"));
    // `AppStoryCtx::new` canonicalizes `HOME` before setting cwd, so keep env
    // vars on the same canonical path that the app will use.
    let home = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
    // SAFETY: env vars are set to the same constant path on every call;
    // concurrent reads from other threads see a stable value.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("XDG_STATE_HOME", home.join("state"));
    std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    std::env::set_var("XDG_DATA_HOME", home.join("data"));
    // Wipe everything in `home` so the next scenario sees an empty
    // filesystem. We can't `remove_dir_all` `home` itself because the current
    // process reuses it across fuzz inputs.
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

fn harness_root(kind: &str) -> PathBuf {
    std::env::temp_dir().join("smelt-fuzz-harness").join(kind)
}

fn process_dir_name() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let exe: String = exe
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("p{}-{exe}", std::process::id())
}

fn cleanup_stale_harness_dirs(kind: &str, current_dir: &Path) {
    let root = harness_root(kind);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current_dir {
            continue;
        }
        let Some(pid) = entry.file_name().to_str().and_then(process_dir_pid) else {
            continue;
        };
        if !process_is_alive(pid) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn process_dir_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('p')?;
    let digits = rest.split_once('-')?.0;
    digits.parse().ok()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
