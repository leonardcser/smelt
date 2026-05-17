//! `smelt.prompt` bindings — the main editable input surface.
//!
//! `win()` returns a `Win` userdata for the prompt input so plugins can
//! bind keys / events via the chainable handle API. `text()` snapshots
//! the current buffer; `set_text(s)` replaces it.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.prompt",
    doc = "The main editable input surface: win handle, text get/set, and cursor control. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let prompt_tbl = lua.create_table()?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "win",
        "Return a `Win` handle for the prompt input. Use `win:key(...)` and `win:on(...)` to attach plugin behaviour.",
        &[],
        lua,
        |_, ()| Ok(super::win::LuaWin { id: crate::app::PROMPT_WIN }),
    )?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "text",
        "Return the prompt input buffer's current text.",
        &[],
        lua,
        |_, ()| {
            Ok(
                crate::lua::try_with_app(|app| app.prompt_buf().source().to_string())
                    .unwrap_or_default(),
            )
        },
    )?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "set_text",
        "Replace the prompt buffer with `text`. The cursor lands at the end and undo state is reset.",
        &["text"],
        lua,
        |_, text: String|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let mut pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                app.input.replace_text(&mut pctx, text);
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "set_section",
        "Set the named prompt section (e.g. selection context, attached files) to `content`. Sections render above the editable text and are submitted with the next turn.",
        &["name", "content"],
        lua,
        |_, (name, content): (String, String)|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.prompt_sections.set(&name, content));
            Ok(())
        },
    )?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "remove_section",
        "Remove the named prompt section. No-op if the section does not exist.",
        &["name"],
        lua,
        |_, name: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.prompt_sections.remove(&name));
            Ok(())
        },
    )?;
    register_ghost(lua, &prompt_tbl)?;
    smelt.set("prompt", prompt_tbl)?;
    Ok(())
}

#[lua_module(
    name = "smelt.prompt.ghost",
    doc = "Ghost text on the prompt — dim suggestion shown after the cursor. UiHost-only."
)]
fn register_ghost(lua: &Lua, prompt_tbl: &mlua::Table) -> LuaResult<()> {
    let ghost_tbl = lua.create_table()?;
    register_ui_fn(
        &ghost_tbl,
        "smelt.prompt.ghost",
        "set",
        "Set the prompt's ghost text (the dim suggestion shown after the cursor). Replaces any existing ghost completion.",
        &["text"],
        lua,
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.set_prompt_completer(text));
            Ok(())
        },
    )?;
    register_ui_fn(
        &ghost_tbl,
        "smelt.prompt.ghost",
        "clear",
        "Clear the prompt's ghost text. Idempotent.",
        &[],
        lua,
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.clear_prompt_completer());
            Ok(())
        },
    )?;
    prompt_tbl.set("ghost", ghost_tbl)?;
    Ok(())
}
