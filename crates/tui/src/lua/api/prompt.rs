//! `smelt.prompt` bindings — the main editable input surface.
//!
//! `win()` returns a `Win` userdata for the prompt input so plugins can
//! bind keys / events via the chainable handle API. `text()` snapshots
//! the current buffer; `set_text(s)` replaces it.

use mlua::prelude::*;
use smelt_buffer::attachment::ATTACHMENT_MARKER;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "prompt",
        "The main editable input surface: win handle, text get/set, and cursor control. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "win",
        "Return a `Win` handle for the prompt input. Use `win:key(...)` and `win:on(...)` to attach plugin behaviour.",
        &[],
        |_, ()| Ok(super::win::LuaWin { id: crate::app::PROMPT_WIN }),
    )?;
    m.fn_(
        "text",
        "Return the prompt input buffer's current text. Internal attachment markers are stripped — plugins see only the user-visible characters.",
        &[],
        |_, ()| {
            Ok(crate::lua::try_with_app(|app| {
                app.prompt_buf().source().replace(ATTACHMENT_MARKER, "")
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "set_text",
        "Replace the prompt buffer with `text`. The cursor lands at the end and undo state is reset.",
        &["text"],
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let mut pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                app.input.replace_text(&mut pctx, text);
            });
            Ok(())
        },
    )?;
    m.fn_(
        "set_section",
        "Set the named prompt section (e.g. selection context, attached files) to `content`. Sections render above the editable text and are submitted with the next turn.",
        &["name", "content"],
        |_, (name, content): (String, String)| -> LuaResult<()> {
            crate::lua::with_app(|app| app.prompt_sections.set(&name, content));
            Ok(())
        },
    )?;
    m.fn_(
        "remove_section",
        "Remove the named prompt section. No-op if the section does not exist.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.prompt_sections.remove(&name));
            Ok(())
        },
    )?;
    register_ghost(&m)?;
    Ok(())
}

fn register_ghost(prompt: &LuaMod) -> LuaResult<()> {
    let m = prompt.sub(
        "ghost",
        "Ghost text on the prompt — dim suggestion shown after the cursor. UiHost-only.",
    )?;
    m.fn_(
        "set",
        "Set the prompt's ghost text (the dim suggestion shown after the cursor). Replaces any existing ghost completion.",
        &["text"],
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.set_prompt_completer(text));
            Ok(())
        },
    )?;
    m.fn_(
        "clear",
        "Clear the prompt's ghost text. Idempotent.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.clear_prompt_completer());
            Ok(())
        },
    )?;
    Ok(())
}
