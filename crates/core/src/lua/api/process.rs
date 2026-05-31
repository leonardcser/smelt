//! `smelt.process` — run/spawn/list/kill processes against the `ProcessRegistry`.

use mlua::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::shared::DefaultShell;
use crate::lua::LuaShared;
use crate::process;

fn current_shell_spec(shared: &Arc<LuaShared>) -> process::ShellSpec {
    shared
        .default_shell
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .map(|s| process::ShellSpec {
            program: s.program,
            args: s.args,
        })
        .unwrap_or_default()
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "process",
        "Run, spawn, list, and kill processes against the `ProcessRegistry`. spawned processes are non-blocking; run processes wait for completion.",
        Tier::Host,
    )?;
    m.fn_(
        "list",
        "Return the registry of running processes as rows of `{ id, command, elapsed_secs }`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let procs =
                crate::host::try_with_core(|core| core.processes.list()).unwrap_or_default();
            let out = lua.create_table()?;
            for (i, p) in procs.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", p.id)?;
                row.set("command", p.command)?;
                row.set("elapsed_secs", p.started_at.elapsed().as_secs())?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "kill",
        "Stop the registered process with `id`. Schedules the kill asynchronously; no-op when no host is installed.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::host::with_core(|core| {
                let registry = core.processes.clone();
                tokio::spawn(async move {
                    let _ = registry.stop(&id).await;
                });
            });
            Ok(())
        },
    )?;
    m.fn_(
        "read_output",
        "Drain buffered output from the registered process `id`. Returns `{ text, running, exit_code? }`, or an empty table when no such process exists.",
        &["id"],
        |lua, id: String| -> LuaResult<mlua::Table> {
            let read = crate::host::try_with_core(|core| core.processes.read(&id));
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
        },
    )?;
    {
        let shared_spawn = Arc::clone(shared);
        m.fn_(
            "spawn_bg",
            "Spawn `command` as a background child registered with the process registry. The wrapping shell defaults to `sh -c` and can be overridden process-wide via `smelt.process.set_default_shell`. Returns the process id; raises if no host is installed or the spawn fails.",
            &["command"],
            move |_, command: String| -> LuaResult<String> {
                let (registry, now) = crate::host::try_with_core(|core| {
                    (core.processes.clone(), core.clock.instant_now())
                })
                .ok_or_else(|| mlua::Error::external("process.spawn_bg: app unavailable"))?;
                let shell = current_shell_spec(&shared_spawn);
                let mut cmd = tokio::process::Command::new(&shell.program);
                for a in &shell.args {
                    cmd.arg(a);
                }
                cmd.arg(&command)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                #[cfg(unix)]
                cmd.process_group(0);
                let child = cmd
                    .spawn()
                    .map_err(|e| mlua::Error::external(e.to_string()))?;
                let id = registry.next_id();
                let (done_tx, _done_rx) = tokio::sync::mpsc::unbounded_channel();
                registry.spawn(id.clone(), &command, child, done_tx, now);
                Ok(id)
            },
        )?;
    }
    {
        let s = Arc::clone(shared);
        m.private_fn(
            "__start_run",
            &["task_id", "cmd", "args", "opts"],
            move |_,
                  (task_id, cmd, args, opts): (
                u64,
                String,
                Option<Vec<String>>,
                Option<mlua::Table>,
            )|
                  -> LuaResult<()> {
                let parsed = parse_run_options(opts.as_ref())?;
                let args = args.unwrap_or_default();
                let cancel = crate::lua::current_task_cancel().unwrap_or_default();
                let sink = s.resume_sink();
                tokio::spawn(async move {
                    let payload = match process::run_async(&cmd, &args, &parsed, cancel).await {
                        Ok(process::RunOutcome::Done(out)) => serde_json::json!({
                            "stdout": out.stdout,
                            "stderr": out.stderr,
                            "exit_code": out.exit_code,
                            "timed_out": out.timed_out,
                        }),
                        Ok(process::RunOutcome::Cancelled) => {
                            serde_json::json!({ "__cancelled": true })
                        }
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    };
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            },
        )?;
    }
    {
        let shared_run_streaming = Arc::clone(shared);
        m.fn_(
            "run_streaming",
            "Run `command` with a `timeout_ms` deadline, streaming each output line into the live tool call `call_id` and resolving task `task_id` with `{ content, is_error, timed_out }` (or `{ __cancelled = true }` if cancelled).",
            &["task_id", "call_id", "command", "timeout_ms"],
            move |_, (task_id, call_id, command, timeout_ms): (u64, String, String, u64)| -> LuaResult<()> {
                let injector = crate::host::try_with_core(|core| core.engine.injector())
                    .ok_or_else(|| {
                        mlua::Error::external("process.run_streaming: app unavailable")
                    })?;
                let sink = shared_run_streaming.resume_sink();
                let cancel = crate::lua::current_task_cancel();
                let timeout = std::time::Duration::from_millis(timeout_ms);
                let shell = current_shell_spec(&shared_run_streaming);
                tokio::spawn(async move {
                    let on_line = |line: String| {
                        injector.inject_tool_output(call_id.clone(), line);
                    };
                    let out = process::run_streaming_with_shell(
                        &command,
                        timeout,
                        shell,
                        on_line,
                        cancel.clone(),
                    )
                    .await;
                    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                        let payload = serde_json::json!({ "__cancelled": true });
                        sink.resolve_json(task_id, payload);
                        return;
                    }
                    let payload = serde_json::json!({
                        "content": out.content,
                        "is_error": out.is_error,
                        "timed_out": out.timed_out,
                    });
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "set_default_shell",
            "Override the wrapping shell used by `spawn_bg` and `run_streaming` for string-form commands. `opts.program` is the executable (e.g. `\"/bin/zsh\"`); `opts.args` is the leading argv (e.g. `{ \"-fc\" }`) — the command string is appended after these. Pass `nil` (no args) to revert to the default `sh -c`.",
            &["opts"],
            move |_, opts: Option<mlua::Table>| -> LuaResult<()> {
                let Some(t) = opts else {
                    if let Ok(mut slot) = s.default_shell.lock() {
                        *slot = None;
                    }
                    return Ok(());
                };
                let program: String = t.get("program")?;
                let args: Vec<String> = t.get::<Option<Vec<String>>>("args")?.unwrap_or_default();
                if let Ok(mut slot) = s.default_shell.lock() {
                    *slot = Some(DefaultShell { program, args });
                }
                Ok(())
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "get_default_shell",
            "Return the current default shell as `{ program, args }`, or `nil` when the built-in `sh -c` default is in effect.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Value> {
                let snapshot = s.default_shell.lock().ok().and_then(|s| s.clone());
                let Some(spec) = snapshot else {
                    return Ok(mlua::Value::Nil);
                };
                let t = lua.create_table()?;
                t.set("program", spec.program)?;
                let args = lua.create_table()?;
                for (i, a) in spec.args.iter().enumerate() {
                    args.set(i + 1, a.as_str())?;
                }
                t.set("args", args)?;
                Ok(mlua::Value::Table(t))
            },
        )?;
    }

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
