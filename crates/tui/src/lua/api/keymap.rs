//! `smelt.keymap` — register chord→callback and expose `keymap.help`.
//! Chords and modes are canonicalized at registration; unknown values raise immediately.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    smelt_keymap: &mlua::Table,
    shared: &Arc<LuaShared>,
) -> LuaResult<()> {
    // smelt.keymap.help_sections() — layered binding help from `crate::keymap::hints`.
    let keymap_tbl = lua.create_table()?;
    keymap_tbl.set(
        "help_sections",
        lua.create_function(|lua, ()| {
            let vim_enabled =
                crate::lua::try_with_app(|app| app.input.vim_enabled()).unwrap_or(false);
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
        })?,
    )?;
    smelt_keymap.set("help", keymap_tbl.get::<mlua::Function>("help_sections")?)?;

    {
        let s = shared.clone();
        smelt_keymap.set(
            "set",
            lua.create_function(
                move |lua, (mode, chord, handler): (String, String, mlua::Function)| {
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
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.keymaps.lock() {
                        map.insert((canonical_mode, canonical_chord), LuaHandle { key });
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    Ok(())
}
