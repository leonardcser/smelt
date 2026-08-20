//! `smelt.path` - pure path arithmetic (normalize, join, relative, expand, display, etc.).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
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

    let canonical_context = Arc::clone(shared);
    m.fn_(
        "canonical",
        "Resolve `p` to its canonical absolute form (following symlinks). Returns `(path, nil)` on success or `(nil, err_string)` on failure.",
        &["p"],
        move |_, p: String| {
            match crate::path::canonical(canonical_context.resolve_project_path(p)) {
                Ok(resolved) => Ok((Some(to_string(resolved)), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;

    m.fn_(
        "relative",
        "Return the path of `target` expressed relative to `base`.",
        &["base", "target"],
        |_, (base, target): (String, String)| Ok(to_string(crate::path::relative(&base, &target))),
    )?;

    let expand_context = Arc::clone(shared);
    m.fn_(
        "expand",
        "Expand config-path syntax in `p`: leading `~`, `$VAR`, and `${VAR}`. Does not invoke a shell, expand globs, or canonicalize symlinks.",
        &["p"],
        move |_, p: String| -> LuaResult<String> {
            crate::path::expand_from(&p, &expand_context.runtime_home())
                .map(to_string)
                .map_err(mlua::Error::external)
        },
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

    let display_context = Arc::clone(shared);
    m.fn_(
        "display",
        "Return a user-friendly rendering of `p` for UI display (e.g. with the home dir abbreviated to `~`).",
        &["p"],
        move |_, p: String| {
            Ok(crate::path_display::display_path_from(
                &p,
                &display_context.evaluation_cwd(),
                &display_context.runtime_home(),
            ))
        },
    )?;

    let streaming_display_context = Arc::clone(shared);
    m.fn_(
        "display_streaming",
        "Return a user-friendly rendering of a possibly partial path for streaming tool summaries. Absolute paths that may still collapse to the current working directory or home directory return an empty string until enough path has arrived.",
        &["p"],
        move |_, p: String| {
            Ok(crate::path_display::display_path_streaming_from(
                &p,
                &streaming_display_context.evaluation_cwd(),
                &streaming_display_context.runtime_home(),
            ))
        },
    )?;

    m.fn_(
        "config_dir",
        "Return the absolute path to smelt's runtime config directory.",
        &[],
        |_, ()| {
            Ok(
                crate::host::try_with_core(|core| to_string(core.env.config_dir().clone()))
                    .unwrap_or_default(),
            )
        },
    )?;

    m.fn_(
        "commands_dir",
        "Return the absolute path to the slash-commands directory under the runtime config root.",
        &[],
        |_, ()| {
            Ok(
                crate::host::try_with_core(|core| {
                    to_string(core.env.config_dir().join("commands"))
                })
                .unwrap_or_default(),
            )
        },
    )?;

    Ok(())
}

fn to_string(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}
