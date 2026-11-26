//! Indexed diff and asynchronous Git snapshot primitives.

use super::document::LuaDocument;
use crate::lua::doc::{record_class, Tier};
use crate::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use smelt_transcript::diff::{Diff, DiffView};
use std::sync::{Arc, Mutex};

/// The repository behind a working snapshot, which file changes apply to.
struct Repository {
    root: std::path::PathBuf,
}

#[derive(Clone)]
struct LuaDiff(Arc<DiffView>, Option<Arc<Repository>>);

impl LuaType for LuaDiff {
    fn lua_type() -> String {
        "smelt.diff.Diff".into()
    }
}

impl FromLua for LuaDiff {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(value) => Ok(value.borrow::<Self>()?.clone()),
            _ => Err(LuaError::external("expected a smelt.diff.Diff")),
        }
    }
}

fn file_metadata(lua: &Lua, file: &smelt_transcript::diff::File) -> LuaResult<mlua::Table> {
    let row = lua.create_table()?;
    row.set("key", lua.create_string(&file.raw_path)?)?;
    row.set("path", file.path.as_str())?;
    row.set("old_path", file.old_path.as_str())?;
    row.set("status", file.status)?;
    row.set("section", file.section.name())?;
    row.set("additions", file.additions)?;
    row.set("deletions", file.deletions)?;
    row.set("binary", file.binary)?;
    row.set("hl_group", file.highlight_group())?;
    Ok(row)
}

fn section(name: &str) -> LuaResult<smelt_transcript::diff::Section> {
    match name {
        "unstaged" => Ok(smelt_transcript::diff::Section::Unstaged),
        "staged" => Ok(smelt_transcript::diff::Section::Staged),
        "changes" => Ok(smelt_transcript::diff::Section::Changes),
        _ => Err(LuaError::external(
            "section must be unstaged, staged, or changes",
        )),
    }
}

fn tree_section(name: &str) -> LuaResult<smelt_transcript::diff::tree::TreeSection> {
    use smelt_transcript::diff::tree::{CommitList, TreeSection};
    match name {
        "unpushed" => Ok(TreeSection::Commits(CommitList::Unpushed)),
        "unpulled" => Ok(TreeSection::Commits(CommitList::Unpulled)),
        _ => section(name).map(TreeSection::Files).map_err(|_| {
            LuaError::external("section must be unstaged, staged, changes, unpushed, or unpulled")
        }),
    }
}

struct LuaDiffTree(Arc<smelt_transcript::diff::tree::DiffTree>);

impl LuaType for LuaDiffTree {
    fn lua_type() -> String {
        "smelt.diff.Tree".into()
    }
}

impl mlua::UserData for LuaDiffTree {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |_, this, ()| Ok(LuaDocument(this.0.clone())));
        methods.add_method("width", |_, this, width: usize| {
            this.0.set_width(width);
            Ok(())
        });
        methods.add_method("commit_row", |_, this, id: String| {
            Ok(this.0.commit_row(&id))
        });
        methods.add_method("section_row", |_, this, name: String| {
            Ok(this.0.section_row(tree_section(&name)?))
        });
        methods.add_method("collapsed", |_, this, ()| Ok(this.0.collapsed()));
        methods.add_method(
            "restore_group",
            |_, this, (previous, row): (LuaAnyUserData, u64)| {
                let previous = previous.borrow::<LuaDiffTree>()?;
                Ok(this.0.restore_group(&previous.0, row))
            },
        );
        methods.add_method("toggle", |_, this, row: u64| Ok(this.0.toggle(row)));
        methods.add_method(
            "file_row",
            |_, this, (index, reveal): (usize, Option<bool>)| {
                Ok(index
                    .checked_sub(1)
                    .and_then(|index| this.0.file_row(index, reveal.unwrap_or(false))))
            },
        );
        methods.add_method("node", |lua, this, row: u64| {
            this.0
                .node(row)
                .map(|node| {
                    let value = lua.create_table()?;
                    value.set("index", node.file.map(|file| file + 1))?;
                    if let Some(commit) = node.commit {
                        let row = lua.create_table()?;
                        row.set("id", commit.id)?;
                        row.set("subject", commit.subject)?;
                        value.set("commit", row)?;
                    }
                    value.set("first", node.first + 1)?;
                    value.set("last", node.end)?;
                    value.set("parent", node.parent)?;
                    value.set("children", node.children)?;
                    value.set("section", node.section.name())?;
                    value.set("group", node.group)?;
                    value.set("expanded", node.expanded)?;
                    Ok(value)
                })
                .transpose()
        });
    }
}

