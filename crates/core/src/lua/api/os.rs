//! `smelt.os` — environment and system primitives (getenv, setenv, platform, cwd, pid, etc.).

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let os = lua.create_table()?;
    record_module_doc(
        "smelt.os",
        "Environment and system primitives: getenv, setenv, platform, cwd, pid, etc.",
    );

    register_fn(
        &os,
        "smelt.os",
        "getenv",
        "Return the value of the environment variable `name`, or `nil` if it is not set.",
        &["name"],
        lua,
        |_, name: String| Ok(std::env::var(name).ok()),
    )?;

    register_fn(
        &os,
        "smelt.os",
        "setenv",
        "Set the process environment variable `name` to `value`. Mutates the live process env; visible to subsequent `getenv` calls and child processes.",
        &["name", "value"],
        lua,
        |_, (name, value): (String, String)|  -> LuaResult<()>{
            // SAFETY: Lua runs on a single thread; setenv on POSIX is
            // safe so long as nothing else is reading concurrently.
            unsafe { std::env::set_var(name, value) };
            Ok(())
        },
    )?;

    register_fn(
        &os,
        "smelt.os",
        "unsetenv",
        "Remove the environment variable `name` from the process environment.",
        &["name"],
        lua,
        |_, name: String| -> LuaResult<()> {
            unsafe { std::env::remove_var(name) };
            Ok(())
        },
    )?;

    register_fn(
        &os,
        "smelt.os",
        "platform",
        "Return the target operating system as reported by `std::env::consts::OS` (e.g. `\"macos\"`, `\"linux\"`).",
        &[],
        lua,
        |_, ()| Ok(std::env::consts::OS),
    )?;

    register_fn(
        &os,
        "smelt.os",
        "arch",
        "Return the target CPU architecture as reported by `std::env::consts::ARCH` (e.g. `\"x86_64\"`, `\"aarch64\"`).",
        &[],
        lua,
        |_, ()| Ok(std::env::consts::ARCH),
    )?;

    register_fn(
        &os,
        "smelt.os",
        "tempdir",
        "Return the platform temporary directory path.",
        &[],
        lua,
        |_, ()| Ok(std::env::temp_dir().to_string_lossy().into_owned()),
    )?;

    register_fn(
        &os,
        "smelt.os",
        "home",
        "Return the user's home directory, or `nil` if it cannot be determined.",
        &[],
        lua,
        |_, ()| Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
    )?;

    register_fn(
        &os,
        "smelt.os",
        "cwd",
        "Return the current working directory as `(path, nil)`, or `(nil, err_string)` on failure.",
        &[],
        lua,
        |_, ()| match std::env::current_dir() {
            Ok(p) => Ok((Some(p.to_string_lossy().into_owned()), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &os,
        "smelt.os",
        "set_cwd",
        "Change the process working directory to `p`. Returns `(true, nil)` on success or `(false, err_string)` on failure.",
        &["p"],
        lua,
        |_, p: String| match std::env::set_current_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        },
    )?;

    register_fn(
        &os,
        "smelt.os",
        "pid",
        "Return the OS process id of the running smelt instance.",
        &[],
        lua,
        |_, ()| Ok(std::process::id()),
    )?;

    smelt.set("os", os)?;
    Ok(())
}
