mod agent;
pub mod auth;
pub(crate) mod cancel;
pub(crate) mod compact;
pub(crate) mod config;
pub mod image;
pub mod log;

pub mod paths;

pub mod pricing;
pub mod provider;
pub mod redact;
pub(crate) mod result_dedup;
pub(crate) mod skills;
pub mod tools;
pub(crate) mod trim;

use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Default auto-compaction threshold, as a percentage of the context window.
const DEFAULT_COMPACT_THRESHOLD_PERCENT: u64 = 80;

/// Environment variable that overrides the auto-compaction threshold.
/// Accepts an integer percentage in `[10, 95]`.
const COMPACT_THRESHOLD_ENV: &str = "SMELT_COMPACT_THRESHOLD_PERCENT";

/// Auto-compaction threshold as a percentage of the context window.
/// Reads `SMELT_COMPACT_THRESHOLD_PERCENT` at call time; falls back to 80.
pub fn compact_threshold_percent() -> u64 {
    std::env::var(COMPACT_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|p| (10..=95).contains(p))
        .unwrap_or(DEFAULT_COMPACT_THRESHOLD_PERCENT)
}

pub use compact::SUMMARY_PREFIX;
pub use config::ModelConfig;
pub use paths::{config_dir, data_dir, home_dir, state_dir};

pub use provider::{Provider, ProviderKind};
pub use skills::SkillLoader;

struct PromptContext<'a> {
    cwd: &'a std::path::Path,
    write_access: bool,
    skills_section: Option<&'a str>,
    extra_instructions: Option<&'a str>,
}

pub(crate) fn build_system_prompt_full(
    mode: protocol::AgentMode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
    skill_section: Option<&str>,
) -> String {
    let ctx = PromptContext {
        cwd,
        write_access: matches!(mode, protocol::AgentMode::Apply | protocol::AgentMode::Yolo),
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let template_src = include_str!("prompts/system.txt");
    let env = minijinja::Environment::new();
    let template = env
        .template_from_str(template_src)
        .expect("system prompt template should parse");

    let rendered = template
        .render(minijinja::context! {
            cwd => ctx.cwd.display().to_string(),
            write_access => ctx.write_access,
            skills_section => ctx.skills_section.unwrap_or(""),
            extra_instructions => ctx.extra_instructions.unwrap_or(""),
        })
        .expect("system prompt template should render");

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

pub struct EngineConfig {
    pub api: ApiConfig,
    pub model: String,
    /// Per-task model overrides; `None` falls back to the live primary.
    pub auxiliary: AuxiliaryModelConfig,
    pub instructions: Option<String>,
    /// When set, replaces the built-in system prompt template entirely.
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    pub skills: Option<Arc<SkillLoader>>,
    pub auto_compact: bool,
    /// `None` causes the engine to fetch this from the provider API on first use.
    pub context_window: Option<u32>,
    pub redact_secrets: bool,
}

pub use protocol::AuxiliaryTask;

impl EngineConfig {
    /// Returns the model+api for an auxiliary task, falling back to the primary.
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

pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    event_rx: mpsc::UnboundedReceiver<EngineEvent>,
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

    pub fn injector(&self) -> EventInjector {
        EventInjector {
            event_tx: self.event_tx.clone(),
        }
    }

    /// Build a handle backed by caller-owned channels, with no agent task.
    /// Used by the storybook harness to drive UI state without a real engine.
    pub fn for_test() -> (
        Self,
        mpsc::UnboundedReceiver<UiCommand>,
        mpsc::UnboundedSender<EngineEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let handle = EngineHandle {
            cmd_tx,
            event_tx: event_tx.clone(),
            event_rx,
        };
        (handle, cmd_rx, event_tx)
    }
}

#[derive(Clone)]
pub struct EventInjector {
    event_tx: mpsc::UnboundedSender<EngineEvent>,
}

impl EventInjector {
    pub fn inject_tool_output(&self, call_id: String, chunk: String) {
        let _ = self
            .event_tx
            .send(EngineEvent::ToolOutput { call_id, chunk });
    }
}

/// Start the engine. Must be called from within a tokio runtime.
pub fn start(config: EngineConfig, dispatcher: Box<dyn tools::ToolDispatcher>) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let event_tx_clone = event_tx.clone();
    tokio::spawn(agent::engine_task(config, dispatcher, cmd_rx, event_tx));

    EngineHandle {
        cmd_tx,
        event_tx: event_tx_clone,
        event_rx,
    }
}
