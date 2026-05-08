//! `smelt.mode` — get/set/cycle agent mode. `set` and `cycle` are stubs here; TUI/Lua override them.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let mode_tbl = lua.create_table()?;

    mode_tbl.set(
        "get",
        lua.create_function(|_, ()| {
            Ok(
                crate::host::try_with_core(|core| core.config.mode.as_str().to_string())
                    .unwrap_or_default(),
            )
        })?,
    )?;

    mode_tbl.set(
        "set",
        lua.create_function(|_, _v: String| {
            // No-op in core; TUI overrides this binding.
            Ok(())
        })?,
    )?;

    mode_tbl.set(
        "cycle_list",
        lua.create_function(|lua, ()| {
            let cycle: Vec<String> = crate::host::try_with_core(|core| {
                let cycle: &[protocol::AgentMode] = if core.config.mode_cycle.is_empty() {
                    protocol::AgentMode::ALL
                } else {
                    &core.config.mode_cycle
                };
                cycle.iter().map(|m| m.as_str().to_string()).collect()
            })
            .unwrap_or_default();
            let t = lua.create_table()?;
            for (i, label) in cycle.into_iter().enumerate() {
                t.set(i + 1, label)?;
            }
            Ok(t)
        })?,
    )?;

    mode_tbl.set("cycle", lua.create_function(|_, ()| Ok(()))?)?;

    smelt.set("mode", mode_tbl)?;
    Ok(())
}
