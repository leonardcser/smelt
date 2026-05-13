//! `smelt.clipboard` — read/write the system clipboard.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let clipboard_tbl = lua.create_table()?;
    record_module_doc("smelt.clipboard", "Read and write the system clipboard.");

    register_fn(
        &clipboard_tbl,
        "smelt.clipboard",
        "write",
        "Write `text` to the system clipboard. Raises if the clipboard backend is unavailable.",
        &["text"],
        lua,
        |_, text: String| -> LuaResult<()> {
            crate::host::with_core(|core| core.clipboard.write(&text))
                .map_err(LuaError::RuntimeError)?;
            Ok(())
        },
    )?;
    register_fn(
        &clipboard_tbl,
        "smelt.clipboard",
        "read",
        "Read the current clipboard contents as a string, or `nil` if empty/unavailable.",
        &[],
        lua,
        |_, ()| Ok(crate::host::try_with_core(|core| core.clipboard.read()).flatten()),
    )?;

    smelt.set("clipboard", clipboard_tbl)?;
    Ok(())
}
