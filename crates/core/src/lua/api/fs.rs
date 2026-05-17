//! `smelt.fs` — sync filesystem primitives. Errors use `(value, err_string)` convention.

use crate::fs::FlockGuard;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fs = LuaMod::under(
        lua,
        smelt,
        "fs",
        "Sync filesystem primitives. Errors use the `(value, err_string)` convention so callers can distinguish failures without pcall.",
        Tier::Host,
    )?;
    fs.fn_(
        "read",
        "Read `p` into a string. Returns `(content, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::read_to_string(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "write",
        "Write `contents` to file `p`, creating it if necessary. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p", "contents"],
        |_, (p, contents): (String, mlua::String)| match crate::fs::write(&p, contents.as_bytes()) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "exists",
        "Return `true` if a filesystem entry exists at `p`.",
        &["p"],
        |_, p: String| Ok(crate::fs::exists(&p)),
    )?;

    fs.fn_(
        "is_file",
        "Return `true` if `p` exists and refers to a regular file.",
        &["p"],
        |_, p: String| Ok(crate::fs::is_file(&p)),
    )?;

    fs.fn_(
        "is_dir",
        "Return `true` if `p` exists and refers to a directory.",
        &["p"],
        |_, p: String| Ok(crate::fs::is_dir(&p)),
    )?;

    fs.fn_(
        "read_dir",
        "List the immediate entries of directory `p`. Returns `(entries, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::read_dir(&p) {
            Ok(entries) => Ok((Some(paths_to_strings(entries)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "mkdir",
        "Create directory `p` (parents must exist). Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::mkdir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "mkdir_all",
        "Create directory `p` along with any missing parent directories. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::mkdir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "remove_file",
        "Delete the file at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::remove_file(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "remove_dir",
        "Delete the empty directory at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::remove_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "remove_dir_all",
        "Recursively delete the directory tree rooted at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::remove_dir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "rename",
        "Rename or move `from` to `to`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["from", "to"],
        |_, (from, to): (String, String)| match crate::fs::rename(&from, &to) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "copy",
        "Copy file `from` to `to`. Returns `(bytes_copied, nil)` on success or `(nil, err_string)` on failure.",
        &["from", "to"],
        |_, (from, to): (String, String)| match crate::fs::copy(&from, &to) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "size",
        "Return the size of file `p` in bytes. Returns `(size, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::size(&p) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    fs.fn_(
        "glob",
        "Find paths matching `pattern` under `path` (defaults to cwd). Returns the matches sorted newest-first, capped at `opts.max` (default 200). On error returns `(nil, err_string)`.",
        &["pattern", "path", "opts"],
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

    let file_state = fs.sub(
        "file_state",
        "Cached file-state tracker used by tools to detect external modifications between reads and writes.",
    )?;

    file_state.fn_(
        "has",
        "Return `true` if the file-state cache has a recorded entry for `p`.",
        &["p"],
        |_, p: String| Ok(crate::host::try_with_core(|core| core.files.has(&p)).unwrap_or(false)),
    )?;

    file_state.fn_(
        "get",
        "Look up the cached file-state entry for `p`. Returns `{ content, mtime_ms, read_range }` or `nil` when no entry exists.",
        &["p"],
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

    file_state.fn_(
        "record_read",
        "Record that `p` was read at byte range `[offset, offset+limit)` with `content` so subsequent staleness checks know what the agent has seen.",
        &["p", "content", "offset", "limit"],
        |_, (p, content, offset, limit): (String, String, u64, u64)| {
            crate::host::try_with_core(|core| {
                core.files
                    .record_read(&p, content, (offset as usize, limit as usize));
            });
            Ok(())
        },
    )?;

    file_state.fn_(
        "record_write",
        "Record that `p` was written with `content` so subsequent staleness checks see the latest state.",
        &["p", "content"],
        |_, (p, content): (String, String)| -> LuaResult<()> {
            crate::host::try_with_core(|core| {
                core.files.record_write(&p, content);
            });
            Ok(())
        },
    )?;

    file_state.fn_(
        "staleness_error",
        "Return an error message describing why the cached state of `p` is stale relative to disk, or `nil` if it is up to date. `noun` (default `\"file\"`) labels the entity in the message.",
        &["p", "noun"],
        |_, (p, noun): (String, Option<String>)| -> LuaResult<Option<String>> {
            let noun = noun.unwrap_or_else(|| "file".into());
            Ok(crate::host::try_with_core(|core| {
                crate::fs::staleness_error(&core.files, &p, &noun)
            })
            .flatten())
        },
    )?;

    file_state.fn_(
        "mtime_ms",
        "Return the modification time of `p` in milliseconds since the UNIX epoch. Returns `(ms, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::fs::file_mtime_ms(&p) {
            Ok(ms) => Ok((Some(ms), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    fs.tbl.set(
        "try_flock",
        lua.create_function(|_, p: String| match crate::fs::try_flock(&p) {
            Ok(guard) => Ok((Some(FlockHandle::new(guard)), None)),
            Err(err) => Ok((None, Some(err))),
        })?,
    )?;

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

fn paths_to_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}