impl mlua::UserData for LuaDiff {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |_, this, ()| Ok(LuaDocument(this.0.clone())));
        methods.add_method("view", |_, this, ()| {
            Ok(LuaDiff(Arc::new(this.0.fork()), this.1.clone()))
        });
        methods.add_method("tree", |_, this, collapsed: Option<Vec<String>>| {
            let tree = this.0.tree();
            tree.restore_collapsed(&collapsed.unwrap_or_default());
            Ok(LuaDiffTree(Arc::new(tree)))
        });
        methods.add_method("files", |lua, this, ()| {
            let files = lua.create_table()?;
            for (index, file) in this.0.diff.files.iter().enumerate() {
                files.set(index + 1, file_metadata(lua, file)?)?;
            }
            Ok(files)
        });
        methods.add_method("totals", |_, this, ()| {
            let files = &this.0.diff.files;
            Ok((
                files.iter().map(|file| file.additions).sum::<u64>(),
                files.iter().map(|file| file.deletions).sum::<u64>(),
                files.iter().any(|file| file.binary),
            ))
        });
        methods.add_method("file_count", |_, this, ()| Ok(this.0.diff.files.len()));
        methods.add_method("file", |lua, this, index: usize| {
            index
                .checked_sub(1)
                .and_then(|i| this.0.diff.files.get(i))
                .map(|file| file_metadata(lua, file))
                .transpose()
        });
        methods.add_method(
            "find_file",
            |_, this, (name, key): (String, mlua::LuaString)| {
                let section = section(&name)?;
                Ok(this
                    .0
                    .diff
                    .files
                    .iter()
                    .position(|file| {
                        file.section == section && file.raw_path == key.as_bytes().as_ref()
                    })
                    .map(|i| i + 1))
            },
        );
        methods.add_method("section_range", |_, this, name: String| {
            let section = section(&name)?;
            let files = &this.0.diff.files;
            Ok((
                files.partition_point(|file| file.section < section) + 1,
                files.partition_point(|file| file.section <= section),
            ))
        });
        methods.add_method("file_row", |_, this, index: usize| {
            Ok(index
                .checked_sub(1)
                .and_then(|index| this.0.file_row(index)))
        });
        methods.add_method("file_at", |_, this, row: u64| {
            Ok(this.0.file_at(row).map(|index| index + 1))
        });
        methods.add_method("hunk", |_, this, (row, forward): (u64, bool)| {
            Ok(this.0.hunk(row, forward))
        });
        methods.add_method("toggle_fold", |_, this, row: u64| {
            Ok(this.0.toggle_fold(row))
        });
        methods.add_method(
            "restore",
            |_, this, (previous, cursor, top): (LuaDiff, u64, u64)| {
                Ok(this.0.restore(&previous.0, cursor, top))
            },
        );
    }
}

enum GitResult {
    Snapshot(crate::git::DiffSnapshot),
    History(crate::git::review::Page),
    Stats(std::collections::HashMap<String, crate::git::review::Stats>),
    IndexUpdated {
        root: std::path::PathBuf,
        refresh: Result<crate::git::DiffSnapshot, String>,
    },
}

type DiffResult = Arc<Mutex<Option<Result<GitResult, String>>>>;
struct LuaDiffJob(DiffResult);

impl LuaType for LuaDiffJob {
    fn lua_type() -> String {
        "userdata".into()
    }
}

