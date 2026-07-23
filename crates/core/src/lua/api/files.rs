//! `smelt.files` - warm workspace file search.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::workspace_files::{
    AcceptRequest, ItemKind, SearchRequest, SearchResponse, WorkspaceFilesStatus,
};
use mlua::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let files = LuaMod::under(
        lua,
        smelt,
        "files",
        "Workspace file search. Owns background indexing, filesystem watching, and recent-selection ranking.",
        Tier::Host,
    )?;

    let search_context = Arc::clone(shared);
    files.fn_(
        "search",
        "Search workspace files. Returns `{ items, total_matched, total_files, total_dirs, scanned, scanning, searching, ready, status, message, root }`. Items are `{ label, path, insert_text, kind, score }`. Options: `{ limit?, offset?, include_dirs?, cwd? }`.",
        &["query", "opts"],
        move |lua, (query, opts): (String, Option<mlua::Table>)| -> LuaResult<mlua::Table> {
            let _perf = smelt_perf::perf::begin("files:search");
            let opts = SearchOpts::from_lua(opts)?;
            let response = crate::host::try_with_core(|core| {
                let cwd = opts
                    .cwd
                    .map(|cwd| search_context.resolve_project_path(cwd))
                    .unwrap_or_else(|| {
                        if search_context.external_effects_active() {
                            core.env.cwd()
                        } else {
                            search_context.evaluation_cwd()
                        }
                    });
                core.workspace_files.search_interactive(SearchRequest {
                    query,
                    cwd,
                    limit: opts.limit,
                    offset: opts.offset,
                    include_dirs: opts.include_dirs,
                })
            });
            match response {
                Some(Ok(response)) => {
                    let _perf = smelt_perf::perf::begin("files:search:lua_response");
                    response_to_lua(lua, response)
                }
                Some(Err(err)) => error_response_to_lua(lua, err),
                None => error_response_to_lua(lua, "files.search: no app context".to_string()),
            }
        },
    )?;

    let accept_context = Arc::clone(shared);
    files.fn_(
        "accept",
        "Record a selected file result for recent-selection ranking. Options: `{ cwd? }`. Returns `(true, nil)` or `(false, err)`.",
        &["item", "opts"],
        move |_, (item, opts): (mlua::Table, Option<mlua::Table>)| -> LuaResult<(bool, Option<String>)> {
            let opts = AcceptOpts::from_lua(opts)?;
            let path = item
                .get::<Option<String>>("path")?
                .or_else(|| item.get::<Option<String>>("label").ok().flatten())
                .unwrap_or_default();
            if path.is_empty() {
                return Ok((false, Some("files.accept: item.path is required".to_string())));
            }
            let result = crate::host::try_with_core(|core| {
                let cwd = opts
                    .cwd
                    .map(|cwd| accept_context.resolve_project_path(cwd))
                    .unwrap_or_else(|| {
                        if accept_context.external_effects_active() {
                            core.env.cwd()
                        } else {
                            accept_context.evaluation_cwd()
                        }
                    });
                core.workspace_files.accept(AcceptRequest { cwd, path })
            });
            match result {
                Some(Ok(())) => Ok((true, None)),
                Some(Err(err)) => Ok((false, Some(err))),
                None => Ok((false, Some("files.accept: no app context".to_string()))),
            }
        },
    )?;

    let status_context = Arc::clone(shared);
    files.fn_(
        "status",
        "Return indexing status for the current workspace or `opts.cwd`: `{ root, initialized, files, scanned, scanning, watcher_ready, warmup_complete }`.",
        &["opts"],
        move |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let cwd = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten())
                .map(PathBuf::from);
            let result = crate::host::try_with_core(|core| {
                let cwd = cwd
                    .map(|cwd| status_context.resolve_project_path(cwd))
                    .unwrap_or_else(|| {
                        if status_context.external_effects_active() {
                            core.env.cwd()
                        } else {
                            status_context.evaluation_cwd()
                        }
                    });
                core.workspace_files.status(&cwd)
            });
            match result {
                Some(Ok(status)) => status_to_lua(lua, status),
                Some(Err(err)) => error_status_to_lua(lua, err),
                None => error_status_to_lua(lua, "files.status: no app context".to_string()),
            }
        },
    )?;

    let rescan_context = Arc::clone(shared);
    files.fn_(
        "rescan",
        "Trigger an asynchronous full rescan for the current workspace or `opts.cwd`. Returns `(true, nil)` or `(false, err)`.",
        &["opts"],
        move |_, opts: Option<mlua::Table>| -> LuaResult<(bool, Option<String>)> {
            let cwd = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten())
                .map(PathBuf::from);
            let result = crate::host::try_with_core(|core| {
                let cwd = cwd
                    .map(|cwd| rescan_context.resolve_project_path(cwd))
                    .unwrap_or_else(|| {
                        if rescan_context.external_effects_active() {
                            core.env.cwd()
                        } else {
                            rescan_context.evaluation_cwd()
                        }
                    });
                core.workspace_files.rescan(&cwd)
            });
            match result {
                Some(Ok(())) => Ok((true, None)),
                Some(Err(err)) => Ok((false, Some(err))),
                None => Ok((false, Some("files.rescan: no app context".to_string()))),
            }
        },
    )?;

    Ok(())
}

