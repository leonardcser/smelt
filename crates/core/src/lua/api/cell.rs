//! `smelt.cell` — typed reactive cell registry. Surface is a flat
//! table for one-shot reads/writes and a callable that hands back
//! a sticky [`CellHandle`] userdata for repeated access (`local c =
//! smelt.cell("foo"); c:set(1)`).

use crate::lua::doc::{record_alias, record_class, register_fn};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaClassDecl, LuaType, LuaTypeTuple};
use crate::lua::LuaHandle;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

/// Lua-facing string type for cell names. Renders as
/// `string | "vim_mode" | "agent_mode" | ...` in the generated
/// LuaCATS so plugin authors get autocomplete for the well-known
/// runtime cells while custom names declared via `smelt.cell.new`
/// still type-check.
#[derive(Clone, Debug)]
pub struct LuaCellName(pub String);

impl LuaType for LuaCellName {
    fn lua_type() -> String {
        record_alias(LuaAliasDecl {
            name: "smelt.cell.Name",
            doc: "Name of a reactive cell. Open alias — plugin-defined cells \
declared via `smelt.cell.new` are accepted alongside the well-known \
runtime cells listed here.",
            variants: crate::cells::SEEDED_CELL_NAMES.to_vec(),
            open: true,
        });
        "smelt.cell.Name".into()
    }
}

impl LuaTypeTuple for LuaCellName {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("name");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

impl FromLua for LuaCellName {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        let s: String = FromLua::from_lua(value, lua)?;
        Ok(LuaCellName(s))
    }
}

impl IntoLua for LuaCellName {
    fn into_lua(self, lua: &Lua) -> LuaResult<mlua::Value> {
        IntoLua::into_lua(self.0, lua)
    }
}

impl std::ops::Deref for LuaCellName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

#[lua_module(
    name = "smelt.cell",
    doc = "Typed reactive cell registry. Surface is a flat table for one-shot \
reads/writes and a callable that hands back a sticky handle for repeated \
access (`local c = smelt.cell(\"foo\"); c:set(1)`). \
[`smelt.au`](au.md) is an nvim-shaped alias of `subscribe`/`set`."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    record_class(LuaClassDecl {
        name: "smelt.cell.CellHandle",
        doc: "Sticky handle returned by `smelt.cell(name)`. Provides `:get()`, `:set(value)`, `:subscribe(handler)`, `:unsubscribe(id)`, and `:name()` methods.",
        fields: crate::class_methods! {
            "get" => fn() -> mlua::Value, "Return the current cell value, or `nil` when the cell isn't declared.",
            "set" => fn(value: mlua::Value) -> bool, "Publish a new value. Returns `true` on success.",
            "subscribe" => fn(handler: LuaCallback<mlua::Value, ()>) -> Option<i64>, "Register handler(value) to fire on every set. Returns subscription id or `nil`.",
            "unsubscribe" => fn(id: i64) -> bool, "Drop the subscription with id. Returns `true` on success.",
            "name" => fn() -> String, "Return the cell name.",
        },
    });

    let cell_tbl = lua.create_table()?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "new",
        "Declare a cell named `name` with `initial` as its starting value. No-op if the cell already exists.",
        &["name", "initial"],
        lua,
        |lua, (name, initial): (LuaCellName, mlua::Value)| -> LuaResult<()> {
            let key = lua.create_registry_value(initial)?;
            crate::host::try_with_core(|core| {
                core.cells.declare(name.0, LuaCellValue { key });
            });
            Ok(())
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "get",
        "Return the current value of `name`, or `nil` when the cell isn't declared.",
        &["name"],
        lua,
        |lua, name: LuaCellName| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_core(|core| core.cells.get_lua(&name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "set",
        "Publish a new value to `name`. Returns `true` on success, `false` when the runtime has no host or the cell is undeclared.",
        &["name", "value"],
        lua,
        |lua, (name, value): (LuaCellName, mlua::Value)| -> LuaResult<bool> {
            let key = lua.create_registry_value(value)?;
            Ok(crate::host::try_with_core(|core| {
                core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
            })
            .unwrap_or(false))
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "subscribe",
        "Register `handler(value)` to fire on every `set`. Returns a subscription id (integer) or `nil` if the runtime has no host.",
        &["name", "handler"],
        lua,
        |lua,
         (name, handler): (LuaCellName, LuaCallback<mlua::Value, ()>)|
         -> LuaResult<Option<i64>> {
            let key = lua.create_registry_value(handler.into_inner())?;
            let id = crate::host::try_with_core(|core| {
                core.cells
                    .subscribe_kind(&name, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
            })
            .flatten();
            Ok(id.map(|n| n as i64))
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "unsubscribe",
        "Drop the subscription with id `id` from `name`. Returns `true` on success.",
        &["name", "id"],
        lua,
        |_, (name, id): (LuaCellName, u64)| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_core(|core| core.cells.unsubscribe(&name, id))
                    .unwrap_or(false),
            )
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "glob_subscribe",
        "Register `handler(name, value)` for every cell whose name matches `pattern` (glob syntax). Returns a glob-subscription id.",
        &["pattern", "handler"],
        lua,
        |lua,
         (pattern, handler): (String, LuaCallback<(String, mlua::Value), ()>)|
         -> LuaResult<u64> {
            let pat = glob::Pattern::new(&pattern)
                .map_err(|e| LuaError::RuntimeError(format!("invalid glob `{pattern}`: {e}")))?;
            let key = lua.create_registry_value(handler.into_inner())?;
            Ok(crate::host::try_with_core(|core| {
                core.cells
                    .glob_subscribe(pat, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
            })
            .unwrap_or(0))
        },
    )?;

    register_fn(
        &cell_tbl,
        "smelt.cell",
        "glob_unsubscribe",
        "Drop the glob subscription with id `id`. Returns `true` on success.",
        &["id"],
        lua,
        |_, id: u64| -> LuaResult<bool> {
            Ok(crate::host::try_with_core(|core| core.cells.unsubscribe_glob(id)).unwrap_or(false))
        },
    )?;

    // Metatable __call so `smelt.cell(name)` returns a sticky handle.
    // Not user-facing as a function on the table; lives outside the
    // doc-recorded surface.
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
