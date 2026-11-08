//! Native row-source handles shared by Lua data providers and UI windows.

use crate::lua::doc::{record_class, Tier};
use crate::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use smelt_buffer::document::{RowSource, TextDocument};
use std::sync::Arc;

#[derive(Clone)]
pub struct LuaDocument(pub Arc<dyn RowSource>);

impl LuaType for LuaDocument {
    fn lua_type() -> String {
        "smelt.document.Document".into()
    }
}

impl FromLua for LuaDocument {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(value) => Ok(value.borrow::<Self>()?.clone()),
            _ => Err(LuaError::external("expected a smelt.document.Document")),
        }
    }
}

impl mlua::UserData for LuaDocument {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("row_count", |_, this, ()| Ok(this.0.snapshot().total_rows));
    }
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "document",
        "Indexed read-only documents shared by native data providers and Lua-created windows.",
        Tier::Host,
    )?;
    record_class(LuaClassDecl {
        name: "smelt.document.Document",
        classification: crate::lua::doc::classification_for_type("smelt.document.Document"),
        doc: "Native random-access row source. Attach with `win:document(source)`; each window owns its viewport, scratch storage and highlighting subscription. Sharing a document shares its row projection, including folds; use `diff:view()` for independent diff folds. Only requested rows are materialized. Copy/search access does not request background highlighting. Row coordinates are zero-based.",
        fields: vec![LuaClassField { name: "row_count", ty: "fun(self: smelt.document.Document): integer".into(), optional: false, doc: "Current visible row count, including fold markers." }],
    });
    m.fn_("text", "Index plain text once without creating a Lua table or buffer line per row. Attach the result with `win:document(source)` for viewport-only rendering and Vim navigation.", &["text"],
        |_, text: String| -> LuaResult<LuaDocument> { Ok(LuaDocument(Arc::new(TextDocument::new(text)))) })?;
    let file_shared = Arc::clone(shared);
    m.fn_("file", "Read a file for previewing, up to `opts.max_bytes` (default 16 MiB; longer files end with a `[preview truncated]` row). Returns a document of numbered source lines, or `(nil, err)` where `err` is `\"binary\"` for binary files. Only rows a window shows are built; their syntax colours are parsed on a background thread from a nearby point, so a deep line costs the same as the first. Options: `{ max_bytes?, query?, regex?, line? }`. Lines matching `query` (same rules as `smelt.files.grep`) are highlighted, and the 1-based `line` gets the cursor-line background.", &["path", "opts"],
        move |_, (path, opts): (String, Option<mlua::Table>)| -> LuaResult<(Option<LuaDocument>, Option<String>)> {
            use super::files::field;
            let max_bytes = field::<usize>(&opts, "max_bytes")?.filter(|&n| n > 0).unwrap_or(16 << 20);
            let pattern = match field::<String>(&opts, "query")? {
                Some(query) if !query.is_empty() => smelt_transcript::code_search::Pattern::new(
                    &query,
                    field::<bool>(&opts, "regex")?.unwrap_or(false),
                )
                .ok(),
                _ => None,
            };
            let focus = field::<u32>(&opts, "line")?.filter(|&n| n > 0);
            let path = file_shared.resolve_project_path(std::path::PathBuf::from(path));
            let marks = smelt_transcript::file_preview::Marks { pattern, focus };
            match smelt_transcript::file_preview::FilePreview::open(&path, max_bytes, marks) {
                Ok(preview) => {
                    preview.set_wakeup(file_shared.wakeup_tx.get().cloned());
                    Ok((Some(LuaDocument(Arc::new(preview))), None))
                }
                Err(smelt_transcript::file_preview::OpenError::Binary) => Ok((None, Some("binary".into()))),
                Err(smelt_transcript::file_preview::OpenError::Io(err)) => Ok((None, Some(err))),
            }
        })?;
    Ok(())
}
