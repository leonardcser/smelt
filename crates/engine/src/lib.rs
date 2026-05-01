mod agent;
pub mod auth;
pub(crate) mod cancel;
pub(crate) mod compact;
pub(crate) mod config;
pub mod config_file;
pub mod image;
pub mod log;
pub(crate) mod mcp;
pub mod paths;
pub mod permissions;

pub mod pricing;
pub mod provider;
pub mod redact;
pub mod registry;
pub(crate) mod skills;
pub mod socket;
pub mod tools;

use protocol::{EngineEvent, UiCommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Default auto-compaction threshold, as a percentage of the context window.
const DEFAULT_COMPACT_THRESHOLD_PERCENT: u64 = 80;

/// Environment variable that overrides the auto-compaction threshold.
/// Accepts an integer percentage in `[10, 95]`.
const COMPACT_THRESHOLD_ENV: &str = "SMELT_COMPACT_THRESHOLD_PERCENT";

/// Auto-compaction trigger threshold as a percentage of the context window.
///
/// Reads `SMELT_COMPACT_THRESHOLD_PERCENT` at call time (it's a cheap env
/// lookup, and reading each check keeps behavior easy to verify from tests
/// and lets the user change it without restarting the engine process).
/// Invalid or out-of-range values fall back to the 80% default.
pub fn compact_threshold_percent() -> u64 {
    std::env::var(COMPACT_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|p| (10..=95).contains(p))
        .unwrap_or(DEFAULT_COMPACT_THRESHOLD_PERCENT)
}

pub use compact::SUMMARY_PREFIX;
pub use config::ModelConfig;
pub use mcp::McpServerConfig;
pub use paths::{config_dir, home_dir, state_dir};
pub use permissions::Permissions;
pub use provider::{Provider, ProviderKind};
pub use skills::SkillLoader;

/// Context for rendering the system prompt template.
struct PromptContext<'a> {
    cwd: &'a std::path::Path,
    interactive: bool,
    write_access: bool,
    multi_agent: Option<&'a AgentPromptConfig>,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub(crate) fn build_system_prompt_full(
    mode: protocol::AgentMode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
    agent_config: Option<&AgentPromptConfig>,
    skill_section: Option<&str>,
    interactive: bool,
) -> String {
    let ctx = PromptContext {
        cwd,
        interactive,
        write_access: matches!(mode, protocol::AgentMode::Apply | protocol::AgentMode::Yolo),
        multi_agent: agent_config,
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

/// Render the system prompt template with the given context.
fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let template_src = include_str!("prompts/system.txt");
    let env = minijinja::Environment::new();
    let template = env
        .template_from_str(template_src)
        .expect("system prompt template should parse");

    let is_child = ctx.multi_agent.map(|m| m.depth > 0).unwrap_or(false);
    let agent_id = ctx.multi_agent.map(|m| m.agent_id.as_str()).unwrap_or("");
    let parent_id = ctx
        .multi_agent
        .and_then(|m| m.parent_id.as_deref())
        .unwrap_or("unknown");
    let siblings = ctx
        .multi_agent
        .map(|m| m.siblings.join(", "))
        .unwrap_or_default();

    let rendered = template
        .render(minijinja::context! {
            cwd => ctx.cwd.display().to_string(),
            interactive => ctx.interactive,
            write_access => ctx.write_access,
            multi_agent => ctx.multi_agent.is_some(),
            is_child => is_child,
            agent_id => agent_id,
            parent_id => parent_id,
            siblings => siblings,
            skills_section => ctx.skills_section.unwrap_or(""),
            extra_instructions => ctx.extra_instructions.unwrap_or(""),
        })
        .expect("system prompt template should render");

    // Collapse runs of 3+ blank lines into 2 (section separators).
    let mut result = String::with_capacity(rendered.len());
    let mut blank_count = 0u32;
    for line in rendered.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

/// Configuration for the multi-agent section of the system prompt.
#[derive(Clone)]
pub struct AgentPromptConfig {
    pub agent_id: String,
    pub depth: u8,
    pub parent_id: Option<String>,
    /// Sibling agent names (other children of the same parent).
    pub siblings: Vec<String>,
}

/// Multi-agent configuration. Present when multi-agent mode is enabled.
pub struct MultiAgentConfig {
    pub depth: u8,
    pub max_depth: u8,
    pub max_agents: u8,
    pub parent_pid: Option<u32>,
}

/// API connection and model configuration, grouped for clarity.
#[derive(Clone)]
pub struct ApiConfig {
    pub base: String,
    pub key: String,
    pub key_env: String,
    pub provider_type: String,
    pub model_config: ModelConfig,
}

#[derive(Clone)]
pub struct RequestModelConfig {
    pub model: String,
    pub api: ApiConfig,
}

#[derive(Clone, Default)]
pub struct AuxiliaryModelConfig {
    pub title: Option<RequestModelConfig>,
    pub prediction: Option<RequestModelConfig>,
    pub compaction: Option<RequestModelConfig>,
    pub btw: Option<RequestModelConfig>,
}

/// Configuration for the engine. Constructed once by the binary.
pub struct EngineConfig {
    pub api: ApiConfig,
    /// Initial primary model name.
    pub model: String,
    /// Per-task auxiliary model overrides. Tasks with `None` fall back to
    /// the live primary at request time.
    pub auxiliary: AuxiliaryModelConfig,
    pub instructions: Option<String>,
    /// When set, replaces the entire system prompt (skips the built-in
    /// template, mode overlays, and AGENTS.md instructions).
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
    /// Runtime approvals shared between engine and TUI.
    pub runtime_approvals: Arc<std::sync::RwLock<permissions::RuntimeApprovals>>,
    /// Multi-agent settings. `None` when multi-agent is disabled.
    pub multi_agent: Option<MultiAgentConfig>,
    /// True when a human is present (TUI mode). False for headless/subagent.
    pub interactive: bool,
    /// MCP server configurations.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Pre-loaded skill loader.
    pub skills: Option<Arc<SkillLoader>>,
    /// Auto-compact when context usage crosses the threshold.
    pub auto_compact: bool,
    /// Context window size (tokens). When `None`, the engine fetches it from
    /// the provider API at startup. User config can override via
    /// `context_window`.
    pub context_window: Option<u32>,
    /// When true, redact detected secrets from messages sent to the LLM,
    /// debug logs, and inter-agent socket communication.
    pub redact_secrets: bool,
}

pub use protocol::AuxiliaryTask;

impl EngineConfig {
    /// Resolve the model+api to use for an auxiliary task. Falls back to the
    /// primary model when no dedicated auxiliary model is configured.
    ///
    /// Note: a `SetModel` applied mid-turn (via `Turn::apply_model_change`)
    /// updates the active turn's provider but does not propagate back here,
    /// so a `/btw` arriving in the same turn will use the pre-switch primary.
    /// The next `SetModel` between turns re-syncs.
    pub(crate) fn aux_or_primary(&self, task: AuxiliaryTask) -> RequestModelConfig {
        let slot = match task {
            AuxiliaryTask::Title => &self.auxiliary.title,
            AuxiliaryTask::Prediction => &self.auxiliary.prediction,
            AuxiliaryTask::Compaction => &self.auxiliary.compaction,
            AuxiliaryTask::Btw => &self.auxiliary.btw,
        };
        slot.clone().unwrap_or_else(|| RequestModelConfig {
            model: self.model.clone(),
            api: self.api.clone(),
        })
    }
}

/// Handle to a running engine. Send commands, receive events.
pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    runtime_approvals: Arc<std::sync::RwLock<permissions::RuntimeApprovals>>,
    agent_msg_tx: Option<tokio::sync::broadcast::Sender<tools::AgentMessageNotification>>,
    spawned_rx: Option<mpsc::UnboundedReceiver<tools::SpawnedChild>>,
    /// Stashed subagent spawn config. `None` when multi-agent is disabled
    /// or this process has reached `max_depth`. Read back by
    /// `spawn_subagent` (called from the Lua-side `spawn_agent` tool).
    subagent_config: Option<tools::SubagentConfig>,
}

impl EngineHandle {
    pub fn send(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.event_rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn runtime_approvals(&self) -> Arc<std::sync::RwLock<permissions::RuntimeApprovals>> {
        Arc::clone(&self.runtime_approvals)
    }

    /// Drain spawned child handles (stdout pipes for subagent streaming).
    pub fn drain_spawned(&mut self) -> Vec<tools::SpawnedChild> {
        let Some(ref mut rx) = self.spawned_rx else {
            return vec![];
        };
        let mut children = vec![];
        while let Ok(child) = rx.try_recv() {
            children.push(child);
        }
        children
    }

    /// Snapshot of subagent spawn settings — `(depth, max_depth, max_agents)`.
    /// `None` when multi-agent is disabled. The Lua `spawn_agent` tool calls
    /// this to expose the multi-agent shape (and the "are we allowed to
    /// spawn at this depth?" check) without taking the heavier
    /// `SubagentConfig` route.
    pub fn subagent_meta(&self) -> Option<(u8, u8, u8)> {
        self.subagent_config
            .as_ref()
            .map(|c| (c.depth, c.max_depth, c.max_agents))
    }

    /// Spawn a subagent process. Wraps the same `std::process::Command`
    /// dance the retired `SpawnAgentTool` performed: registers a fresh
    /// `agent_id`, points the child's stderr at
    /// `<session_dir>/agent_logs/<pid>.log`, posts the captured stdout
    /// to the spawned-child channel for the frontend to consume, and
    /// returns the new agent's id.
    ///
    /// `blocking` is informational — the child runs the same way
    /// regardless. The frontend reads it back from `SpawnedChild` to
    /// decide whether to render a live overlay or a one-line static
    /// block.
    ///
    /// Returns `Err` if multi-agent is disabled, the depth budget is
    /// exhausted, the child count is at the `max_agents` cap, or the
    /// spawn itself fails.
    pub fn spawn_subagent(
        &self,
        prompt: String,
        blocking: bool,
        session_dir: &std::path::Path,
    ) -> Result<String, String> {
        let cfg = self
            .subagent_config
            .as_ref()
            .ok_or_else(|| "multi-agent disabled".to_string())?;

        if cfg.depth >= cfg.max_depth {
            return Err(format!(
                "cannot spawn: max depth reached ({})",
                cfg.max_depth
            ));
        }

        let current = registry::children_of(cfg.pid).len();
        if current >= cfg.max_agents as usize {
            return Err(format!(
                "cannot spawn: already at max agents ({}) for this session",
                cfg.max_agents
            ));
        }

        let exe = std::env::current_exe().map_err(|e| format!("cannot find binary: {e}"))?;
        let child_depth = cfg.depth + 1;

        let mut cmd = std::process::Command::new(&exe);
        cmd.args([
            "--subagent",
            "--multi-agent",
            "--parent-pid",
            &cfg.pid.to_string(),
            "--depth",
            &child_depth.to_string(),
            "--max-agents",
            &cfg.max_agents.to_string(),
            "--api-base",
            &cfg.api_base,
            "--api-key-env",
            &cfg.api_key_env,
            "--type",
            &cfg.provider_type,
            "-m",
            &cfg.model,
        ]);
        cmd.arg(&prompt);
        cmd.stdin(std::process::Stdio::null());
        cmd.env("FORCE_COLOR", "1");

        if cfg.provider_type == ProviderKind::Codex.as_config_str() {
            if let Some(tokens) = provider::codex::CodexTokens::load() {
                cmd.env(provider::codex::CODEX_TOKENS_ENV, tokens.to_env_json());
            }
        }

        let agent_id = registry::next_agent_id();

        cmd.stdout(std::process::Stdio::piped());
        let log_dir = session_dir.join("agent_logs");
        let _ = std::fs::create_dir_all(&log_dir);
        match std::fs::File::create(log_dir.join(format!("{agent_id}.log"))) {
            Ok(f) => {
                cmd.stderr(f);
            }
            Err(_) => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn subagent: {e}"))?;
        let pid = child.id();

        let _ = std::fs::rename(
            log_dir.join(format!("{agent_id}.log")),
            log_dir.join(format!("{pid}.log")),
        );

        let _ = registry::register(&registry::RegistryEntry {
            agent_id: agent_id.clone(),
            pid,
            parent_pid: Some(cfg.pid),
            git_root: Some(cfg.scope.clone()),
            git_branch: None,
            cwd: cfg.scope.clone(),
            status: registry::AgentStatus::Working,
            task_slug: None,
            session_id: String::new(),
            socket_path: String::new(),
            depth: child_depth,
            started_at: String::new(),
        });

        if let Some(ref tx) = cfg.spawned_tx {
            if let Some(stdout) = child.stdout.take() {
                let _ = tx.send(tools::SpawnedChild {
                    agent_id: agent_id.clone(),
                    pid,
                    stdout,
                    prompt: prompt.clone(),
                    blocking,
                });
            }
        }

        // Drop the child handle. Rust's Child::drop closes pipes but
        // does NOT kill the process — the subagent continues running
        // independently.
        drop(child);

        Ok(agent_id)
    }

    /// Create a cloneable injector for external tasks (socket bridge, watchers)
    /// that need to inject events into the engine's event stream.
    pub fn injector(&self) -> EventInjector {
        EventInjector {
            event_tx: self.event_tx.clone(),
            agent_msg_tx: self.agent_msg_tx.clone(),
        }
    }
}

/// Cloneable handle for injecting events from external async tasks.
#[derive(Clone)]
pub struct EventInjector {
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    agent_msg_tx: Option<tokio::sync::broadcast::Sender<tools::AgentMessageNotification>>,
}

impl EventInjector {
    /// Subscribe to the agent-message broadcast channel. `None` when
    /// multi-agent is disabled or this process can't spawn children
    /// (e.g. is itself a subagent at non-zero depth). The Lua
    /// `spawn_agent` tool calls this through `smelt.agent.wait_for_message`
    /// for blocking spawn semantics.
    pub fn subscribe_agent_msg(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<tools::AgentMessageNotification>> {
        self.agent_msg_tx.as_ref().map(|tx| tx.subscribe())
    }

    pub fn inject_agent_message(&self, from_id: String, from_slug: String, message: String) {
        if let Some(ref tx) = self.agent_msg_tx {
            let _ = tx.send(tools::AgentMessageNotification {
                from_id: from_id.clone(),
                message: message.clone(),
            });
        }
        let _ = self.event_tx.send(EngineEvent::AgentMessage {
            from_id,
            from_slug,
            message,
        });
    }

    pub fn inject_agent_exited(&self, agent_id: String, exit_code: Option<i32>) {
        let _ = self.event_tx.send(EngineEvent::AgentExited {
            agent_id,
            exit_code,
        });
    }

    /// Stream a chunk of output for an in-flight tool call. Used by
    /// the tui-side bash tool to emit `EngineEvent::ToolOutput` per
    /// line as a child process runs, matching the streaming UX of
    /// the legacy engine-side `BashTool`.
    pub fn inject_tool_output(&self, call_id: String, chunk: String) {
        let _ = self
            .event_tx
            .send(EngineEvent::ToolOutput { call_id, chunk });
    }
}

/// Start the engine. Returns a handle for bidirectional communication.
///
/// `files` is the shared file-state cache — read_file populates it, edit/write
/// consult it for staleness checks. The caller passes its own clone so the
/// frontend can expose the same cache to Lua tools while engine-side tools are
/// migrating off the Rust impl.
///
/// MCP servers are connected asynchronously — this must be called from
/// within a tokio runtime.
pub fn start(config: EngineConfig, files: tools::FileStateCache) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Broadcast channel for agent message notifications (blocking spawn_agent).
    // Only created for interactive agents (depth == 0) that can spawn children.
    let agent_msg_tx = if config.multi_agent.as_ref().is_some_and(|ma| ma.depth == 0) {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        Some(tx)
    } else {
        None
    };

    // Channel for spawned child stdout handles (used for streaming).
    let (spawned_tx, spawned_rx) = mpsc::unbounded_channel();

    let subagent_config = config.multi_agent.as_ref().map(|ma| {
        let scope = config.cwd.to_string_lossy().into_owned();
        let my_pid = std::process::id();
        tools::SubagentConfig {
            scope,
            pid: my_pid,
            depth: ma.depth,
            max_depth: ma.max_depth,
            max_agents: ma.max_agents,
            api_base: config.api.base.clone(),
            api_key_env: config.api.key_env.clone(),
            model: config.api.model_config.name.clone().unwrap_or_default(),
            provider_type: config.api.provider_type.clone(),
            agent_msg_tx: agent_msg_tx.clone(),
            spawned_tx: Some(spawned_tx),
        }
    });

    let registry = tools::build_tools(files);

    let runtime_approvals = Arc::clone(&config.runtime_approvals);
    let has_multi_agent = config.multi_agent.is_some();
    let event_tx_clone = event_tx.clone();
    tokio::spawn(agent::engine_task(config, registry, cmd_rx, event_tx));

    EngineHandle {
        cmd_tx,
        event_tx: event_tx_clone,
        event_rx,
        runtime_approvals,
        agent_msg_tx,
        spawned_rx: if has_multi_agent {
            Some(spawned_rx)
        } else {
            None
        },
        subagent_config,
    }
}
