//! `smelt.provider` - config-time provider and model registration.

use mlua::prelude::*;
use std::sync::Arc;

use crate::config::{ModelConfig, ProviderConfig};
use crate::lua::doc::Tier;
use crate::lua::lua_type::{LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaShared;
use lua_doc_derive::LuaOpts;

/// One model entry in a provider's `models` list. Plugin authors can
/// pass either a bare model id string or a full table - the wrapper
/// handles both forms transparently.
#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.provider.Model")]
pub struct LuaProviderModel {
    /// Model id as it appears in API requests.
    pub name: Option<String>,
    /// Default sampling temperature.
    pub temperature: Option<f64>,
    /// Default nucleus-sampling cutoff.
    pub top_p: Option<f64>,
    /// Default top-k sampling cutoff.
    pub top_k: Option<u32>,
    /// Default minimum-probability cutoff.
    pub min_p: Option<f64>,
    /// Default repeat penalty.
    pub repeat_penalty: Option<f64>,
    /// Whether the model supports tool calls.
    pub tool_calling: Option<bool>,
    /// Cost per 1M input tokens in USD.
    pub input_cost: Option<f64>,
    /// Cost per 1M output tokens in USD.
    pub output_cost: Option<f64>,
    /// Cost per 1M cache-read tokens in USD.
    pub cache_read_cost: Option<f64>,
    /// Cost per 1M cache-write tokens in USD.
    pub cache_write_cost: Option<f64>,
    /// Maximum output tokens for this model. Defaults to the model's own limit, falling back to 4096 if unknown.
    pub max_tokens: Option<u32>,
    /// Per-level token budgets for budget-based thinking.
    pub thinking_budgets: Option<LuaThinkingBudgets>,
    /// Total context window, in tokens.
    pub context_window: Option<u32>,
    /// Whether this model supports reasoning/thinking parameters.
    pub supports_reasoning: Option<bool>,
    /// Native reasoning labels for the picker and cycling, for example { "off", "low", "medium", "xhigh" }. Set supports_reasoning = true to enable request parameters. For OpenAI-compatible models, off sends reasoning_effort = "none".
    pub supported_reasoning_efforts: Option<Vec<String>>,
    /// Fallback when the selected effort is unsupported. Must be in supported_reasoning_efforts when supplied.
    pub default_reasoning_effort: Option<String>,
    /// Whether this model supports accelerated inference.
    pub supports_fast_mode: Option<bool>,
    /// Input modalities supported by this model, for example { "text", "image", "pdf" }.
    pub input_modalities: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone)]
pub struct LuaThinkingBudgets {
    pub low: u32,
    pub medium: u32,
    pub high: u32,
    pub max: u32,
}

impl From<LuaThinkingBudgets> for protocol::ThinkingBudgets {
    fn from(t: LuaThinkingBudgets) -> Self {
        Self {
            low: t.low,
            medium: t.medium,
            high: t.high,
            max: t.max,
        }
    }
}

impl crate::lua::lua_type::LuaType for LuaThinkingBudgets {
    fn lua_type() -> String {
        "table".into()
    }
}

impl mlua::FromLua for LuaThinkingBudgets {
    fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> mlua::Result<Self> {
        let t = match value {
            mlua::Value::Table(t) => t,
            other => {
                return Err(mlua::Error::external(format!(
                    "thinking_budgets must be a table, got {}",
                    other.type_name()
                )))
            }
        };
        Ok(Self {
            low: t.get("low").unwrap_or(2048),
            medium: t.get("medium").unwrap_or(8192),
            high: t.get("high").unwrap_or(16384),
            max: t.get("max").unwrap_or(16384),
        })
    }
}

/// Wrapper that accepts either a `string` model id or a full
/// [`LuaProviderModel`] table. The derive emits FromLua expecting a
/// table only; we hand-roll the union here.
#[derive(Debug)]
pub struct LuaModelEntry(pub ModelConfig);

