//! `smelt.engine` bindings — live engine reads (busy state),
//! turn-driver writes (submit, cancel, compact), and the `ask`
//! auxiliary request primitive. Mode get/set/cycle live under
//! `smelt.mode`; reasoning effort lives under `smelt.reasoning`;
//! model get/set/list live under `smelt.model`; per-session cost /
//! context-token / context-window / messages snapshot live under
//! `smelt.session`.

use super::app_read;
use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let engine_tbl = lua.create_table()?;

    engine_tbl.set("is_busy", app_read!(lua, |app| app.agent.is_some()))?;

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

    smelt.set("engine", engine_tbl)?;
    Ok(())
}
