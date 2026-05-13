//! `smelt.path` — pure path arithmetic (normalize, join, relative, expand, display, etc.).

use crate::lua::doc::register_fn;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use std::path::{Path, PathBuf};

#[lua_module(
    name = "smelt.path",
    doc = "Pure path arithmetic: normalize, join, relative, expand, display, etc."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let path_tbl = lua.create_table()?;
    register_fn(
        &path_tbl,
        "smelt.path",
        "normalize",
        "Normalize `p` by collapsing redundant `.`, `..`, and separator components without touching the filesystem.",
        &["p"],
        lua,
        |_, p: String| Ok(to_string(crate::path::normalize(&p))),
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "canonical",
        "Resolve `p` to its canonical absolute form (following symlinks). Returns `(path, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match crate::path::canonical(&p) {
            Ok(resolved) => Ok((Some(to_string(resolved)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "relative",
        "Return the path of `target` expressed relative to `base`.",
        &["base", "target"],
        lua,
        |_, (base, target): (String, String)| Ok(to_string(crate::path::relative(&base, &target))),
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "expand",
        "Expand a leading `~` in `p` to the user's home directory.",
        &["p"],
        lua,
        |_, p: String| -> LuaResult<String> { Ok(to_string(crate::path::expand_home(&p))) },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "join",
        "Join the variadic `parts` into a single path using the platform separator.",
        &["parts"],
        lua,
        |_, parts: mlua::Variadic<String>| -> LuaResult<String> {
            let mut out = PathBuf::new();
            for part in parts {
                out.push(part);
            }
            Ok(to_string(out))
        },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "parent",
        "Return the parent directory of `p`, or `nil` if `p` has no parent component.",
        &["p"],
        lua,
        |_, p: String| -> LuaResult<Option<String>> {
            Ok(Path::new(&p).parent().map(|x| to_string(x.to_path_buf())))
        },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "basename",
        "Return the final component (file name) of `p`, or `nil` if `p` ends in `..`.",
        &["p"],
        lua,
        |_, p: String| -> LuaResult<Option<String>> {
            Ok(Path::new(&p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned()))
        },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "extension",
        "Return the file extension of `p` (without the leading dot), or `nil` if there is none.",
        &["p"],
        lua,
        |_, p: String| {
            Ok(Path::new(&p)
                .extension()
                .map(|s| s.to_string_lossy().into_owned()))
        },
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "is_absolute",
        "Return `true` if `p` is an absolute path on the current platform.",
        &["p"],
        lua,
        |_, p: String| Ok(Path::new(&p).is_absolute()),
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "display",
        "Return a user-friendly rendering of `p` for UI display (e.g. with the home dir abbreviated to `~`).",
        &["p"],
        lua,
        |_, p: String| Ok(crate::tools::display_path(&p)),
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "config_dir",
        "Return the absolute path to smelt's user config directory.",
        &[],
        lua,
        |_, ()| Ok(to_string(crate::config::config_dir())),
    )?;

    register_fn(
        &path_tbl,
        "smelt.path",
        "commands_dir",
        "Return the absolute path to the slash-commands directory under the user config root.",
        &[],
        lua,
        |_, ()| Ok(to_string(crate::config::config_dir().join("commands"))),
    )?;

    smelt.set("path", path_tbl)?;
    Ok(())
}

fn to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}
