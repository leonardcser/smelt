//! `smelt.cell` bindings — typed reactive registry shared with Rust
//! subscribers and built-in cells. The flat `smelt.cell.{new, get, set,
//! subscribe, unsubscribe, glob_subscribe, glob_unsubscribe}` API plus
//! the `smelt.cell(name)` userdata handle both route to the same
//! `Cells` registry on the host.

use crate::lua::LuaHandle;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    let cell_tbl = lua.create_table()?;

    // `smelt.cell.new(name, initial)` declares a cell and stores
    // `initial` as its starting value. Idempotent: redeclaring resets
    // the value and drops every prior subscriber. Returns nothing —
    // callers pass the same `name` to `get` / `set` / `subscribe`.
    cell_tbl.set(
        "new",
        lua.create_function(
            |lua, (name, initial): (String, mlua::Value)| -> LuaResult<()> {
                let key = lua.create_registry_value(initial)?;
                crate::host::try_with_host(|host| {
                    host.cells().declare(name, LuaCellValue { key });
                });
                Ok(())
            },
        )?,
    )?;

    // `smelt.cell.get(name)` returns the cell's current value or
    // `nil` when undeclared / when no projector is registered for
    // the cell's value type.
    cell_tbl.set(
        "get",
        lua.create_function(|lua, name: String| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_host(|host| host.cells().get_lua(&name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        })?,
    )?;

    // `smelt.cell.set(name, value)` replaces the cell's value and
    // queues every subscriber for fire on the next drain. Returns
    // `true` on success, `false` when the cell isn't declared.
    cell_tbl.set(
        "set",
        lua.create_function(
            |lua, (name, value): (String, mlua::Value)| -> LuaResult<bool> {
                let key = lua.create_registry_value(value)?;
                Ok(crate::host::try_with_host(|host| {
                    host.cells().set_dyn(&name, Rc::new(LuaCellValue { key }))
                })
                .unwrap_or(false))
            },
        )?,
    )?;

    // `smelt.cell.subscribe(name, fn)` registers a Lua callback to
    // fire each time `name` is `set`. Returns the subscription id
    // `unsubscribe` accepts, or `nil` when `name` isn't declared.
    cell_tbl.set(
        "subscribe",
        lua.create_function(
            |lua, (name, handler): (String, mlua::Function)| -> LuaResult<mlua::Value> {
                let key = lua.create_registry_value(handler)?;
                let id = crate::host::try_with_host(|host| {
                    host.cells()
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

    // `smelt.cell.unsubscribe(name, id)` drops the subscription and
    // returns `true` on success, `false` when the cell or id is
    // unknown.
    cell_tbl.set(
        "unsubscribe",
        lua.create_function(|_, (name, id): (String, u64)| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_host(|host| host.cells().unsubscribe(&name, id))
                    .unwrap_or(false),
            )
        })?,
    )?;

    // `smelt.cell:glob_subscribe(pattern, fn)` fires `fn(name, value)`
    // for every cell whose name matches the glob pattern. Returns the
    // id `glob_unsubscribe` accepts. Errors when `pattern` is not a
    // valid glob.
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
                Ok(crate::host::try_with_host(|host| {
                    host.cells()
                        .glob_subscribe(pat, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
                })
                .unwrap_or(0))
            },
        )?,
    )?;

    // `smelt.cell.glob_unsubscribe(id)` drops a glob subscription and
    // returns `true` on success, `false` when `id` is unknown.
    cell_tbl.set(
        "glob_unsubscribe",
        lua.create_function(|_, id: u64| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_host(|host| host.cells().unsubscribe_glob(id))
                    .unwrap_or(false),
            )
        })?,
    )?;

    // `smelt.cell(name)` returns a `CellHandle` userdata bound to
    // `name`. Methods `:get()`, `:set(v)`, `:subscribe(fn)`,
    // `:unsubscribe(id)` route to the same registry the flat API
    // uses — `cell.new(name, …)` must have run first.
    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (_tbl, name): (mlua::Table, String)| Ok(CellHandle { name }))?,
    )?;
    cell_tbl.set_metatable(Some(mt))?;

    smelt.set("cell", cell_tbl)?;
    Ok(())
}

/// Userdata handle returned by `smelt.cell(name)`. Stores the cell
/// name; methods reach the live `Cells` registry via the TLS host
/// pointer the same way the flat `smelt.cell.*` bindings do.
struct CellHandle {
    name: String,
}

impl mlua::UserData for CellHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        use crate::cells::{LuaCellValue, SubscriberKind};
        use std::rc::Rc;

        methods.add_method("get", |lua, this, _: ()| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_host(|host| host.cells().get_lua(&this.name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        });

        methods.add_method("set", |lua, this, value: mlua::Value| -> LuaResult<bool> {
            let key = lua.create_registry_value(value)?;
            Ok(crate::host::try_with_host(|host| {
                host.cells()
                    .set_dyn(&this.name, Rc::new(LuaCellValue { key }))
            })
            .unwrap_or(false))
        });

        methods.add_method(
            "subscribe",
            |lua, this, handler: mlua::Function| -> LuaResult<mlua::Value> {
                let key = lua.create_registry_value(handler)?;
                let id = crate::host::try_with_host(|host| {
                    host.cells()
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
                crate::host::try_with_host(|host| host.cells().unsubscribe(&this.name, id))
                    .unwrap_or(false),
            )
        });

        methods.add_method("name", |_, this, _: ()| -> LuaResult<String> {
            Ok(this.name.clone())
        });
    }
}
