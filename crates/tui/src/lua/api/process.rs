//! `smelt.process` bindings — list, kill, read output, spawn
//! background processes against the same `ProcessRegistry` the
//! engine uses for `bash run_in_background=true`.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let process_tbl = lua.create_table()?;
    process_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let procs = crate::lua::try_with_app(|app| app.core.engine.processes().list())
                .unwrap_or_default();
            let out = lua.create_table()?;
            for (i, p) in procs.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", p.id)?;
                row.set("command", p.command)?;
                row.set("elapsed_secs", p.started_at.elapsed().as_secs())?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    process_tbl.set(
        "kill",
        lua.create_function(|_, id: String| {
            crate::lua::with_app(|app| {
                let registry = app.core.engine.processes().clone();
                tokio::spawn(async move {
                    let _ = registry.stop(&id).await;
                });
            });
            Ok(())
        })?,
    )?;
    process_tbl.set(
        "read_output",
        lua.create_function(|lua, id: String| {
            let read = crate::lua::try_with_app(|app| app.core.engine.processes().read(&id));
            match read {
                Some(Ok((text, running, exit_code))) => {
                    let t = lua.create_table()?;
                    t.set("text", text)?;
                    t.set("running", running)?;
                    if let Some(code) = exit_code {
                        t.set("exit_code", code)?;
                    }
                    Ok(t)
                }
                _ => lua.create_table(),
            }
        })?,
    )?;
    // smelt.process.spawn_bg(command) → string id, or raises on
    // spawn error. Adds the child to the same `ProcessRegistry`
    // that the engine uses, so `smelt.process.list/read_output/kill`
    // (and the core `read_process_output` / `stop_process` tools)
    // observe it the same way as `bash run_in_background=true`.
    process_tbl.set(
        "spawn_bg",
        lua.create_function(|_, command: String| -> LuaResult<String> {
            let registry = crate::lua::try_with_app(|app| app.core.engine.processes().clone())
                .ok_or_else(|| mlua::Error::external("process.spawn_bg: app unavailable"))?;
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c")
                .arg(&command)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(unix)]
            cmd.process_group(0);
            let child = cmd
                .spawn()
                .map_err(|e| mlua::Error::external(e.to_string()))?;
            let id = registry.next_id();
            // Discard channel — plugin-spawned processes don't emit
            // `EngineEvent::ProcessCompleted` today.
            let (done_tx, _done_rx) = tokio::sync::mpsc::unbounded_channel();
            registry.spawn(id.clone(), &command, child, done_tx);
            Ok(id)
        })?,
    )?;
    smelt.set("process", process_tbl)?;
    Ok(())
}