impl mlua::UserData for LuaDiffJob {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "take",
            |lua, this, ()| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
                let result = this
                    .0
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| LuaError::external("git diff result is not available"))?;
                match result {
                    Ok(value) => {
                        let result = lua.create_table()?;
                        let snapshot = match value {
                            GitResult::History(page) => {
                                result.set("base", page.base)?;
                                result.set("base_id", page.base_id)?;
                                result.set("head", page.head)?;
                                result.set("more", page.more)?;
                                result.set("commits", commit_rows(lua, page.commits)?)?;
                                return Ok((Some(result), None));
                            }
                            GitResult::Stats(stats) => {
                                for (id, stats) in stats {
                                    let row = lua.create_table()?;
                                    row.set("additions", stats.additions)?;
                                    row.set("deletions", stats.deletions)?;
                                    row.set("binary", stats.binary)?;
                                    result.set(id, row)?;
                                }
                                return Ok((Some(result), None));
                            }
                            GitResult::Snapshot(snapshot) => Some(snapshot),
                            GitResult::IndexUpdated { root, refresh } => {
                                result.set("index_updated", true)?;
                                result.set("root", root.to_string_lossy().as_ref())?;
                                match refresh {
                                    Ok(snapshot) => Some(snapshot),
                                    Err(error) => {
                                        result.set("refresh_error", error)?;
                                        None
                                    }
                                }
                            }
                        };
                        if let Some(snapshot) = snapshot {
                            result.set("root", snapshot.root.to_string_lossy().as_ref())?;
                            if let Some(tracking) = snapshot.tracking {
                                result.set("branch", tracking.branch)?;
                                result.set("head", tracking.head)?;
                                result.set("upstream", tracking.upstream)?;
                                result.set("ahead", tracking.ahead)?;
                                result.set("behind", tracking.behind)?;
                                result.set("operation", tracking.operation)?;
                            }
                            result.set(
                                "diff",
                                LuaDiff(
                                    snapshot.view.clone(),
                                    (!snapshot.view.diff.is_comparison()).then(|| {
                                        Arc::new(Repository {
                                            root: snapshot.root,
                                        })
                                    }),
                                ),
                            )?;
                        }
                        Ok((Some(result), None))
                    }
                    Err(error) => Ok((None, Some(error))),
                }
            },
        );
    }
}

/// A `git.index` or `git.discard` request.
use crate::git::HunkAction;

fn change_action(action: &str) -> LuaResult<HunkAction> {
    Ok(match action {
        "stage" => HunkAction::Index(crate::git::IndexAction::Stage),
        "unstage" => HunkAction::Index(crate::git::IndexAction::Unstage),
        "toggle" => HunkAction::Index(crate::git::IndexAction::Toggle),
        "discard" => HunkAction::Discard,
        _ => {
            return Err(LuaError::external(
                "action must be stage, unstage, toggle, or discard",
            ))
        }
    })
}

/// Run `change` on the repository behind `diff` off the UI thread, then load
/// a fresh working snapshot for task `id`.
fn start_change<F, Fut>(
    shared: &Arc<LuaShared>,
    id: u64,
    diff: LuaDiff,
    change: F,
) -> LuaResult<LuaDiffJob>
where
    F: FnOnce(std::path::PathBuf, LuaDiff, tokio_util::sync::CancellationToken) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send,
{
    let repository = Arc::clone(
        diff.1
            .as_ref()
            .ok_or_else(|| LuaError::external("index operations require a Git snapshot"))?,
    );
    let context = diff.0.context();
    let result: DiffResult = Arc::new(Mutex::new(None));
    let output = Arc::clone(&result);
    let cancel = crate::lua::current_task_cancel().unwrap_or_default();
    let sink = shared.resume_sink();
    let wakeup = shared.wakeup_tx.get().cloned();
    tokio::spawn(async move {
        let root = repository.root.clone();
        let mutation = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            change(root, diff, cancel.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            Err(
                "git operation timed out after 120 seconds; refresh to verify repository state"
                    .into(),
            )
        });
        let value = match mutation {
            Err(error) => Err(error),
            Ok(()) => {
                let refresh =
                    crate::git::working_diff(repository.root.clone(), context, cancel).await;
                if let Ok(snapshot) = &refresh {
                    snapshot.view.set_wakeup(wakeup);
                }
                Ok(GitResult::IndexUpdated {
                    root: repository.root.clone(),
                    refresh,
                })
            }
        };
        *output.lock().unwrap() = Some(value);
        sink.resolve_json(id, serde_json::Value::Null);
    });
    Ok(LuaDiffJob(result))
}

