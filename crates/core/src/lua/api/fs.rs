//! `smelt.fs` - sync filesystem primitives. Errors use `(value, err_string)` convention.

use crate::fs::FlockGuard;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::watchers::{WatcherEntry, WatcherState};
use crate::lua::LuaShared;
use mlua::prelude::*;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
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
        "read_limited",
        "Read at most `max_bytes` bytes from `p`. Returns `({ content, truncated }, nil)` on success or `(nil, err_string)` on failure.",
        &["p", "max_bytes"],
        |lua, (p, max_bytes): (String, usize)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            match crate::fs::read_to_string_limited(&p, max_bytes) {
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
        "record_read_with_mtime",
        "Record that `p` was read with a caller-provided mtime in milliseconds, avoiding an extra stat call.",
        &["p", "content", "offset", "limit", "mtime_ms"],
        |_, (p, content, offset, limit, mtime_ms): (String, String, u64, u64, u64)| {
            crate::host::try_with_core(|core| {
                core.files.record_read_with_mtime(
                    &p,
                    content,
                    (offset as usize, limit as usize),
                    mtime_ms,
                );
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

    {
        let s = shared.clone();
        fs.private_fn(
            "__start_file_info",
            &["task_id", "path"],
            move |_, (task_id, path): (u64, String)| -> LuaResult<()> {
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
                                    let n = file.read(&mut sample).unwrap_or(0);
                                    sample.truncate(n);
                                    let file_kind = if sample.starts_with(b"%PDF-") {
                                        "pdf"
                                    } else if engine::image::sniff_image_mime(&sample).is_some() {
                                        "image"
                                    } else if std::str::from_utf8(&sample).is_ok() {
                                        "text"
                                    } else {
                                        "binary"
                                    };
                                    serde_json::json!({
                                        "is_file": true,
                                        "len": meta.len(),
                                        "kind": file_kind,
                                        "mtime_ms": crate::fs::file_mtime_ms(&path).unwrap_or(0),
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
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            let mtime_ms = crate::fs::file_mtime_ms(&path).unwrap_or(0);
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
            move |_, (task_id, path, contents): (u64, String, mlua::String)| -> LuaResult<()> {
                let bytes = contents.as_bytes().to_vec();
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match std::fs::write(&path, &bytes) {
                        Ok(()) => {
                            let mtime_ms = crate::fs::file_mtime_ms(&path).unwrap_or(0);
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
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::checked_write_file(&path, &contents, &files) {
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
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::checked_edit_file(
                        &path,
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
                s.resume_sink().spawn_blocking_resolve(task_id, move || {
                    match crate::fs::glob_with_limits(
                        &pattern,
                        &path,
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
                let state = Arc::new(Mutex::new(WatcherState::default()));
                let state_clone = Arc::clone(&state);
                let sink = s.resume_sink();
                let watcher_result = RecommendedWatcher::new(
                    move |res: notify::Result<notify::Event>| {
                        let Ok(event) = res else { return };
                        let payload = event_to_json(&event);
                        // Single critical section: push the event, and if a
                        // task is armed, drain everything in the same lock.
                        // Decoupling the push from the drain creates a window
                        // where __watch_arm could be re-entered and steal the
                        // pending list out from under us - keep them atomic.
                        let resume = {
                            let Ok(mut st) = state_clone.lock() else {
                                return;
                            };
                            if st.closed {
                                return;
                            }
                            st.pending.push(payload);
                            match st.armed.take() {
                                Some(task_id) => Some((task_id, std::mem::take(&mut st.pending))),
                                None => None,
                            }
                        };
                        if let Some((task_id, drained)) = resume {
                            sink.resolve_json(task_id, serde_json::Value::Array(drained));
                        }
                    },
                    Config::default(),
                );
                let mut watcher = match watcher_result {
                    Ok(w) => w,
                    Err(err) => return Ok((None, Some(err.to_string()))),
                };
                if let Err(err) = watcher.watch(Path::new(&path), mode) {
                    return Ok((None, Some(err.to_string())));
                }
                let entry = WatcherEntry {
                    state,
                    _watcher: watcher,
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

fn event_to_json(event: &notify::Event) -> serde_json::Value {
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
    let (kind, detail) = match event.kind {
        EventKind::Create(k) => (
            "create",
            Some(match k {
                CreateKind::File => "file",
                CreateKind::Folder => "folder",
                CreateKind::Any => "any",
                CreateKind::Other => "other",
            }),
        ),
        EventKind::Modify(m) => match m {
            ModifyKind::Name(RenameMode::From) => ("rename", Some("from")),
            ModifyKind::Name(RenameMode::To) => ("rename", Some("to")),
            ModifyKind::Name(RenameMode::Both) => ("rename", Some("both")),
            ModifyKind::Name(_) => ("rename", None),
            ModifyKind::Data(_) => ("modify", Some("data")),
            ModifyKind::Metadata(_) => ("modify", Some("metadata")),
            ModifyKind::Any => ("modify", Some("any")),
            ModifyKind::Other => ("modify", Some("other")),
        },
        EventKind::Remove(k) => (
            "remove",
            Some(match k {
                RemoveKind::File => "file",
                RemoveKind::Folder => "folder",
                RemoveKind::Any => "any",
                RemoveKind::Other => "other",
            }),
        ),
        EventKind::Access(a) => (
            "access",
            Some(match a {
                AccessKind::Open(_) => "open",
                AccessKind::Close(AccessMode::Write) => "close_write",
                AccessKind::Close(_) => "close",
                AccessKind::Read => "read",
                AccessKind::Any => "any",
                AccessKind::Other => "other",
            }),
        ),
        EventKind::Other => ("other", None),
        EventKind::Any => ("any", None),
    };
    let paths: Vec<String> = event
        .paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let mut obj = serde_json::Map::new();
    obj.insert("kind".into(), serde_json::Value::String(kind.into()));
    if let Some(d) = detail {
        obj.insert("detail".into(), serde_json::Value::String(d.into()));
    }
    obj.insert(
        "paths".into(),
        serde_json::Value::Array(paths.into_iter().map(serde_json::Value::String).collect()),
    );
    serde_json::Value::Object(obj)
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
