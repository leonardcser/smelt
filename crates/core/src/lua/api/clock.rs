//! `smelt.clock` — wall-clock time primitives.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "clock",
        "Wall-clock time primitives. Host-tier so plugins can read absolute time in both TUI and headless contexts.",
        Tier::Host,
    )?;
    m.fn_(
        "unix_ms",
        "Return the current Unix timestamp in milliseconds. Backed by the host clock so tests can freeze time by overriding this function or swapping in a virtual clock.",
        &[],
        |_, ()| {
            let ms = crate::host::try_with_core(|core| {
                core.clock
                    .system_now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            })
            .unwrap_or(0);
            Ok(ms)
        },
    )?;
    Ok(())
}
