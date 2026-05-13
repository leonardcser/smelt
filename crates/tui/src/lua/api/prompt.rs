//! `smelt.prompt` bindings — the main editable input surface.
//!
//! `win_id()` returns the stable `WinId` so plugins can reuse
//! `smelt.win.on_event(prompt, "text_changed", …)` and
//! `smelt.win.set_keymap(prompt, …)`. `text()` snapshots the
//! current buffer; `set_text(s)` replaces it.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let prompt_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.prompt",
        "The main editable input surface: win_id, text get/set, and cursor control. UiHost-only.",
    );
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "win_id",
        "Return the stable `WinId` for the prompt input. Use with `smelt.win.on_event` and `smelt.win.set_keymap` to attach plugin behaviour.",
        &[],
        lua,
        |_, ()| Ok(crate::app::PROMPT_WIN.0),
    )?;
    register_ui_fn(
        &prompt_tbl,
        "smelt.prompt",
        "text",
        "Return `text` from the app state.",
        &[],
        lua,
        |_, ()| Ok(crate::lua::try_with_app(|app| app.input.source.clone()).unwrap_or_default()),
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
                let mode = app.vim_mode;
                app.input.replace_text(text, None, mode);
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
    smelt.set("prompt", prompt_tbl)?;
    Ok(())
}
