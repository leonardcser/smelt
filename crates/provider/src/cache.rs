use crate::ToolDefinition;

/// Per-request prompt-cache strategy. Anthropic uses `cache_control`
/// markers; OpenAI-family providers use a `prompt_cache_key` routing hint
/// (session-scoped) that improves hit rate under load. The key is a
/// performance optimization, not telemetry - without it OpenAI's prefix
/// cache works opportunistically; with it the request routes to a shard
/// that already saw the prefix.
#[derive(Clone, Debug, Default)]
pub struct CacheConfig {
    pub anthropic_markers: bool,
    /// Use the 1-hour TTL instead of 5 minutes (Anthropic only).
    pub ttl_long: bool,
    /// Session-stable identifier sent to OpenAI / Codex as
    /// `prompt_cache_key`. Should be the same for every request in a
    /// session and differ across sessions. Clamped to 64 chars.
    pub prompt_cache_key: Option<String>,
}

/// OpenAI accepts up to 256 chars but recommends shorter; pi-mono uses
/// 64. Session ids in smelt are SHA256 hex (64 chars) so most keys pass
/// through unchanged. Clamping is by char count (not bytes) so non-ASCII
/// keys never split mid-codepoint.
pub fn clamp_prompt_cache_key(key: &str) -> String {
    key.chars().take(64).collect()
}

/// Sort tool definitions by name in place. The cached prompt prefix
/// includes the tools section; any registration-order drift would
/// silently invalidate the cache. Every caller that hands tools to
/// `Provider::chat` MUST sort first; this is the canonical helper.
pub fn sort_tools_for_cache_stability(tools: &mut [ToolDefinition]) {
    tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionSchema;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition::new(FunctionSchema {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        })
    }

    #[test]
    fn clamp_prompt_cache_key_keeps_first_64_chars() {
        let key = "x".repeat(80);
        assert_eq!(clamp_prompt_cache_key(&key), "x".repeat(64));
    }

    #[test]
    fn clamp_prompt_cache_key_respects_char_boundaries() {
        let key = "é".repeat(80);
        let clamped = clamp_prompt_cache_key(&key);
        assert_eq!(clamped.chars().count(), 64);
        assert!(clamped.is_char_boundary(clamped.len()));
    }

    #[test]
    fn sort_tools_for_cache_stability_sorts_by_name() {
        let mut tools = vec![tool("z"), tool("a"), tool("m")];
        sort_tools_for_cache_stability(&mut tools);
        let names: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names, ["a", "m", "z"]);
    }
}
