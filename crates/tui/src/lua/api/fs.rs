//! `smelt.fs` bindings — sync filesystem primitives over `tui::fs`.
//! Host-tier (works in tui and headless) — no Ui touch.
//!
//! Errors flow through the `(value, err)` Lua convention: success
//! returns `(value, nil)`, failure returns `(nil, error_string)`. This
//! lets plugin code do `local data, err = smelt.fs.read(p)` without
//! `pcall`.

use engine::tools::FlockGuard;
use mlua::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fs = lua.create_table()?;

    fs.set(
        "read",
        lua.create_function(|_, p: String| match crate::fs::read_to_string(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "write",
        lua.create_function(|_, (p, contents): (String, mlua::String)| {
            match crate::fs::write(&p, contents.as_bytes()) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        })?,
    )?;

    fs.set(
        "exists",
        lua.create_function(|_, p: String| Ok(crate::fs::exists(&p)))?,
    )?;

    fs.set(
        "is_file",
        lua.create_function(|_, p: String| Ok(crate::fs::is_file(&p)))?,
    )?;

    fs.set(
        "is_dir",
        lua.create_function(|_, p: String| Ok(crate::fs::is_dir(&p)))?,
    )?;

    fs.set(
        "read_dir",
        lua.create_function(|_, p: String| match crate::fs::read_dir(&p) {
            Ok(entries) => Ok((Some(paths_to_strings(entries)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "mkdir",
        lua.create_function(|_, p: String| match crate::fs::mkdir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "mkdir_all",
        lua.create_function(|_, p: String| match crate::fs::mkdir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_file",
        lua.create_function(|_, p: String| match crate::fs::remove_file(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_dir",
        lua.create_function(|_, p: String| match crate::fs::remove_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_dir_all",
        lua.create_function(|_, p: String| match crate::fs::remove_dir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "rename",
        lua.create_function(|_, (from, to): (String, String)| {
            match crate::fs::rename(&from, &to) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        })?,
    )?;

    fs.set(
        "copy",
        lua.create_function(
            |_, (from, to): (String, String)| match crate::fs::copy(&from, &to) {
                Ok(n) => Ok((Some(n), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            },
        )?,
    )?;

    fs.set(
        "mtime",
        lua.create_function(|_, p: String| match crate::fs::mtime_secs(&p) {
            Ok(value) => Ok((value, None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "size",
        lua.create_function(|_, p: String| match crate::fs::size(&p) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "glob",
        lua.create_function(|_, args: (String, Option<String>, Option<mlua::Table>)| {
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
        })?,
    )?;

    fs.set("file_state", build_file_state(lua)?)?;

    fs.set(
        "try_flock",
        lua.create_function(|_, p: String| match engine::tools::try_flock(&p) {
            Ok(guard) => Ok((Some(FlockHandle::new(guard)), None)),
            Err(err) => Ok((None, Some(err))),
        })?,
    )?;

    smelt.set("fs", fs)?;
    Ok(())
}

/// Userdata wrapper for an exclusive advisory lock acquired via
/// `engine::tools::try_flock`. Released on `:release()` or when garbage
/// collected. Lua tools that mutate a file under a flock acquire one of
/// these and let it drop when the write completes.
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

/// `smelt.fs.file_state` — shared mtime + content + read-range cache.
/// Read by Lua `read_file` / `write_file` / `edit_file` / `notebook_edit`
/// during their migration off the engine impls. Backed by the same
/// `engine::tools::FileStateCache` engine-side tools see, parked on
/// `Core.files`.
fn build_file_state(lua: &Lua) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;

    t.set(
        "has",
        lua.create_function(|_, p: String| {
            Ok(crate::lua::try_with_app(|app| app.core.files.has(&p)).unwrap_or(false))
        })?,
    )?;

    t.set(
        "get",
        lua.create_function(|lua, p: String| {
            let Some(state) = crate::lua::try_with_app(|app| app.core.files.get(&p)).flatten()
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
        })?,
    )?;

    t.set(
        "record_read",
        lua.create_function(
            |_, (p, content, offset, limit): (String, String, u64, u64)| {
                crate::lua::try_with_app(|app| {
                    app.core
                        .files
                        .record_read(&p, content, (offset as usize, limit as usize));
                });
                Ok(())
            },
        )?,
    )?;

    t.set(
        "record_write",
        lua.create_function(|_, (p, content): (String, String)| {
            crate::lua::try_with_app(|app| {
                app.core.files.record_write(&p, content);
            });
            Ok(())
        })?,
    )?;

    t.set(
        "staleness_error",
        lua.create_function(|_, (p, noun): (String, Option<String>)| {
            let noun = noun.unwrap_or_else(|| "file".into());
            Ok(crate::lua::try_with_app(|app| {
                engine::tools::staleness_error(&app.core.files, &p, &noun)
            })
            .flatten())
        })?,
    )?;

    t.set(
        "mtime_ms",
        lua.create_function(|_, p: String| match engine::tools::file_mtime_ms(&p) {
            Ok(ms) => Ok((Some(ms), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    Ok(t)
}

fn paths_to_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}
