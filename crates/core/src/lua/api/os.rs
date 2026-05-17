//! `smelt.os` — environment and system primitives (getenv, setenv, platform, cwd, pid, etc.).

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
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

    m.fn_(
        "cwd",
        "Return the current working directory as `(path, nil)`, or `(nil, err_string)` on failure.",
        &[],
        |_, ()| match std::env::current_dir() {
            Ok(p) => Ok((Some(p.to_string_lossy().into_owned()), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
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
        "pid",
        "Return the OS process id of the running smelt instance.",
        &[],
        |_, ()| Ok(std::process::id()),
    )?;

    Ok(())
}
