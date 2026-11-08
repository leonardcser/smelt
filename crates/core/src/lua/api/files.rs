//! `smelt.files` - warm workspace file search.

use super::document::LuaDocument;
use crate::lua::doc::{record_class, Tier};
use crate::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::workspace_files::{
    AcceptRequest, ItemKind, SearchRequest, SearchResponse, SearchSource, WorkspaceFilesStatus,
};
use mlua::prelude::*;
use smelt_transcript::code_search::{Pattern, Presentation, Search};
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let files = LuaMod::supported(
        lua,
        smelt,
        "files",
        "Workspace file search. Owns background indexing, filesystem watching, and recent-selection ranking.",
        Tier::Host,
    )?;

    let search_context = Arc::clone(shared);
    files.fn_(
        "search",
        "Search workspace files. Returns `{ items, total_matched, total_files, total_dirs, scanned, scanning, searching, ready, status, message, root }`. Items are `{ label, path, insert_text, kind, score }`. Options: `{ limit?, offset?, include_dirs?, cwd?, source? }`. Source is `workspace` (default) or `git` (tracked files only).",
        &["query", "opts"],
        move |lua, (query, opts): (String, Option<mlua::Table>)| -> LuaResult<mlua::Table> {
            let _perf = smelt_perf::perf::begin("files:search");
            let opts = SearchOpts::from_lua(opts)?;
            let response = crate::host::try_with_core(|core| {
                let cwd = session_cwd(&search_context, core, opts.cwd);
                core.workspace_files.search_interactive(SearchRequest {
                    query,
                    cwd,
                    limit: opts.limit,
                    offset: opts.offset,
                    include_dirs: opts.include_dirs,
                    source: opts.source,
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

    let grep_context = Arc::clone(shared);
    files.fn_(
        "grep",
        "Search the contents of workspace files and stream matching lines. Returns a `smelt.files.Grep`, or `(nil, err)` for an invalid pattern. The query is literal unless `opts.regex`, and case-insensitive unless it has an uppercase letter. Files come from the same index as `files.search`; binary files are skipped. Files are searched in parallel and results arrive in path order. Options: `{ cwd?, source?, limit?, regex?, reverse?, icons? }`. `limit` caps kept lines (default 10000); counting continues past it. `reverse` puts the first match on the last row and fills short lists with blank rows above (see `fill`). `icons = { colors?, light? }` adds file icons before paths.",
        &["query", "opts"],
        move |_, (query, opts): (String, Option<mlua::Table>)| -> LuaResult<(Option<LuaGrep>, Option<String>)> {
            let GrepOpts { search, regex, limit, presentation } = GrepOpts::from_lua(opts)?;
            let pattern = match Pattern::new(&query, regex) {
                Ok(pattern) => pattern,
                Err(err) => return Ok((None, Some(err))),
            };
            let list = crate::host::try_with_core(|core| {
                let cwd = session_cwd(&grep_context, core, search.cwd);
                core.workspace_files.files(&cwd, search.source)
            });
            match list {
                Some(Ok(list)) => Ok((
                    Some(LuaGrep {
                        search: Arc::new(Search::start(
                            list.root,
                            list.files,
                            pattern,
                            limit,
                            presentation,
                            grep_context.wakeup_tx.get().cloned(),
                        )),
                        indexing: list.scanning,
                    }),
                    None,
                )),
                Some(Err(err)) => Ok((None, Some(err))),
                None => Ok((None, Some("files.grep: no app context".to_string()))),
            }
        },
    )?;
    record_class(LuaClassDecl {
        name: "smelt.files.Grep",
        classification: crate::lua::doc::classification_for_type("smelt.files.Grep"),
        doc: "A running content search from `smelt.files.grep`. Rows are zero-based. The search stops on `cancel` or when collected.",
        fields: vec![
            LuaClassField { name: "document", ty: "fun(self: smelt.files.Grep): smelt.document.Document".into(), optional: false, doc: "Row source for `win:document`: one `path:line:column: text` row per matching line, with syntax colours and highlighted matches. Grows while the search runs." },
            LuaClassField { name: "progress", ty: "fun(self: smelt.files.Grep): { rows: integer, total: integer, files_searched: integer, files_total: integer, done: boolean, indexing: boolean, version: integer }".into(), optional: false, doc: "Search progress. `rows` are kept lines, `total` all matching lines. `indexing` means the file index was still being built when the search started. `version` changes with any other field." },
            LuaClassField { name: "sync", ty: "fun(self: smelt.files.Grep): integer".into(), optional: false, doc: "Show every match found so far in the document. The document only changes on `sync` and `fill`, so a caller can keep its selection in step. Returns how many rows appeared above the existing ones: shift the cursor and scroll by that much to keep the same rows on screen. Always 0 unless reversed." },
            LuaClassField { name: "item", ty: "fun(self: smelt.files.Grep, row: integer): { index: integer, path: string, line: integer, column: integer, text: string }?".into(), optional: false, doc: "The match on a document row, or nil for a fill row: 1-based match index in path order, root-relative path, 1-based line and character column, and trimmed line text." },
            LuaClassField { name: "row", ty: "fun(self: smelt.files.Grep, index: integer): integer?".into(), optional: false, doc: "Document row of the 1-based match index, or nil if it is not shown yet." },
            LuaClassField { name: "fill", ty: "fun(self: smelt.files.Grep, rows: integer): integer".into(), optional: false, doc: "Keep at least `rows` rows in a reversed document, filling above the matches, so a short list sits at the bottom of a window that tall. Returns the rows added above, as `sync` does; negative when it removed some." },
            LuaClassField { name: "root", ty: "fun(self: smelt.files.Grep): string".into(), optional: false, doc: "Directory that item paths are relative to." },
            LuaClassField { name: "cancel", ty: "fun(self: smelt.files.Grep)".into(), optional: false, doc: "Stop searching. Rows found so far stay available." },
        ],
    });

    let accept_context = Arc::clone(shared);
    files.live_only_fn(
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
                let cwd = session_cwd(&accept_context, core, opts.cwd);
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
            let cwd = field::<String>(&opts, "cwd")?
                .map(PathBuf::from);
            let result = crate::host::try_with_core(|core| {
                let cwd = session_cwd(&status_context, core, cwd);
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
    files.live_only_fn(
        "rescan",
        "Trigger an asynchronous full rescan for the current workspace or `opts.cwd`. Returns `(true, nil)` or `(false, err)`.",
        &["opts"],
        move |_, opts: Option<mlua::Table>| -> LuaResult<(bool, Option<String>)> {
            let cwd = field::<String>(&opts, "cwd")?
                .map(PathBuf::from);
            let result = crate::host::try_with_core(|core| {
                let cwd = session_cwd(&rescan_context, core, cwd);
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
struct LuaGrep {
    search: Arc<Search>,
    indexing: bool,
}

impl LuaType for LuaGrep {
    fn lua_type() -> String {
        "smelt.files.Grep".into()
    }
}

impl mlua::UserData for LuaGrep {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |_, this, ()| {
            Ok(LuaDocument(this.search.clone()))
        });
        methods.add_method("sync", |_, this, ()| Ok(this.search.sync()));
        methods.add_method("progress", |lua, this, ()| {
            let table = lua.create_table()?;
            this.search.with_progress(|progress| {
                table.set("rows", progress.matches.len())?;
                table.set("total", progress.total)?;
                table.set("files_searched", progress.files_searched)?;
                table.set("files_total", progress.files_total)?;
                table.set("done", progress.done)?;
                table.set("version", progress.version)
            })?;
            table.set("indexing", this.indexing)?;
            Ok(table)
        });
        methods.add_method("item", |lua, this, row: usize| {
            let Some(index) = this.search.match_at(row) else {
                return Ok(None);
            };
            let Some(found) = this
                .search
                .with_progress(|progress| progress.matches.get(index).cloned())
            else {
                return Ok(None);
            };
            let table = lua.create_table()?;
            table.set("index", index + 1)?;
            table.set("path", this.search.path(found.file).unwrap_or_default())?;
            table.set("line", found.line)?;
            table.set("column", found.column)?;
            table.set("text", found.text)?;
            Ok(Some(table))
        });
        methods.add_method("row", |_, this, index: usize| {
            Ok(index
                .checked_sub(1)
                .and_then(|index| this.search.row_of(index)))
        });
        methods.add_method("fill", |_, this, rows: usize| Ok(this.search.fill(rows)));
        methods.add_method("root", |_, this, ()| {
            Ok(this.search.root().to_string_lossy().into_owned())
        });
        methods.add_method("cancel", |_, this, ()| {
            this.search.cancel();
            Ok(())
        });
    }
}

/// Optional field `key` of `opts`. A value of the wrong type is an error.
pub(super) fn field<T: mlua::FromLua>(
    opts: &Option<mlua::Table>,
    key: &str,
) -> LuaResult<Option<T>> {
    opts.as_ref()
        .map_or(Ok(None), |opts| opts.get::<Option<T>>(key))
}

/// `cwd` against the project, or the cwd a call without one runs in.
fn session_cwd(shared: &LuaShared, core: &crate::runtime::Core, cwd: Option<PathBuf>) -> PathBuf {
    cwd.map(|cwd| shared.resolve_project_path(cwd))
        .unwrap_or_else(|| {
            if shared.external_effects_active() {
                core.env.cwd()
            } else {
                shared.evaluation_cwd()
            }
        })
}

#[derive(Clone)]
struct SearchOpts {
    limit: usize,
    offset: usize,
    include_dirs: bool,
    source: SearchSource,
    cwd: Option<PathBuf>,
}

impl SearchOpts {
    fn from_lua(opts: Option<mlua::Table>) -> LuaResult<Self> {
        let source = match opts
            .as_ref()
            .map(|t| t.get::<LuaValue>("source"))
            .transpose()?
        {
            None | Some(LuaValue::Nil) => SearchSource::Workspace,
            Some(LuaValue::String(value)) if value.as_bytes().as_ref() == b"workspace" => {
                SearchSource::Workspace
            }
            Some(LuaValue::String(value)) if value.as_bytes().as_ref() == b"git" => {
                SearchSource::Git
            }
            _ => {
                return Err(mlua::Error::runtime(
                    "files.search: source must be 'workspace' or 'git'",
                ))
            }
        };
        Ok(Self {
            source,
            limit: field::<u64>(&opts, "limit")?
                .map(|n| n as usize)
                .unwrap_or(200),
            offset: field::<u64>(&opts, "offset")?
                .map(|n| n as usize)
                .unwrap_or(0),
            include_dirs: field::<bool>(&opts, "include_dirs")?.unwrap_or(true),
            cwd: field::<String>(&opts, "cwd")?.map(PathBuf::from),
        })
    }
}

struct GrepOpts {
    search: SearchOpts,
    regex: bool,
    limit: usize,
    presentation: Presentation,
}

impl GrepOpts {
    fn from_lua(opts: Option<mlua::Table>) -> LuaResult<Self> {
        let icons = field::<mlua::Table>(&opts, "icons")?
            .map(|icons| {
                let icons = Some(icons);
                LuaResult::Ok(smelt_transcript::content::file_icons::FileIconOptions::new(
                    true,
                    field::<bool>(&icons, "colors")?.unwrap_or(true),
                    field::<bool>(&icons, "light")?.unwrap_or(false),
                    None,
                ))
            })
            .transpose()?;
        Ok(Self {
            regex: field::<bool>(&opts, "regex")?.unwrap_or(false),
            limit: field::<u64>(&opts, "limit")?.map_or(10_000, |n| n as usize),
            presentation: Presentation {
                reverse: field::<bool>(&opts, "reverse")?.unwrap_or(false),
                icons,
            },
            search: SearchOpts::from_lua(opts)?,
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
            cwd: field::<String>(&opts, "cwd")?.map(PathBuf::from),
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
        } else if !response.ready {
            "error"
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
