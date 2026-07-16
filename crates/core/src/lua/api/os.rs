//! `smelt.os` - environment and system primitives (getenv, setenv, platform, cwd, pid, etc.).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "os",
        "Environment and system primitives: getenv, setenv, platform, cwd, pid, etc.",
        Tier::Host,
    )?;
    m.fn_(
        "getenv",
        "Return the value of the environment variable `name`, or `nil` if it is not set.",
        &["name"],
        |_, name: String| Ok(std::env::var(name).ok()),
    )?;

    m.fn_(
        "setenv",
        "Set the process environment variable `name` to `value`. Mutates the live process env; visible to subsequent `getenv` calls and child processes.",
        &["name", "value"],
        |_, (name, value): (String, String)| -> LuaResult<()> {
            // `std::env::set_var` panics on empty / `=`-containing / NUL-
            // containing names; reject them as a Lua error instead.
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(mlua::Error::RuntimeError(format!(
                    "setenv: invalid name {name:?}"
                )));
            }
            if value.contains('\0') {
                return Err(mlua::Error::RuntimeError(
                    "setenv: value contains NUL".into(),
                ));
            }
            // SAFETY: Lua runs on a single thread; setenv on POSIX is
            // safe so long as nothing else is reading concurrently.
            unsafe { std::env::set_var(name, value) };
            Ok(())
        },
    )?;

    m.fn_(
        "unsetenv",
        "Remove the environment variable `name` from the process environment.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            // Mirror `setenv`'s validation; the libc call panics on the
            // same illegal inputs.
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                return Err(mlua::Error::RuntimeError(format!(
                    "unsetenv: invalid name {name:?}"
                )));
            }
            unsafe { std::env::remove_var(name) };
            Ok(())
        },
    )?;

    m.fn_(
        "platform",
        "Return the target operating system as reported by `std::env::consts::OS` (e.g. `\"macos\"`, `\"linux\"`).",
        &[],
        |_, ()| Ok(std::env::consts::OS),
    )?;

    m.fn_(
        "arch",
        "Return the target CPU architecture as reported by `std::env::consts::ARCH` (e.g. `\"x86_64\"`, `\"aarch64\"`).",
        &[],
        |_, ()| Ok(std::env::consts::ARCH),
    )?;

    m.fn_(
        "tempdir",
        "Return the platform temporary directory path.",
        &[],
        |_, ()| Ok(std::env::temp_dir().to_string_lossy().into_owned()),
    )?;

    m.fn_(
        "home",
        "Return the user's home directory, or `nil` if it cannot be determined.",
        &[],
        |_, ()| Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
    )?;

    let cwd_context = Arc::clone(shared);
    m.fn_(
        "cwd",
        "Return the current working directory as `(path, nil)`, or `(nil, err_string)` on failure.",
        &[],
        move |_, ()| {
            Ok((
                Some(cwd_context.evaluation_cwd().to_string_lossy().into_owned()),
                None::<String>,
            ))
        },
    )?;

    m.fn_(
        "set_cwd",
        "Change the process working directory to `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        |_, p: String| match std::env::set_current_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "exe_path",
        "Return the filesystem path to the running smelt binary as `(path, nil)` on success, or `(nil, err_string)` on failure. Useful for plugins that re-exec the binary or report install location.",
        &[],
        |_, ()| match std::env::current_exe() {
            Ok(p) => Ok((Some(p.to_string_lossy().into_owned()), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    m.fn_(
        "pid",
        "Return the OS process id of the running smelt instance.",
        &[],
        |_, ()| Ok(std::process::id()),
    )?;

    m.fn_(
        "open_url",
        "Open `url` in the system's default browser. Only `http(s)://`, `mailto:`, and `file://` URLs are accepted. Returns `(true, nil)` on a successful spawn, or `(false, err_string)` if the scheme is rejected or every launcher errored.",
        &["url"],
        |_, url: String| Ok(match engine::opener::open_url(&url) {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        }),
    )?;

    m.fn_(
        "open_url_if_available",
        "Open `url` only when the host environment can auto-open a browser. Returns `{ opened = bool, error = string?, reason = string? }`.",
        &["url"],
        |lua, url: String| -> LuaResult<mlua::Table> {
            let result = lua.create_table()?;
            match engine::opener::open_url_if_available(&url) {
                engine::opener::OpenResult::Opened => {
                    result.set("opened", true)?;
                }
                engine::opener::OpenResult::Unavailable(reason) => {
                    result.set("opened", false)?;
                    result.set("reason", reason)?;
                }
                engine::opener::OpenResult::Failed(err) => {
                    result.set("opened", false)?;
                    result.set("error", err)?;
                }
            }
            Ok(result)
        },
    )?;

    Ok(())
}
