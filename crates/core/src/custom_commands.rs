//! Custom-command turn data. Lua populates these structs via
//! `smelt.engine.submit_command`; `TuiApp::begin_custom_command_turn` consumes them.

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
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub display: String,
    pub body: String,
    pub overrides: CommandOverrides,
}
