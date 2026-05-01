//! `smelt.path` bindings — pure path arithmetic over `tui::path`.
//! Host-tier (works in tui and headless) — no Ui touch.

use mlua::prelude::*;
use std::path::{Path, PathBuf};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let path_tbl = lua.create_table()?;

    path_tbl.set(
        "normalize",
        lua.create_function(|_, p: String| Ok(to_string(crate::path::normalize(&p))))?,
    )?;

    path_tbl.set(
        "canonical",
        lua.create_function(|_, p: String| match crate::path::canonical(&p) {
            Ok(resolved) => Ok((Some(to_string(resolved)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    path_tbl.set(
        "relative",
        lua.create_function(|_, (base, target): (String, String)| {
            Ok(to_string(crate::path::relative(&base, &target)))
        })?,
    )?;

    path_tbl.set(
        "expand",
        lua.create_function(|_, p: String| Ok(to_string(crate::path::expand_home(&p))))?,
    )?;

    path_tbl.set(
        "join",
        lua.create_function(|_, parts: mlua::Variadic<String>| {
            let mut out = PathBuf::new();
            for part in parts {
                out.push(part);
            }
            Ok(to_string(out))
        })?,
    )?;

    path_tbl.set(
        "parent",
        lua.create_function(|_, p: String| {
            Ok(Path::new(&p).parent().map(|x| to_string(x.to_path_buf())))
        })?,
    )?;

    path_tbl.set(
        "basename",
        lua.create_function(|_, p: String| {
            Ok(Path::new(&p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned()))
        })?,
    )?;

    path_tbl.set(
        "extension",
        lua.create_function(|_, p: String| {
            Ok(Path::new(&p)
                .extension()
                .map(|s| s.to_string_lossy().into_owned()))
        })?,
    )?;

    path_tbl.set(
        "is_absolute",
        lua.create_function(|_, p: String| Ok(Path::new(&p).is_absolute()))?,
    )?;

    // `smelt.path.display(p)` — the path the way smelt shows it in
    // confirm dialogs and tool summaries: relative to cwd if inside,
    // absolute otherwise. `"."` for cwd itself.
    path_tbl.set(
        "display",
        lua.create_function(|_, p: String| Ok(engine::tools::display_path(&p)))?,
    )?;

    // `smelt.path.config_dir()` — `~/.config/smelt` (or the
    // platform-specific equivalent). Resolved through the engine's
    // path helper so headless and tui agree on the lookup.
    path_tbl.set(
        "config_dir",
        lua.create_function(|_, ()| Ok(to_string(crate::config::config_dir())))?,
    )?;

    // `smelt.path.commands_dir()` — `~/.config/smelt/commands`. Used
    // by the custom-commands plugin to scan for user-defined `/foo`
    // markdown templates at startup.
    path_tbl.set(
        "commands_dir",
        lua.create_function(|_, ()| Ok(to_string(crate::config::config_dir().join("commands"))))?,
    )?;

    smelt.set("path", path_tbl)?;
    Ok(())
}

fn to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}
