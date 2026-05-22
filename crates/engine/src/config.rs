use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub name: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub tool_calling: Option<bool>,
    /// Cost per 1M input tokens in USD. Overrides built-in pricing.
    pub input_cost: Option<f64>,
    /// Cost per 1M output tokens in USD. Overrides built-in pricing.
    pub output_cost: Option<f64>,
    /// Cost per 1M cache-read tokens in USD.
    pub cache_read_cost: Option<f64>,
    /// Cost per 1M cache-write tokens in USD.
    pub cache_write_cost: Option<f64>,
    /// Token budget for models that use budget-based thinking
    /// (e.g. Kimi via the Anthropic-compatible endpoint).
    pub thinking_budget: Option<u32>,
}

impl ModelConfig {
    pub(crate) fn tool_calling(&self) -> bool {
        self.tool_calling.unwrap_or(true)
    }

    pub(crate) fn with_overrides(mut self, overrides: &protocol::ModelConfigOverrides) -> Self {
        if let Some(v) = overrides.temperature {
            self.temperature = Some(v);
        }
        if let Some(v) = overrides.top_p {
            self.top_p = Some(v);
        }
        if let Some(v) = overrides.top_k {
            self.top_k = Some(v);
        }
        if let Some(v) = overrides.min_p {
            self.min_p = Some(v);
        }
        if let Some(v) = overrides.repeat_penalty {
            self.repeat_penalty = Some(v);
        }
        if let Some(v) = overrides.thinking_budget {
            self.thinking_budget = Some(v);
        }
        self
    }
}