impl FromLua for LuaModelEntry {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::String(s) => Ok(Self(ModelConfig {
                name: Some(s.to_string_lossy().to_string()),
                ..Default::default()
            })),
            mlua::Value::Table(_) => {
                let m: LuaProviderModel = FromLua::from_lua(value, lua)?;
                let parse_effort = |label: String| {
                    protocol::ReasoningEffort::parse(&label)
                        .ok_or_else(|| mlua::Error::external("reasoning effort must not be empty"))
                };
                let supported_reasoning_efforts = m
                    .supported_reasoning_efforts
                    .map(|labels| {
                        labels
                            .into_iter()
                            .map(parse_effort)
                            .collect::<LuaResult<Vec<_>>>()
                    })
                    .transpose()?;
                let default_reasoning_effort =
                    m.default_reasoning_effort.map(parse_effort).transpose()?;
                if let (Some(efforts), Some(default)) =
                    (&supported_reasoning_efforts, &default_reasoning_effort)
                {
                    if !efforts.contains(default) {
                        return Err(mlua::Error::external(
                            "default_reasoning_effort must be in supported_reasoning_efforts",
                        ));
                    }
                }
                Ok(Self(ModelConfig {
                    name: m.name,
                    temperature: m.temperature,
                    top_p: m.top_p,
                    top_k: m.top_k,
                    min_p: m.min_p,
                    repeat_penalty: m.repeat_penalty,
                    tool_calling: m.tool_calling,
                    input_cost: m.input_cost,
                    output_cost: m.output_cost,
                    cache_read_cost: m.cache_read_cost,
                    cache_write_cost: m.cache_write_cost,
                    max_tokens: m.max_tokens,
                    thinking_budgets: m.thinking_budgets.map(Into::into),
                    context_window: m.context_window,
                    supports_reasoning: m.supports_reasoning,
                    supported_reasoning_efforts,
                    default_reasoning_effort,
                    supports_fast_mode: m.supports_fast_mode,
                    input_modalities: m.input_modalities,
                }))
            }
            other => Err(mlua::Error::external(format!(
                "smelt.provider.register: each model entry must be a string or table, got {}",
                other.type_name()
            ))),
        }
    }
}

impl LuaType for LuaModelEntry {
    fn lua_type() -> String {
        // Trigger the LuaProviderModel class registration so the
        // sibling type page picks it up even though we never type
        // `LuaProviderModel` directly in a sig.
        let _ = <LuaProviderModel as LuaType>::lua_type();
        "string|smelt.provider.Model".into()
    }
}

