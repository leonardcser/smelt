//! `smelt.perf` — timing helpers for Lua.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let perf_tbl = lua.create_table()?;

    perf_tbl.set(
        "time",
        lua.create_function(|_, (label, cb): (String, mlua::Function)| {
            let label: &'static str = Box::leak(label.into_boxed_str());
            let _perf = smelt_perf::perf::begin(label);
            cb.call::<mlua::MultiValue>(())
        })?,
    )?;

    smelt.set("perf", perf_tbl)?;
    Ok(())
}
