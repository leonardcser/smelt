//! `smelt.fs` - sync filesystem primitives. Errors use `(value, err_string)` convention.

use crate::fs::FlockGuard;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::watchers::WatcherEntry;
use crate::lua::LuaShared;
use mlua::prelude::*;
use notify::RecursiveMode;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let fs = LuaMod::under(
        lua,
        smelt,
        "fs",
        "Sync filesystem primitives. Errors use the `(value, err_string)` convention so callers can distinguish failures without pcall.",
        Tier::Host,
    )?;
    let read_context = Arc::clone(shared);
    fs.fn_(
        "read",
        "Read `p` into a string. Returns `(content, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::read_to_string(read_context.resolve_project_path(p)) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    let read_limited_context = Arc::clone(shared);
    fs.fn_(
        "read_limited",
        "Read at most `max_bytes` bytes from `p`. Returns `({ content, truncated }, nil)` on success or `(nil, err_string)` on failure.",
        &["p", "max_bytes"],
        move |lua, (p, max_bytes): (String, usize)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            match crate::fs::read_to_string_limited(read_limited_context.resolve_project_path(p), max_bytes) {
                Ok(read) => {
                    let t = lua.create_table()?;
                    t.set("content", read.content)?;
                    t.set("truncated", read.truncated)?;
                    Ok((Some(t), None))
                }
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;

    let write_context = Arc::clone(shared);
    fs.fn_(
        "write",
        "Write `contents` to file `p`, creating it if necessary. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p", "contents"],
        move |_, (p, contents): (String, mlua::LuaString)| {
            match crate::fs::write(write_context.resolve_project_path(p), contents.as_bytes()) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        },
    )?;

    let exists_context = Arc::clone(shared);
    fs.fn_(
        "exists",
        "Return `true` if a filesystem entry exists at `p`.",
        &["p"],
        move |_, p: String| Ok(crate::fs::exists(exists_context.resolve_project_path(p))),
    )?;

    let is_file_context = Arc::clone(shared);
    fs.fn_(
        "is_file",
        "Return `true` if `p` exists and refers to a regular file.",
        &["p"],
        move |_, p: String| Ok(crate::fs::is_file(is_file_context.resolve_project_path(p))),
    )?;

    let is_dir_context = Arc::clone(shared);
    fs.fn_(
        "is_dir",
        "Return `true` if `p` exists and refers to a directory.",
        &["p"],
        move |_, p: String| Ok(crate::fs::is_dir(is_dir_context.resolve_project_path(p))),
    )?;

    let read_dir_context = Arc::clone(shared);
    fs.fn_(
        "read_dir",
        "List the immediate entries of directory `p`. Returns `(entries, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::read_dir(read_dir_context.resolve_project_path(p)) {
            Ok(entries) => Ok((Some(paths_to_strings(entries)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    let completion_context = Arc::clone(shared);
    fs.fn_(
        "complete_path",
        "List immediate filesystem completions under `dir` matching `prefix`. Directory entries are returned first, then files, case-insensitive alphabetical, capped by `opts.limit` (default 200). Hidden names are included only when `prefix` starts with `.`. `opts.insert_prefix` controls inserted text and defaults to `dir` with a trailing separator. Returns `({ items }, nil)` or `(nil, err_string)`. Items are `{ label, path, insert_text, kind, description }`.",
        &["dir", "prefix", "opts"],
        move |lua, (dir, prefix, opts): (String, String, Option<mlua::Table>)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            let limit = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u64>>("limit").ok().flatten())
                .map(|n| n as usize)
                .unwrap_or(200);
            let insert_prefix = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("insert_prefix").ok().flatten())
                .unwrap_or_else(|| default_insert_prefix(&dir));
            let scan_dir = completion_context.resolve_project_path(&dir);
            match complete_path_rows(&scan_dir.to_string_lossy(), &prefix, &insert_prefix, limit) {
                Ok(rows) => complete_path_to_lua(lua, rows).map(|table| (Some(table), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;

    let mkdir_context = Arc::clone(shared);
    fs.fn_(
        "mkdir",
        "Create directory `p` (parents must exist). Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::mkdir(mkdir_context.resolve_project_path(p)) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    let mkdir_all_context = Arc::clone(shared);
    fs.fn_(
        "mkdir_all",
        "Create directory `p` along with any missing parent directories. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::mkdir_all(mkdir_all_context.resolve_project_path(p)) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    let remove_file_context = Arc::clone(shared);
    fs.fn_(
        "remove_file",
        "Delete the file at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::remove_file(remove_file_context.resolve_project_path(p)) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    let remove_dir_context = Arc::clone(shared);
    fs.fn_(
        "remove_dir",
        "Delete the empty directory at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::remove_dir(remove_dir_context.resolve_project_path(p)) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    let remove_dir_all_context = Arc::clone(shared);
    fs.fn_(
        "remove_dir_all",
        "Recursively delete the directory tree rooted at `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::remove_dir_all(remove_dir_all_context.resolve_project_path(p)) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    let rename_context = Arc::clone(shared);
    fs.fn_(
        "rename",
        "Rename or move `from` to `to`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["from", "to"],
        move |_, (from, to): (String, String)| {
            match crate::fs::rename(
                rename_context.resolve_project_path(from),
                rename_context.resolve_project_path(to),
            ) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        },
    )?;

    let copy_context = Arc::clone(shared);
    fs.fn_(
        "copy",
        "Copy file `from` to `to`. Returns `(bytes_copied, nil)` on success or `(nil, err_string)` on failure.",
        &["from", "to"],
        move |_, (from, to): (String, String)| {
            match crate::fs::copy(
                copy_context.resolve_project_path(from),
                copy_context.resolve_project_path(to),
            ) {
                Ok(n) => Ok((Some(n), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;

    let size_context = Arc::clone(shared);
    fs.fn_(
        "size",
        "Return the size of file `p` in bytes. Returns `(size, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::size(size_context.resolve_project_path(p)) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    let glob_context = Arc::clone(shared);
    fs.fn_(
        "glob",
        "Find paths matching `pattern` under `path` (defaults to cwd). Returns the matches sorted newest-first, capped at `opts.max` (default 200). On error returns `(nil, err_string)`.",
        &["pattern", "path", "opts"],
        move |_, args: (String, Option<String>, Option<mlua::Table>)| -> LuaResult<(Option<Vec<String>>, Option<String>)> {
            let (pattern, path, opts) = args;
            let dir = glob_context.resolve_project_path(path.unwrap_or_default());
            let max = opts
                .as_ref()
                .and_then(|t| t.get::<Option<u64>>("max").ok().flatten())
                .map(|n| n as usize)
                .unwrap_or(200);
            match crate::fs::glob(&pattern, &dir.to_string_lossy(), max) {
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

    let file_state_has_context = Arc::clone(shared);
    file_state.fn_(
        "has",
        "Return `true` if the file-state cache has a recorded entry for `p`.",
        &["p"],
        move |_, p: String| {
            let p = file_state_has_context.resolve_project_path(p);
            Ok(
                crate::host::try_with_core(|core| core.files.has(&p.to_string_lossy()))
                    .unwrap_or(false),
            )
        },
    )?;

    let file_state_get_context = Arc::clone(shared);
    file_state.fn_(
        "get",
        "Look up the cached file-state entry for `p`. Returns `{ content, mtime_ms, read_range }` or `nil` when no entry exists.",
        &["p"],
        move |lua, p: String| -> LuaResult<mlua::Value> {
            let p = file_state_get_context.resolve_project_path(p);
            let Some(state) =
                crate::host::try_with_core(|core| core.files.get(&p.to_string_lossy())).flatten()
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

    let record_read_context = Arc::clone(shared);
    file_state.fn_(
        "record_read",
        "Record that `p` was read at byte range `[offset, offset+limit)` with `content` so subsequent staleness checks know what the agent has seen.",
        &["p", "content", "offset", "limit"],
        move |_, (p, content, offset, limit): (String, String, u64, u64)| {
            let p = record_read_context.resolve_project_path(p);
            crate::host::try_with_core(|core| {
                core.files.record_read(
                    &p.to_string_lossy(),
                    content,
                    (offset as usize, limit as usize),
                );
            });
            Ok(())
        },
    )?;

    let record_read_with_mtime_context = Arc::clone(shared);
    file_state.fn_(
        "record_read_with_mtime",
        "Record that `p` was read with a caller-provided mtime in milliseconds, avoiding an extra stat call.",
        &["p", "content", "offset", "limit", "mtime_ms"],
        move |_, (p, content, offset, limit, mtime_ms): (String, String, u64, u64, u64)| {
            let p = record_read_with_mtime_context.resolve_project_path(p);
            crate::host::try_with_core(|core| {
                core.files.record_read_with_mtime(
                    &p.to_string_lossy(),
                    content,
                    (offset as usize, limit as usize),
                    mtime_ms,
                );
            });
            Ok(())
        },
    )?;

    let record_write_context = Arc::clone(shared);
    file_state.fn_(
        "record_write",
        "Record that `p` was written with `content` so subsequent staleness checks see the latest state.",
        &["p", "content"],
        move |_, (p, content): (String, String)| -> LuaResult<()> {
            let p = record_write_context.resolve_project_path(p);
            crate::host::try_with_core(|core| {
                core.files.record_write(&p.to_string_lossy(), content);
            });
            Ok(())
        },
    )?;

    let staleness_context = Arc::clone(shared);
    file_state.fn_(
        "staleness_error",
        "Return an error message describing why the cached state of `p` is stale relative to disk, or `nil` if it is up to date. `noun` (default `\"file\"`) labels the entity in the message.",
        &["p", "noun"],
        move |_, (p, noun): (String, Option<String>)| -> LuaResult<Option<String>> {
            let noun = noun.unwrap_or_else(|| "file".into());
            let p = staleness_context.resolve_project_path(p);
            Ok(crate::host::try_with_core(|core| {
                crate::fs::staleness_error(&core.files, &p.to_string_lossy(), &noun)
            })
            .flatten())
        },
    )?;

    let mtime_context = Arc::clone(shared);
    file_state.fn_(
        "mtime_ms",
        "Return the modification time of `p` in milliseconds since the UNIX epoch. Returns `(ms, nil)` or `(nil, err_string)` on failure.",
        &["p"],
        move |_, p: String| match crate::fs::file_mtime_ms(
            &mtime_context.resolve_project_path(p).to_string_lossy(),
        ) {
            Ok(ms) => Ok((Some(ms), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    let flock_context = Arc::clone(shared);
    fs.tbl.set(
        "try_flock",
        lua.create_function(move |_, p: String| {
            let path = flock_context.resolve_project_path(p);
            match crate::fs::try_flock(&path.to_string_lossy()) {
                Ok(guard) => Ok((Some(FlockHandle::new(guard)), None)),
                Err(err) => Ok((None, Some(err))),
            }
        })?,
    )?;

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_file_info",
            &["task_id", "path"],
            move |_, (task_id, path): (u64, String)| -> LuaResult<()> {
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::metadata(&path) {
                        Ok(meta) => {
                            if !meta.is_file() {
                                return serde_json::json!({ "err": "not a regular file" });
                            }
                            match std::fs::File::open(&path) {
                                Ok(mut file) => {
                                    use std::io::Read;
                                    let mut sample = vec![0u8; 8192];
                                    let n = match file.read(&mut sample) {
                                        Ok(n) => n,
                                        Err(err) => {
                                            return serde_json::json!({ "err": err.to_string() });
                                        }
                                    };
                                    sample.truncate(n);
                                    let file_kind =
                                        classify_file_sample(&sample, meta.len() > n as u64);
                                    serde_json::json!({
                                        "is_file": true,
                                        "len": meta.len(),
                                        "kind": file_kind,
                                        "mtime_ms": crate::fs::file_mtime_ms(&path.to_string_lossy()).unwrap_or(0),
                                    })
                                }
                                Err(err) => serde_json::json!({ "err": err.to_string() }),
                            }
                        }
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_read",
            &["task_id", "path"],
            move |_, (task_id, path): (u64, String)| -> LuaResult<()> {
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            let mtime_ms =
                                crate::fs::file_mtime_ms(&path.to_string_lossy()).unwrap_or(0);
                            serde_json::json!({ "content": content, "mtime_ms": mtime_ms })
                        }
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_write",
            &["task_id", "path", "contents"],
            move |_, (task_id, path, contents): (u64, String, mlua::LuaString)| -> LuaResult<()> {
                let path = s.resolve_project_path(path);
                let bytes = contents.as_bytes().to_vec();
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => {
                            let mtime_ms =
                                crate::fs::file_mtime_ms(&path.to_string_lossy()).unwrap_or(0);
                            serde_json::json!({ "ok": true, "mtime_ms": mtime_ms })
                        }
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_write_file",
            &["task_id", "path", "contents"],
            move |_, (task_id, path, contents): (u64, String, String)| -> LuaResult<()> {
                let files = crate::host::try_with_core(|core| core.files.clone());
                let Some(files) = files else {
                    s.resume_sink().resolve_json(
                        task_id,
                        serde_json::json!({ "err": "write_file: no app context" }),
                    );
                    return Ok(());
                };
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::checked_write_file(&path.to_string_lossy(), &contents, &files)
                    {
                        Ok(bytes) => serde_json::json!({ "bytes": bytes }),
                        Err(err) => serde_json::json!({ "err": err }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__plan_edit_file",
            &["path", "old_string", "new_string", "replace_all"],
            move |lua,
                  (path, old_string, new_string, replace_all): (String, String, String, bool)|
                  -> LuaResult<mlua::Table> {
                let result = match crate::host::try_with_core(|core| core.files.clone()) {
                    Some(files) => {
                        let path = s.resolve_project_path(path);
                        crate::fs::checked_plan_edit_file(
                            &path.to_string_lossy(),
                            &old_string,
                            &new_string,
                            replace_all,
                            &files,
                        )
                    }
                    None => Err("edit_file: no app context".into()),
                };
                let plan = lua.create_table()?;
                match result {
                    Ok(outcome) => {
                        plan.set("old_content", outcome.old_content)?;
                        plan.set("new_content", outcome.new_content)?;
                    }
                    Err(err) => plan.set("err", err)?,
                }
                Ok(plan)
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_edit_file",
            &["task_id", "path", "old_string", "new_string", "replace_all"],
            move |_,
                  (task_id, path, old_string, new_string, replace_all): (
                u64,
                String,
                String,
                String,
                bool,
            )|
                  -> LuaResult<()> {
                let files = crate::host::try_with_core(|core| core.files.clone());
                let Some(files) = files else {
                    s.resume_sink().resolve_json(
                        task_id,
                        serde_json::json!({ "err": "edit_file: no app context" }),
                    );
                    return Ok(());
                };
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::checked_edit_file(
                        &path.to_string_lossy(),
                        &old_string,
                        &new_string,
                        replace_all,
                        &files,
                    ) {
                        Ok(outcome) => serde_json::json!({
                            "old_content": outcome.old_content,
                            "new_content": outcome.new_content,
                        }),
                        Err(err) => serde_json::json!({ "err": err }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_mkdir_all",
            &["task_id", "path"],
            move |_, (task_id, path): (u64, String)| -> LuaResult<()> {
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::create_dir_all(&path) {
                        Ok(()) => serde_json::json!({ "ok": true }),
                        Err(err) => serde_json::json!({ "err": err.to_string() }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_glob",
            &["task_id", "pattern", "path", "opts"],
            move |_,
                  (task_id, pattern, path, opts): (u64, String, String, Option<mlua::Table>)|
                  -> LuaResult<()> {
                let max = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("max").ok().flatten())
                    .map(|n| n as usize)
                    .unwrap_or(200);
                let max_scanned = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("max_scanned").ok().flatten())
                    .map(|n| n as usize)
                    .unwrap_or(100_000);
                let timeout = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("timeout_ms").ok().flatten())
                    .map(std::time::Duration::from_millis)
                    .unwrap_or_else(|| std::time::Duration::from_secs(30));
                let path = s.resolve_project_path(path);
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::glob_with_limits(
                        &pattern,
                        &path.to_string_lossy(),
                        max,
                        Some(max_scanned),
                        Some(timeout),
                    ) {
                        Ok(mut search) => {
                            search.matches.sort_by_key(|m| std::cmp::Reverse(m.mtime));
                            let paths: Vec<String> =
                                search.matches.into_iter().map(|m| m.path).collect();
                            serde_json::json!({
                                "paths": paths,
                                "scanned": search.scanned,
                                "truncated": search.match_limit_hit,
                                "scan_limit_hit": search.scan_limit_hit,
                                "timed_out": search.timed_out,
                            })
                        }
                        Err(err) => serde_json::json!({ "err": err }),
                    }
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__watch_register",
            &["path", "opts"],
            move |_,
                  (path, opts): (String, Option<mlua::Table>)|
                  -> LuaResult<(Option<u64>, Option<String>)> {
                let recursive = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("recursive").ok().flatten())
                    .unwrap_or(true);
                let mode = if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                let id = s.next_watcher_id.fetch_add(1, Ordering::Relaxed);
                let entry = match WatcherEntry::new(
                    s.resolve_project_path(path),
                    mode,
                    s.resume_sink(),
                    s.external_effects_active(),
                ) {
                    Ok(entry) => entry,
                    Err(error) => return Ok((None, Some(error))),
                };
                if let Ok(mut map) = s.watchers.lock() {
                    map.insert(id, entry);
                }
                Ok((Some(id), None))
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__watch_arm",
            &["watcher_id", "task_id"],
            move |_, (watcher_id, task_id): (u64, u64)| -> LuaResult<()> {
                let sink = s.resume_sink();
                let Ok(map) = s.watchers.lock() else {
                    sink.resolve_json(task_id, serde_json::Value::Null);
                    return Ok(());
                };
                let Some(entry) = map.get(&watcher_id) else {
                    sink.resolve_json(task_id, serde_json::Value::Null);
                    return Ok(());
                };
                let drained = {
                    let Ok(mut st) = entry.state.lock() else {
                        sink.resolve_json(task_id, serde_json::Value::Null);
                        return Ok(());
                    };
                    if st.closed {
                        sink.resolve_json(task_id, serde_json::Value::Null);
                        return Ok(());
                    }
                    if st.pending.is_empty() {
                        st.armed = Some(task_id);
                        None
                    } else {
                        Some(std::mem::take(&mut st.pending))
                    }
                };
                if let Some(events) = drained {
                    sink.resolve_json(task_id, serde_json::Value::Array(events));
                }
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        fs.private_fn(
            "__watch_stop",
            &["watcher_id"],
            move |_, watcher_id: u64| -> LuaResult<()> {
                let entry = s
                    .watchers
                    .lock()
                    .ok()
                    .and_then(|mut m| m.remove(&watcher_id));
                if let Some(entry) = entry {
                    let armed = entry.state.lock().ok().and_then(|mut st| {
                        st.closed = true;
                        st.armed.take()
                    });
                    if let Some(task_id) = armed {
                        s.resume_sink()
                            .resolve_json(task_id, serde_json::Value::Null);
                    }
                }
                Ok(())
            },
        )?;
    }

    Ok(())
}

fn classify_file_sample(sample: &[u8], has_more: bool) -> &'static str {
    if sample.starts_with(b"%PDF-") {
        "pdf"
    } else if engine::image::sniff_image_mime(sample).is_some() {
        "image"
    } else {
        match std::str::from_utf8(sample) {
            Ok(_) => "text",
            // A fixed-size sample may stop inside a valid multibyte character.
            Err(err) if has_more && err.error_len().is_none() => "text",
            Err(_) => "binary",
        }
    }
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

#[derive(Debug)]
struct PathCompletionRow {
    label: String,
    path: String,
    insert_text: String,
    kind: &'static str,
}

fn complete_path_rows(
    dir: &str,
    prefix: &str,
    insert_prefix: &str,
    limit: usize,
) -> std::io::Result<Vec<PathCompletionRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let include_hidden = prefix.starts_with('.');
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty()
            || (!include_hidden && name.starts_with('.'))
            || !name.starts_with(prefix)
        {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        let label = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        rows.push(PathCompletionRow {
            insert_text: format!("{insert_prefix}{label}"),
            label,
            path: entry.path().to_string_lossy().into_owned(),
            kind: if is_dir { "dir" } else { "file" },
        });
    }
    rows.sort_by_cached_key(|row| {
        (
            kind_rank(row.kind),
            row.label.to_lowercase(),
            row.label.clone(),
        )
    });
    if rows.len() > limit {
        rows.truncate(limit);
    }
    Ok(rows)
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "dir" => 0,
        _ => 1,
    }
}

fn default_insert_prefix(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else if dir.ends_with(std::path::MAIN_SEPARATOR) {
        dir.to_owned()
    } else {
        format!("{dir}{}", std::path::MAIN_SEPARATOR)
    }
}

fn complete_path_to_lua(lua: &Lua, rows: Vec<PathCompletionRow>) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("status", if rows.is_empty() { "empty" } else { "ready" })?;
    table.set("ready", true)?;
    let items = lua.create_table_with_capacity(rows.len(), 0)?;
    for (idx, row) in rows.into_iter().enumerate() {
        let item = lua.create_table_with_capacity(0, 6)?;
        item.set("label", row.label)?;
        item.set("path", row.path)?;
        item.set("insert_text", row.insert_text)?;
        item.set("kind", row.kind)?;
        if row.kind == "dir" {
            item.set("description", "directory")?;
        }
        items.raw_set(idx + 1, item)?;
    }
    table.set("items", items)?;
    Ok(table)
}

fn paths_to_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::classify_file_sample;

    #[test]
    fn file_sample_split_inside_utf8_character_is_text() {
        let content = format!("{}─", "a".repeat(8191));
        let sample = &content.as_bytes()[..8192];

        assert!(std::str::from_utf8(sample).is_err());
        assert_eq!(classify_file_sample(sample, true), "text");
    }

    #[test]
    fn incomplete_utf8_at_end_of_file_is_binary() {
        assert_eq!(classify_file_sample(b"text\xe2", false), "binary");
    }

    #[test]
    fn malformed_utf8_inside_sample_is_binary() {
        assert_eq!(classify_file_sample(b"text\xffmore", true), "binary");
    }
}
