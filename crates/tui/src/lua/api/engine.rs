//! `smelt.engine` bindings — turn-driver writes (cancel, compact),
//! the `ask` auxiliary request primitive, and `submit_command` for
//! Lua-rendered slash-command turns (`/reflect`, `/simplify`,
//! user-defined custom commands). Mode get/set/cycle live under
//! `smelt.mode`; reasoning effort lives under `smelt.reasoning`;
//! model get/set/list live under `smelt.model`; per-session cost /
//! context-token / context-window / messages snapshot live under
//! `smelt.session`.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let engine_tbl = lua.create_table()?;

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

    // smelt.engine.multi_agent() -> bool. Read-only view of the
    // process-wide multi-agent config flag (set once at startup from
    // CLI / config). Lua plugins branch on this when their template
    // body differs between solo and multi-agent runs.
    engine_tbl.set(
        "multi_agent",
        lua.create_function(|_, ()| {
            Ok(crate::lua::try_with_app(|app| app.core.config.multi_agent).unwrap_or(false))
        })?,
    )?;

    // smelt.engine.submit_command(name, body, overrides?) — start a
    // turn from a Lua-rendered slash-command template. `name` is the
    // bare command (e.g. `"reflect"`) and shows in the transcript as
    // `/name`; `body` is the fully resolved prompt the LLM sees;
    // optional `overrides` is a Lua table mirroring the YAML
    // frontmatter on user-defined commands (`provider`, `model`,
    // `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`,
    // `reasoning_effort`, `tools`, `bash`, `web_fetch`; the three
    // rule-set keys take a sub-table with `allow` / `ask` / `deny`
    // arrays). No-op when an agent turn is already running.
    engine_tbl.set(
        "submit_command",
        lua.create_function(
            |_, (name, body, overrides): (String, String, Option<mlua::Table>)| {
                let parsed = match overrides.as_ref() {
                    Some(t) => parse_overrides(t)?,
                    None => Default::default(),
                };
                crate::lua::with_app(|app| {
                    if app.agent.is_some() {
                        app.notify_error(format!("cannot run /{name} while agent is working"));
                        return;
                    }
                    let cmd = crate::custom_commands::CustomCommand {
                        name,
                        body,
                        overrides: parsed,
                    };
                    let turn = app.begin_custom_command_turn(cmd);
                    app.agent = Some(turn);
                });
                Ok(())
            },
        )?,
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

fn parse_overrides(t: &mlua::Table) -> LuaResult<crate::custom_commands::CommandOverrides> {
    use crate::custom_commands::{CommandOverrides, RuleOverride};

    fn rule(t: &mlua::Table, key: &str) -> LuaResult<Option<RuleOverride>> {
        let Some(sub) = t.get::<Option<mlua::Table>>(key)? else {
            return Ok(None);
        };
        Ok(Some(RuleOverride {
            allow: list(&sub, "allow")?,
            ask: list(&sub, "ask")?,
            deny: list(&sub, "deny")?,
        }))
    }
    fn list(t: &mlua::Table, key: &str) -> LuaResult<Vec<String>> {
        let Some(v) = t.get::<Option<mlua::Table>>(key)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for pair in v.sequence_values::<String>() {
            out.push(pair?);
        }
        Ok(out)
    }

    Ok(CommandOverrides {
        provider: t.get::<Option<String>>("provider")?,
        model: t.get::<Option<String>>("model")?,
        temperature: t.get::<Option<f64>>("temperature")?,
        top_p: t.get::<Option<f64>>("top_p")?,
        top_k: t.get::<Option<u32>>("top_k")?,
        min_p: t.get::<Option<f64>>("min_p")?,
        repeat_penalty: t.get::<Option<f64>>("repeat_penalty")?,
        reasoning_effort: t.get::<Option<String>>("reasoning_effort")?,
        tools: rule(t, "tools")?,
        bash: rule(t, "bash")?,
        web_fetch: rule(t, "web_fetch")?,
    })
}
