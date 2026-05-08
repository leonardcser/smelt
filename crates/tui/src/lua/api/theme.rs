//! `smelt.theme` bindings — read / write theme roles, snapshot the
//! current palette, enumerate built-in presets.

use super::{
    color_ansi_from_lua, color_to_lua, theme_role_get, theme_role_set, theme_snapshot_pairs,
};
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let theme_tbl = lua.create_table()?;
    theme_tbl.set(
        "accent",
        lua.create_function(|lua, ()| {
            let color = crate::lua::with_app(|app| app.ui.theme().accent_color());
            color_to_lua(lua, color)
        })?,
    )?;
    theme_tbl.set(
        "get",
        lua.create_function(|lua, role: String| {
            let color = crate::lua::with_app(|app| theme_role_get(app.ui.theme(), &role))
                .ok_or_else(|| LuaError::RuntimeError(format!("unknown theme role: {role}")))?;
            color_to_lua(lua, color)
        })?,
    )?;
    theme_tbl.set(
        "set",
        lua.create_function(|_, (role, value): (String, mlua::Table)| {
            let ansi = color_ansi_from_lua(&value)?;
            crate::lua::with_app(|app| theme_role_set(app.ui.theme_mut(), &role, ansi))
        })?,
    )?;
    theme_tbl.set(
        "link",
        lua.create_function(|_, (from, to): (String, String)| {
            crate::lua::with_app(|app| app.ui.theme_mut().link(from, to));
            Ok(())
        })?,
    )?;
    theme_tbl.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            let pairs = crate::lua::with_app(|app| theme_snapshot_pairs(app.ui.theme()));
            for (name, color) in pairs {
                t.set(name, color_to_lua(lua, color)?)?;
            }
            Ok(t)
        })?,
    )?;
    theme_tbl.set(
        "is_light",
        lua.create_function(|_, ()| Ok(crate::lua::with_app(|app| app.ui.theme().is_light())))?,
    )?;
    // Built-in color presets for Lua-side pickers.
    theme_tbl.set(
        "presets",
        lua.create_function(|lua, ()| {
            let list = lua.create_table()?;
            for (i, (name, detail, ansi)) in crate::theme::PRESETS.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("name", *name)?;
                entry.set("detail", *detail)?;
                entry.set("ansi", *ansi)?;
                list.set(i + 1, entry)?;
            }
            Ok(list)
        })?,
    )?;
    smelt.set("theme", theme_tbl)?;
    Ok(())
}
