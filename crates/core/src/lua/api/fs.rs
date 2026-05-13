//! `smelt.fs` — sync filesystem primitives. Errors use `(value, err_string)` convention.

use crate::fs::FlockGuard;
use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fs = lua.create_table()?;
    record_module_doc("smelt.fs", "Sync filesystem primitives. Errors use the `(value, err_string)` convention so callers can distinguish failures without pcall.");

    register_fn(
        &fs,
        "smelt.fs",
        "read",
        "Read `p` into a string. Returns `(content, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::read_to_string(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "write",
        "Write `contents` to file `p`, creating it if necessary. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p", "contents"],
        lua,
        |_, (p, contents): (String, mlua::String)| match crate::fs::write(&p, contents.as_bytes()) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "exists",
        "Return `true` if a filesystem entry exists at `p`.",
        &["p"],
        lua,
        |_, p: String| Ok(crate::fs::exists(&p)),
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "is_file",
        "Return `true` if `p` exists and refers to a regular file.",
        &["p"],
        lua,
        |_, p: String| Ok(crate::fs::is_file(&p)),
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "is_dir",
        "Return `true` if `p` exists and refers to a directory.",
        &["p"],
        lua,
        |_, p: String| Ok(crate::fs::is_dir(&p)),
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "read_dir",
        "List the immediate entries of directory `p`. Returns `(entries, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::read_dir(&p) {
            Ok(entries) => Ok((Some(paths_to_strings(entries)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "mkdir",
        "Create directory `p` (parents must exist). Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::mkdir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "mkdir_all",
        "Create directory `p` along with any missing parent directories. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::mkdir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "remove_file",
        "Delete the file at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::remove_file(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "remove_dir",
        "Delete the empty directory at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::remove_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "remove_dir_all",
        "Recursively delete the directory tree rooted at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::remove_dir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "rename",
        "Rename or move `from` to `to`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["from", "to"],
        lua,
        |_, (from, to): (String, String)| match crate::fs::rename(&from, &to) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "copy",
        "Copy file `from` to `to`. Returns `(bytes_copied, nil)` on success or `(nil, err_string)` on failure.",
        &["from", "to"],
        lua,
        |_, (from, to): (String, String)| match crate::fs::copy(&from, &to) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "mtime",
        "Return the modification time of `p` in seconds since the UNIX epoch. Returns `(secs, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::mtime_secs(&p) {
            Ok(value) => Ok((value, None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "size",
        "Return the size of file `p` in bytes. Returns `(size, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::size(&p) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &fs,
        "smelt.fs",
        "glob",
        "Find paths matching `pattern` under `path` (defaults to cwd). Returns the matches sorted newest-first, capped at `opts.max` (default 200). On error returns `(nil, err_string)`.",
        &["pattern", "path", "opts"],
        lua,
        |_, args: (String, Option<String>, Option<mlua::Table>)| -> LuaResult<(Option<Vec<String>>, Option<String>)> {
            let (pattern, path, opts) = args;
            let dir = path.unwrap_or_default();
            let max = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u64>>("max").ok().flatten())
                .map(|n| n as usize)
                .unwrap_or(200);
            match crate::fs::glob(&pattern, &dir, max) {
                Ok(mut matches) => {
                    matches.sort_by_key(|m| std::cmp::Reverse(m.mtime));
                    let paths: Vec<String> = matches.into_iter().map(|m| m.path).collect();
                    Ok((Some(paths), None))
                }
                Err(err) => Ok((None, Some(err))),
            }
        },
    )?;

    fs.set("file_state", build_file_state(lua)?)?;

    fs.set(
        "try_flock",
        lua.create_function(|_, p: String| match crate::fs::try_flock(&p) {
            Ok(guard) => Ok((Some(FlockHandle::new(guard)), None)),
            Err(err) => Ok((None, Some(err))),
        })?,
    )?;

    smelt.set("fs", fs)?;
    Ok(())
}

struct FlockHandle(RefCell<Option<FlockGuard>>);

impl FlockHandle {
    fn new(guard: FlockGuard) -> Self {
        Self(RefCell::new(Some(guard)))
    }
}

impl LuaUserData for FlockHandle {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("release", |_, this, ()| {
            this.0.borrow_mut().take();
            Ok(())
        });
    }
}

#[lua_module]
fn build_file_state(lua: &Lua) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;

    register_fn(
        &t,
        "smelt.fs",
        "has",
        "Return `true` if the file-state cache has a recorded entry for `p`.",
        &["p"],
        lua,
        |_, p: String| Ok(crate::host::try_with_core(|core| core.files.has(&p)).unwrap_or(false)),
    )?;

    register_fn(
        &t,
        "smelt.fs",
        "get",
        "Look up the cached file-state entry for `p`. Returns `{ content, mtime_ms, read_range }` or `nil` when no entry exists.",
        &["p"],
        lua,
        |lua, p: String| -> LuaResult<mlua::Value> {
            let Some(state) = crate::host::try_with_core(|core| core.files.get(&p)).flatten()
            else {
                return Ok(LuaNil);
            };
            let row = lua.create_table()?;
            row.set("content", state.content)?;
            row.set("mtime_ms", state.mtime_ms)?;
            match state.read_range {
                Some((offset, limit)) => {
                    let range = lua.create_table()?;
                    range.set("offset", offset as u64)?;
                    range.set("limit", limit as u64)?;
                    row.set("read_range", range)?;
                }
                None => row.set("read_range", LuaNil)?,
            }
            Ok(LuaValue::Table(row))
        },
    )?;

    register_fn(
        &t,
        "smelt.fs",
        "record_read",
        "Record that `p` was read at byte range `[offset, offset+limit)` with `content` so subsequent staleness checks know what the agent has seen.",
        &["p", "content", "offset", "limit"],
        lua,
        |_, (p, content, offset, limit): (String, String, u64, u64)| {
            crate::host::try_with_core(|core| {
                core.files
                    .record_read(&p, content, (offset as usize, limit as usize));
            });
            Ok(())
        },
    )?;

    register_fn(
        &t,
        "smelt.fs",
        "record_write",
        "Record that `p` was written with `content` so subsequent staleness checks see the latest state.",
        &["p", "content"],
        lua,
        |_, (p, content): (String, String)|  -> LuaResult<()>{
            crate::host::try_with_core(|core| {
                core.files.record_write(&p, content);
            });
            Ok(())
        },
    )?;

    register_fn(
        &t,
        "smelt.fs",
        "staleness_error",
        "Return an error message describing why the cached state of `p` is stale relative to disk, or `nil` if it is up to date. `noun` (default `\"file\"`) labels the entity in the message.",
        &["p", "noun"],
        lua,
        |_, (p, noun): (String, Option<String>)| -> LuaResult<Option<String>> {
            let noun = noun.unwrap_or_else(|| "file".into());
            Ok(crate::host::try_with_core(|core| {
                crate::fs::staleness_error(&core.files, &p, &noun)
            })
            .flatten())
        },
    )?;

    register_fn(
        &t,
        "smelt.fs",
        "mtime_ms",
        "Return the modification time of `p` in milliseconds since the UNIX epoch. Returns `(ms, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::fs::file_mtime_ms(&p) {
            Ok(ms) => Ok((Some(ms), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    Ok(t)
}

fn paths_to_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}
