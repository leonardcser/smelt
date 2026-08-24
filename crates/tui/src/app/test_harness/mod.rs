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
use std::sync::{Arc, Mutex, OnceLock};

use smelt_test_support::ProcessEnvironmentGuard;
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

/// Read-only window state exposed to external scenario and replay drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSnapshot {
    pub cpos: usize,
    pub source_len: usize,
    pub vim_mode: VimMode,
    pub selection_anchor: Option<usize>,
    pub viewport: Option<crate::smelt_edit::WindowViewport>,
    pub gutter_pad_left: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedWindowSnapshot {
    pub win: WinId,
    pub lines: Vec<String>,
    pub rows: crate::smelt_edit::MaterializedRows,
    pub viewport: crate::smelt_edit::WindowViewport,
    pub scroll_top: crate::smelt_edit::RowIndex,
}

/// Read-only transcript viewport state used by interaction and rendering tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptWindowSnapshot {
    pub buf: crate::smelt_edit::BufId,
    pub scroll_top: crate::smelt_edit::RowIndex,
    pub following_tail: bool,
    pub viewport: Option<crate::smelt_edit::WindowViewport>,
    pub document_view: crate::smelt_edit::DocumentViewState,
    pub row_cursor: Option<crate::smelt_edit::DocPosition>,
    pub materialized_rows: Option<crate::smelt_edit::MaterializedRows>,
    pub vim_mode: VimMode,
    pub gutter_pad_left: u16,
    pub effective_endpoint: usize,
    cursor_absolute_row: Option<crate::smelt_edit::RowIndex>,
    pub search_ranges: Vec<crate::smelt_edit::SelectionRange>,
}

impl TranscriptWindowSnapshot {
    pub fn scroll_top(&self) -> crate::smelt_edit::RowIndex {
        self.scroll_top
    }

    pub fn is_following_tail(&self) -> bool {
        self.following_tail
    }

    pub fn document_view_state(&self) -> crate::smelt_edit::DocumentViewState {
        self.document_view
    }

    pub fn row_cursor(&self) -> Option<crate::smelt_edit::DocPosition> {
        self.row_cursor
    }

    pub fn materialized_rows(&self) -> Option<crate::smelt_edit::MaterializedRows> {
        self.materialized_rows
    }

    pub fn has_materialized_rows(&self) -> bool {
        self.materialized_rows.is_some()
    }

    pub fn local_visual_row(
        &self,
        absolute_row: crate::smelt_edit::RowIndex,
    ) -> crate::smelt_edit::RowIndex {
        if self.document_view.active {
            self.document_view.materialized.local_row(absolute_row)
        } else {
            absolute_row
        }
    }

    pub fn effective_endpoint(&self) -> usize {
        self.effective_endpoint
    }

    pub fn cursor_screen_row(&self, viewport_rows: u16) -> Option<u16> {
        let relative = self.cursor_absolute_row?.checked_sub(self.scroll_top)?;
        (relative < viewport_rows as crate::smelt_edit::RowIndex).then_some(relative as u16)
    }
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
    // Focused unit tests beside the owning `TuiApp` modules may inspect internals.
    // External harness consumers and normal harness builds stay on semantic APIs.
    #[cfg(test)]
    pub(crate) app: TuiApp,
    #[cfg(not(test))]
    app: TuiApp,
    pub clock: Arc<VirtualClock>,
    cmd_rx: Option<mpsc::UnboundedReceiver<UiCommand>>,
    output_injector: Option<engine::EngineOutputInjector>,
    actions: Vec<Action>,
    quit: bool,
    /// Allocation delta for the most recent `feed_one`. `None` when no event
    /// has been fed yet.
    last_alloc: Option<AllocDelta>,
    transcript_scroll_probe: transcript_scroll::TranscriptScrollProbeState,
    _runtime_dir: Option<tempfile::TempDir>,
}

