//! `smelt.keymap` — register chord→callback and expose `keymap.help`.
//! Chords and modes are canonicalized at registration; unknown values raise immediately.

use crate::lua::{LuaHandle, LuaShared};
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;
use smelt_core::lua::lua_type::LuaCallback;
use std::sync::Arc;

#[lua_module(
    name = "smelt.keymap",
    doc = "Register key bindings and query layered help sections. UiHost-only."
)]
pub(super) fn register(
    lua: &Lua,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    let keymap_tbl = lua.create_table()?;
    register_ui_fn(
        &keymap_tbl,
        "smelt.keymap",
        "help_sections",
        "Return layered keybinding help as `{ title, entries = { { label, detail } } }` rows. Filters vim-only chords when vim mode is disabled.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let vim_enabled =
                crate::lua::try_with_app(|app| app.input.vim_enabled(app.prompt_win()))
                    .unwrap_or(false);
            let sections = crate::keymap::hints::help_sections(vim_enabled);
            let out = lua.create_table()?;
            for (i, (title, entries)) in sections.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("title", title)?;
                let entries_tbl = lua.create_table()?;
                for (j, (label, detail)) in entries.into_iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set("label", label)?;
                    entry.set("detail", detail)?;
                    entries_tbl.set(j + 1, entry)?;
                }
                row.set("entries", entries_tbl)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    smelt_keymap.set("help", keymap_tbl.get::<mlua::Function>("help_sections")?)?;

    {
        let s = shared.clone();
        register_ui_fn(
            smelt_keymap,
            "smelt.keymap",
            "set",
            "Bind `chord` in `mode` to a Lua callback. `mode` is `\"n\"|\"i\"|\"v\"|\"\"` (or the long form `normal`/`insert`/`visual`); the chord is canonicalized at registration and unknown values raise immediately.",
            &["mode", "chord", "handler"],
            lua,
            move |lua,
                  (mode, chord, handler): (String, String, LuaCallback<(), ()>)|
                  -> LuaResult<()> {
                let canonical_mode = crate::lua::normalize_mode(&mode).ok_or_else(
                    || {
                        LuaError::RuntimeError(format!(
                            "keymap.set: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                        ))
                    },
                )?;
                let canonical_chord = crate::lua::canonicalize_chord_sequence(&chord)
                    .ok_or_else(|| {
                        LuaError::RuntimeError(format!(
                            "keymap.set: unknown chord `{chord}`"
                        ))
                    })?;
                let key = lua.create_registry_value(handler.into_inner())?;
                if let Ok(mut map) = s.keymaps.lock() {
                    map.insert((canonical_mode, canonical_chord), LuaHandle { key });
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}
