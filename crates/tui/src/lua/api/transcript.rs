//! `smelt.transcript` bindings — read the rendered transcript display
//! text and yank the current block. Thin live-state surface over `TuiApp`.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let transcript_tbl = lua.create_table()?;
    transcript_tbl.set(
        "text",
        app_read!(lua, |app| app
            .full_transcript_display_text(app.core.config.settings.show_thinking)
            .join("\n")),
    )?;
    transcript_tbl.set(
        "yank_block",
        lua.create_function(|_, ()| {
            crate::lua::with_app(|app| app.yank_current_block());
            Ok(())
        })?,
    )?;
    smelt.set("transcript", transcript_tbl)?;
    Ok(())
}
