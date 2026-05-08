use crate::config;
use engine::ModelConfig;
use protocol::{AgentMode, ReasoningEffort};

pub struct AppConfig {
    pub model: String,
    pub api_base: String,
    pub api_key_env: String,
    pub provider_type: String,

    pub available_models: Vec<config::ResolvedModel>,
    pub model_config: ModelConfig,

    pub cli_model_override: bool,
    pub cli_api_base_override: bool,
    pub cli_api_key_env_override: bool,

    pub mode: AgentMode,
    pub mode_cycle: Vec<AgentMode>,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,

    pub settings: config::ResolvedSettings,
    pub context_window: Option<u32>,
}
