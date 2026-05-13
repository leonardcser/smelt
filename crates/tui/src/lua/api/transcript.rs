//! `smelt.transcript` bindings — read the rendered transcript display
//! text. Thin live-state surface over `TuiApp`.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.transcript",
    doc = "Read rendered transcript display text. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let transcript_tbl = lua.create_table()?;
    register_ui_fn(
        &transcript_tbl,
        "smelt.transcript",
        "text",
        "Return the full transcript as a single newline-joined string (post-render display text, with thinking blocks visible according to the `show_thinking` setting).",
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
    smelt.set("transcript", transcript_tbl)?;
    Ok(())
}
