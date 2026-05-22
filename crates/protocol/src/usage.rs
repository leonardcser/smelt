//! Token usage, turn metadata, and per-turn overrides.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Parsed token usage from an API response.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

impl TokenUsage {
    /// Add another usage report into this accumulator.
    pub fn accumulate(&mut self, other: &TokenUsage) {
        fn add(a: &mut Option<u32>, b: Option<u32>) {
            if let Some(v) = b {
                *a = Some(a.unwrap_or(0) + v);
            }
        }
        add(&mut self.prompt_tokens, other.prompt_tokens);
        add(&mut self.completion_tokens, other.completion_tokens);
        add(&mut self.cache_read_tokens, other.cache_read_tokens);
        add(&mut self.cache_write_tokens, other.cache_write_tokens);
        add(&mut self.reasoning_tokens, other.reasoning_tokens);
    }
}

/// Per-turn metadata emitted by the engine at turn completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMeta {
    pub elapsed_ms: u64,
    pub avg_tps: Option<f64>,
    pub interrupted: bool,
    /// Per-tool-call elapsed times, keyed by call_id.
    #[serde(default)]
    pub tool_elapsed: HashMap<String, u64>,
}

/// Model-parameter overrides applied to a single turn.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfigOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
}

/// Permission rule-set override (allow / ask / deny glob patterns).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSetOverride {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

/// Per-turn permission overrides. `tools` matches tool *names*; every
/// other entry in `subcommands` is a per-tool subpattern bucket
/// (`bash`, `web_fetch`, `mcp`, or any tool that registers one).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<RuleSetOverride>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub subcommands: HashMap<String, RuleSetOverride>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- TokenUsage::accumulate ----

    #[test]
    fn accumulate_adds_other_to_none_self_sets_value() {
        let mut acc = TokenUsage::default();
        let other = TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            ..Default::default()
        };
        acc.accumulate(&other);
        assert_eq!(acc.prompt_tokens, Some(10));
        assert_eq!(acc.completion_tokens, Some(5));
        assert!(acc.cache_read_tokens.is_none());
    }

    #[test]
    fn accumulate_sums_when_both_sides_have_value() {
        let mut acc = TokenUsage {
            prompt_tokens: Some(7),
            ..Default::default()
        };
        acc.accumulate(&TokenUsage {
            prompt_tokens: Some(3),
            ..Default::default()
        });
        assert_eq!(acc.prompt_tokens, Some(10));
    }

    #[test]
    fn accumulate_preserves_self_when_other_field_is_none() {
        let mut acc = TokenUsage {
            prompt_tokens: Some(7),
            ..Default::default()
        };
        acc.accumulate(&TokenUsage::default());
        assert_eq!(acc.prompt_tokens, Some(7));
    }

    #[test]
    fn accumulate_threads_all_five_fields() {
        let mut acc = TokenUsage {
            prompt_tokens: Some(1),
            completion_tokens: Some(2),
            cache_read_tokens: Some(3),
            cache_write_tokens: Some(4),
            reasoning_tokens: Some(5),
        };
        acc.accumulate(&TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            cache_read_tokens: Some(30),
            cache_write_tokens: Some(40),
            reasoning_tokens: Some(50),
        });
        assert_eq!(acc.prompt_tokens, Some(11));
        assert_eq!(acc.completion_tokens, Some(22));
        assert_eq!(acc.cache_read_tokens, Some(33));
        assert_eq!(acc.cache_write_tokens, Some(44));
        assert_eq!(acc.reasoning_tokens, Some(55));
    }

    // ---- TokenUsage serde ----

    #[test]
    fn token_usage_omits_none_fields_on_serialize() {
        let u = TokenUsage {
            prompt_tokens: Some(5),
            ..Default::default()
        };
        let v = serde_json::to_value(u).unwrap();
        assert_eq!(v, json!({"prompt_tokens": 5}));
    }

    #[test]
    fn token_usage_deserialize_defaults_missing_fields_to_none() {
        let u: TokenUsage = serde_json::from_value(json!({})).unwrap();
        assert!(u.prompt_tokens.is_none());
        assert!(u.completion_tokens.is_none());
    }

    // ---- TurnMeta ----

    #[test]
    fn turn_meta_tool_elapsed_defaults_to_empty_on_deserialize() {
        let m: TurnMeta = serde_json::from_value(json!({
            "elapsed_ms": 123,
            "avg_tps": null,
            "interrupted": false
        }))
        .unwrap();
        assert_eq!(m.elapsed_ms, 123);
        assert!(m.tool_elapsed.is_empty());
    }

    // ---- ModelConfigOverrides ----

    #[test]
    fn model_config_overrides_skips_none_fields() {
        let o = ModelConfigOverrides {
            temperature: Some(0.5),
            ..Default::default()
        };
        let v = serde_json::to_value(o).unwrap();
        assert_eq!(v, json!({"temperature": 0.5}));
    }

    #[test]
    fn model_config_overrides_defaults_on_empty_object() {
        let o: ModelConfigOverrides = serde_json::from_value(json!({})).unwrap();
        assert!(o.temperature.is_none());
        assert!(o.top_p.is_none());
        assert!(o.top_k.is_none());
        assert!(o.min_p.is_none());
        assert!(o.repeat_penalty.is_none());
    }

    // ---- RuleSetOverride ----

    #[test]
    fn rule_set_override_defaults_all_lists_to_empty() {
        let r: RuleSetOverride = serde_json::from_value(json!({})).unwrap();
        assert!(r.allow.is_empty());
        assert!(r.ask.is_empty());
        assert!(r.deny.is_empty());
    }

    // ---- PermissionOverrides ----

    #[test]
    fn permission_overrides_omits_empty_subcommands_map() {
        let p = PermissionOverrides::default();
        let v = serde_json::to_value(p).unwrap();
        assert!(v.get("subcommands").is_none());
        assert!(v.get("tools").is_none());
    }

    #[test]
    fn permission_overrides_includes_non_empty_subcommands() {
        let mut p = PermissionOverrides::default();
        p.subcommands.insert(
            "bash".into(),
            RuleSetOverride {
                allow: vec!["ls".into()],
                ..Default::default()
            },
        );
        let v = serde_json::to_value(p).unwrap();
        assert_eq!(v["subcommands"]["bash"]["allow"][0], json!("ls"));
    }
}
