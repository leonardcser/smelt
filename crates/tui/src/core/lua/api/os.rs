//! `smelt.os` bindings — environment + system primitives. Host-tier
//! (works in tui and headless). Pure Rust-side surface; no Rust
//! capability module backs it (each binding is a one-liner over
//! `std`).

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let os = lua.create_table()?;

    os.set(
        "getenv",
        lua.create_function(|_, name: String| Ok(std::env::var(name).ok()))?,
    )?;

    os.set(
        "setenv",
        lua.create_function(|_, (name, value): (String, String)| {
            // SAFETY: Lua runs on a single thread; setenv on POSIX is
            // safe so long as nothing else is reading concurrently.
            unsafe { std::env::set_var(name, value) };
            Ok(())
        })?,
    )?;

    os.set(
        "unsetenv",
        lua.create_function(|_, name: String| {
            unsafe { std::env::remove_var(name) };
            Ok(())
        })?,
    )?;

    os.set(
        "platform",
        lua.create_function(|_, ()| Ok(std::env::consts::OS))?,
    )?;

    os.set(
        "arch",
        lua.create_function(|_, ()| Ok(std::env::consts::ARCH))?,
    )?;

    os.set(
        "tempdir",
        lua.create_function(|_, ()| Ok(std::env::temp_dir().to_string_lossy().into_owned()))?,
    )?;

    os.set(
        "home",
        lua.create_function(
            |_, ()| Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
        )?,
    )?;

    os.set(
        "cwd",
        lua.create_function(|_, ()| match std::env::current_dir() {
            Ok(p) => Ok((Some(p.to_string_lossy().into_owned()), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    os.set(
        "set_cwd",
        lua.create_function(|_, p: String| match std::env::set_current_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    os.set("pid", lua.create_function(|_, ()| Ok(std::process::id()))?)?;

    smelt.set("os", os)?;
    Ok(())
}
