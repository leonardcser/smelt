//! Custom-command turn data. The user-facing flow (file scanning,
//! frontmatter parsing, exec block evaluation, slash-command
//! registration) lives in `runtime/lua/smelt/plugins/custom_commands.lua`
//! and the built-in `/reflect` / `/simplify` plugins; the Lua side
//! hands the rendered body and parsed overrides to
//! `smelt.engine.submit_command`, which builds these structs and calls
//! `TuiApp::begin_custom_command_turn`.

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
    pub tools: Option<RuleOverride>,
    pub bash: Option<RuleOverride>,
    pub web_fetch: Option<RuleOverride>,
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub body: String,
    pub overrides: CommandOverrides,
}
