mod agent;
pub mod config;
pub mod image;
pub mod log;
pub mod permissions;
pub mod plan;
pub mod provider;
pub mod tools;

use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub fn state_dir() -> PathBuf {
    const APP_NAME: &str = "agent";
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("state")
        })
        .join(APP_NAME)
}

pub use config::ModelConfig;
pub use permissions::Permissions;
pub use provider::Provider;

/// Assemble the system prompt from the base template, mode overlay, cwd, and
/// optional extra instructions (e.g. from AGENTS.md files).
pub fn build_system_prompt(
    mode: protocol::Mode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
) -> String {
    let base = include_str!("prompts/system.txt");
    let overlay = match mode {
        protocol::Mode::Apply | protocol::Mode::Yolo => include_str!("prompts/system_apply.txt"),
        protocol::Mode::Plan => include_str!("prompts/system_plan.txt"),
        protocol::Mode::Normal => "",
    };

    let mut prompt = format!("{base}\n\nYou are working in: {cwd}", cwd = cwd.display());

    if !overlay.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(overlay);
    }

    if let Some(instructions) = extra_instructions {
        prompt.push_str("\n\n");
        prompt.push_str(instructions);
    }

    prompt
}

/// Configuration for the engine. Constructed once by the binary.
pub struct EngineConfig {
    pub api_base: String,
    pub api_key: String,
    pub model_config: ModelConfig,
    pub system_prompt: String,
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
}

/// Handle to a running engine. Send commands, receive events.
pub struct EngineHandle {
    pub cmd_tx: mpsc::UnboundedSender<UiCommand>,
    pub event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    pub processes: tools::ProcessRegistry,
    pub permissions: Arc<Permissions>,
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
}

/// Start the engine. Returns a handle for bidirectional communication.
pub fn start(config: EngineConfig) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let processes = tools::ProcessRegistry::new();
    let registry = tools::build_tools(processes.clone());

    let permissions = Arc::clone(&config.permissions);
    let processes_clone = processes.clone();
    tokio::spawn(agent::engine_task(
        config,
        registry,
        processes_clone,
        cmd_rx,
        event_tx,
    ));

    EngineHandle {
        cmd_tx,
        event_rx,
        processes,
        permissions,
    }
}
