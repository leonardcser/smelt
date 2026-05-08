//! `smelt.au` — nvim-shaped aliases over `smelt.cell` subscribe/set.

use crate::lua::LuaHandle;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    let au_tbl = lua.create_table()?;

    au_tbl.set(
        "on",
        lua.create_function(
            |lua, (name, handler): (String, mlua::Function)| -> LuaResult<mlua::Value> {
                let key = lua.create_registry_value(handler)?;
                let id = crate::host::try_with_core(|core| {
                    core.cells
                        .subscribe_kind(&name, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
                })
                .flatten();
                Ok(match id {
                    Some(id) => mlua::Value::Integer(id as i64),
                    None => mlua::Value::Nil,
                })
            },
        )?,
    )?;

    au_tbl.set(
        "fire",
        lua.create_function(
            |lua, (name, payload): (String, mlua::Value)| -> LuaResult<bool> {
                let key = lua.create_registry_value(payload)?;
                Ok(crate::host::try_with_core(|core| {
                    core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
                })
                .unwrap_or(false))
            },
        )?,
    )?;

    smelt.set("au", au_tbl)?;
    Ok(())
}
