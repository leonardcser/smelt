//! `smelt.grep` — ripgrep wrapper. `rg` exit 1 (no match) is not an error; check `exit_code`.

use crate::grep;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "grep",
        "Ripgrep wrapper for searching files. Exit code 1 (no match) is not an error.",
        Tier::Host,
    )?;
    {
        let s = Arc::clone(shared);
        m.fn_(
            "__run_async_start",
            "Begin an async ripgrep run for `pattern` over `path`. Resolves `task_id` with `{ stdout, stderr, exit_code, timed_out }` on completion, `{ __cancelled = true }` if the calling coroutine is cancelled (child is killed), or `{ err }` on spawn failure. `opts` mirrors `smelt.grep.run`. Used internally by `smelt.grep.run`.",
            &["task_id", "pattern", "path", "opts"],
            move |_, (task_id, pattern, path, opts): (u64, String, String, Option<mlua::Table>)| -> LuaResult<()> {
                let parsed = parse_options(opts.as_ref())?;
                let cancel = crate::lua::current_task_cancel().unwrap_or_default();
                let sink = s.resume_sink();
                tokio::spawn(async move {
                    let payload = match grep::run_async(&pattern, &path, &parsed, cancel).await {
                        Ok(grep::RunOutcome::Done(out)) => serde_json::json!({
                            "stdout": out.stdout,
                            "stderr": out.stderr,
                            "exit_code": out.exit_code,
                            "timed_out": out.timed_out,
                        }),
                        Ok(grep::RunOutcome::Cancelled) => {
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

    Ok(())
}

fn parse_options(opts: Option<&mlua::Table>) -> LuaResult<grep::Options> {
    let Some(t) = opts else {
        return Ok(grep::Options::default());
    };

    let mode = match t.get::<Option<String>>("mode")?.as_deref() {
        Some("files_with_matches") => grep::Mode::FilesWithMatches,
        Some("count") => grep::Mode::Count,
        Some("content") | None => grep::Mode::Content,
        Some(other) => {
            return Err(LuaError::RuntimeError(format!(
                "unknown grep mode: {other}"
            )));
        }
    };

    Ok(grep::Options {
        mode,
        case_insensitive: t.get::<Option<bool>>("case_insensitive")?.unwrap_or(false),
        multiline: t.get::<Option<bool>>("multiline")?.unwrap_or(false),
        line_numbers: t.get::<Option<bool>>("line_numbers")?.unwrap_or(false),
        before_context: t.get::<Option<u32>>("before_context")?.unwrap_or(0),
        after_context: t.get::<Option<u32>>("after_context")?.unwrap_or(0),
        context: t.get::<Option<u32>>("context")?.unwrap_or(0),
        glob: t.get::<Option<String>>("glob")?,
        file_type: t.get::<Option<String>>("type")?,
        timeout: t
            .get::<Option<u64>>("timeout_secs")?
            .map(Duration::from_secs),
    })
}
