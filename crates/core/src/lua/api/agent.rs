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
        "Enable immutable request snapshots for a subagent plugin. Optional opts.max_concurrent controls shared child concurrency (default 16, range 1-64). opts.max_cost_usd and opts.max_tokens gate new child requests using observed parent-plus-child totals. opts.max_requests limits main requests per worker assignment. Budgets are disabled by default and in-flight requests can exceed them. opts.compact_at_tokens sets the child-local compaction threshold; otherwise 80% of a known context window is used. Disabled by default; children cannot enable or create forks. Configuration applies on the next spawn or follow-up; lowering the limit does not cancel running children.",
        &["opts"],
        |lua, opts: Option<mlua::Table>| -> LuaResult<()> {
            if crate::lua::current_subagent().is_some() {
                return Err(mlua::Error::external("subagents cannot enable forks"));
            }
            if let Some(opts) = &opts {
                for key in ["max_cost_usd", "max_tokens", "max_requests", "compact_at_tokens"] {
                    if matches!(opts.get::<mlua::Value>(key)?, mlua::Value::Number(value) if !value.is_finite()) {
                        return Err(mlua::Error::external("subagent budgets and compaction thresholds must be positive and finite"));
                    }
                }
            }
            let options: crate::agents::AgentOptions = opts.map(|opts| decode(lua, mlua::Value::Table(opts))).transpose()?.unwrap_or_default();
            options.validate().map_err(mlua::Error::external)?;
            lua.set_named_registry_value("__smelt_agent_options", crate::lua::serde_to_lua(lua, &options)?)?;
            lua.set_named_registry_value("__smelt_agent_forks_enabled", true)
        },
    )?;
    let fork_cards = Arc::clone(shared);
    m.fn_(
        "fork",
        "Queue one or more child agents from the current provider-ready request. Every batch member receives the same task and snapshot. Count defaults to one (maximum 16); label optionally supplies a short display task without changing model input. Only a parent model tool may call this API. Awaits archive restoration before allocation; must run inside a Lua task. Returns records with numeric id, readable name, group, session_id, parent_id, short task, status, result, error, activity, elapsed_ms, requests, persistence_error, cost_usd and usage. Usage contains cumulative child-only prompt_tokens, completion_tokens, cache_read_tokens, cache_write_tokens and reasoning_tokens when reported. Reasoning is included in completion tokens, not an additional bucket.",
        &["task", "count", "label"],
        move |lua, (task, count, label): (String, Option<usize>, Option<String>)| -> LuaResult<mlua::Value> {
            if crate::lua::current_tool_invocation().is_none() {
                return Err(mlua::Error::external("fork requires an active model tool invocation"));
            }
            let options = configured_options(lua)?;
            let agents = crate::host::with_core(|core| {
                let max = options.max_concurrent;
                core.configure_agents(options)?;
                let agents = core.spawn_agents(task, count.unwrap_or(1), label, max)?;
                for info in &agents { fork_cards.publish_agent(&core.agents.children[&info.id]); }
                Ok::<_, String>(agents)
            })
                .map_err(mlua::Error::external)?;
            crate::lua::json_to_lua(lua, &serde_json::to_value(agents).map_err(mlua::Error::external)?)
        },
    )?;
    let listing_cards = Arc::clone(shared);
    m.fn_(
        "runs",
        "List runtime-owned subagents in creation order, optionally restricted to a parent session. Status is queued, running, completed, blocked, cancelled or failed. Records include readable name, short task, recent activity, elapsed_ms and request counts. With parent_id, archived workers load in the background and appear on subsequent calls. Includes cumulative child-only cost_usd and usage as returned by fork. Records survive Lua reloads.",
        &["parent_id"],
        move |lua, parent_id: Option<String>| -> LuaResult<mlua::Value> {
            let agents = crate::host::with_core(|core| {
                if let Some(parent_id) = &parent_id { core.agents.restore(&core.sessions, parent_id)?; }
                let children = core.agents.children.values()
                    .filter(|child| parent_id.as_ref().is_none_or(|id| id == &child.info.parent_id));
                Ok::<_, String>(children.map(|child| {
                    listing_cards.publish_agent(child);
                    child.info()
                }).collect::<Vec<_>>())
            }).map_err(mlua::Error::external)?;
            crate::lua::json_to_lua(lua, &serde_json::to_value(agents).map_err(mlua::Error::external)?)
        },
    )?;
    m.private_live_only_fn(
        "__peek",
        &["parent_id", "id"],
        |lua, (parent_id, target): (String, mlua::Value)| -> LuaResult<mlua::Value> {
            let target = decode::<serde_json::Value>(lua, target)?;
            let output = crate::host::with_core(|core| {
                let id = core.agents.resolve(&parent_id, &target)?;
                core.agents.peek(&parent_id, id)
            })
            .map_err(mlua::Error::external)?;
            crate::lua::json_to_lua(
                lua,
                &serde_json::to_value(output).map_err(mlua::Error::external)?,
            )
        },
    )?;
    m.fn_(
        "restore",
        "Wait for native worker records belonging to parent_id to restore. Must run inside a Lua task; failures and cancellation remain explicit. Does not launch workers.",
        &["parent_id"],
        |_, parent_id: String| -> LuaResult<bool> {
            crate::host::with_core(|core| core.agents.restore(&core.sessions, &parent_id))
                .map_err(mlua::Error::external)
        },
    )?;
    m.private_live_only_fn("__parent", &[], |_, (): ()| -> LuaResult<String> {
        if crate::lua::current_tool_invocation().is_none() {
            return Err(mlua::Error::external(
                "fork requires an active model tool invocation",
            ));
        }
        crate::host::with_core(|core| {
            core.engine
                .fork_snapshot()
                .map(|snapshot| snapshot.parent_session_id().to_owned())
        })
        .map_err(mlua::Error::external)
    })?;
    let restore_shared = Arc::clone(shared);
    m.private_live_only_fn("__start_restore", &["task_id", "parent_id"], move |_, (task_id, parent_id): (u64, String)| -> LuaResult<()> {
        let mut ready = crate::host::with_core(|core| core.agents.restoration_ready(&parent_id))
            .ok_or_else(|| mlua::Error::external("subagent restoration is not pending"))?;
        let sink = restore_shared.resume_sink();
        let cancel = crate::lua::current_task_cancel().unwrap_or_default();
        tokio::spawn(async move {
            let payload = tokio::select! {
                biased;
                _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
                result = ready.wait_for(Option::is_some) => match result {
                    Ok(result) => match result.as_ref().expect("restoration finished") {
                        Ok(()) => serde_json::json!({ "ready": true }),
                        Err(error) => serde_json::json!({ "error": error }),
                    },
                    Err(_) => serde_json::json!({ "error": "subagent restoration worker stopped" }),
                },
            };
            sink.resolve_json(task_id, payload);
        });
        Ok(())
    })?;
    let wait_shared = Arc::clone(shared);
    m.private_live_only_fn(
        "__start_wait",
        &["task_id", "parent_id", "ids"],
        move |lua, (task_id, parent_id, targets): (u64, String, mlua::Value)| -> LuaResult<()> {
            let targets = decode::<Vec<serde_json::Value>>(lua, targets)?;
            let mut completions = crate::host::with_core(|core| {
                let ids = targets
                    .iter()
                    .map(|target| core.agents.resolve(&parent_id, target))
                    .collect::<Result<Vec<_>, _>>()?;
                core.agents.completions(&parent_id, &ids)
            })
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
                let payload = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
                    result = completed => match result {
                        Ok(runs) => serde_json::json!({ "runs": runs }),
                        Err(error) => serde_json::json!({ "error": error }),
                    },
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
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

    m.fn_("totals", "Return child-only and parent-plus-child usage and costs observed in this process. Reasoning tokens are already included in completion tokens.", &["parent_id"], |lua, parent_id: String| -> LuaResult<mlua::Value> {
        let totals = crate::host::with_core(|core| {
            let (usage, cost_usd) = core.agents.family_totals(&parent_id);
            let (child_usage, child_cost_usd) = core.agents.totals(&parent_id);
            serde_json::json!({ "usage": usage, "cost_usd": cost_usd, "child_usage": child_usage, "child_cost_usd": child_cost_usd })
        });
        crate::lua::json_to_lua(lua, &totals)
    })?;
    m.fn_("report", "Record a completed or blocked assignment report from the active child tool. This does not end its engine turn; the child must finish its response. Only subagents may report.", &["status", "report"], |_, (status, report): (String, String)| -> LuaResult<()> {
        if crate::lua::current_subagent().is_none() || crate::lua::current_tool_invocation().is_none() { return Err(mlua::Error::external("report requires an active subagent tool")); }
        if !matches!(status.as_str(), "completed" | "blocked") || report.trim().is_empty() { return Err(mlua::Error::external("provide a completed or blocked status and a nonempty report")); }
        crate::host::with_core(|core| core.agent_report = Some((status, report)));
        Ok(())
    })?;
    m.private_live_only_fn(
        "__stop",
        &["parent_id", "target"],
        |lua, (parent_id, target): (String, mlua::Value)| -> LuaResult<()> {
            let target = decode::<serde_json::Value>(lua, target)?;
            crate::host::with_core(|core| {
                let id = core.agents.resolve(&parent_id, &target)?;
                core.cancel_agent(id)
            })
            .map_err(mlua::Error::external)
        },
    )?;
    m.fn_("stop_all", "Cancel every queued or running child owned by the specified parent session. Does not interrupt the parent.", &["parent_id"], |_, parent_id: String| -> LuaResult<()> {
        if crate::lua::current_subagent().is_some() { return Err(mlua::Error::external("subagents cannot stop sibling agents")); }
        crate::host::with_core(|core| core.cancel_agents_for(&parent_id));
        Ok(())
    })?;
    let cards = Arc::clone(shared);
    m.private_fn(
        "__card",
        &["session_id"],
        move |lua, session_id: Option<String>| -> LuaResult<mlua::Value> {
            let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
                return Ok(mlua::Value::Nil);
            };
            let (mut info, archive_pending) = {
                let cards = cards
                    .agent_cards
                    .lock()
                    .map_err(|_| mlua::Error::external("subagent presentation lock poisoned"))?;
                cards.get(&session_id).map_or((None, false), |card| {
                    let (info, pending) = card.snapshot();
                    (Some(info), pending)
                })
            };
            if let Some(info) = &mut info {
                if info.status == "running" {
                    info.elapsed_ms = info
                        .started_at_ms
                        .map_or(0, |start| crate::session::now_ms().saturating_sub(start));
                }
            }
            let mut value = serde_json::to_value(info).map_err(mlua::Error::external)?;
            if let Some(fields) = value.as_object_mut() {
                fields.insert("archive_pending".into(), archive_pending.into());
            }
            crate::lua::json_to_lua(lua, &value)
        },
    )?;
    let resume_shared = Arc::clone(shared);
    m.private_live_only_fn(
        "__start_follow_up",
        &["task_id", "parent_id", "target"],
        move |lua, (task_id, parent_id, target): (u64, String, mlua::Value)| -> LuaResult<()> {
            if crate::lua::current_subagent().is_some()
                || crate::lua::current_tool_invocation().is_none()
            {
                return Err(mlua::Error::external(
                    "follow-up requires an active parent tool",
                ));
            }
            let target = decode::<serde_json::Value>(lua, target)?;
            let mut completion = crate::host::with_core(|core| -> Result<_, String> {
                let id = core.agents.resolve(&parent_id, &target)?;
                let child = &core.agents.children[&id];
                if matches!(child.info.status.as_str(), "queued" | "running") {
                    return Err(
                        "wait for the subagent to finish before assigning a follow-up".into(),
                    );
                }
                Ok(core.agents.completions(&parent_id, &[id])?.remove(0))
            })
            .map_err(mlua::Error::external)?;
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = resume_shared.resume_sink();
            tokio::spawn(async move {
                let payload = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
                    result = completion.wait_for(Option::is_some) => match result {
                        Ok(_) => serde_json::json!({ "ready": true }),
                        Err(_) => serde_json::json!({ "error": "subagent runtime ended" }),
                    },
                };
                sink.resolve_json(task_id, payload);
            });
            Ok(())
        },
    )?;
    let follow_up_cards = Arc::clone(shared);
    m.private_live_only_fn(
        "__follow_up",
        &["parent_id", "target", "prompt"],
        move |lua,
              (parent_id, target, prompt): (String, mlua::Value, String)|
              -> LuaResult<mlua::Value> {
            if crate::lua::current_subagent().is_some()
                || crate::lua::current_tool_invocation().is_none()
            {
                return Err(mlua::Error::external(
                    "follow-up requires an active parent tool",
                ));
            }
            let target = decode::<serde_json::Value>(lua, target)?;
            let options = configured_options(lua)?;
            let info = crate::host::with_core(|core| {
                core.configure_agents(options)?;
                let id = core.agents.resolve(&parent_id, &target)?;
                let info = core.follow_up_agent(&parent_id, id, prompt)?;
                follow_up_cards.publish_agent(&core.agents.children[&id]);
                Ok::<_, String>(info)
            })
            .map_err(mlua::Error::external)?;
            crate::lua::json_to_lua(
                lua,
                &serde_json::to_value(info).map_err(mlua::Error::external)?,
            )
        },
    )?;

    let internal = crate::lua::module::internal_api_table(lua, "smelt.agent")?;
    let install: mlua::Function = lua
        .load(
            r#"
        return function(agent, native_restore, native_fork, parent, start, smelt)
            local function restore(parent_id)
                if native_restore(parent_id) then return true end
                local task_id = smelt.task.alloc()
                start(task_id, parent_id)
                local result = smelt.task.wait(task_id)
                -- Apply records even on failure to release the pending attempt for retry.
                local applied = native_restore(parent_id)
                if result.error then error(result.error, 0) end
                if not applied then error("subagent restoration did not complete", 0) end
                return true
            end
            agent.restore = restore
            agent.fork = function(task, count, label)
                restore(parent())
                return native_fork(task, count, label)
            end
        end
    "#,
        )
        .eval()?;
    install.call::<()>((
        m.tbl.clone(),
        m.tbl.get::<mlua::Function>("restore")?,
        m.tbl.get::<mlua::Function>("fork")?,
        internal.get::<mlua::Function>("__parent")?,
        internal.get::<mlua::Function>("__start_restore")?,
        smelt.clone(),
    ))?;
    Ok(())
}

fn configured_options(lua: &Lua) -> LuaResult<crate::agents::AgentOptions> {
    lua.named_registry_value::<mlua::Value>("__smelt_agent_options")
        .ok()
        .map(|value| decode(lua, value))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn decode<T: serde::de::DeserializeOwned>(lua: &Lua, value: mlua::Value) -> LuaResult<T> {
    crate::lua::lua_to_serde(lua, &value)
        .ok_or_else(|| mlua::Error::external("invalid subagent arguments"))
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