#[derive(Clone)]
struct SearchOpts {
    limit: usize,
    offset: usize,
    include_dirs: bool,
    cwd: Option<PathBuf>,
}

impl SearchOpts {
    fn from_lua(opts: Option<mlua::Table>) -> LuaResult<Self> {
        Ok(Self {
            limit: opts
                .as_ref()
                .and_then(|t| t.get::<Option<u64>>("limit").ok().flatten())
                .map(|n| n as usize)
                .unwrap_or(200),
            offset: opts
                .as_ref()
                .and_then(|t| t.get::<Option<u64>>("offset").ok().flatten())
                .map(|n| n as usize)
                .unwrap_or(0),
            include_dirs: opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("include_dirs").ok().flatten())
                .unwrap_or(true),
            cwd: opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten())
                .map(PathBuf::from),
        })
    }
}

#[derive(Clone)]
struct AcceptOpts {
    cwd: Option<PathBuf>,
}

impl AcceptOpts {
    fn from_lua(opts: Option<mlua::Table>) -> LuaResult<Self> {
        Ok(Self {
            cwd: opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("cwd").ok().flatten())
                .map(PathBuf::from),
        })
    }
}

fn response_to_lua(lua: &Lua, response: SearchResponse) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("root", response.root.to_string_lossy().to_string())?;
    table.set("total_matched", response.total_matched as u64)?;
    table.set("total_files", response.total_files as u64)?;
    table.set("total_dirs", response.total_dirs as u64)?;
    table.set("scanned", response.scanned as u64)?;
    table.set("scanning", response.scanning)?;
    table.set("searching", response.searching)?;
    table.set("ready", response.ready)?;
    table.set(
        "status",
        if response.scanning || response.searching {
            "loading"
        } else if response.items.is_empty() {
            "empty"
        } else {
            "ready"
        },
    )?;
    if let Some(message) = response.message {
        table.set("message", message)?;
    }

    let items = lua.create_table_with_capacity(response.items.len(), 0)?;
    for (index, item) in response.items.into_iter().enumerate() {
        let row = lua.create_table_with_capacity(0, 6)?;
        row.set("id", item.id)?;
        row.set("label", item.label)?;
        row.set("path", item.path)?;
        row.set("insert_text", item.insert_text)?;
        row.set("kind", item.kind.as_str())?;
        row.set("score", item.score)?;
        if item.kind == ItemKind::Dir {
            row.set("description", "directory")?;
        }
        items.raw_set(index + 1, row)?;
    }
    table.set("items", items)?;
    Ok(table)
}

fn error_response_to_lua(lua: &Lua, err: String) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("items", lua.create_table()?)?;
    table.set("status", "error")?;
    table.set("message", err)?;
    table.set("ready", false)?;
    table.set("scanning", false)?;
    table.set("searching", false)?;
    table.set("scanned", 0u64)?;
    table.set("total_matched", 0u64)?;
    table.set("total_files", 0u64)?;
    table.set("total_dirs", 0u64)?;
    Ok(table)
}

fn status_to_lua(lua: &Lua, status: WorkspaceFilesStatus) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("root", status.root.to_string_lossy().to_string())?;
    table.set("initialized", status.initialized)?;
    table.set("files", status.files as u64)?;
    table.set("scanned", status.scanned as u64)?;
    table.set("scanning", status.scanning)?;
    table.set("watcher_ready", status.watcher_ready)?;
    table.set("warmup_complete", status.warmup_complete)?;
    Ok(table)
}

fn error_status_to_lua(lua: &Lua, err: String) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    table.set("initialized", false)?;
    table.set("error", err)?;
    table.set("files", 0u64)?;
    table.set("scanned", 0u64)?;
    table.set("scanning", false)?;
    table.set("watcher_ready", false)?;
    table.set("warmup_complete", false)?;
    Ok(table)
}
