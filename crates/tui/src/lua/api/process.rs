//! `smelt.process` bindings — list, kill, read output, spawn
//! background processes against the same `ProcessRegistry` the
//! engine uses for `bash run_in_background=true`.
//!
//! `smelt.process.run` is the synchronous short-lived counterpart over
//! `tui::process::run` — `Command::new(cmd).args(args).output()` with
//! timeout, cwd, env, optional stdin. Long-lived bidirectional
//! children land in `tui::subprocess` (P3.a).

use mlua::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use crate::process;

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
    process_tbl.set(
        "run",
        lua.create_function(
            |lua, (cmd, args, opts): (String, Option<Vec<String>>, Option<mlua::Table>)| {
                let parsed = parse_run_options(opts.as_ref())?;
                let args = args.unwrap_or_default();
                match process::run(&cmd, &args, &parsed) {
                    Ok(out) => Ok((Some(output_to_lua(lua, &out)?), None)),
                    Err(err) => Ok((None, Some(err.to_string()))),
                }
            },
        )?,
    )?;
    // smelt.process.run_streaming(task_id, call_id, command, timeout_ms)
    //
    // Spawns `sh -c command` on a tokio task, streams stdout+stderr
    // via `EngineEvent::ToolOutput` as lines arrive, and on exit
    // pushes `TaskEvent::ExternalResolvedJson` so a Lua coroutine
    // parked on `smelt.task.wait(task_id)` resumes with
    // `{ content, is_error, timed_out }`. The bash tool uses this —
    // any other streaming subprocess tool composes it the same way.
    process_tbl.set(
        "run_streaming",
        lua.create_function(
            |_, (task_id, call_id, command, timeout_ms): (u64, String, String, u64)| {
                let pair = crate::lua::try_with_app(|app| {
                    (
                        app.core.engine.injector(),
                        app.core.lua.shared().resume_sink(),
                    )
                });
                let Some((injector, sink)) = pair else {
                    return Err(mlua::Error::external(
                        "process.run_streaming: app unavailable",
                    ));
                };
                let timeout = std::time::Duration::from_millis(timeout_ms);
                tokio::spawn(async move {
                    let on_line = |line: String| {
                        injector.inject_tool_output(call_id.clone(), line);
                    };
                    let out = process::run_streaming(&command, timeout, on_line).await;
                    let payload = serde_json::json!({
                        "content": out.content,
                        "is_error": out.is_error,
                        "timed_out": out.timed_out,
                    });
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            },
        )?,
    )?;

    smelt.set("process", process_tbl)?;
    Ok(())
}

fn parse_run_options(opts: Option<&mlua::Table>) -> LuaResult<process::Options> {
    let Some(t) = opts else {
        return Ok(process::Options::default());
    };

    let mut env = HashMap::new();
    if let Some(e) = t.get::<Option<mlua::Table>>("env")? {
        for pair in e.pairs::<String, String>() {
            let (k, v) = pair?;
            env.insert(k, v);
        }
    }

    Ok(process::Options {
        cwd: t.get::<Option<String>>("cwd")?,
        env,
        timeout: t
            .get::<Option<u64>>("timeout_secs")?
            .map(Duration::from_secs),
        stdin: t.get::<Option<String>>("stdin")?,
    })
}

fn output_to_lua(lua: &Lua, out: &process::Output) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("stdout", out.stdout.clone())?;
    t.set("stderr", out.stderr.clone())?;
    t.set("exit_code", out.exit_code)?;
    t.set("timed_out", out.timed_out)?;
    Ok(t)
}