fn commit_rows(lua: &Lua, commits: Vec<crate::git::review::Commit>) -> LuaResult<mlua::Table> {
    let rows = lua.create_table()?;
    for (index, commit) in commits.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("id", commit.id)?;
        row.set("author", commit.author)?;
        row.set("age", commit.age)?;
        row.set("subject", commit.subject)?;
        row.set("message", commit.message)?;
        rows.set(index + 1, row)?;
    }
    Ok(rows)
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::advanced(lua, smelt, "diff", "Compact unified-patch indexing, metadata and expandable context folds. Rendering cost is proportional to the viewport, not patch length.", Tier::Host)?;
    record_class(LuaClassDecl {
        name: "smelt.diff.File",
        classification: crate::lua::doc::classification_for_type("smelt.diff.File"),
        doc: "One section's changed file metadata. Paths are repository-relative; partially staged files have a separate entry in each section. Status is ?, A, D, M, R, T, or U (unmerged).",
        fields: vec![
            LuaClassField {
                name: "path",
                ty: "string".into(),
                optional: false,
                doc: "New path (old path for a deletion).",
            },
            LuaClassField {
                name: "old_path",
                ty: "string".into(),
                optional: false,
                doc: "Original path, including for a rename.",
            },
            LuaClassField {
                name: "status",
                ty: "string".into(),
                optional: false,
                doc: "Git change status.",
            },
            LuaClassField {
                name: "additions",
                ty: "integer".into(),
                optional: false,
                doc: "Added lines.",
            },
            LuaClassField {
                name: "deletions",
                ty: "integer".into(),
                optional: false,
                doc: "Removed lines.",
            },
            LuaClassField {
                name: "hl_group",
                ty: "string".into(),
                optional: false,
                doc: "Theme foreground group for the file's change kind.",
            },
            LuaClassField {
                name: "key",
                ty: "string".into(),
                optional: false,
                doc: "Raw path bytes, for stable section-local identity across snapshots. Use path for display.",
            },
            LuaClassField {
                name: "section",
                ty: "'unstaged'|'staged'|'changes'".into(),
                optional: false,
                doc: "Patch section. Untracked files belong to unstaged with status ?.",
            },
            LuaClassField {
                name: "binary",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether Git reports a binary patch instead of line changes.",
            },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.TreeNode",
        classification: crate::lua::doc::classification_for_type("smelt.diff.TreeNode"),
        doc: "Metadata for one visible file-tree row. File indices are one-based and tree rows are zero-based.",
        fields: vec![
            LuaClassField { name: "index", ty: "integer".into(), optional: true, doc: "File index, or nil for a directory or commit." },
            LuaClassField { name: "commit", ty: "{ id: string, subject: string }".into(), optional: true, doc: "The commit an unpushed or unpulled row lists." },
            LuaClassField { name: "first", ty: "integer".into(), optional: false, doc: "First descendant file index." },
            LuaClassField { name: "last", ty: "integer".into(), optional: false, doc: "Inclusive last descendant file index, including collapsed descendants. Less than first for an empty section." },
            LuaClassField { name: "parent", ty: "integer".into(), optional: true, doc: "Visible parent directory row." },
            LuaClassField { name: "children", ty: "boolean".into(), optional: false, doc: "Whether this row is an expandable directory. Section headings are fixed." },
            LuaClassField { name: "section", ty: "'unstaged'|'staged'|'changes'|'unpushed'|'unpulled'".into(), optional: false, doc: "Section containing this row." },
            LuaClassField { name: "group", ty: "boolean".into(), optional: false, doc: "Whether this is a section header." },
            LuaClassField { name: "expanded", ty: "boolean".into(), optional: false, doc: "Whether the directory is expanded. Always true for section headings." },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.Tree",
        classification: crate::lua::doc::classification_for_type("smelt.diff.Tree"),
        doc: "Independent virtual file sidebar sharing immutable file metadata and ordering. Each tree owns its collapsed directories; each attached window supplies its own width. Only requested rows are formatted; folder toggles rebuild compact visible-node indices, not offscreen strings or spans.",
        fields: vec![
            LuaClassField { name: "document", ty: "fun(self: smelt.diff.Tree): smelt.document.Document".into(), optional: false, doc: "Row source for a window. Includes neutral filenames, colored status letters, folder triangles, and green/red counts immediately after file and section labels, separated by single spaces." },
            LuaClassField { name: "width", ty: "fun(self: smelt.diff.Tree, width: integer)".into(), optional: false, doc: "Set fallback content width for row access outside a window. Attached windows use their own content width automatically." },
            LuaClassField { name: "node", ty: "fun(self: smelt.diff.Tree, row: integer): smelt.diff.TreeNode?".into(), optional: false, doc: "Metadata for one visible row, or nil for the blank section separator or an invalid row. Constant-time lookup." },
            LuaClassField { name: "file_row", ty: "fun(self: smelt.diff.Tree, index: integer, reveal: boolean?): integer?".into(), optional: false, doc: "Visible row of a file, or nil when hidden. With reveal=true, expand its ancestors first. Logarithmic lookup when no folders change." },
            LuaClassField { name: "toggle", ty: "fun(self: smelt.diff.Tree, row: integer): integer?".into(), optional: false, doc: "Toggle a directory and return its row. Files, section headings, the blank separator and invalid rows return nil." },
            LuaClassField { name: "collapsed", ty: "fun(self: smelt.diff.Tree): string[]".into(), optional: false, doc: "Opaque collapsed directory keys for restoring a new snapshot." },
            LuaClassField { name: "commit_row", ty: "fun(self: smelt.diff.Tree, id: string): integer?".into(), optional: false, doc: "Visible row of an unpushed or unpulled commit, or nil when the tree does not list it." },
            LuaClassField { name: "section_row", ty: "fun(self: smelt.diff.Tree, section: 'unstaged'|'staged'|'changes'|'unpushed'|'unpulled'): integer?".into(), optional: false, doc: "Visible row of a section header. File sections always have one, even when empty; a commit section has one only when the tree lists it." },
            LuaClassField { name: "restore_group", ty: "fun(self: smelt.diff.Tree, previous: smelt.diff.Tree, row: integer): integer?".into(), optional: false, doc: "Restore a folder or section selection across snapshots by section and raw directory identity. Never expands folders. Returns the nearest visible ancestor if hidden, or its section header if the directory disappeared. Returns nil for a file, separator or invalid row. Directory lookup is logarithmic." },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.Diff",
        classification: crate::lua::doc::classification_for_type("smelt.diff.Diff"),
        doc: "Indexed unified patch with a continuous foldable row projection. File indices are one-based; display rows are zero-based. No patch line tables cross into Lua.",
        fields: vec![
            LuaClassField { name: "document", ty: "fun(self: smelt.diff.Diff): smelt.document.Document".into(), optional: false, doc: "Row source for `win:document`. Native storage is shared, not copied." },
            LuaClassField { name: "view", ty: "fun(self: smelt.diff.Diff): smelt.diff.Diff".into(), optional: false, doc: "Create an independent view, initially preserving the current folds. Shares immutable patch storage, metadata and syntax caches, not subsequent fold changes. Each attached window submits independent syntax demand." },
            LuaClassField { name: "tree", ty: "fun(self: smelt.diff.Diff, collapsed: string[]?): smelt.diff.Tree".into(), optional: false, doc: "Create an independent virtual file tree. Optionally restore opaque collapsed keys from an earlier tree. Section headers include file counts and section-local line totals; attached windows provide their own width. A working `smelt.git.diff` snapshot also lists its unpushed and unpulled commits after the file sections, each list headed by the upstream name and its commit count." },
            LuaClassField { name: "restore", ty: "fun(self: smelt.diff.Diff, previous: smelt.diff.Diff, cursor: integer, top: integer): integer?, integer?".into(), optional: false, doc: "Restore expanded context and return cursor/top rows for a refreshed snapshot, matching section, raw path and source line numbers rather than display offsets. A missing file returns nil for its anchor. Does not change the previous view." },
            LuaClassField { name: "files", ty: "fun(self: smelt.diff.Diff): smelt.diff.File[]".into(), optional: false, doc: "All file metadata, grouped by section and directory-first within each section. Prefer file(index) for bounded UI-thread allocation." },
            LuaClassField { name: "totals", ty: "fun(self: smelt.diff.Diff): integer, integer, boolean".into(), optional: false, doc: "Added and deleted line totals, plus whether any file is binary. Linear in changed files, not patch lines. Working snapshots count each section separately." },
            LuaClassField { name: "file_count", ty: "fun(self: smelt.diff.Diff): integer".into(), optional: false, doc: "Number of section-local file entries. A partial file counts twice." },
            LuaClassField { name: "file", ty: "fun(self: smelt.diff.Diff, index: integer): smelt.diff.File?".into(), optional: false, doc: "Metadata for one file entry. Constant-time lookup." },
            LuaClassField { name: "find_file", ty: "fun(self: smelt.diff.Diff, section: 'unstaged'|'staged'|'changes', key: string): integer?".into(), optional: false, doc: "Find a file by section and raw path bytes when restoring selection in a new snapshot." },
            LuaClassField { name: "section_range", ty: "fun(self: smelt.diff.Diff, section: 'unstaged'|'staged'|'changes'): integer, integer".into(), optional: false, doc: "Inclusive first and last file indices for a section. First exceeds last when the section is empty." },
            LuaClassField { name: "file_row", ty: "fun(self: smelt.diff.Diff, index: integer): integer?".into(), optional: false, doc: "Current display row of a file header. Accounts for expanded folds." },
            LuaClassField { name: "file_at", ty: "fun(self: smelt.diff.Diff, row: integer): integer?".into(), optional: false, doc: "File containing a display row. Logarithmic lookup." },
            LuaClassField { name: "hunk", ty: "fun(self: smelt.diff.Diff, row: integer, forward: boolean): integer?".into(), optional: false, doc: "Next/previous change group, strictly after/before row. Full-context Git patches retain navigation between edits separated by unchanged lines." },
            LuaClassField { name: "toggle_fold", ty: "fun(self: smelt.diff.Diff, row: integer): integer?".into(), optional: false, doc: "Toggle the context fold containing row. Returns the fold marker's row, or nil. Does not reparse the patch." },
        ],
    });
    let parse_shared = Arc::clone(shared);
    m.fn_("parse", "Index Git patch rows and folds without tokenizing source. `context` defaults to 3 visible lines at each edge of an unchanged run. Row indexing is linear and synchronous; use `smelt.git.diff` for asynchronous repository acquisition. Syntax and inline highlights are computed on demand in a background worker with bounded caches and old/new parser checkpoints. Each visible window submits its own demand; copying and searching only read cached results. Inline results publish before distant syntax seeks; large replacement blocks compare only viewport pairs. The first 128-256 rows of the previous and next file are prefetched after visible work, without keeping the UI repainting. Text and navigation are immediately available; highlights appear when ready without blocking a frame.", &["patch", "context"],
        move |_, (patch, context): (String, Option<usize>)| -> LuaResult<LuaDiff> {
            let view = Arc::new(DiffView::new(Arc::new(Diff::parse(patch)), context.unwrap_or(3)));
            view.set_wakeup(parse_shared.wakeup_tx.get().cloned());
            Ok(LuaDiff(view, None))
        })?;
    let git = LuaMod::advanced(lua, smelt, "git", "Asynchronous Git history, read-only revision comparisons, explicit file or range staging, and discarding unstaged changes. `git.history(opts?)` loads bounded commit pages and `git.stats(opts)` their line totals. `git.diff({revision, base?})` compares a commit to its first parent or a branch to its merge base; historical snapshots cannot be staged. `git.diff(opts?)` without a revision yields separate unstaged (index to worktree, including untracked files) and staged (HEAD to index) patches, with the branch, its upstream and ahead/behind counts from the same `git status`; its file tree lists the unpushed and unpulled commits. Unborn repositories use the empty tree for HEAD. `git.index(diff, file, action, last?)` updates the selected file or inclusive range's real index entries within one section, and `git.discard(diff, file, last?)` throws away its unstaged worktree changes; then acquires and indexes a fresh grouped snapshot off-thread. Mutation success is explicit (`index_updated=true`); if only the refresh fails, the result contains `refresh_error` and no `diff`. Retain the old snapshot as stale and refresh before another mutation. The original snapshot remains immutable.", Tier::Host)?;
    record_class(LuaClassDecl {
        name: "smelt.git.Commit",
        classification: crate::lua::doc::classification_for_type("smelt.git.Commit"),
        doc: "One commit of a history page or a tracking list.",
        fields: vec![
            LuaClassField {
                name: "id",
                ty: "string".into(),
                optional: false,
                doc: "Full commit ID.",
            },
            LuaClassField {
                name: "author",
                ty: "string".into(),
                optional: false,
                doc: "Author name.",
            },
            LuaClassField {
                name: "age",
                ty: "string".into(),
                optional: false,
                doc: "Relative author date, such as `2 days ago`.",
            },
            LuaClassField {
                name: "subject",
                ty: "string".into(),
                optional: false,
                doc: "First line of the message.",
            },
            LuaClassField {
                name: "message",
                ty: "string".into(),
                optional: false,
                doc: "Full raw message.",
            },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.git.Snapshot",
        classification: crate::lua::doc::classification_for_type("smelt.git.Snapshot"),
        doc: "A `smelt.git.diff`, `git.index` or `git.discard` result. Branch fields are present only for working snapshots.",
        fields: vec![
            LuaClassField { name: "root", ty: "string".into(), optional: false, doc: "Repository root." },
            LuaClassField { name: "diff", ty: "smelt.diff.Diff".into(), optional: true, doc: "The snapshot. Nil when a mutation succeeded but its refresh failed." },
            LuaClassField { name: "index_updated", ty: "boolean".into(), optional: true, doc: "True when `git.index` or `git.discard` changed the repository." },
            LuaClassField { name: "refresh_error", ty: "string".into(), optional: true, doc: "Why the refresh after a successful mutation failed." },
            LuaClassField { name: "branch", ty: "string".into(), optional: true, doc: "Checked-out branch. Nil when detached." },
            LuaClassField { name: "head", ty: "string".into(), optional: true, doc: "`HEAD` commit. Nil before the first commit." },
            LuaClassField { name: "upstream", ty: "string".into(), optional: true, doc: "Upstream such as `origin/main`. Nil when none is configured or it was deleted." },
            LuaClassField { name: "ahead", ty: "integer".into(), optional: true, doc: "Commits on `HEAD` but not on the upstream." },
            LuaClassField { name: "behind", ty: "integer".into(), optional: true, doc: "Commits on the upstream but not on `HEAD`." },
            LuaClassField { name: "operation", ty: "string".into(), optional: true, doc: "Rebase, merge, cherry-pick, revert or bisect in progress." },
        ],
    });
    let history_shared = Arc::clone(shared);
    git.private_live_only_fn(
        "__start_history",
        &["task_id", "opts"],
        move |_, (id, opts): (u64, mlua::Table)| -> LuaResult<LuaDiffJob> {
            let cwd = opts
                .get::<Option<String>>("cwd")?
                .map(|path| history_shared.resolve_project_path(path))
                .unwrap_or_else(|| history_shared.evaluation_cwd());
            let base = opts.get::<Option<String>>("base")?;
            let branch = opts.get::<Option<bool>>("branch")?.unwrap_or(false);
            let head = opts.get::<Option<String>>("head")?;
            let skip = opts.get::<Option<usize>>("skip")?.unwrap_or(0);
            let result: DiffResult = Arc::new(Mutex::new(None));
            let output = result.clone();
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = history_shared.resume_sink();
            tokio::spawn(async move {
                let value =
                    crate::git::review::history(cwd, base, branch, head, skip, cancel).await;
                *output.lock().unwrap() = Some(value.map(GitResult::History));
                sink.resolve_json(id, serde_json::Value::Null);
            });
            Ok(LuaDiffJob(result))
        },
    )?;
    let stats_shared = Arc::clone(shared);
    git.private_live_only_fn(
        "__start_stats",
        &["task_id", "opts"],
        move |_, (id, opts): (u64, mlua::Table)| -> LuaResult<LuaDiffJob> {
            let cwd = opts
                .get::<Option<String>>("cwd")?
                .map(|path| stats_shared.resolve_project_path(path))
                .unwrap_or_else(|| stats_shared.evaluation_cwd());
            let commits = opts.get::<Vec<String>>("commits")?;
            let result: DiffResult = Arc::new(Mutex::new(None));
            let output = result.clone();
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = stats_shared.resume_sink();
            tokio::spawn(async move {
                let value = crate::git::review::stats(cwd, commits, cancel).await;
                *output.lock().unwrap() = Some(value.map(GitResult::Stats));
                sink.resolve_json(id, serde_json::Value::Null);
            });
            Ok(LuaDiffJob(result))
        },
    )?;
    let index_shared = Arc::clone(shared);
    let shared = Arc::clone(shared);
    git.private_live_only_fn(
        "__start_diff",
        &["task_id", "opts"],
        move |lua, (id, opts): (u64, Option<mlua::Table>)| -> LuaResult<LuaDiffJob> {
            let opts = match opts {
                Some(opts) => opts,
                None => lua.create_table()?,
            };
            let cwd = opts
                .get::<Option<String>>("cwd")?
                .map(|path| shared.resolve_project_path(path))
                .unwrap_or_else(|| shared.evaluation_cwd());
            let context = opts.get::<Option<usize>>("context")?.unwrap_or(3);
            let revision = opts.get::<Option<String>>("revision")?;
            let base = opts.get::<Option<String>>("base")?;
            if base.is_some() && revision.is_none() {
                return Err(LuaError::external("base requires revision"));
            }
            let result: DiffResult = Arc::new(Mutex::new(None));
            let output = Arc::clone(&result);
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = shared.resume_sink();
            let wakeup = shared.wakeup_tx.get().cloned();
            tokio::spawn(async move {
                let value = match revision {
                    Some(revision) => {
                        crate::git::review::comparison(cwd, revision, base, context, cancel).await
                    }
                    None => crate::git::working_diff(cwd, context, cancel).await,
                };
                if let Ok(snapshot) = &value {
                    snapshot.view.set_wakeup(wakeup);
                }
                *output.lock().unwrap() = Some(value.map(GitResult::Snapshot));
                sink.resolve_json(id, serde_json::Value::Null);
            });
            Ok(LuaDiffJob(result))
        },
    )?;
    let change_shared = Arc::clone(&index_shared);
    git.private_live_only_fn(
        "__start_change",
        &["task_id", "diff", "file", "action", "last"],
        move |_,
              (id, diff, index, action, last): (u64, LuaDiff, usize, String, Option<usize>)|
              -> LuaResult<LuaDiffJob> {
            let start = index
                .checked_sub(1)
                .ok_or_else(|| LuaError::external("invalid diff file index"))?;
            let end = last.unwrap_or(index);
            if start >= end || end > diff.0.diff.files.len() {
                return Err(LuaError::external("invalid diff file range"));
            }
            if diff.0.diff.files[start].section != diff.0.diff.files[end - 1].section {
                return Err(LuaError::external(
                    "index selection must belong to one section",
                ));
            }
            let action = change_action(&action)?;
            start_change(
                &change_shared,
                id,
                diff,
                move |root, diff, cancel| async move {
                    let files = &diff.0.diff.files[start..end];
                    match action {
                        HunkAction::Index(action) => {
                            crate::git::update_index(root, files, action, cancel).await
                        }
                        HunkAction::Discard => crate::git::discard(root, files, cancel).await,
                    }
                },
            )
        },
    )?;
    git.private_live_only_fn(
        "__start_hunk_change",
        &["task_id", "diff", "first", "last", "action"],
        move |_,
              (id, diff, first, last, action): (u64, LuaDiff, u64, u64, String)|
              -> LuaResult<LuaDiffJob> {
            let action = change_action(&action)?;
            let selection = diff
                .0
                .selection(first.min(last)..=first.max(last))
                .ok_or_else(|| LuaError::external("no changes selected"))?;
            start_change(
                &index_shared,
                id,
                diff,
                move |root, diff, cancel| async move {
                    crate::git::change_hunks(root, &diff.0.diff, &selection, action, cancel).await
                },
            )
        },
    )?;
    Ok(())
}
