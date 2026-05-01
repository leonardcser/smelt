//! Custom-command turn data. The user-facing flow (file scanning,
//! frontmatter parsing, exec block evaluation, slash-command
//! registration) lives in `runtime/lua/smelt/plugins/custom_commands.lua`
//! and the built-in `/reflect` / `/simplify` plugins; the Lua side
//! hands the rendered body and parsed overrides to
//! `smelt.engine.submit_command`, which builds these structs and calls
//! `TuiApp::begin_custom_command_turn`.

#[derive(Debug, Clone, Default)]
pub(crate) struct RuleOverride {
    pub(crate) allow: Vec<String>,
    pub(crate) ask: Vec<String>,
    pub(crate) deny: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CommandOverrides {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) top_k: Option<u32>,
    pub(crate) min_p: Option<f64>,
    pub(crate) repeat_penalty: Option<f64>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tools: Option<RuleOverride>,
    pub(crate) bash: Option<RuleOverride>,
    pub(crate) web_fetch: Option<RuleOverride>,
}

#[derive(Debug, Clone)]
pub(crate) struct CustomCommand {
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) overrides: CommandOverrides,
}
