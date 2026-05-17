//! `smelt.theme` bindings — read / write theme roles, snapshot the
//! current palette, enumerate built-in presets.

use super::{color_ansi_from_lua, color_to_lua, group_color, theme_set, theme_snapshot_pairs};
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "theme",
        "Read and write theme highlight groups, snapshot the current palette, and enumerate built-in color presets. UiHost-only. Highlight groups follow nvim's PascalCase convention (`Comment`, `SmeltAccent`, …). Writing `SmeltAccent` or `SmeltSlug` is special: it bumps the corresponding palette index and rebuilds dependent groups.",
        Tier::UiHost,
    )?;
    m.fn_(
        "get",
        "Return the resolved foreground (or background) color for highlight group `group` (PascalCase: `Comment`, `ErrorMsg`, `SmeltAccent`, …) as a `{ ansi, rgb? }` table. Unknown groups resolve to the terminal's default fg.",
        &["group"],
        |lua, group: String| -> LuaResult<mlua::Table> {
            let color = crate::lua::with_app(|app| group_color(app.ui.theme(), &group));
            color_to_lua(lua, color)
        },
    )?;
    m.fn_(
        "set",
        "Set highlight group `group`'s color. Pass a `{ ansi = N }` or `{ rgb = { r, g, b } }` table; RGB snaps to the closest 256-color slot. Setting `SmeltAccent` or `SmeltSlug` also bumps the corresponding palette index and rebuilds dependent groups; other groups only have their fg replaced.",
        &["group", "value"],
        |_, (group, value): (String, mlua::Table)| -> LuaResult<()> {
            let ansi = color_ansi_from_lua(&value)?;
            crate::lua::with_app(|app| theme_set(app.ui.theme_mut(), &group, ansi));
            Ok(())
        },
    )?;
    m.fn_(
        "link",
        "Alias theme role `from` to `to` so reads of `from` resolve to `to`'s current color. Lets plugins reuse semantic groups (`MyPluginAccent` → `SmeltAccent`).",
        &["from", "to"],
        |_, (from, to): (String, String)| -> LuaResult<()> {
            crate::lua::with_app(|app| app.ui.theme_mut().link(from, to));
            Ok(())
        },
    )?;
    m.fn_(
        "snapshot",
        "Snapshot every known theme role and its current color into a `{ role = color }` table. Useful for theme-aware pickers and diagnostic dumps.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let t = lua.create_table()?;
            let pairs = crate::lua::with_app(|app| theme_snapshot_pairs(app.ui.theme()));
            for (name, color) in pairs {
                t.set(name, color_to_lua(lua, color)?)?;
            }
            Ok(t)
        },
    )?;
    m.fn_(
        "is_light",
        "Return `true` if the active theme is a light theme. Lets plugins flip glyphs or contrast levels based on the current palette.",
        &[],
        |_, ()| Ok(crate::lua::with_app(|app| app.ui.theme().is_light())),
    )?;
    // Built-in color presets for Lua-side pickers.
    m.fn_(
        "presets",
        "Built-in color presets for Lua-side pickers.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let list = lua.create_table()?;
            for (i, (name, detail, ansi)) in crate::theme::PRESETS.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("name", *name)?;
                entry.set("detail", *detail)?;
                entry.set("ansi", *ansi)?;
                list.set(i + 1, entry)?;
            }
            Ok(list)
        },
    )?;
    Ok(())
}
