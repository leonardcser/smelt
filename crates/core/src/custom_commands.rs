//! Custom-command turn data. The user-facing flow (file scanning,
//! frontmatter parsing, exec block evaluation, slash-command
//! registration) lives in `runtime/lua/smelt/plugins/custom_commands.lua`
//! and the built-in `/reflect` / `/simplify` plugins; the Lua side
//! hands the rendered body and parsed overrides to
//! `smelt.engine.submit_command`, which builds these structs and calls
//! `TuiApp::begin_custom_command_turn`.

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
    pub body: String,
    pub overrides: CommandOverrides,
}
