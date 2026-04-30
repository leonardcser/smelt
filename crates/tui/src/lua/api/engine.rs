//! `smelt.engine` bindings — live engine reads (model, busy state,
//! cost, tokens), turn-driver writes (set_model, submit, cancel,
//! compact), the `ask` auxiliary request primitive, and the
//! message-history snapshot. Mode get/set/cycle live under
//! `smelt.mode`; reasoning effort lives under `smelt.reasoning`.

use super::app_read;
use crate::lua::{messages_to_lua, LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let engine_tbl = lua.create_table()?;

    engine_tbl.set("model", app_read!(lua, |app| app.core.config.model.clone()))?;
    engine_tbl.set("is_busy", app_read!(lua, |app| app.agent.is_some()))?;
    engine_tbl.set(
        "cost",
        app_read!(lua, |app| app.core.session.session_cost_usd),
    )?;
    engine_tbl.set(
        "context_tokens",
        app_read!(lua, |app| app.core.session.context_tokens),
    )?;
    engine_tbl.set(
        "context_window",
        app_read!(lua, |app| app.core.config.context_window),
    )?;

    engine_tbl.set(
        "set_model",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| app.apply_model(&v));
            Ok(())
        })?,
    )?;
    // smelt.engine.models() → array of `{key, name, provider}`
    // for the prompt-docked `/model` picker.
    engine_tbl.set(
        "models",
        lua.create_function(|lua, ()| {
            let out = lua.create_table()?;
            if let Some(res) = crate::lua::try_with_app(|app| -> LuaResult<()> {
                for (i, m) in app.core.config.available_models.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("key", m.key.clone())?;
                    entry.set("name", m.model_name.clone())?;
                    entry.set("provider", m.provider_name.clone())?;
                    out.set(i + 1, entry)?;
                }
                Ok(())
            }) {
                res?;
            }
            Ok(out)
        })?,
    )?;
    engine_tbl.set(
        "submit",
        lua.create_function(|_, v: String| {
            crate::lua::with_app(|app| app.queued_messages.push(v));
            Ok(())
        })?,
    )?;
    engine_tbl.set(
        "cancel",
        lua.create_function(|_, ()| {
            crate::lua::with_app(|app| app.core.engine.send(protocol::UiCommand::Cancel));
            Ok(())
        })?,
    )?;
    engine_tbl.set(
        "compact",
        lua.create_function(|_, instructions: Option<String>| {
            crate::lua::with_app(|app| app.compact_or_notify(instructions));
            Ok(())
        })?,
    )?;

    // smelt.engine.submit_builtin_command(name, arg?) — start a
    // custom-command turn from a built-in prompt template (rendered
    // with the current `multi_agent` context, frontmatter overrides
    // applied). Used by Lua plugins for `/reflect` and `/simplify`.
    engine_tbl.set(
        "submit_builtin_command",
        lua.create_function(|_, (name, arg): (String, Option<String>)| {
            crate::lua::with_app(|app| {
                let mut input = format!("/{name}");
                if let Some(a) = arg.as_deref() {
                    let trimmed = a.trim();
                    if !trimmed.is_empty() {
                        input.push(' ');
                        input.push_str(trimmed);
                    }
                }
                let multi = app.core.config.multi_agent;
                let Some(cmd) = crate::builtin_commands::resolve(&input, multi) else {
                    app.notify_error(format!("unknown builtin command: /{name}"));
                    return;
                };
                if app.agent.is_some() {
                    app.notify_error(format!("cannot run /{name} while agent is working"));
                    return;
                }
                let turn = app.begin_custom_command_turn(cmd);
                app.agent = Some(turn);
            });
            Ok(())
        })?,
    )?;

    // smelt.engine.ask({ system, messages?, question?, task?, on_response })
    {
        let s = shared.clone();
        engine_tbl.set(
            "ask",
            lua.create_function(move |lua, spec: mlua::Table| {
                let system: String = spec.get("system")?;
                let task_str: Option<String> = spec.get("task")?;
                let task = match task_str.as_deref() {
                    Some("title") => protocol::AuxiliaryTask::Title,
                    Some("prediction") => protocol::AuxiliaryTask::Prediction,
                    Some("compaction") => protocol::AuxiliaryTask::Compaction,
                    Some("btw") | None => protocol::AuxiliaryTask::Btw,
                    Some(other) => {
                        return Err(mlua::Error::external(format!(
                            "engine.ask: unknown task {other:?}; expected one of title / prediction / compaction / btw"
                        )));
                    }
                };
                let on_response: Option<mlua::Function> = spec.get("on_response")?;

                let mut messages = Vec::new();
                if let Ok(msgs) = spec.get::<mlua::Table>("messages") {
                    for pair in msgs.sequence_values::<mlua::Table>().flatten() {
                        let role: String = pair.get("role")?;
                        let content: String = pair.get("content")?;
                        let msg = match role.as_str() {
                            "user" => protocol::Message::user(protocol::Content::text(&content)),
                            "assistant" => protocol::Message::assistant(
                                Some(protocol::Content::text(&content)),
                                None,
                                None,
                            ),
                            _ => continue,
                        };
                        messages.push(msg);
                    }
                }
                if let Ok(question) = spec.get::<String>("question") {
                    messages.push(protocol::Message::user(protocol::Content::text(&question)));
                }

                let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                if let Some(func) = on_response {
                    let key = lua.create_registry_value(func)?;
                    if let Ok(mut cbs) = s.callbacks.lock() {
                        cbs.insert(id, LuaHandle { key });
                    }
                }

                crate::lua::with_app(|app| {
                    app.core.engine.send(protocol::UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        task,
                    })
                });
                Ok(id)
            })?,
        )?;
    }

    // smelt.engine.history() → [{role, content, tool_calls?, tool_call_id?}]
    engine_tbl.set(
        "history",
        lua.create_function(|lua, ()| {
            let history = crate::lua::try_with_app(|app| app.core.session.messages.clone())
                .unwrap_or_default();
            messages_to_lua(lua, &history)
        })?,
    )?;

    smelt.set("engine", engine_tbl)?;
    Ok(())
}
