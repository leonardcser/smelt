//! Custom-command turn data. Lua populates these structs via
//! `smelt.engine.submit_command`; `TuiApp::begin_custom_command_turn` consumes them.

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct RuleOverride {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CommandOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub reasoning_effort: Option<String>,
    /// Tool-name decision overrides (allow/ask/deny lists of names).
    pub tools: Option<RuleOverride>,
    /// Per-tool subpattern overrides keyed by tool name (`bash`,
    /// `web_fetch`, `mcp`, or any tool that registers a bucket).
    pub subcommands: HashMap<String, RuleOverride>,
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub display: String,
    pub body: String,
    pub overrides: CommandOverrides,
}
