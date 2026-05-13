//! `smelt.perf` — timing helpers for Lua.

use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module(
    name = "smelt.perf",
    doc = "Lightweight scope timers that feed `smelt.metrics.perf_snapshot`. Wrap a hot Lua block to see where time goes when perf collection is enabled."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let perf_tbl = lua.create_table()?;

    register_fn(
        &perf_tbl,
        "smelt.perf",
        "time",
        "Run `fn()` and record its elapsed time under `label`. Returns whatever `fn` returns (single value). Cheap when perf collection is disabled.",
        &["label", "fn"],
        lua,
        |_, (label, cb): (String, LuaCallback<(), mlua::Value>)| -> LuaResult<mlua::Value> {
            let label: &'static str = Box::leak(label.into_boxed_str());
            let _perf = smelt_perf::perf::begin(label);
            cb.as_function().call::<mlua::Value>(())
        },
    )?;

    smelt.set("perf", perf_tbl)?;
    Ok(())
}
