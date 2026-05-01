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
            Ok(LuaValue::Table(row))
        })?,
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
