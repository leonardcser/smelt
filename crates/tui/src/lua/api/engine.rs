//! `smelt.engine` — cancel, `ask`, and `submit_command` for Lua-rendered turns.

use crate::lua::{LuaHandle, LuaShared};
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::lua::api::reasoning::LuaReasoningEffort;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::lua_type::LuaCallback;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// One message in a `smelt.engine.ask` conversation.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskMessage")]
pub struct LuaAskMessage {
    /// Either `"user"` or `"assistant"`. Other roles are silently dropped.
    pub role: String,
    /// Message body as plain text.
    pub content: String,
}

/// Subcommand rule override accepted inside `CommandOverrides`. Mirrors
/// the front-matter `{ allow?, ask?, deny? }` shape.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.engine.RuleOverride")]
pub struct LuaRuleOverride {
    /// Patterns that auto-allow.
    #[lua(default)]
    pub allow: Vec<String>,
    /// Patterns that always prompt.
    #[lua(default)]
    pub ask: Vec<String>,
    /// Patterns that auto-deny.
    #[lua(default)]
    pub deny: Vec<String>,
}

impl From<LuaRuleOverride> for smelt_core::custom_commands::RuleOverride {
    fn from(r: LuaRuleOverride) -> Self {
        Self {
            allow: r.allow,
            ask: r.ask,
            deny: r.deny,
        }
    }
}

/// Front-matter override block accepted by
/// `smelt.engine.submit_command`. Mirrors what plugin commands set in
/// their markdown header. Tool-name keys (e.g. `bash`, `edit`) become
/// per-subcommand pattern buckets.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.engine.CommandOverrides")]
pub struct LuaCommandOverrides {
    /// Override the command description shown in `/help`.
    pub description: Option<String>,
    /// Force a specific provider for this command's turn.
    pub provider: Option<String>,
    /// Force a specific model id.
    pub model: Option<String>,
    /// Sampling temperature override.
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff override.
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff override.
    pub top_k: Option<u32>,
    /// Minimum-probability cutoff override.
    pub min_p: Option<f64>,
    /// Repeat-penalty override.
    pub repeat_penalty: Option<f64>,
    /// Reasoning-effort override; one of the `smelt.reasoning.Effort` strings.
    pub reasoning_effort: Option<String>,
    /// Per-tool `allow`/`ask`/`deny` patterns for the duration of the turn.
    pub tools: Option<LuaRuleOverride>,
    /// Per-subcommand pattern buckets keyed by tool name.
    #[lua(rest)]
    pub subcommands: std::collections::HashMap<String, LuaRuleOverride>,
}

