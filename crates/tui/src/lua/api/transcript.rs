//! `smelt.transcript` bindings — read the rendered transcript display
//! text. Thin live-state surface over `TuiApp`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "transcript",
        "Read rendered transcript display text. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "text",
        "Return the full transcript as a single newline-joined string (post-render display text, with thinking blocks visible according to the `show_thinking` setting).",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                app.full_transcript_display_text(app.core.config.settings.show_thinking)
                    .join("\n")
            })
            .unwrap_or_default())
        },
    )?;
    Ok(())
}
