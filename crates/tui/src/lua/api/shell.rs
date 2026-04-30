//! `smelt.shell` bindings — pure parsing helpers reused from the core
//! bash tool. Plugins that wrap `bash` (like `background_commands`)
//! call these to validate commands the same way before spawning.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let shell_tbl = lua.create_table()?;
    shell_tbl.set(
        "split",
        lua.create_function(|_, command: String| {
            Ok(engine::permissions::split_shell_commands(&command))
        })?,
    )?;
    shell_tbl.set(
        "split_with_ops",
        lua.create_function(|lua, command: String| {
            let parts = engine::permissions::split_shell_commands_with_ops(&command);
            let out = lua.create_table()?;
            for (i, (cmd, op)) in parts.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("command", cmd)?;
                if let Some(op) = op {
                    row.set("op", op)?;
                }
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    shell_tbl.set(
        "check_interactive",
        lua.create_function(|_, command: String| {
            Ok(engine::tools::check_interactive(&command).map(String::from))
        })?,
    )?;
    shell_tbl.set(
        "check_background_op",
        lua.create_function(|_, command: String| {
            Ok(engine::tools::check_shell_background_operator(&command))
        })?,
    )?;
    smelt.set("shell", shell_tbl)?;
    Ok(())
}
