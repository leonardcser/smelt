//! `smelt.au` bindings — nvim-shaped `au.on` / `au.fire` aliases over
//! `smelt.cell(name):subscribe` / `:set`. The underlying registry is
//! one and the same; this surface exists for nvim familiarity.

use crate::lua::LuaHandle;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::app::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    let au_tbl = lua.create_table()?;

    // `smelt.au.on(name, fn)` — thin alias over
    // `smelt.cell(name):subscribe(fn)`. The cell must already be
    // declared; subscribing to an undeclared name returns `nil`.
    au_tbl.set(
        "on",
        lua.create_function(
            |lua, (name, handler): (String, mlua::Function)| -> LuaResult<mlua::Value> {
                let key = lua.create_registry_value(handler)?;
                let id = crate::lua::try_with_app(|app| {
                    app.core
                        .cells
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

    // `smelt.au.fire(name, payload)` — thin alias over
    // `smelt.cell(name):set(payload)`. Returns `true` on success,
    // `false` when the cell isn't declared.
    au_tbl.set(
        "fire",
        lua.create_function(
            |lua, (name, payload): (String, mlua::Value)| -> LuaResult<bool> {
                let key = lua.create_registry_value(payload)?;
                Ok(crate::lua::try_with_app(|app| {
                    app.core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
                })
                .unwrap_or(false))
            },
        )?,
    )?;

    smelt.set("au", au_tbl)?;
    Ok(())
}
