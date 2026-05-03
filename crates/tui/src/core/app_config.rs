//! Static + runtime configuration that drives the engine and the
//! TUI's behaviour: the active provider/model triple, the agent and
//! reasoning modes (and their cycle lists), the resolved settings
//! flags, and the CLI override flags that take precedence over saved
//! session values.

use crate::{config, state};
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

    pub settings: state::ResolvedSettings,
    pub context_window: Option<u32>,
}