impl From<LuaCommandOverrides> for smelt_core::custom_commands::CommandOverrides {
    fn from(o: LuaCommandOverrides) -> Self {
        Self {
            provider: o.provider,
            model: o.model,
            temperature: o.temperature,
            top_p: o.top_p,
            top_k: o.top_k,
            min_p: o.min_p,
            repeat_penalty: o.repeat_penalty,
            reasoning_effort: o.reasoning_effort,
            tools: o.tools.map(Into::into),
            subcommands: o
                .subcommands
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

/// Structured JSON-output specification for `smelt.engine.ask`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskResponseFormat")]
pub struct LuaAskResponseFormat {
    /// Schema name (used by some providers as the response_format label).
    pub name: String,
    /// JSON schema describing the expected response shape. Accepts a Lua
    /// table that round-trips through `lua_table_to_json` into a JSON value.
    pub schema: mlua::Table,
}

/// Typed error table delivered to `on_response` when the underlying
/// provider call fails. `kind` is a stable string the caller can branch
/// on; `message` is a human-readable single-line description. The
/// struct exists purely as a doc / LuaCATS schema target — the actual
/// table is built in `LuaRuntime::fire_ask_callback` because it lands
/// on a callback path that bypasses `FromLua` decoding.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskError")]
pub struct LuaAskErrorTable {
    /// One of `"network" | "rate_limited" | "quota" | "invalid_response" | "context_window" | "cancelled" | "other"`.
    pub kind: String,
    /// Human-readable single-line description (newlines collapsed to spaces).
    pub message: String,
}

/// Spec for `smelt.engine.ask`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskSpec")]
pub struct LuaAskSpec {
    /// System prompt sent before the conversation.
    pub system: String,
    /// Prior turns. Each message is `{ role = "user"|"assistant", content = "..." }`.
    #[lua(default)]
    pub messages: Vec<LuaAskMessage>,
    /// Single-shot question appended as a final user message after `messages`.
    pub question: Option<String>,
    /// Model reference (`"provider/model"` or a bare name resolved against
    /// the configured providers). When `nil`, falls back to the primary model.
    pub model: Option<String>,
    /// JSON-schema response constraint.
    pub response_format: Option<LuaAskResponseFormat>,
    /// Reasoning effort for the request; defaults to `"off"`.
    pub reasoning_effort: Option<LuaReasoningEffort>,
    /// When `true`, the engine wraps the call in a trim-on-overflow loop:
    /// on context-window errors it drops the oldest message (preserving
    /// the system prompt at index 0) and retries, up to `max_trims` times.
    /// Defaults to `false`.
    #[lua(default)]
    pub trim_on_overflow: bool,
    /// Maximum number of trim-and-retry passes; only consulted when
    /// `trim_on_overflow` is true. Defaults to 20.
    pub max_trims: Option<u32>,
    /// Fires once with `(content, err)`. On success `err` is `nil` and
    /// `content` carries the assistant text. On failure `err` is a
    /// `smelt.engine.AskError` table and `content` is `""`.
    pub on_response: Option<LuaCallback<(String, Option<LuaAskErrorTable>), ()>>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "engine",
        "LLM engine control — cancel, ask, submit commands, and request tool approval. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "cancel",
        "Cancel the in-flight turn. In-flight background `smelt.engine.ask` requests are unaffected and will still fire their callbacks; plugins owning `smelt.spinner.busy` tokens are responsible for releasing them.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                app.core.engine.send(protocol::UiCommand::Cancel);
            });
            Ok(())
        },
    )?;
    m.fn_(
        "is_running",
        "Return `true` if an agent turn is currently in flight (a request is being streamed or a tool is executing).",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.agent.is_some()).unwrap_or(false)),
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "on_context_limit",
            "Register a recovery hook the engine calls when a provider returns a context-window error mid-turn. `fn` is called as `fn(messages, reply)` — `messages` is the conversation up to that point (excluding the system prompt at index 1) and `reply` is a Lua function the hook MUST call exactly once with either a shorter messages array (engine swaps it in and retries the turn) or `nil` (engine aborts with the existing TurnError). The first registered hook to call `reply` wins; later hooks are ignored. Returns an `off()` that removes this hook. Bundled `compact.lua` registers a hook that runs the standard summarization flow.",
            &["fn"],
            move |lua, func: mlua::Function| -> LuaResult<mlua::Function> {
                let id = s.hooks.context_limit.register(lua, func, "")?;
                s.hooks.context_limit.off_for(lua, id)
            },
        )?;
    }
    m.fn_(
        "reload",
        "Re-evaluate every Lua surface: clears every command, keymap, statusline source, tool, hook, timer, and cell subscriber, wipes non-stdlib `package.loaded` entries, then re-runs the bundled autoload modules, `init.lua`, global plugins, and `.smelt/init.lua` + `.smelt/plugins/*`. `early.lua` is intentionally skipped — its CLI-flag and `smelt.builtins.disable` effects are startup-only.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if app.agent.is_some() {
                    app.notify_error("cannot reload while agent is working".into());
                    return;
                }
                if app.ui.active_modal().is_some() {
                    // Modal overlays (dialogs, confirm pickers) hold Lua
                    // callbacks; clearing them mid-flight would invalidate
                    // the user's pending action.
                    app.notify_error("cannot reload while a modal dialog is open".into());
                    return;
                }
                app.reload_lua();
            });
            Ok(())
        },
    )?;

    m.fn_(
        "submit_command",
        "Start an agent turn from a Lua-defined custom command (`/name`). Notifies and no-ops if an agent is already running. See [`smelt.engine.CommandOverrides`](types.md#smeltenginecommandoverrides) for the override shape.",
        &["name", "body", "overrides"],
        |_,
         (name, body, overrides): (String, String, Option<LuaCommandOverrides>)|
         -> LuaResult<()> {
            let parsed = overrides.map(Into::into).unwrap_or_default();
            crate::lua::with_app(|app| {
                if app.agent.is_some() {
                    app.notify_error(format!("cannot run /{name} while agent is working"));
                    return;
                }
                let cmd = smelt_core::custom_commands::CustomCommand {
                    name,
                    body,
                    overrides: parsed,
                };
                let turn = app.begin_custom_command_turn(cmd);
                app.agent = Some(turn);
            });
            Ok(())
        },
    )?;

    // smelt.engine.ask({system, messages?, question?, model?, response_format?, reasoning_effort?, trim_on_overflow?, max_trims?, on_response})
    {
        let s = shared.clone();
        m.fn_(
            "ask",
            "Run an out-of-band LLM request without touching the main turn. `spec.model` selects an alternate model (defaults to the primary), `spec.response_format` enforces a JSON schema, `spec.reasoning_effort` controls effort (defaults to `\"off\"`), `spec.trim_on_overflow` wraps the call in a trim-and-retry loop for context-window errors (`spec.max_trims` caps the number of drops, default 20). `spec.on_response` fires once with `(content, err)`; returns the request id.",
            &["spec"],
            move |lua, spec: LuaAskSpec| -> LuaResult<u64> {
                let mut messages: Vec<protocol::Message> = spec
                    .messages
                    .into_iter()
                    .filter_map(|m| match m.role.as_str() {
                        "user" => Some(protocol::Message::user(protocol::Content::text(
                            &m.content,
                        ))),
                        "assistant" => Some(protocol::Message::assistant(
                            Some(protocol::Content::text(&m.content)),
                            None,
                            None,
                        )),
                        _ => None,
                    })
                    .collect();
                if let Some(q) = spec.question {
                    messages.push(protocol::Message::user(protocol::Content::text(&q)));
                }

                let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                if let Some(cb) = spec.on_response {
                    let handle = LuaHandle::from_func(lua, cb.into_inner())?;
                    if let Ok(mut cbs) = s.callbacks.lock() {
                        cbs.insert(id, handle);
                    }
                }

                let system = spec.system;
                let response_format = spec.response_format.map(|f| protocol::AskResponseFormat {
                    name: f.name,
                    schema: smelt_core::lua::api::lua_table_to_json(lua, &f.schema),
                });
                let reasoning_effort = spec
                    .reasoning_effort
                    .map(Into::into)
                    .unwrap_or(protocol::ReasoningEffort::Off);
                let trim_on_overflow = spec.trim_on_overflow;
                let max_trims = spec.max_trims.unwrap_or(20);
                let model_ref = spec.model;
                crate::lua::with_app(|app| {
                    let model = model_ref.and_then(|r| resolve_model_for_ask(app, &r));
                    app.core.engine.send(protocol::UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        model,
                        response_format,
                        reasoning_effort,
                        trim_on_overflow,
                        max_trims,
                    })
                });
                Ok(id)
            },
        )?;
    }

    Ok(())
}

/// Resolve a Lua-provided model reference into an `AskModel` carrying api
/// base / key / provider type. Notifies on error and returns `None`.
fn resolve_model_for_ask(
    app: &mut crate::app::TuiApp,
    reference: &str,
) -> Option<protocol::AskModel> {
    let resolved =
        match smelt_core::config::resolve_model_ref(&app.core.config.available_models, reference) {
            Ok(m) => m.clone(),
            Err(err) => {
                app.notify_error(format!("smelt.engine: {err}"));
                return None;
            }
        };
    let api_key = app
        .resolve_api_key_for_env(&resolved.api_key_env)
        .unwrap_or_default();
    Some(protocol::AskModel {
        model: resolved.model_name,
        api_base: resolved.api_base,
        api_key,
        provider_type: resolved.provider_type,
    })
}
