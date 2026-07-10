use crate::config;
use protocol::{
    AgentMode, ModelConfig, ModelTarget, ReasoningEffort, RequestAuditMode, RequestRuntimeConfig,
};

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
    pub cli_mode_cycle_override: bool,

    pub mode: AgentMode,
    pub mode_cycle: Vec<AgentMode>,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,

    pub settings: config::ResolvedSettings,
    /// Immutable environment policy captured at launch. `None` keeps request
    /// audit controlled by the live Lua setting.
    pub request_audit_override: Option<RequestAuditMode>,
    pub remember: config::RememberConfig,
    pub context_window: Option<u32>,
}

impl AppConfig {
    pub fn supports_fast_mode(&self) -> bool {
        self.provider_type == "codex" && self.model_config.supports_fast_mode == Some(true)
    }

    /// Construct the active target after the caller resolves its API key.
    pub fn model_target(&self, api_key: String) -> ModelTarget {
        ModelTarget {
            model: self.model.clone(),
            api_base: self.api_base.clone(),
            api_key,
            provider_type: self.provider_type.clone(),
            config: self.model_config.clone(),
        }
    }

    /// Snapshot request-scoped settings. The environment audit policy is an
    /// immutable launch override and therefore wins over live Lua settings.
    pub fn request_runtime_config(&self) -> RequestRuntimeConfig {
        let request_audit = self.request_audit_override.unwrap_or_else(|| {
            RequestAuditMode::parse(&self.settings.request_audit).unwrap_or_default()
        });
        RequestRuntimeConfig {
            redact_secrets: self.settings.redact_secrets,
            cache_ttl_long: self.settings.cache_ttl_long,
            request_audit,
        }
    }
}
