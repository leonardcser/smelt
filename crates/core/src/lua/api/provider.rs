//! `smelt.provider` — config-time provider and model registration.

use mlua::prelude::*;
use std::sync::Arc;

use crate::config::{ModelConfig, ProviderConfig};
use crate::lua::doc::Tier;
use crate::lua::hooks::composite_off;
use crate::lua::lua_type::{LuaCallback, LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use lua_doc_derive::LuaOpts;

/// One model entry in a provider's `models` list. Plugin authors can
/// pass either a bare model id string or a full table — the wrapper
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
    /// Cost per input token in USD.
    pub input_cost: Option<f64>,
    /// Cost per output token in USD.
    pub output_cost: Option<f64>,
    /// Cost per cache-read token in USD.
    pub cache_read_cost: Option<f64>,
    /// Cost per cache-write token in USD.
    pub cache_write_cost: Option<f64>,
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
    let m = LuaMod::under(
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
            "Declare a provider named `name`. Re-registering replaces the previous entry of the same name.",
            &["name", "cfg"],
            move |_lua, (name, cfg): (String, LuaProviderConfig)| -> LuaResult<()> {
                let provider = ProviderConfig {
                    name: Some(name),
                    provider_type: Some(cfg.provider_type),
                    api_base: Some(cfg.api_base),
                    api_key_env: cfg.api_key_env,
                    models: cfg.models.into_iter().map(|m| m.0).collect(),
                };
                let mut providers = shared.providers.lock().unwrap_or_else(|e| e.into_inner());
                providers.retain(|p| {
                    p.name.as_deref() != Some(&provider.name.clone().unwrap_or_default())
                });
                providers.push(provider);
                Ok(())
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
`{ on_request = fn?, on_response = fn? }`:\n\n\
- `on_request(messages)` — runs just before the engine calls the provider. `messages` is the full conversation history (an array of `{ role, content, tool_calls? }` rows including the system prompt at index 1). Return a replacement array to mutate it; any other return value leaves the history untouched.\n\
- `on_response(message)` — runs after the assistant message is fully assembled but before it's appended to history. `message` is the same `{ role = \"assistant\", content?, tool_calls? }` shape used everywhere else. Return a replacement table to mutate it; any other return leaves it as-is.\n\n\
Hooks fire in registration order. Each hook sees the previous hook's replacement. Returns an `off()` function that removes this middleware.\n\n\
For streaming observation use `smelt.cell(\"stream_delta\"):subscribe( ...)` — synchronous mutation of mid-stream tokens isn't safe because the parser owns the partial state.",
            &["mw"],
            move |lua, mw: mlua::Table| -> LuaResult<LuaCallback<(), bool>> {
                let on_request: Option<mlua::Function> = mw.get("on_request").ok();
                let on_response: Option<mlua::Function> = mw.get("on_response").ok();
                if on_request.is_none() && on_response.is_none() {
                    return Err(LuaError::RuntimeError(
                        "provider.middleware: at least one of on_request/on_response is required"
                            .to_string(),
                    ));
                }
                let mut parts = Vec::with_capacity(2);
                if let Some(f) = on_request {
                    let id = s.hooks.provider_request.register(lua, f, "")?;
                    parts.push((Arc::clone(&s.hooks.provider_request), id));
                }
                if let Some(f) = on_response {
                    let id = s.hooks.provider_response.register(lua, f, "")?;
                    parts.push((Arc::clone(&s.hooks.provider_response), id));
                }
                let off = composite_off(lua, parts)?;
                LuaCallback::<(), bool>::from_lua(mlua::Value::Function(off), lua)
            },
        )?;
    }

    Ok(())
}
