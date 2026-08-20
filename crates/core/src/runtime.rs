//! Headless-safe runtime core shared by `TuiApp` and `HeadlessApp`.

use super::{
    confirms::Confirms, engine_client::EngineClient, runtime_state::RuntimeState, signals,
    signals::Signals, timers::Timers, NullSink, Osc52Sink, StartupOverrides, SystemSink,
};
use crate::process::ProcessRegistry;
use engine::{EngineHandle, SkillLoader};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontendKind {
    /// Interactive terminal (`TuiApp`).
    Tui,
    /// One-shot CLI (`smelt -p "..."` / `--headless`). No Ui, no human input.
    Headless,
}

impl FrontendKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FrontendKind::Tui => "tui",
            FrontendKind::Headless => "headless",
        }
    }

    /// `true` for `Tui`; `false` for headless.
    pub(crate) fn is_interactive(self) -> bool {
        matches!(self, FrontendKind::Tui)
    }
}

pub struct Core {
    pub config: RuntimeState,
    pub startup_overrides: StartupOverrides,
    /// Identity of the committed Lua generation. Candidate callbacks compare
    /// against this before performing runtime-only effects.
    pub lua_generation: u64,
    pub confirms: Confirms,
    pub clipboard: crate::Clipboard,
    pub timers: Timers,
    pub signals: Signals,
    pub engine: EngineClient,
    pub frontend: FrontendKind,
    pub skills: Option<Arc<SkillLoader>>,
    pub files: crate::fs::FileStateCache,
    pub workspace_files: crate::workspace_files::WorkspaceFiles,
    pub processes: ProcessRegistry,
    pub permissions: crate::permissions::PermissionsHandle,
    pub workspace_permissions: crate::permissions::store::WorkspacePermissionStore,
    /// MCP server registry. Shared `Arc` with the engine's
    /// `McpDispatcher`; `None` when the user declared no MCP servers.
    /// Lua introspection reads through this handle without locking out
    /// the engine's tool dispatch path.
    pub mcp: Option<Arc<crate::mcp::McpManager>>,
    /// Source of monotonic + wall-clock time. Same instance backs the
    /// engine task; swap in [`engine::clock::VirtualClock`] for tests
    /// that need to drive time deterministically.
    pub clock: Arc<dyn engine::clock::Clock>,
    /// Recent user choices rooted in this runtime's application state path.
    pub recent: crate::state::RecentStore,
    /// Canonical session storage and its derived catalog, rooted in the runtime state path.
    pub sessions: crate::session::SessionStorage,
    /// Process-level env snapshot: pid, home, xdg dirs, working directory,
    /// available parallelism. Callers read here instead of touching
    /// `std::env` / `std::process` directly.
    pub env: Arc<engine::env::RuntimeEnv>,
}

impl Core {
    pub fn new(
        config: RuntimeState,
        startup_overrides: StartupOverrides,
        engine: EngineHandle,
        frontend: FrontendKind,
        permissions: crate::permissions::PermissionsHandle,
        clock: Arc<dyn engine::clock::Clock>,
        env: Arc<engine::env::RuntimeEnv>,
    ) -> Self {
        permissions.install_home(env.home().clone());
        let cwd = env.cwd().to_str().map(String::from).unwrap_or_default();
        let signals = signals::build_with_builtins(signals::SignalSeeds {
            vim_mode: "Insert".to_string(),
            agent_mode: config.mode.as_str().to_string(),
            model: config.active_model().map(|model| model.key.clone()),
            reasoning: config.reasoning_effort.label().to_string(),
            cwd,
            session_title: String::new(),
            branch: String::new(),
        });
        let confirms = Confirms::new();
        let confirms_flag = confirms.is_clear_flag();
        let workspace_files = crate::workspace_files::WorkspaceFiles::new(env.state_dir().clone());
        let workspace_permissions =
            crate::permissions::store::WorkspacePermissionStore::new(env.state_dir().clone());
        let recent = crate::state::RecentStore::from_env(&env);
        let sessions = crate::session::SessionStorage::from_env(&env);
        // Read before the struct literal moves `config` into the field below.
        let clipboard =
            crate::Clipboard::new(clipboard_sink(frontend, config.settings.system_clipboard));
        Self {
            config,
            startup_overrides,
            lua_generation: 0,
            confirms,
            clipboard,
            timers: Timers::new(Arc::clone(&clock)),
            signals,
            engine: EngineClient::new(engine, confirms_flag),
            frontend,
            skills: None,
            files: crate::fs::FileStateCache::new(),
            workspace_files,
            processes: ProcessRegistry::new(),
            permissions,
            workspace_permissions,
            mcp: None,
            clock,
            recent,
            sessions,
            env,
        }
    }

    /// Swap the clipboard sink to match the `system_clipboard` setting. Off
    /// installs a [`NullSink`] for the TUI so the kill ring stays purely
    /// internal (no OS clipboard read/write); on restores the frontend's
    /// default sink. Headless always keeps the subprocess clipboard sink.
    pub fn set_system_clipboard_enabled(&mut self, enabled: bool) {
        self.clipboard
            .swap_sink(clipboard_sink(self.frontend, enabled));
    }
}

/// Pick the clipboard sink for `frontend`. The TUI normally uses OSC 52 (works
/// over SSH/tmux), but falls back to a no-op [`NullSink`] when the user turns
/// off `system_clipboard`. Headless always uses the subprocess clipboard.
fn clipboard_sink(frontend: FrontendKind, system_clipboard: bool) -> Box<dyn crate::Sink + Send> {
    match frontend {
        FrontendKind::Tui if system_clipboard => Box::new(Osc52Sink),
        FrontendKind::Tui => Box::new(NullSink),
        FrontendKind::Headless => Box::new(SystemSink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_sink_without_system_clipboard_is_internal_only() {
        // `system_clipboard = false` installs a `NullSink`: it never reads the
        // OS clipboard (so the paste-sync can't clobber the internal kill ring)
        // and its writes are no-ops.
        let mut sink = clipboard_sink(FrontendKind::Tui, false);
        assert_eq!(sink.read(), None);
        assert!(sink.write("anything").is_ok());
    }
}
