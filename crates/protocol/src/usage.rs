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
