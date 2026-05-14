//! `smelt.engine` — cancel, compact, `ask`, and `submit_command` for Lua-rendered turns.

use crate::lua::{LuaHandle, LuaShared};
use lua_doc_derive::{lua_module, LuaAlias, LuaOpts};
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;
use smelt_core::lua::lua_type::LuaCallback;
use std::sync::Arc;

/// Auxiliary task tag accepted by `smelt.engine.ask`. Routes the
/// request to a dedicated auxiliary model when one is configured.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.engine.AskTask", mirror = "protocol::AuxiliaryTask")]
pub enum LuaAskTask {
    Title,
    Prediction,
    Compaction,
    Btw,
}

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
/// [`smelt.engine.submit_command`]. Mirrors what plugin commands set in
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
    /// Routing tag; defaults to `"btw"`.
    pub task: Option<LuaAskTask>,
    /// Fires once with the assistant's reply string.
    pub on_response: Option<LuaCallback<String, ()>>,
}

#[lua_module(
    name = "smelt.engine",
    doc = "LLM engine control — cancel, ask, submit commands, and request tool approval. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let engine_tbl = lua.create_table()?;
    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "cancel",
        "Cancel the in-flight turn. If a compaction is running, bumps the compact epoch and marks the working state interrupted; otherwise sends `Cancel` to the engine.",
        &[],
        lua,
        |_, ()|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                if app.working.is_compacting() {
                    app.compact_epoch += 1;
                    app.working.finish(
                        smelt_core::working::TurnOutcome::Interrupted,
                        app.core.clock.instant_now(),
                    );
                    app.notify("compaction cancelled".into());
                } else {
                    app.core.engine.send(protocol::UiCommand::Cancel);
                }
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "is_running",
        "Return `true` if an agent turn is currently in flight (a request is being streamed or a tool is executing).",
        &[],
        lua,
        |_, ()| Ok(crate::lua::try_with_app(|app| app.agent.is_some()).unwrap_or(false)),
    )?;
    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "is_compacting",
        "Return `true` while a transcript compaction is running.",
        &[],
        lua,
        |_, ()| Ok(crate::lua::try_with_app(|app| app.working.is_compacting()).unwrap_or(false)),
    )?;
    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "reload",
        "Re-evaluate every Lua surface: clears every command, keymap, statusline source, tool, hook, timer, and cell subscriber, wipes non-stdlib `package.loaded` entries, then re-runs the bundled autoload modules, `init.lua`, global plugins, and `.smelt/init.lua` + `.smelt/plugins/*`. `early.lua` is intentionally skipped — its CLI-flag and `smelt.builtins.disable` effects are startup-only.",
        &[],
        lua,
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

    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "compact",
        "Start a transcript compaction with optional extra `instructions` for the summarizer. Notifies and no-ops if compaction is unavailable in the current state.",
        &["instructions"],
        lua,
        |_, instructions: Option<String>|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.compact_or_notify(instructions));
            Ok(())
        },
    )?;

    register_ui_fn(
        &engine_tbl,
        "smelt.engine",
        "submit_command",
        "Start an agent turn from a Lua-defined custom command (`/name`). Notifies and no-ops if an agent is already running. See [`smelt.engine.CommandOverrides`](types.md#smeltenginecommandoverrides) for the override shape.",
        &["name", "body", "overrides"],
        lua,
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

    // smelt.engine.ask({system, messages?, question?, task?, on_response})
    {
        let s = shared.clone();
        register_ui_fn(
            &engine_tbl,
            "smelt.engine",
            "ask",
            "Run an out-of-band auxiliary LLM request (title / prediction / compaction / btw) without touching the main turn. `spec.on_response` fires once with the assistant's reply; returns the request id.",
            &["spec"],
            lua,
            move |lua, spec: LuaAskSpec| -> LuaResult<u64> {
                let task = spec.task.map(Into::into).unwrap_or_default();

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
                crate::lua::with_app(|app| {
                    app.core.engine.send(protocol::UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        task,
                    })
                });
                Ok(id)
            },
        )?;
    }

    smelt.set("engine", engine_tbl)?;
    Ok(())
}
