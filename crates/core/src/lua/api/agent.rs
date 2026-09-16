//! `smelt.agent` - adjust agent-facing prompt context from Lua plugins.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) const SYSTEM_PROMPT_FRAGMENTS_REGISTRY: &str = "__smelt_agent_system_prompt_fragments";

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "agent",
        "Agent-facing prompt customization for Lua plugins.",
        Tier::Host,
    )?;

    m.fn_(
        "add_system_prompt",
        "Append concise guidance to the system prompt while this Lua runtime is active. Intended for plugins that register tools needing extra usage policy. Returns a `Reg` whose `:remove()` removes the fragment.",
        &["text"],
        |lua, text: String| -> LuaResult<LuaReg> {
            let fragments = match lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY) {
                Ok(table) => table,
                Err(_) => {
                    let table = lua.create_table()?;
                    lua.set_named_registry_value(SYSTEM_PROMPT_FRAGMENTS_REGISTRY, table.clone())?;
                    table
                }
            };
            let id = fragments.raw_len() + 1;
            fragments.raw_set(id, text)?;

            let lua = lua.weak();
            Ok(LuaReg::new(move || {
                let Some(lua) = lua.try_upgrade() else {
                    return false;
                };
                let Ok(fragments) =
                    lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY)
                else {
                    return false;
                };
                let _ = fragments.raw_set(id, mlua::Value::Nil);
                true
            }))
        },
    )?;

    m.fn_(
        "enable_forks",
        "Enable immutable request snapshots for a subagent plugin. Optional opts.max_concurrent controls the shared child concurrency limit (default 16, range 1-64). Disabled by default; children cannot enable or create forks. Configuration applies on the next spawn; lowering the limit does not cancel running children.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<()> {
            if crate::lua::current_subagent().is_some() {
                return Err(mlua::Error::external("subagents cannot enable forks"));
            }
            let value = opts.map(|opts| opts.get::<mlua::Value>("max_concurrent")).transpose()?.unwrap_or(mlua::Value::Nil);
            let max = match value {
                mlua::Value::Nil => crate::agents::DEFAULT_MAX_CONCURRENT,
                mlua::Value::Integer(n) if (1..=crate::agents::MAX_PENDING as i64).contains(&n) => n as usize,
                mlua::Value::Number(n) if (1.0..=crate::agents::MAX_PENDING as f64).contains(&n) && n.fract() == 0.0 => n as usize,
                _ => return Err(mlua::Error::external("max_concurrent must be an integer between 1 and 64")),
            };
            lua.set_named_registry_value("__smelt_agent_max_concurrent", max)?;
            lua.set_named_registry_value("__smelt_agent_forks_enabled", true)
        },
    )?;
    m.fn_(
        "fork",
        "Queue one or more child agents from the current provider-ready request. Every batch member receives the same task and snapshot. Count defaults to one (maximum 16); label optionally supplies a short display task without changing model input. Only a parent model tool may call this API. Returns run records with id, group, session_id, parent_id, task, status, result, error, cost_usd and usage. Usage contains cumulative child-only prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens and reasoning_tokens when reported. Reasoning is included in completion tokens, not an additional bucket.",
        &["task", "count", "label"],
        |lua, (task, count, label): (String, Option<usize>, Option<String>)| -> LuaResult<mlua::Value> {
            if crate::lua::current_tool_invocation().is_none() {
                return Err(mlua::Error::external("fork requires an active model tool invocation"));
            }
            let max = lua.named_registry_value::<usize>("__smelt_agent_max_concurrent").unwrap_or(crate::agents::DEFAULT_MAX_CONCURRENT);
            let agents = crate::host::with_core(|core| core.spawn_agents(task, count.unwrap_or(1), label, max))
                .map_err(mlua::Error::external)?;
            crate::lua::json_to_lua(lua, &serde_json::to_value(agents).map_err(mlua::Error::external)?)
        },
    )?;
    m.fn_(
        "runs",
        "List runtime-owned subagents in creation order, optionally restricted to a parent session. Status is queued, running, completed, cancelled or failed. Includes cumulative child-only cost_usd and usage as returned by fork. Records survive Lua reloads.",
        &["parent_id"],
        |lua, parent_id: Option<String>| -> LuaResult<mlua::Value> {
            let agents = crate::host::with_core(|core| core.agents.children.values()
                .filter(|child| parent_id.as_ref().is_none_or(|id| id == &child.info.parent_id))
                .map(|child| child.info.clone()).collect::<Vec<_>>());
            crate::lua::json_to_lua(lua, &serde_json::to_value(agents).map_err(mlua::Error::external)?)
        },
    )?;
    let wait_shared = Arc::clone(shared);
    m.private_live_only_fn(
        "__start_wait",
        &["task_id", "parent_id", "ids", "timeout_ms"],
        move |_,
              (task_id, parent_id, ids, timeout_ms): (u64, String, Vec<u64>, Option<u64>)|
              -> LuaResult<()> {
            if timeout_ms.is_some_and(|ms| ms > 600000) {
                return Err(mlua::Error::external(
                    "timeout_ms must be between 0 and 600000",
                ));
            }
            let mut completions =
                crate::host::with_core(|core| core.agents.completions(&parent_id, &ids))
                    .map_err(mlua::Error::external)?;
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = wait_shared.resume_sink();
            tokio::spawn(async move {
                let completed = async {
                    let mut runs = Vec::with_capacity(completions.len());
                    for receiver in &mut completions {
                        let value = receiver
                            .wait_for(Option::is_some)
                            .await
                            .map_err(|_| "subagent runtime ended")?;
                        runs.push(value.as_ref().expect("completed subagent").clone());
                    }
                    Ok::<_, &str>(runs)
                };
                let deadline = async {
                    match timeout_ms {
                        Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                        None => std::future::pending().await,
                    }
                };
                let payload = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
                    result = completed => match result {
                        Ok(runs) => serde_json::json!({ "done": true, "runs": runs }),
                        Err(error) => serde_json::json!({ "error": error }),
                    },
                    _ = deadline => serde_json::json!({ "done": false }),
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
        },
    )?;
    m.private_live_only_fn(
        "__notify_when_done",
        &["parent_id", "ids"],
        |_, (parent_id, ids): (String, Vec<u64>)| -> LuaResult<()> {
            crate::host::with_core(|core| core.notify_when_agents_finish(parent_id, ids))
                .map_err(mlua::Error::external)
        },
    )?;
    m.fn_(
        "stop",
        "Cancel a queued or running child without cancelling its parent or siblings. Finished runs remain available for inspection.",
        &["id"],
        |_, id: u64| -> LuaResult<()> {
            crate::host::with_core(|core| core.cancel_agent(id)).map_err(mlua::Error::external)
        },
    )?;

    Ok(())
}

pub fn system_prompt_fragments(lua: &Lua) -> Vec<String> {
    let Ok(fragments) = lua.named_registry_value::<mlua::Table>(SYSTEM_PROMPT_FRAGMENTS_REGISTRY)
    else {
        return Vec::new();
    };
    let mut keyed = Vec::new();
    for (id, text) in fragments.pairs::<usize, String>().flatten() {
        if !text.trim().is_empty() {
            keyed.push((id, text));
        }
    }
    keyed.sort_by_key(|(id, _)| *id);
    keyed.into_iter().map(|(_, text)| text).collect()
}
