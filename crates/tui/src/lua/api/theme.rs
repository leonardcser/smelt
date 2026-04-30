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
            let color = crate::lua::with_ui_host(|host| host.ui().theme().accent_color());
            color_to_lua(lua, color)
        })?,
    )?;
    theme_tbl.set(
        "get",
        lua.create_function(|lua, role: String| {
            let color =
                crate::lua::with_ui_host(|host| theme_role_get(host.ui().theme(), &role))
                    .ok_or_else(|| LuaError::RuntimeError(format!("unknown theme role: {role}")))?;
            color_to_lua(lua, color)
        })?,
    )?;
    theme_tbl.set(
        "set",
        lua.create_function(|_, (role, value): (String, mlua::Table)| {
            let ansi = color_ansi_from_lua(&value)?;
            crate::lua::with_ui_host(|host| theme_role_set(host.ui().theme_mut(), &role, ansi))
        })?,
    )?;
    theme_tbl.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            let pairs = crate::lua::with_ui_host(|host| theme_snapshot_pairs(host.ui().theme()));
            for (name, color) in pairs {
                t.set(name, color_to_lua(lua, color)?)?;
            }
            Ok(t)
        })?,
    )?;
    theme_tbl.set(
        "is_light",
        lua.create_function(|_, ()| {
            Ok(crate::lua::with_ui_host(|host| {
                host.ui().theme().is_light()
            }))
        })?,
    )?;
    // Built-in color presets (name, description, ANSI-256 value).
    // Exposed so Lua-side pickers (`/theme`, `/color`) can use
    // them instead of hard-coding the list.
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
