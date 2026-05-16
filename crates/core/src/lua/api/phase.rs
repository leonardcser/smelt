//! `smelt.phase` — query the current boot phase.
//!
//! Returns one of `"early"`, `"init"`, `"running"`. Plugins use this to
//! branch behavior or assert preconditions:
//!
//! ```lua
//! if smelt.phase() ~= "early" then
//!   error("cli.register_flag must be called from early.lua")
//! end
//! ```

use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let s = shared.clone();
    let phase_fn = lua.create_function(move |_, ()| -> LuaResult<&'static str> {
        Ok(s.phase().as_str())
    })?;
    smelt.set("phase", phase_fn)?;
    Ok(())
}
