use serde::{Deserialize, Serialize};

use crate::{ModelConfigOverrides, ThinkingBudgets};

/// Complete model/provider value carried across every engine request boundary.
///
/// API keys are resolved immediately before dispatch. The custom `Debug`
/// implementation prevents accidental disclosure in command or test logs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelTarget {
    pub model: String,
    pub api_base: String,
    pub api_key: String,
    pub provider_type: String,
    pub config: ModelConfig,
}

impl std::fmt::Debug for ModelTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelTarget")
            .field("model", &self.model)
            .field("api_base", &self.api_base)
            .field("api_key", &"[REDACTED]")
            .field("provider_type", &self.provider_type)
            .field("config", &self.config)
            .finish()
    }
}

/// Fully resolved model behavior and metadata.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Maximum output tokens for this model.
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

impl ModelConfig {
    pub fn tool_calling(&self) -> bool {
        self.tool_calling.unwrap_or(true)
    }

    pub fn with_overrides(mut self, overrides: &ModelConfigOverrides) -> Self {
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
        if let Some(v) = overrides.tool_calling {
            self.tool_calling = Some(v);
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

/// Request-audit persistence policy resolved for a request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestAuditMode {
    Off,
    #[default]
    Summary,
    Full,
}

impl RequestAuditMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Some(Self::Off),
            "summary" | "summaries" => Some(Self::Summary),
            "full" | "trace" => Some(Self::Full),
            _ => None,
        }
    }
}

/// Runtime settings snapshotted for one turn or auxiliary request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestRuntimeConfig {
    pub redact_secrets: bool,
    pub cache_ttl_long: bool,
    pub request_audit: RequestAuditMode,
}

/// Model metadata fetched from managed providers and shared with config/model resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_output_tokens: Option<u32>,
    pub supports_reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub supports_fast_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub input_modalities: Option<Vec<String>>,
}

impl ModelMetadata {
    pub fn matches_name(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self
                .display_name
                .as_deref()
                .is_some_and(|display| display.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_request_values_round_trip() {
        let target = ModelTarget {
            model: "model-a".into(),
            api_base: "https://example.test/v1".into(),
            api_key: "secret-key".into(),
            provider_type: "openai".into(),
            config: ModelConfig {
                temperature: Some(0.4),
                tool_calling: Some(false),
                input_modalities: Some(vec!["text".into(), "image".into()]),
                ..Default::default()
            },
        };
        let runtime = RequestRuntimeConfig {
            redact_secrets: true,
            cache_ttl_long: true,
            request_audit: RequestAuditMode::Full,
        };

        let target_json = serde_json::to_string(&target).unwrap();
        let runtime_json = serde_json::to_string(&runtime).unwrap();
        assert_eq!(
            serde_json::from_str::<ModelTarget>(&target_json).unwrap(),
            target
        );
        assert_eq!(
            serde_json::from_str::<RequestRuntimeConfig>(&runtime_json).unwrap(),
            runtime
        );
    }

    #[test]
    fn model_target_debug_redacts_api_key() {
        let target = ModelTarget {
            model: "model-a".into(),
            api_base: "https://example.test".into(),
            api_key: "do-not-print-this".into(),
            provider_type: "openai".into(),
            config: ModelConfig::default(),
        };

        let debug = format!("{target:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("do-not-print-this"));
    }

    #[test]
    fn model_config_overrides_every_request_field_and_preserves_metadata() {
        let config = ModelConfig {
            top_p: Some(0.25),
            input_cost: Some(2.0),
            ..Default::default()
        }
        .with_overrides(&ModelConfigOverrides {
            temperature: Some(0.7),
            top_p: None,
            top_k: Some(40),
            min_p: Some(0.1),
            repeat_penalty: Some(1.1),
            tool_calling: Some(false),
            max_tokens: Some(1234),
            thinking_budgets: Some(ThinkingBudgets {
                low: 1,
                medium: 2,
                high: 3,
                max: 4,
            }),
        });

        assert_eq!(config.temperature, Some(0.7));
        assert_eq!(config.top_p, Some(0.25));
        assert_eq!(config.top_k, Some(40));
        assert_eq!(config.min_p, Some(0.1));
        assert_eq!(config.repeat_penalty, Some(1.1));
        assert_eq!(config.tool_calling, Some(false));
        assert_eq!(config.max_tokens, Some(1234));
        assert_eq!(
            config.thinking_budgets,
            Some(ThinkingBudgets {
                low: 1,
                medium: 2,
                high: 3,
                max: 4,
            })
        );
        assert_eq!(config.input_cost, Some(2.0));
    }
}
