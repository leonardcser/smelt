//! `smelt.reasoning` — get/set/cycle reasoning effort. Mirrors `smelt.mode`; stubs overridden by TUI/Lua.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let reasoning_tbl = lua.create_table()?;

    reasoning_tbl.set(
        "get",
        lua.create_function(|_, ()| {
            Ok(
                crate::host::try_with_core(|core| core.config.reasoning_effort.label().to_string())
                    .unwrap_or_default(),
            )
        })?,
    )?;

    reasoning_tbl.set(
        "set",
        lua.create_function(|_, _v: String| {
            // No-op in core; TUI overrides this binding.
            Ok(())
        })?,
    )?;

    reasoning_tbl.set(
        "cycle_list",
        lua.create_function(|lua, ()| {
            let labels: Vec<String> = crate::host::try_with_core(|core| {
                core.config
                    .reasoning_cycle
                    .iter()
                    .map(|e| e.label().to_string())
                    .collect()
            })
            .unwrap_or_default();
            let t = lua.create_table()?;
            for (i, label) in labels.into_iter().enumerate() {
                t.set(i + 1, label)?;
            }
            Ok(t)
        })?,
    )?;

    reasoning_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("reasoning", reasoning_tbl)?;
    Ok(())
}
