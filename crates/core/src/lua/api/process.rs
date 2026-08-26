//! `smelt.process` - process helpers backed by the shell job supervisor.

use mlua::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

fn output_table(
    lua: &Lua,
    result: Option<Result<process::JobOutput, String>>,
) -> LuaResult<mlua::Table> {
    match result {
        Some(Ok(out)) => {
            let t = lua.create_table()?;
            t.set("text", out.text)?;
            t.set("running", out.running)?;
            if let Some(code) = out.exit_code {
                t.set("exit_code", code)?;
            }
            if let Some(termination) = out.termination {
                t.set("termination", termination.as_str())?;
            }
            if let Some(pid) = out.pid {
                t.set("pid", pid)?;
            }
            t.set("elapsed_secs", out.elapsed_secs)?;
            Ok(t)
        }
        _ => lua.create_table(),
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "process",
        "Run subprocesses and manage contained shell jobs. Background jobs are non-blocking; foreground jobs stream bounded output and wait for completion.",
        Tier::Host,
    )?;
    m.fn_(
        "list",
        "Return running shell jobs as rows of `{ id, pid?, command, elapsed_secs }`. `id` is an opaque stable job ID; `pid` is present when the OS exposes the top-level child PID.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let procs =
                crate::host::try_with_core(|core| core.jobs.list()).unwrap_or_default();
            let out = lua.create_table()?;
            for (i, p) in procs.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", p.id)?;
                if let Some(pid) = p.pid {
                    row.set("pid", pid)?;
                }
                row.set("command", p.command)?;
                row.set("elapsed_secs", p.elapsed_secs)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "kill",
        "Stop the supervised shell job with `id`. Schedules containment termination asynchronously; no-op when no host is installed.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::host::with_core(|core| {
                let supervisor = core.jobs.clone();
                tokio::spawn(async move {
                    let _ = supervisor.stop(&id).await;
                });
            });
            Ok(())
        },
    )?;
    m.fn_(
        "detach_foreground",
        "Stop following the most recently started detachable foreground job and leave the same supervisor-owned job running in the background. Returns true when a detach request was sent.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| core.jobs.detach_latest_foreground().requested())
                .unwrap_or(false))
        },
    )?;
    {
        let s = Arc::clone(shared);
        m.private_fn(
            "__start_stop",
            &["task_id", "id"],
            move |_, (task_id, id): (u64, String)| -> LuaResult<()> {
                let supervisor =
                    crate::host::try_with_core(|core| core.jobs.clone()).ok_or_else(|| {
                        mlua::Error::external("process.__start_stop: app unavailable")
                    })?;
                let sink = s.resume_sink();
                tokio::spawn(async move {
                    let payload = match supervisor.stop(&id).await {
                        Ok(output) => serde_json::json!({ "text": output.text }),
                        Err(err) => serde_json::json!({ "err": err }),
                    };
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
            },
        )?;
    }
    m.fn_(
        "read_output",
        "Drain bounded output from supervised job `id`. Returns `{ text, running, exit_code?, termination?, elapsed_secs, pid? }`, or an empty table when the job does not exist or its completed snapshot has been evicted.",
        &["id"],
        |lua, id: String| -> LuaResult<mlua::Table> {
            output_table(lua, crate::host::try_with_core(|core| core.jobs.drain_output(&id)))
        },
    )?;
    m.fn_(
        "output",
        "Return the bounded output snapshot for supervised job `id` without draining it. Returns `{ text, running, exit_code?, termination?, elapsed_secs, pid? }`, or an empty table when the job does not exist or its completed snapshot has been evicted.",
        &["id"],
        |lua, id: String| -> LuaResult<mlua::Table> {
            output_table(lua, crate::host::try_with_core(|core| core.jobs.snapshot_output(&id)))
        },
    )?;
    {
        let shared_spawn = Arc::clone(shared);
        m.private_fn(
            "__start_spawn_bg",
            &["task_id", "command"],
            move |_, (task_id, command): (u64, String)| -> LuaResult<()> {
                let (supervisor, now) = crate::host::try_with_core(|core| {
                    (core.jobs.clone(), core.clock.instant_now())
                })
                .ok_or_else(|| mlua::Error::external("process.spawn_bg: app unavailable"))?;
                let cwd = shared_spawn.evaluation_cwd();
                let shell = current_shell_spec(&shared_spawn);
                let sink = shared_spawn.resume_sink();
                tokio::spawn(async move {
                    let payload = match supervisor
                        .spawn_background(&command, &shell, &cwd, now)
                        .await
                    {
                        Ok(id) => serde_json::json!({ "id": id }),
                        Err(error) => serde_json::json!({ "err": error.to_string() }),
                    };
                    sink.resolve_json(task_id, payload);
                });
                Ok(())
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
                let runtime_cwd = s.evaluation_cwd();
                let parsed = parse_run_options(opts.as_ref(), &runtime_cwd)?;
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
            "Run `command` as a contained job with a `timeout_ms` deadline, streaming bounded output into live tool call `call_id` and resolving task `task_id` with `{ content, is_error, timed_out, background_id?, termination? }` (or `{ __cancelled = true }` if cancelled). When `background_on_timeout` is true, the same supervised job keeps running in the background.",
            &["task_id", "call_id", "command", "timeout_ms", "background_on_timeout"],
            move |_, (task_id, call_id, command, timeout_ms, background_on_timeout): (u64, String, String, u64, bool)| -> LuaResult<()> {
                let (injector, supervisor, now) = crate::host::try_with_core(|core| {
                    (
                        core.engine.injector(),
                        core.jobs.clone(),
                        core.clock.instant_now(),
                    )
                })
                    .ok_or_else(|| {
                        mlua::Error::external("process.run_streaming: app unavailable")
                    })?;
                let invocation_id = crate::lua::current_tool_invocation()
                    .map(|invocation| invocation.invocation_id)
                    .ok_or_else(|| {
                        mlua::Error::external(
                            "process.run_streaming: no active tool invocation",
                        )
                    })?;
                let cwd = shared_run_streaming.evaluation_cwd();
                let sink = shared_run_streaming.resume_sink();
                let cancel = crate::lua::current_task_cancel();
                let timeout = std::time::Duration::from_millis(timeout_ms);
                let shell = current_shell_spec(&shared_run_streaming);
                tokio::spawn(async move {
                    let on_line = |line: String| {
                        injector.inject_tool_output(invocation_id, call_id.clone(), line);
                    };
                    let out = supervisor
                        .run(
                            &command,
                            process::JobRunConfig {
                                timeout: Some(timeout),
                                shell,
                                cwd,
                                started_at: now,
                                cancel: cancel.clone(),
                                background_on_timeout,
                                detachable: true,
                            },
                            on_line,
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
                        "background_id": out.background_id,
                        "termination": out.termination.map(protocol::JobTermination::as_str),
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
            "Override the wrapping shell used by `spawn_bg` and `run_streaming` for string-form commands. `opts.program` is the executable (e.g. `\"/bin/zsh\"`); `opts.args` is the leading argv (e.g. `{ \"-fc\" }`), and the command string is appended after these. Pass `nil` (no args) to revert to the default `sh -c`.",
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

fn parse_run_options(
    opts: Option<&mlua::Table>,
    runtime_cwd: &Path,
) -> LuaResult<process::Options> {
    let Some(t) = opts else {
        return Ok(process::Options {
            cwd: runtime_cwd.to_path_buf(),
            env: HashMap::new(),
            timeout: None,
            stdin: None,
            max_output_bytes: None,
        });
    };

    let mut env = HashMap::new();
    if let Some(e) = t.get::<Option<mlua::Table>>("env")? {
        for pair in e.pairs::<String, String>() {
            let (k, v) = pair?;
            env.insert(k, v);
        }
    }

    let cwd = t
        .get::<Option<String>>("cwd")?
        .map(PathBuf::from)
        .map(|cwd| {
            if cwd.is_absolute() {
                cwd
            } else {
                runtime_cwd.join(cwd)
            }
        })
        .unwrap_or_else(|| runtime_cwd.to_path_buf());

    let max_output_bytes = t.get::<Option<usize>>("max_output_bytes")?;
    if max_output_bytes.is_some_and(|limit| limit == 0 || limit > process::MAX_OUTPUT_BYTES) {
        return Err(mlua::Error::external(format!(
            "max_output_bytes must be between 1 and {}",
            process::MAX_OUTPUT_BYTES
        )));
    }

    Ok(process::Options {
        cwd,
        env,
        timeout: t
            .get::<Option<u64>>("timeout_secs")?
            .map(Duration::from_secs),
        stdin: t.get::<Option<String>>("stdin")?,
        max_output_bytes,
    })
}
