use protocol::{ReasoningEffort, ThinkingBudgets};
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
    /// Maximum output tokens for this model. Defaults to the model's own limit, falling back to 4096 if unknown.
    pub max_tokens: Option<u32>,
    /// Per-level token budgets for budget-based thinking.
    pub thinking_budgets: Option<ThinkingBudgets>,
    /// Total context window, in tokens, from provider/catalog metadata.
    pub context_window: Option<u32>,
    /// Whether metadata says this model supports reasoning/thinking parameters.
    pub supports_reasoning: Option<bool>,
    /// Whether metadata says this model supports accelerated inference.
    pub supports_fast_mode: Option<bool>,
    /// Input modalities supported by this model, such as `text`, `image`, and `pdf`.
    pub input_modalities: Option<Vec<String>>,
}

pub fn effective_reasoning_effort(
    requested: ReasoningEffort,
    provider_type: &str,
    supports_reasoning: Option<bool>,
) -> ReasoningEffort {
    if requested == ReasoningEffort::Off {
        return ReasoningEffort::Off;
    }

    if supports_reasoning == Some(false)
        || (provider_type == "openai-compatible" && supports_reasoning != Some(true))
    {
        ReasoningEffort::Off
    } else {
        requested
    }
}

impl ModelConfig {
    pub fn tool_calling(&self) -> bool {
        self.tool_calling.unwrap_or(true)
    }

    pub fn with_overrides(mut self, overrides: &protocol::ModelConfigOverrides) -> Self {
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
        if let Some(v) = overrides.max_tokens {
            self.max_tokens = Some(v);
        }
        if let Some(v) = overrides.thinking_budgets {
            self.thinking_budgets = Some(v);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_compatible_reasoning_requires_explicit_support() {
        assert_eq!(
            effective_reasoning_effort(ReasoningEffort::High, "openai-compatible", None),
            ReasoningEffort::Off
        );

        assert_eq!(
            effective_reasoning_effort(ReasoningEffort::High, "openai-compatible", Some(true)),
            ReasoningEffort::High
        );
    }

    #[test]
    fn model_config_with_overrides_threads_each_field() {
        let overrides = protocol::ModelConfigOverrides {
            temperature: Some(0.7),
            top_p: Some(0.8),
            top_k: Some(40),
            min_p: Some(0.1),
            repeat_penalty: Some(1.1),
            max_tokens: Some(1234),
            thinking_budgets: Some(protocol::ThinkingBudgets {
                low: 1,
                medium: 2,
                high: 3,
                max: 4,
            }),
        };
        let cfg = ModelConfig::default().with_overrides(&overrides);
        assert_eq!(cfg.temperature, Some(0.7));
        assert_eq!(cfg.top_p, Some(0.8));
        assert_eq!(cfg.top_k, Some(40));
        assert_eq!(cfg.min_p, Some(0.1));
        assert_eq!(cfg.repeat_penalty, Some(1.1));
        assert_eq!(cfg.max_tokens, Some(1234));
        let budgets = cfg.thinking_budgets.unwrap();
        assert_eq!(budgets.low, 1);
        assert_eq!(budgets.medium, 2);
        assert_eq!(budgets.high, 3);
        assert_eq!(budgets.max, 4);
    }
}
