//! `smelt.clipboard` - read/write the system clipboard.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "clipboard",
        "Read and write the system clipboard.",
        Tier::Host,
    )?;
    m.live_only_fn(
        "write",
        "Write `text` to the system clipboard. Raises if the clipboard backend is unavailable.",
        &["text"],
        |_, text: String| -> LuaResult<()> {
            crate::host::with_core(|core| core.clipboard.write(&text))
                .map_err(LuaError::RuntimeError)?;
            Ok(())
        },
    )?;
    m.live_only_fn(
        "read",
        "Read the current clipboard contents as a string, or `nil` if empty/unavailable.",
        &[],
        |_, ()| Ok(crate::host::try_with_core(|core| core.clipboard.read()).flatten()),
    )?;
    Ok(())
}
