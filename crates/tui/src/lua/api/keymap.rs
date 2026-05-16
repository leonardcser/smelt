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
            "Bind `chord` in `mode` to a Lua callback. `mode` is `\"n\"|\"i\"|\"v\"|\"\"` (or the long form `normal`/`insert`/`visual`); the chord is canonicalized at registration and unknown values raise immediately. Re-binding the same `(mode, chord)` overwrites the prior handler.",
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
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                if let Ok(mut map) = s.keymaps.lock() {
                    map.insert((canonical_mode, canonical_chord), handle);
                }
                Ok(())
            },
        )?;
    }
    {
        let s = shared.clone();
        register_ui_fn(
            smelt_keymap,
            "smelt.keymap",
            "unset",
            "Drop the binding for `chord` in `mode`. `mode` accepts the same forms as `set`. Returns `true` if a binding was removed.",
            &["mode", "chord"],
            lua,
            move |_, (mode, chord): (String, String)| -> LuaResult<bool> {
                let canonical_mode = crate::lua::normalize_mode(&mode).ok_or_else(|| {
                    LuaError::RuntimeError(format!(
                        "keymap.unset: unknown mode `{mode}` (expected \"n\"|\"i\"|\"v\"|\"\" or \"normal\"|\"insert\"|\"visual\")"
                    ))
                })?;
                let canonical_chord = crate::lua::canonicalize_chord_sequence(&chord)
                    .ok_or_else(|| {
                        LuaError::RuntimeError(format!("keymap.unset: unknown chord `{chord}`"))
                    })?;
                Ok(s.keymaps
                    .lock()
                    .map(|mut m| m.remove(&(canonical_mode, canonical_chord)).is_some())
                    .unwrap_or(false))
            },
        )?;
    }
    {
        let s = shared.clone();
        register_ui_fn(
            smelt_keymap,
            "smelt.keymap",
            "list",
            "Return the set of currently-bound `{ mode, chord }` rows. `mode` is the canonical short form (`\"n\"`/`\"i\"`/`\"v\"`/`\"\"`).",
            &[],
            lua,
            move |lua, ()| -> LuaResult<mlua::Table> {
                let mut rows: Vec<(String, String)> = s
                    .keymaps
                    .lock()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                rows.sort();
                let out = lua.create_table()?;
                for (i, (mode, chord)) in rows.into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("mode", mode)?;
                    row.set("chord", chord)?;
                    out.set(i + 1, row)?;
                }
                Ok(out)
            },
        )?;
    }
    Ok(())
}
