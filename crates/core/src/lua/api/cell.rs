//! `smelt.cell` — typed reactive cell registry (flat API + callable userdata handle).

use crate::lua::LuaHandle;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    let cell_tbl = lua.create_table()?;

    cell_tbl.set(
        "new",
        lua.create_function(
            |lua, (name, initial): (String, mlua::Value)| -> LuaResult<()> {
                let key = lua.create_registry_value(initial)?;
                crate::host::try_with_core(|core| {
                    core.cells.declare(name, LuaCellValue { key });
                });
                Ok(())
            },
        )?,
    )?;

    cell_tbl.set(
        "get",
        lua.create_function(|lua, name: String| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_core(|core| core.cells.get_lua(&name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        })?,
    )?;

    cell_tbl.set(
        "set",
        lua.create_function(
            |lua, (name, value): (String, mlua::Value)| -> LuaResult<bool> {
                let key = lua.create_registry_value(value)?;
                Ok(crate::host::try_with_core(|core| {
                    core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
                })
                .unwrap_or(false))
            },
        )?,
    )?;

    cell_tbl.set(
        "subscribe",
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

    cell_tbl.set(
        "unsubscribe",
        lua.create_function(|_, (name, id): (String, u64)| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_core(|core| core.cells.unsubscribe(&name, id))
                    .unwrap_or(false),
            )
        })?,
    )?;

    cell_tbl.set(
        "glob_subscribe",
        lua.create_function(
            |lua,
             (_self, pattern, handler): (mlua::Value, String, mlua::Function)|
             -> LuaResult<u64> {
                let pat = glob::Pattern::new(&pattern).map_err(|e| {
                    LuaError::RuntimeError(format!("invalid glob `{pattern}`: {e}"))
                })?;
                let key = lua.create_registry_value(handler)?;
                Ok(crate::host::try_with_core(|core| {
                    core.cells
                        .glob_subscribe(pat, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
                })
                .unwrap_or(0))
            },
        )?,
    )?;

    cell_tbl.set(
        "glob_unsubscribe",
        lua.create_function(|_, id: u64| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| core.cells.unsubscribe_glob(id)).unwrap_or(false))
        })?,
    )?;

    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (_tbl, name): (mlua::Table, String)| Ok(CellHandle { name }))?,
    )?;
    cell_tbl.set_metatable(Some(mt))?;

    smelt.set("cell", cell_tbl)?;
    Ok(())
}

struct CellHandle {
    name: String,
}

impl mlua::UserData for CellHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        use crate::cells::{LuaCellValue, SubscriberKind};
        use std::rc::Rc;

        methods.add_method("get", |lua, this, _: ()| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_core(|core| core.cells.get_lua(&this.name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        });

        methods.add_method("set", |lua, this, value: mlua::Value| -> LuaResult<bool> {
            let key = lua.create_registry_value(value)?;
            Ok(crate::host::try_with_core(|core| {
                core.cells
                    .set_dyn(&this.name, Rc::new(LuaCellValue { key }))
            })
            .unwrap_or(false))
        });

        methods.add_method(
            "subscribe",
            |lua, this, handler: mlua::Function| -> LuaResult<mlua::Value> {
                let key = lua.create_registry_value(handler)?;
                let id = crate::host::try_with_core(|core| {
                    core.cells
                        .subscribe_kind(&this.name, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
                })
                .flatten();
                Ok(match id {
                    Some(id) => mlua::Value::Integer(id as i64),
                    None => mlua::Value::Nil,
                })
            },
        );

        methods.add_method("unsubscribe", |_, this, id: u64| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_core(|core| core.cells.unsubscribe(&this.name, id))
                    .unwrap_or(false),
            )
        });

        methods.add_method("name", |_, this, _: ()| -> LuaResult<String> {
            Ok(this.name.clone())
        });
    }
}
