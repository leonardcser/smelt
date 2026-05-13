//! `smelt.grep` — ripgrep wrapper. `rg` exit 1 (no match) is not an error; check `exit_code`.

use crate::grep;
use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::time::Duration;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let grep_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.grep",
        "Ripgrep wrapper for searching files. Exit code 1 (no match) is not an error.",
    );

    register_fn(
        &grep_tbl,
        "smelt.grep",
        "run",
        "Run ripgrep with `pattern` over `path`. `opts` accepts `mode` (`content`/`files_with_matches`/`count`), `case_insensitive`, `multiline`, `line_numbers`, `before_context`/`after_context`/`context`, `glob`, `type`, `timeout_secs`. Returns `(result_table, nil)` on success or `(nil, error_string)` on spawn failure. Exit code 1 (no match) is not an error — inspect `exit_code` on the result.",
        &["pattern", "path", "opts"],
        lua,
        |lua, (pattern, path, opts): (String, String, Option<mlua::Table>)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            let parsed = parse_options(opts.as_ref())?;
            match grep::run(&pattern, &path, &parsed) {
                Ok(out) => Ok((Some(output_to_lua(lua, &out)?), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;

    smelt.set("grep", grep_tbl)?;
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

fn output_to_lua(lua: &Lua, out: &grep::Output) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("stdout", out.stdout.clone())?;
    t.set("stderr", out.stderr.clone())?;
    t.set("exit_code", out.exit_code)?;
    t.set("timed_out", out.timed_out)?;
    Ok(t)
}
