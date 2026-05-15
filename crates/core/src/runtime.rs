//! Headless-safe runtime core shared by `TuiApp` and `HeadlessApp`.

use super::{
    app_config::AppConfig, cells, cells::Cells, confirms::Confirms, engine_client::EngineClient,
    timers::Timers, Osc52Sink, SystemSink,
};
use crate::process::ProcessRegistry;
use crate::session::Session;
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
    pub config: AppConfig,
    pub session: Session,
    pub confirms: Confirms,
    pub clipboard: crate::Clipboard,
    pub timers: Timers,
    pub cells: Cells,
    pub engine: EngineClient,
    pub frontend: FrontendKind,
    pub skills: Option<Arc<SkillLoader>>,
    pub files: crate::fs::FileStateCache,
    pub processes: ProcessRegistry,
    pub permissions: Arc<crate::permissions::Permissions>,
    /// MCP server registry. Shared `Arc` with the engine's
    /// `McpDispatcher`; `None` when the user declared no MCP servers.
    /// Lua introspection reads through this handle without locking out
    /// the engine's tool dispatch path.
    pub mcp: Option<Arc<crate::mcp::McpManager>>,
    /// Source of monotonic + wall-clock time. Same instance backs the
    /// engine task; swap in [`engine::clock::VirtualClock`] for tests
    /// that need to drive time deterministically.
    pub clock: Arc<dyn engine::clock::Clock>,
    /// Process-level env snapshot: pid, home, xdg dirs, working directory,
    /// available parallelism. Callers read here instead of touching
    /// `std::env` / `std::process` directly.
    pub env: Arc<engine::env::RuntimeEnv>,
}

impl Core {
    pub fn new(
        config: AppConfig,
        engine: EngineHandle,
        frontend: FrontendKind,
        permissions: Arc<crate::permissions::Permissions>,
        clock: Arc<dyn engine::clock::Clock>,
        env: Arc<engine::env::RuntimeEnv>,
    ) -> Self {
        let cwd = env.cwd().to_str().map(String::from).unwrap_or_default();
        let cells = cells::build_with_builtins(cells::BuiltinSeeds {
            vim_mode: "Insert".to_string(),
            agent_mode: config.mode.as_str().to_string(),
            model: config.model.clone(),
            reasoning: config.reasoning_effort.label().to_string(),
            cwd,
            session_title: String::new(),
            branch: String::new(),
        });
        let confirms = Confirms::new();
        let confirms_flag = confirms.is_clear_flag();
        Self {
            config,
            session: Session::new(env.pid(), env.cwd()),
            confirms,
            clipboard: crate::Clipboard::new(match frontend {
                FrontendKind::Tui => Box::new(Osc52Sink),
                FrontendKind::Headless => Box::new(SystemSink),
            }),
            timers: Timers::new(Arc::clone(&clock)),
            cells,
            engine: EngineClient::new(engine, confirms_flag),
            frontend,
            skills: None,
            files: crate::fs::FileStateCache::new(),
            processes: ProcessRegistry::new(),
            permissions,
            mcp: None,
            clock,
            env,
        }
    }
}
