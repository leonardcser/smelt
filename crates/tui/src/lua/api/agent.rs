//! `smelt.agent` bindings — list / kill child agents, read live
//! agent snapshots, peek at recent agent log lines. Pre-P5 surface;
//! moves to `smelt.subprocess` once the multi-agent capability is
//! built around `tui::subprocess`.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let agent_tbl = lua.create_table()?;
    agent_tbl.set(
        "my_pid",
        lua.create_function(|_, ()| Ok(std::process::id()))?,
    )?;
    agent_tbl.set(
        "workspace_scope",
        lua.create_function(|_, ()| {
            let cwd = std::env::current_dir().unwrap_or_default();
            let scope = engine::paths::git_root(&cwd)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
            Ok(scope)
        })?,
    )?;
    agent_tbl.set(
        "discover",
        lua.create_function(|lua, scope: Option<String>| {
            let scope = scope.unwrap_or_else(|| {
                let cwd = std::env::current_dir().unwrap_or_default();
                engine::paths::git_root(&cwd)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cwd.to_string_lossy().into_owned())
            });
            let entries = engine::registry::discover(&scope);
            let out = lua.create_table()?;
            for (i, e) in entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("pid", e.pid)?;
                match e.parent_pid {
                    Some(p) => row.set("parent_pid", p)?,
                    None => row.set("parent_pid", LuaNil)?,
                }
                row.set("agent_id", e.agent_id)?;
                row.set("session_id", e.session_id)?;
                row.set("cwd", e.cwd)?;
                row.set(
                    "status",
                    match e.status {
                        engine::registry::AgentStatus::Working => "working",
                        engine::registry::AgentStatus::Idle => "idle",
                    },
                )?;
                row.set("task_slug", e.task_slug.unwrap_or_default())?;
                row.set("git_root", e.git_root.unwrap_or_default())?;
                row.set("git_branch", e.git_branch.unwrap_or_default())?;
                row.set("depth", e.depth)?;
                row.set("started_at", e.started_at)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    agent_tbl.set(
        "find_by_id",
        lua.create_function(|lua, agent_id: String| {
            let Some(e) = engine::registry::find_by_id(&agent_id) else {
                return Ok(LuaNil);
            };
            let row = lua.create_table()?;
            row.set("pid", e.pid)?;
            match e.parent_pid {
                Some(p) => row.set("parent_pid", p)?,
                None => row.set("parent_pid", LuaNil)?,
            }
            row.set("agent_id", e.agent_id)?;
            row.set("session_id", e.session_id)?;
            row.set("cwd", e.cwd)?;
            row.set(
                "status",
                match e.status {
                    engine::registry::AgentStatus::Working => "working",
                    engine::registry::AgentStatus::Idle => "idle",
                },
            )?;
            row.set("task_slug", e.task_slug.unwrap_or_default())?;
            row.set("git_root", e.git_root.unwrap_or_default())?;
            row.set("git_branch", e.git_branch.unwrap_or_default())?;
            row.set("depth", e.depth)?;
            row.set("started_at", e.started_at)?;
            row.set("socket_path", e.socket_path)?;
            Ok(LuaValue::Table(row))
        })?,
    )?;
    agent_tbl.set(
        "my_id",
        lua.create_function(|_, ()| {
            let pid = std::process::id();
            Ok(engine::registry::read_entry(pid)
                .map(|e| e.agent_id)
                .unwrap_or_default())
        })?,
    )?;
    agent_tbl.set(
        "my_slug",
        lua.create_function(|_, ()| {
            let pid = std::process::id();
            Ok(engine::registry::read_entry(pid)
                .ok()
                .and_then(|e| e.task_slug)
                .unwrap_or_default())
        })?,
    )?;
    agent_tbl.set(
        "send_message",
        lua.create_function(
            |lua, (socket_path, from_id, from_slug, message): (String, String, String, String)| {
                match engine::socket::send_message_blocking(
                    std::path::Path::new(&socket_path),
                    &from_id,
                    &from_slug,
                    &message,
                ) {
                    Ok(()) => Ok((true, LuaNil)),
                    Err(e) => Ok((false, LuaValue::String(lua.create_string(&e)?))),
                }
            },
        )?,
    )?;
    agent_tbl.set(
        "send_query",
        lua.create_function(
            |lua, (socket_path, from_id, question): (String, String, String)| {
                match engine::socket::send_query_blocking(
                    std::path::Path::new(&socket_path),
                    &from_id,
                    &question,
                ) {
                    Ok(answer) => Ok((LuaValue::String(lua.create_string(&answer)?), LuaNil)),
                    Err(e) => Ok((LuaNil, LuaValue::String(lua.create_string(&e)?))),
                }
            },
        )?,
    )?;
    agent_tbl.set(
        "is_in_tree",
        lua.create_function(|_, (pid, root_pid): (u32, u32)| {
            Ok(engine::registry::is_in_tree(pid, root_pid))
        })?,
    )?;
    agent_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let my_pid = std::process::id();
            let entries = engine::registry::children_of(my_pid);
            let out = lua.create_table()?;
            for (i, e) in entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("pid", e.pid)?;
                row.set("agent_id", e.agent_id)?;
                row.set("session_id", e.session_id)?;
                row.set("cwd", e.cwd)?;
                row.set(
                    "status",
                    match e.status {
                        engine::registry::AgentStatus::Working => "working",
                        engine::registry::AgentStatus::Idle => "idle",
                    },
                )?;
                row.set("task_slug", e.task_slug.unwrap_or_default())?;
                row.set("git_root", e.git_root.unwrap_or_default())?;
                row.set("git_branch", e.git_branch.unwrap_or_default())?;
                row.set("depth", e.depth)?;
                row.set("started_at", e.started_at)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    agent_tbl.set(
        "kill",
        lua.create_function(|_, pid: u32| {
            engine::registry::kill_agent(pid);
            Ok(())
        })?,
    )?;
    agent_tbl.set(
        "snapshots",
        lua.create_function(|lua, ()| {
            let snaps = crate::lua::try_with_app(|app| {
                app.agent_snapshots
                    .lock()
                    .map(|v| v.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
            let out = lua.create_table()?;
            for (i, snap) in snaps.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("agent_id", snap.agent_id)?;
                row.set("prompt", snap.prompt.as_str())?;
                row.set("cost_usd", snap.cost_usd)?;
                if let Some(t) = snap.context_tokens {
                    row.set("context_tokens", t)?;
                }
                let calls = lua.create_table()?;
                for (j, call) in snap.tool_calls.into_iter().enumerate() {
                    let c = lua.create_table()?;
                    c.set("call_id", call.call_id)?;
                    c.set("tool_name", call.tool_name)?;
                    c.set("summary", call.summary)?;
                    c.set(
                        "status",
                        match call.status {
                            crate::app::transcript_model::ToolStatus::Pending => "pending",
                            crate::app::transcript_model::ToolStatus::Confirm => "confirm",
                            crate::app::transcript_model::ToolStatus::Ok => "ok",
                            crate::app::transcript_model::ToolStatus::Err => "err",
                            crate::app::transcript_model::ToolStatus::Denied => "denied",
                        },
                    )?;
                    if let Some(d) = call.elapsed {
                        c.set("elapsed_ms", d.as_millis() as u64)?;
                    }
                    calls.set(j + 1, c)?;
                }
                row.set("tool_calls", calls)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;

    agent_tbl.set(
        "subagent_meta",
        lua.create_function(|lua, ()| {
            let meta = crate::lua::try_with_app(|app| app.core.engine.subagent_meta()).flatten();
            let Some((depth, max_depth, max_agents)) = meta else {
                return Ok(LuaNil);
            };
            let row = lua.create_table()?;
            row.set("depth", depth)?;
            row.set("max_depth", max_depth)?;
            row.set("max_agents", max_agents)?;
            Ok(LuaValue::Table(row))
        })?,
    )?;
    agent_tbl.set(
        "spawn",
        lua.create_function(
            |lua, (prompt, blocking, session_dir): (String, Option<bool>, String)| {
                let blocking = blocking.unwrap_or(false);
                let result = crate::lua::try_with_app(|app| {
                    app.core.engine.spawn_subagent(
                        prompt,
                        blocking,
                        std::path::Path::new(&session_dir),
                    )
                });
                let Some(result) = result else {
                    return Err(mlua::Error::external("agent.spawn: app unavailable"));
                };
                match result {
                    Ok(agent_id) => Ok((LuaValue::String(lua.create_string(&agent_id)?), LuaNil)),
                    Err(err) => Ok((LuaNil, LuaValue::String(lua.create_string(&err)?))),
                }
            },
        )?,
    )?;
    // smelt.agent.wait_for_message(task_id, agent_id, my_pid, timeout_ms)
    //
    // Spawns a tokio task that subscribes to the engine's
    // `AgentMessageNotification` broadcast and resolves `task_id`
    // through the Lua `LuaResumeSink` when one of three things
    // happens: a matching message arrives (`{ message }`), the named
    // child exits without sending a result (`{ error: "... exited
    // without sending a result" }`), or the timeout elapses
    // (`{ error: "... timed out after ..s" }`). Mirrors the
    // `select!` body of the retired Rust `SpawnAgentTool`'s
    // `wait_for_agent`.
    agent_tbl.set(
        "wait_for_message",
        lua.create_function(
            |_, (task_id, agent_id, my_pid, timeout_ms): (u64, String, u32, u64)| {
                let pair = crate::lua::try_with_app(|app| {
                    (
                        app.core.engine.injector().subscribe_agent_msg(),
                        app.core.lua.shared().resume_sink(),
                    )
                });
                let Some((rx_opt, sink)) = pair else {
                    return Err(mlua::Error::external(
                        "agent.wait_for_message: app unavailable",
                    ));
                };
                let Some(mut rx) = rx_opt else {
                    sink.resolve_json(
                        task_id,
                        serde_json::json!({
                            "error": "multi-agent disabled",
                        }),
                    );
                    return Ok(());
                };
                let timeout = std::time::Duration::from_millis(timeout_ms);
                tokio::spawn(async move {
                    use tokio::sync::broadcast::error::RecvError;
                    let deadline = tokio::time::Instant::now() + timeout;
                    let mut child_check =
                        tokio::time::interval(std::time::Duration::from_secs(5));
                    child_check.tick().await; // consume immediate tick
                    loop {
                        tokio::select! {
                            result = rx.recv() => {
                                match result {
                                    Ok(notif) if notif.from_id == agent_id => {
                                        sink.resolve_json(task_id, serde_json::json!({
                                            "message": notif.message,
                                        }));
                                        return;
                                    }
                                    Ok(_) => continue,
                                    Err(RecvError::Lagged(_)) => continue,
                                    Err(RecvError::Closed) => {
                                        sink.resolve_json(task_id, serde_json::json!({
                                            "error": format!("agent {agent_id}: message channel closed"),
                                        }));
                                        return;
                                    }
                                }
                            }
                            _ = tokio::time::sleep_until(deadline) => {
                                sink.resolve_json(task_id, serde_json::json!({
                                    "error": format!("agent {agent_id}: timed out after {}s", timeout.as_secs()),
                                }));
                                return;
                            }
                            _ = child_check.tick() => {
                                let alive = engine::registry::children_of(my_pid)
                                    .iter()
                                    .any(|e| e.agent_id == agent_id
                                        && engine::registry::is_pid_alive(e.pid));
                                if !alive {
                                    sink.resolve_json(task_id, serde_json::json!({
                                        "error": format!("agent {agent_id} exited without sending a result"),
                                    }));
                                    return;
                                }
                            }
                        }
                    }
                });
                Ok(())
            },
        )?,
    )?;

    agent_tbl.set(
        "peek",
        lua.create_function(|lua, (pid, max_lines): (u32, Option<usize>)| {
            let my_pid = std::process::id();
            let entries = engine::registry::children_of(my_pid);
            let Some(entry) = entries.iter().find(|e| e.pid == pid) else {
                return lua.create_table();
            };
            let session = match crate::session::load(&entry.session_id) {
                Some(s) => s,
                None => return lua.create_table(),
            };
            let dir = crate::session::dir_for(&session);
            let lines = engine::registry::read_agent_logs(&dir, pid, max_lines.unwrap_or(200));
            let out = lua.create_table()?;
            for (i, line) in lines.into_iter().enumerate() {
                out.set(i + 1, line)?;
            }
            Ok(out)
        })?,
    )?;
    smelt.set("agent", agent_tbl)?;
    Ok(())
}
