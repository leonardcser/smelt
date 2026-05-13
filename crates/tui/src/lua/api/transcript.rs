//! `smelt.transcript` bindings — read the rendered transcript display
//! text and yank the current block. Thin live-state surface over `TuiApp`.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let transcript_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.transcript",
        "Read rendered transcript display text and yank the current block. UiHost-only.",
    );
    register_ui_fn(
        &transcript_tbl,
        "smelt.transcript",
        "text",
        "Return `text` from the app state.",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                app.full_transcript_display_text(app.core.config.settings.show_thinking)
                    .join("\n")
            })
            .unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &transcript_tbl,
        "smelt.transcript",
        "yank_block",
        "Copy the transcript block under the cursor to the system clipboard. Notifies the user with the copied range; no-op when the cursor is outside any block.",
        &[],
        lua,
        |_, ()|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.yank_current_block());
            Ok(())
        },
    )?;
    smelt.set("transcript", transcript_tbl)?;
    Ok(())
}