pub struct TestAppBuilder {
    vim: Option<bool>,
    mode: AgentMode,
    mode_cycle: Option<Vec<AgentMode>>,
    reasoning_cycle: Option<Vec<ReasoningEffort>>,
    init_lua: Option<std::path::PathBuf>,
    lua_config_dir: Option<std::path::PathBuf>,
    lua_runtime_override: Option<std::path::PathBuf>,
    runtime_home: Option<std::path::PathBuf>,
    cwd: Option<std::path::PathBuf>,
    ephemeral: bool,
    model_available: bool,
    wall_time: Option<SystemTime>,
    engine: Option<EngineHandle>,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            vim: None,
            mode: AgentMode::normal(),
            mode_cycle: None,
            reasoning_cycle: None,
            init_lua: None,
            lua_config_dir: None,
            lua_runtime_override: None,
            runtime_home: None,
            cwd: None,
            ephemeral: false,
            model_available: true,
            wall_time: None,
            engine: None,
        }
    }
}

impl TestAppBuilder {
    /// Set a fixed startup override for vim-mode on the prompt window.
    pub fn with_vim(mut self, vim: bool) -> Self {
        self.vim = Some(vim);
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

    pub fn with_reasoning_cycle(mut self, efforts: Vec<ReasoningEffort>) -> Self {
        self.reasoning_cycle = Some(efforts);
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

    pub(crate) fn with_runtime_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.runtime_home = Some(home.into());
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

    pub fn with_wall_time(mut self, wall_time: SystemTime) -> Self {
        self.wall_time = Some(wall_time);
        self
    }

    pub(crate) fn with_engine(mut self, engine: EngineHandle) -> Self {
        self.engine = Some(engine);
        self
    }

    pub fn build(self) -> TestApp {
        if let Some(home) = self.runtime_home.clone() {
            return self.build_with_runtime_home(home, None);
        }
        let runtime_dir = tempfile::Builder::new()
            .prefix("app-")
            .tempdir_in(managed_harness_dir("apps"))
            .expect("create isolated app runtime directory");
        let home = runtime_dir.path().to_path_buf();
        self.build_with_runtime_home(home, Some(runtime_dir))
    }

    pub(crate) fn build_with_test_home_guard(self, guard: &ProcessEnvironmentGuard) -> TestApp {
        reset_test_home(guard);
        let home = engine::paths::home_dir();
        self.build_with_runtime_home(home, None)
    }

    pub(crate) fn build_with_test_environment_guard(self, guard: &TestEnvironmentGuard) -> TestApp {
        reset_test_home(guard);
        let home = engine::paths::home_dir();
        self.build_with_runtime_home(home, None)
    }

    pub(crate) fn build_without_test_home_reset(self, _guard: &ProcessEnvironmentGuard) -> TestApp {
        let home = engine::paths::home_dir();
        self.build_with_runtime_home(home, None)
    }

    fn build_with_runtime_home(
        self,
        home: PathBuf,
        runtime_dir: Option<tempfile::TempDir>,
    ) -> TestApp {
        let (engine, mut cmd_rx, output_injector) = match self.engine {
            Some(engine) => (engine, None, None),
            None => {
                let (engine, cmd_rx, output_injector) = EngineHandle::for_test();
                (engine, Some(cmd_rx), Some(output_injector))
            }
        };

        let permissions = smelt_core::permissions::PermissionsHandle::new(
            smelt_core::permissions::Permissions::load(),
        );
        let settings = smelt_core::config::ResolvedSettings {
            vim: self.vim.unwrap_or_default(),
            ..Default::default()
        };
        let shared_session = Arc::new(Mutex::new(None));

        let explicit_mode_cycle = self.mode_cycle.clone();
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
        let startup_overrides = smelt_core::StartupOverrides {
            mode_cycle: explicit_mode_cycle,
            reasoning_cycle: self.reasoning_cycle,
            settings: self
                .vim
                .map(|vim| {
                    (
                        "vim".to_string(),
                        smelt_core::config::SettingValue::Bool(vim),
                    )
                })
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let selections = smelt_core::RuntimeSelections {
            model: self.model_available.then(|| "test/test-model".to_string()),
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

        let clock = Arc::new(VirtualClock::new(
            Instant::now(),
            self.wall_time.unwrap_or_else(SystemTime::now),
        ));
        let config_dir = home.join("config").join("smelt");
        let state_dir = home.join("state").join("smelt");
        let cache_dir = home.join("cache").join("smelt");
        let data_dir = home.join("data").join("smelt");
        let app_runtime_dir = home.join("runtime");
        let cwd = self.cwd.unwrap_or_else(|| home.join("cwd"));
        for path in [
            &home,
            &config_dir,
            &state_dir,
            &cache_dir,
            &data_dir,
            &app_runtime_dir,
            &cwd,
        ] {
            std::fs::create_dir_all(path).expect("create isolated app runtime path");
        }
        let lua_config_dir = self.lua_config_dir.unwrap_or_else(|| config_dir.clone());
        let env = Arc::new(engine::env::RuntimeEnv::scripted(
            4242,
            home,
            config_dir,
            state_dir,
            cache_dir,
            data_dir,
            app_runtime_dir,
            cwd.clone(),
            std::num::NonZeroUsize::new(1).unwrap(),
        ));
        let mut lua = crate::lua::LuaRuntime::new_for_runtime(
            &env,
            Some(lua_config_dir),
            self.lua_runtime_override,
            Some(cwd.clone()),
        );
        if let Some(path) = self.init_lua {
            lua.set_init_lua_path(path);
        }
        // Match production's pre-frontend phase so candidate reloads inherit
        // early.lua builtin opt-outs and CLI declarations as launch inputs.
        lua.load_bundled_early();
        if self.model_available {
            lua.lua()
                .load(
                    r#"
                    _G.__smelt_test_provider = smelt.provider.register("test", {
                        type = "openai-compatible",
                        api_base = "https://example.invalid/v1",
                        api_key_env = "",
                        models = { "test-model" },
                    })
                    "#,
                )
                .exec()
                .expect("register harness test provider");
        }
        lua.load_early_init();
        lua.load_project_early_init(&cwd);
        lua.freeze_launch_inputs();

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
                startup_selections: selections,
                session_persistence,
                ..Default::default()
            },
        );

        // Match production startup by finishing generation-zero loading with a
        // live frontend host. Stories skip the `on_ready` drain on purpose:
        // it is reserved for interactive decoration that storybook snapshots
        // should not include.
        if let Some(error) = app.finish_lua_launch(false) {
            panic!("test app Lua launch failed: {error}");
        }
        app.conversation.clear_pending_history_appends();
        app.pump_lua();
        app.drain_idle_work();
        if let Some(rx) = cmd_rx.as_mut() {
            while rx.try_recv().is_ok() {}
        }

        // Pin spinner glyph and wall-clock time for snapshot determinism.
        // The production `smelt.spinner.glyph()` and `wave_color_at()` use
        // wall-clock time, so parallel or sequential test runs land on
        // different frames / colors. Freezing both makes storybook
        // snapshots stable.
        let lua = app.lua.lua().clone();
        let _ = crate::lua::scope_app(&mut app, || {
            lua.load(
                r#"
                smelt.time.now_ms = function() return 0 end
                smelt.spinner.glyph = function() return '✿' end
                "#,
            )
            .exec()
        });

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
            output_injector,
            actions: Vec::new(),
            quit: false,
            last_alloc: None,
            transcript_scroll_probe: transcript_scroll::TranscriptScrollProbeState::default(),
            _runtime_dir: runtime_dir,
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

pub(crate) type TestEnvironmentGuard = ProcessEnvironmentGuard;

pub(crate) fn test_environment_guard() -> TestEnvironmentGuard {
    ProcessEnvironmentGuard::capture()
}

pub(crate) fn test_home_guard() -> ProcessEnvironmentGuard {
    test_environment_guard()
}

pub(crate) fn initialized_test_home_guard() -> ProcessEnvironmentGuard {
    let guard = test_environment_guard();
    reset_test_home(&guard);
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
fn reset_test_home(environment: &ProcessEnvironmentGuard) {
    let dir = TEST_HOME.get_or_init(|| managed_harness_dir("home"));
    // `AppStoryCtx::new` canonicalizes `HOME` before setting cwd, so keep env
    // vars on the same canonical path that the app will use.
    let home = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
    environment.set_var("HOME", &home);
    environment.set_var("XDG_CONFIG_HOME", home.join("config"));
    environment.set_var("XDG_STATE_HOME", home.join("state"));
    environment.set_var("XDG_CACHE_HOME", home.join("cache"));
    environment.set_var("XDG_DATA_HOME", home.join("data"));
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

    smelt_core::session::create_private_dir_all(&smelt_core::config::state_dir())
        .expect("recreate test state directory");
    smelt_core::session::request_session_catalog_reconciliation();
    assert!(
        smelt_core::session::wait_for_session_catalog(std::time::Duration::from_secs(5)),
        "session catalog did not quiesce after resetting the test home"
    );
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

#[cfg(test)]
mod runtime_path_tests {
    use super::*;

    #[test]
    fn ordinary_apps_coexist_with_independent_runtime_storage() {
        let process_cwd = std::env::current_dir().expect("process cwd");
        let process_home = std::env::var_os("HOME");
        let process_state = std::env::var_os("XDG_STATE_HOME");

        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();

        assert_ne!(first.runtime_home(), second.runtime_home());
        assert_ne!(first.session_storage_root(), second.session_storage_root());
        assert_ne!(first.cwd_str(), second.cwd_str());

        let first_id = first.app.conversation.session().id.clone();
        let second_id = second.app.conversation.session().id.clone();
        first
            .app
            .session_append_history(protocol::HistoryItem::user(protocol::Content::text(
                "first runtime",
            )));
        second
            .app
            .session_append_history(protocol::HistoryItem::user(protocol::Content::text(
                "second runtime",
            )));
        first.app.save_session_and_flush();
        second.app.save_session_and_flush();

        assert!(smelt_store::LineageSessionReader::open_existing(
            first.app.core.sessions.sessions_dir(),
            &first_id,
        )
        .is_ok());
        assert!(smelt_store::LineageSessionReader::open_existing(
            second.app.core.sessions.sessions_dir(),
            &second_id,
        )
        .is_ok());
        assert!(smelt_store::LineageSessionReader::try_open_existing(
            first.app.core.sessions.sessions_dir(),
            &second_id,
        )
        .unwrap()
        .is_none());
        assert!(smelt_store::LineageSessionReader::try_open_existing(
            second.app.core.sessions.sessions_dir(),
            &first_id,
        )
        .unwrap()
        .is_none());
        assert_eq!(first.app.core.sessions.list_sessions()[0].id, first_id);
        assert_eq!(second.app.core.sessions.list_sessions()[0].id, second_id);

        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
        assert_eq!(std::env::var_os("HOME"), process_home);
        assert_eq!(std::env::var_os("XDG_STATE_HOME"), process_state);
    }

    #[test]
    fn ordinary_apps_analyze_shell_paths_with_independent_runtime_homes() {
        let first = TestApp::builder().build();
        let second = TestApp::builder().build();

        assert_eq!(
            first.shell_effect_paths("cat ~/private/config"),
            vec![first.runtime_home().join("private/config")]
        );
        assert_eq!(
            second.shell_effect_paths("cat \"$HOME/private/config\""),
            vec![second.runtime_home().join("private/config")]
        );
        assert_ne!(first.runtime_home(), second.runtime_home());
    }

    #[test]
    fn ordinary_apps_publish_status_in_independent_runtime_roots() {
        let process_runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();
        first.publish_public_status();
        second.publish_public_status();

        let first_path = first
            .public_status_path()
            .expect("first status publisher")
            .to_path_buf();
        let second_path = second
            .public_status_path()
            .expect("second status publisher")
            .to_path_buf();
        assert!(first_path.starts_with(first.runtime_dir()));
        assert!(second_path.starts_with(second.runtime_dir()));
        assert_ne!(first_path, second_path);
        assert!(first_path.is_file());
        assert!(second_path.is_file());

        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.is_file());
        assert_eq!(std::env::var_os("XDG_RUNTIME_DIR"), process_runtime);

        drop(second);
        assert!(!second_path.exists());
        assert_eq!(std::env::var_os("XDG_RUNTIME_DIR"), process_runtime);
    }

    #[test]
    fn ordinary_apps_use_independent_lua_http_caches() {
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();
        let key = format!("runtime-cache-isolation-{}", std::process::id());
        let quoted_key = serde_json::to_string(&key).expect("quote HTTP cache key");

        first
            .run_lua_result(&format!(
                "smelt.http.cache.write({quoted_key}, 'first runtime')"
            ))
            .expect("write first runtime cache");
        second
            .run_lua_result(&format!(
                "smelt.http.cache.write({quoted_key}, 'second runtime')"
            ))
            .expect("write second runtime cache");

        let first_value: String = first
            .eval_lua(&format!(
                "return assert(smelt.http.cache.read({quoted_key}))"
            ))
            .expect("read first runtime cache");
        let second_value: String = second
            .eval_lua(&format!(
                "return assert(smelt.http.cache.read({quoted_key}))"
            ))
            .expect("read second runtime cache");
        assert_eq!(first_value, "first runtime");
        assert_eq!(second_value, "second runtime");

        let first_root = first.runtime_cache_root();
        let second_root = second.runtime_cache_root();
        assert_ne!(first_root, second_root);
        let first_files = std::fs::read_dir(first_root.join("web"))
            .expect("read first runtime cache directory")
            .map(|entry| entry.expect("first runtime cache entry").path())
            .collect::<Vec<_>>();
        let second_files = std::fs::read_dir(second_root.join("web"))
            .expect("read second runtime cache directory")
            .map(|entry| entry.expect("second runtime cache entry").path())
            .collect::<Vec<_>>();
        assert_eq!(first_files.len(), 1);
        assert_eq!(second_files.len(), 1);
        assert!(first_files[0].starts_with(&first_root));
        assert!(second_files[0].starts_with(&second_root));
        assert_ne!(first_files[0], second_files[0]);
    }

    #[test]
    fn ordinary_apps_resolve_lua_filesystem_and_display_paths_independently() {
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();
        let file_name = format!("runtime-path-{}.txt", first.app.conversation.session().id);
        let quoted_name = serde_json::to_string(&file_name).expect("quote runtime file name");

        first
            .run_lua_result(&format!(
                "assert(smelt.fs.write({quoted_name}, 'first runtime'))"
            ))
            .expect("write first runtime file");
        second
            .run_lua_result(&format!(
                "assert(smelt.fs.write({quoted_name}, 'second runtime'))"
            ))
            .expect("write second runtime file");

        let first_path = Path::new(first.cwd_str()).join(&file_name);
        let second_path = Path::new(second.cwd_str()).join(&file_name);
        assert_eq!(
            std::fs::read_to_string(&first_path).unwrap(),
            "first runtime"
        );
        assert_eq!(
            std::fs::read_to_string(&second_path).unwrap(),
            "second runtime"
        );
        assert_ne!(first_path, second_path);

        let first_display: String = first
            .eval_lua(&format!(
                "return smelt.path.display({})",
                serde_json::to_string(&first_path.to_string_lossy()).unwrap()
            ))
            .unwrap();
        let second_display: String = second
            .eval_lua(&format!(
                "return smelt.path.display({})",
                serde_json::to_string(&second_path.to_string_lossy()).unwrap()
            ))
            .unwrap();
        assert_eq!(first_display, file_name);
        assert_eq!(second_display, file_name);

        let first_expanded: String = first
            .eval_lua("return smelt.path.expand('~/nested')")
            .unwrap();
        let second_expanded: String = second
            .eval_lua("return smelt.path.expand('$HOME/nested')")
            .unwrap();
        assert_eq!(
            PathBuf::from(first_expanded),
            first.runtime_home().join("nested")
        );
        assert_eq!(
            PathBuf::from(second_expanded),
            second.runtime_home().join("nested")
        );

        let first_summary: String = first
            .eval_lua(&format!(
                "return smelt.tools.default_summary({{ file_path = {} }})",
                serde_json::to_string(&first_path.to_string_lossy()).unwrap()
            ))
            .unwrap();
        let second_summary: String = second
            .eval_lua(&format!(
                "return smelt.tools.default_summary({{ file_path = {} }})",
                serde_json::to_string(&second_path.to_string_lossy()).unwrap()
            ))
            .unwrap();
        assert_eq!(first_summary, file_name);
        assert_eq!(second_summary, file_name);
        assert!(!std::env::current_dir().unwrap().join(&file_name).exists());
    }

    #[test]
    fn ordinary_apps_display_notebook_paths_from_their_runtime_roots() {
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();

        for app in [&mut first, &mut second] {
            let missing = app.runtime_home().join("missing.ipynb");
            let quoted_path =
                serde_json::to_string(&missing.to_string_lossy()).expect("quote notebook path");
            let error: String = app
                .eval_lua(&format!(
                    "local _, err = smelt.notebook.apply_edit({{ notebook_path = {quoted_path} }}); return assert(err)"
                ))
                .expect("evaluate missing notebook edit");
            assert_eq!(error, "file not found: ~/missing.ipynb");
        }
    }

    #[test]
    fn ordinary_apps_persist_lua_state_in_independent_runtime_roots() {
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();
        let key = format!("runtime_isolation_{}", first.app.conversation.session().id);
        let quoted_key = serde_json::to_string(&key).expect("quote Lua state key");

        first
            .run_lua_result(&format!(
                "local s = smelt.state.persistent({quoted_key}, {{ debounce_ms = 100000 }}); s.value = 'first'; smelt.__flush_persistent_state()"
            ))
            .expect("persist first runtime state");
        second
            .run_lua_result(&format!(
                "local s = smelt.state.persistent({quoted_key}, {{ debounce_ms = 100000 }}); s.value = 'second'; smelt.__flush_persistent_state()"
            ))
            .expect("persist second runtime state");

        let relative_path = Path::new("state")
            .join("smelt")
            .join("plugins")
            .join(format!("{key}.json"));
        let first_path = first.runtime_home().join(&relative_path);
        let second_path = second.runtime_home().join(&relative_path);
        let process_path = smelt_core::config::state_dir()
            .join("plugins")
            .join(format!("{key}.json"));
        let first_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first_path).expect("read first runtime state"))
                .expect("parse first runtime state");
        let second_json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&second_path).expect("read second runtime state"),
        )
        .expect("parse second runtime state");

        assert_eq!(first_json["value"], "first");
        assert_eq!(second_json["value"], "second");
        assert_ne!(first_path, second_path);
        assert!(!process_path.exists(), "process-global state was mutated");
    }

    #[test]
    fn ordinary_apps_refresh_agent_inputs_from_their_runtime_paths() {
        let mut first = TestApp::builder().build();
        let mut second = TestApp::builder().build();

        for (app, label) in [(&first, "first"), (&second, "second")] {
            let config_dir = app.runtime_home().join("config/smelt");
            std::fs::create_dir_all(&config_dir).expect("create runtime config directory");
            std::fs::write(
                config_dir.join("AGENTS.md"),
                format!("{label} global instructions"),
            )
            .expect("write runtime instructions");
            std::fs::write(
                std::path::Path::new(app.cwd_str()).join("AGENTS.md"),
                format!("{label} project instructions"),
            )
            .expect("write project instructions");
            let skill_dir = app.runtime_home().join("config/smelt/skills/runtime_probe");
            std::fs::create_dir_all(&skill_dir).expect("create runtime skill directory");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    "---\nname: runtime-probe\ndescription: {label} runtime skill\n---\n{label} skill body\n"
                ),
            )
            .expect("write runtime skill");
        }

        first.app.refresh_agent_inputs();
        second.app.refresh_agent_inputs();

        let first_instructions = first
            .app
            .prompt_inputs
            .instructions
            .as_deref()
            .expect("first runtime instructions");
        let second_instructions = second
            .app
            .prompt_inputs
            .instructions
            .as_deref()
            .expect("second runtime instructions");
        assert!(first_instructions.contains("first global instructions"));
        assert!(first_instructions.contains("first project instructions"));
        assert!(!first_instructions.contains("second"));
        assert!(second_instructions.contains("second global instructions"));
        assert!(second_instructions.contains("second project instructions"));
        assert!(!second_instructions.contains("first"));

        let first_skill = first
            .app
            .core
            .skills
            .as_ref()
            .expect("first skill loader")
            .content("runtime-probe")
            .expect("first runtime skill");
        let second_skill = second
            .app
            .core
            .skills
            .as_ref()
            .expect("second skill loader")
            .content("runtime-probe")
            .expect("second runtime skill");
        assert!(first_skill.contains("first skill body"));
        assert!(!first_skill.contains("second skill body"));
        assert!(second_skill.contains("second skill body"));
        assert!(!second_skill.contains("first skill body"));
    }
}
