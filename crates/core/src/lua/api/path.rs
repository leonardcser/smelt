//! `smelt.path` - pure path arithmetic (normalize, join, relative, expand, display, etc.).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::path::{Path, PathBuf};

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "path",
        "Pure path arithmetic: normalize, join, relative, expand, display, etc.",
        Tier::Host,
    )?;
    m.fn_(
        "normalize",
        "Normalize `p` by collapsing redundant `.`, `..`, and separator components without touching the filesystem.",
        &["p"],
        |_, p: String| Ok(to_string(crate::path::normalize(&p))),
    )?;

    m.fn_(
        "canonical",
        "Resolve `p` to its canonical absolute form (following symlinks). Returns `(path, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        |_, p: String| match crate::path::canonical(&p) {
            Ok(resolved) => Ok((Some(to_string(resolved)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "relative",
        "Return the path of `target` expressed relative to `base`.",
        &["base", "target"],
        |_, (base, target): (String, String)| Ok(to_string(crate::path::relative(&base, &target))),
    )?;

    m.fn_(
        "expand",
        "Expand a leading `~` in `p` to the user's home directory.",
        &["p"],
        |_, p: String| -> LuaResult<String> { Ok(to_string(crate::path::expand_home(&p))) },
    )?;

    m.fn_(
        "join",
        "Join the variadic `parts` into a single path using the platform separator.",
        &["parts"],
        |_, parts: mlua::Variadic<String>| -> LuaResult<String> {
            let mut out = PathBuf::new();
            for part in parts {
                out.push(part);
            }
            Ok(to_string(out))
        },
    )?;

    m.fn_(
        "parent",
        "Return the parent directory of `p`, or `nil` if `p` has no parent component.",
        &["p"],
        |_, p: String| -> LuaResult<Option<String>> {
            Ok(Path::new(&p).parent().map(|x| to_string(x.to_path_buf())))
        },
    )?;

    m.fn_(
        "basename",
        "Return the final component (file name) of `p`, or `nil` if `p` ends in `..`.",
        &["p"],
        |_, p: String| -> LuaResult<Option<String>> {
            Ok(Path::new(&p)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned()))
        },
    )?;

    m.fn_(
        "extension",
        "Return the file extension of `p` (without the leading dot), or `nil` if there is none.",
        &["p"],
        |_, p: String| {
            Ok(Path::new(&p)
                .extension()
                .map(|s| s.to_string_lossy().into_owned()))
        },
    )?;

    m.fn_(
        "is_absolute",
        "Return `true` if `p` is an absolute path on the current platform.",
        &["p"],
        |_, p: String| Ok(Path::new(&p).is_absolute()),
    )?;

    m.fn_(
        "display",
        "Return a user-friendly rendering of `p` for UI display (e.g. with the home dir abbreviated to `~`).",
        &["p"],
        |_, p: String| Ok(crate::tools::display_path(&p)),
    )?;

    m.fn_(
        "config_dir",
        "Return the absolute path to smelt's user config directory.",
        &[],
        |_, ()| Ok(to_string(crate::config::config_dir())),
    )?;

    m.fn_(
        "commands_dir",
        "Return the absolute path to the slash-commands directory under the user config root.",
        &[],
        |_, ()| Ok(to_string(crate::config::config_dir().join("commands"))),
    )?;

    Ok(())
}

fn to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}