impl LuaTypeTuple for LuaModelEntry {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

/// Spec accepted by `smelt.provider.register`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.provider.Config")]
pub struct LuaProviderConfig {
    /// Provider kind tag (`"openai"`, `"anthropic"`, etc.).
    #[lua(rename = "type", default)]
    pub provider_type: String,
    /// Base URL the engine talks to.
    #[lua(default)]
    pub api_base: String,
    /// Environment variable that holds the bearer token.
    pub api_key_env: Option<String>,
    /// Models offered by this provider.
    #[lua(default)]
    pub models: Vec<LuaModelEntry>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "provider",
        "List built-in model providers and register custom ones. Headless-safe.",
        Tier::Host,
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "register",
            "Declare a provider named `name`. Re-registering replaces the previous entry of the same name. Returns a `Reg` whose `:remove()` drops the provider.",
            &["name", "cfg"],
            move |_lua, (name, cfg): (String, LuaProviderConfig)| -> LuaResult<LuaReg> {
                let provider = ProviderConfig {
                    name: Some(name.clone()),
                    provider_type: Some(cfg.provider_type),
                    api_base: Some(cfg.api_base),
                    api_key_env: cfg.api_key_env,
                    models: cfg.models.into_iter().map(|m| m.0).collect(),
                };
                let mut providers = shared.providers.lock().unwrap_or_else(|e| e.into_inner());
                providers.retain(|p| p.name.as_deref() != Some(&name));
                providers.push(provider);
                drop(providers);
                let shared_for_reg = Arc::clone(&shared);
                Ok(LuaReg::new(move || {
                    let mut providers = shared_for_reg
                        .providers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let before = providers.len();
                    providers.retain(|p| p.name.as_deref() != Some(&name));
                    providers.len() != before
                }))
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        m.fn_(
            "list",
            "Return every registered provider as an array of tables. Each entry has `name`, `type`, `api_base`, `api_key_env`, and a `models` array.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                let providers = shared.providers.lock().unwrap_or_else(|e| e.into_inner());
                let out = lua.create_table()?;
                for (i, p) in providers.iter().enumerate() {
                    let t = lua.create_table()?;
                    t.set("name", p.name.clone())?;
                    t.set("type", p.provider_type.clone())?;
                    t.set("api_base", p.api_base.clone())?;
                    t.set("api_key_env", p.api_key_env.clone())?;
                    let models = lua.create_table()?;
                    for (j, m) in p.models.iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("name", m.name.clone())?;
                        row.set("temperature", m.temperature)?;
                        row.set("top_p", m.top_p)?;
                        row.set("top_k", m.top_k)?;
                        row.set("min_p", m.min_p)?;
                        row.set("repeat_penalty", m.repeat_penalty)?;
                        row.set("tool_calling", m.tool_calling)?;
                        row.set("input_cost", m.input_cost)?;
                        row.set("output_cost", m.output_cost)?;
                        row.set("cache_read_cost", m.cache_read_cost)?;
                        row.set("cache_write_cost", m.cache_write_cost)?;
                        row.set("max_tokens", m.max_tokens)?;
                        row.set("supports_reasoning", m.supports_reasoning)?;
                        if let Some(efforts) = &m.supported_reasoning_efforts {
                            row.set("supported_reasoning_efforts", efforts.iter().map(|effort| effort.label()).collect::<Vec<_>>())?;
                        }
                        row.set("default_reasoning_effort", m.default_reasoning_effort.as_ref().map(|effort| effort.label()))?;
                        if let Some(tb) = &m.thinking_budgets {
                            let t = lua.create_table()?;
                            t.set("low", tb.low)?;
                            t.set("medium", tb.medium)?;
                            t.set("high", tb.high)?;
                            t.set("max", tb.max)?;
                            row.set("thinking_budgets", t)?;
                        }
                        models.set(j + 1, row)?;
                    }
                    t.set("models", models)?;
                    out.set(i + 1, t)?;
                }
                Ok(out)
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "middleware",
            "Register provider middleware. `mw` is a table of \
`{ on_response = fn }`:\n\n\
- `on_response(message)` - runs after the assistant message is fully assembled but before it's appended to history. `message` is the same `{ role = \"assistant\", content?, tool_calls? }` shape used everywhere else. Return a replacement table to mutate it; any other return leaves it as-is.\n\n\
Hooks fire in registration order. Each hook sees the previous hook's replacement. Returns a `Reg` whose `:remove()` drops this middleware.\n\n\
For streaming observation use `smelt.events.on(\"stream_delta\", ...)` - synchronous mutation of mid-stream tokens isn't safe because the parser owns the partial state.",
            &["mw"],
            move |lua, mw: mlua::Table| -> LuaResult<LuaReg> {
                let on_response: mlua::Function = mw.get("on_response").map_err(|_| {
                    LuaError::RuntimeError(
                        "provider.middleware: on_response function is required".to_string(),
                    )
                })?;
                let registry = Arc::clone(&s.hooks.provider_response);
                let id = registry.register(lua, on_response, "")?;
                Ok(registry.reg_for(id))
            },
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_reasoning_model_parses_native_levels() {
        let lua = Lua::new();
        let model: LuaModelEntry = lua
            .load(
                r#"return {
            name = "orcarouter/Qwen3.8-27B-Uncensored-NVFP4",
            supports_reasoning = true,
            supported_reasoning_efforts = { "off", "low", "medium", "xhigh", "custom" },
            default_reasoning_effort = "xhigh",
        }"#,
            )
            .eval()
            .unwrap();
        assert_eq!(model.0.supports_reasoning, Some(true));
        assert_eq!(
            model.0.default_reasoning_effort,
            Some(protocol::ReasoningEffort::XHigh)
        );
        assert_eq!(
            model.0.supported_reasoning_efforts.unwrap(),
            vec![
                protocol::ReasoningEffort::Off,
                protocol::ReasoningEffort::Low,
                protocol::ReasoningEffort::Medium,
                protocol::ReasoningEffort::XHigh,
                protocol::ReasoningEffort::Custom("custom".into()),
            ]
        );
    }

    #[test]
    fn custom_reasoning_model_rejects_empty_labels_and_invalid_default() {
        let lua = Lua::new();
        for source in [
            r#"return { supported_reasoning_efforts = { " " } }"#,
            r#"return { default_reasoning_effort = " " }"#,
            r#"return { supported_reasoning_efforts = { "off", "low" }, default_reasoning_effort = "high" }"#,
        ] {
            assert!(
                lua.load(source).eval::<LuaModelEntry>().is_err(),
                "{source}"
            );
        }
    }
}
