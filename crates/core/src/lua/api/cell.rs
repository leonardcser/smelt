//! `smelt.cell` — typed reactive cell registry. `smelt.cell(name)`
//! returns a sticky `Cell` handle whose `:get` / `:set` / `:subscribe`
//! methods drive the cell. `smelt.cell.new(name, initial)` declares a
//! new cell. `smelt.cell.glob(pattern, handler)` subscribes across
//! every cell name matching `pattern`; both subscriptions return a
//! `Reg` userdata whose `:remove()` drops the subscription.

use crate::lua::doc::{record_alias, record_class, Tier};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaClassDecl, LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaHandle;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    record_class(LuaClassDecl {
        name: "smelt.cell.Cell",
        doc: "Sticky handle returned by `smelt.cell(name)`. Setters return the handle for chaining; `:subscribe` returns a `Reg`.",
        fields: crate::class_methods! {
            "get" => fn() -> mlua::Value, "Return the current cell value, or `nil` when the cell isn't declared.",
            "set" => fn(value: mlua::Value) -> LuaCell, "Publish a new value. Returns the handle for chaining.",
            "subscribe" => fn(handler: LuaCallback<mlua::Value, ()>) -> LuaReg, "Register `handler(value)` to fire on every `set`. Returns a `Reg` whose `:remove()` drops the subscription. No-op when called before the host pointer is live (e.g. the pre-TUI plugin pass) — the module body re-runs inside `bring_up_lua` where the bind takes effect.",
            "name" => fn() -> String, "Return the cell name.",
        },
    });

    let _ = shared;

    let m = LuaMod::under(
        lua,
        smelt,
        "cell",
        "Typed reactive cell registry. `smelt.cell(name)` returns a sticky \
`Cell` handle with `:get`, `:set`, `:subscribe`, `:name`. `smelt.cell.new` declares \
a cell with an initial value. `smelt.cell.glob` subscribes across every name \
matching a glob pattern.",
        Tier::Host,
    )?;

    m.fn_(
        "new",
        "Declare a cell named `name` with `initial` as its starting value. No-op if the cell already exists.",
        &["name", "initial"],
        |lua, (name, initial): (LuaCellName, mlua::Value)| -> LuaResult<()> {
            let key = lua.create_registry_value(initial)?;
            crate::host::try_with_core(|core| {
                core.cells.declare(name.0, LuaCellValue { key });
            });
            Ok(())
        },
    )?;

    m.fn_(
        "glob",
        "Register `handler(name, value)` for every cell whose name matches `pattern` (glob syntax). Returns a `Reg` whose `:remove()` drops the glob subscription.",
        &["pattern", "handler"],
        |lua,
         (pattern, handler): (String, LuaCallback<(String, mlua::Value), ()>)|
         -> LuaResult<LuaReg> {
            let pat = glob::Pattern::new(&pattern)
                .map_err(|e| LuaError::RuntimeError(format!("invalid glob `{pattern}`: {e}")))?;
            let handle = LuaHandle::from_func(lua, handler.into_inner())?;
            let id = crate::host::try_with_core(|core| {
                core.cells
                    .glob_subscribe(pat, SubscriberKind::Lua(Rc::new(handle)))
            })
            .unwrap_or(0);
            Ok(LuaReg::new(move || {
                crate::host::try_with_core(|core| core.cells.unsubscribe_glob(id)).unwrap_or(false)
            }))
        },
    )?;

    // Metatable __call so `smelt.cell(name)` returns a sticky handle.
    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (_tbl, name): (mlua::Table, String)| Ok(LuaCell { name }))?,
    )?;
    m.tbl.set_metatable(Some(mt))?;

    Ok(())
}

/// Sticky handle for a single cell. Returned by `smelt.cell(name)`.
pub struct LuaCell {
    name: String,
}

impl LuaType for LuaCell {
    fn lua_type() -> String {
        "smelt.cell.Cell".into()
    }
}

impl mlua::UserData for LuaCell {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        use crate::cells::{LuaCellValue, SubscriberKind};
        use std::rc::Rc;

        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Cell({})", this.name))
        });

        methods.add_method("get", |lua, this, _: ()| -> LuaResult<mlua::Value> {
            Ok(
                crate::host::try_with_core(|core| core.cells.get_lua(&this.name, lua))
                    .unwrap_or(mlua::Value::Nil),
            )
        });

        methods.add_function(
            "set",
            |lua,
             (this_ud, value): (mlua::AnyUserData, mlua::Value)|
             -> LuaResult<mlua::AnyUserData> {
                let name = this_ud.borrow::<LuaCell>()?.name.clone();
                let key = lua.create_registry_value(value)?;
                crate::host::try_with_core(|core| {
                    core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
                });
                Ok(this_ud)
            },
        );

        methods.add_method(
            "subscribe",
            |lua, this, handler: mlua::Function| -> LuaResult<LuaReg> {
                let name = this.name.clone();
                // Host live → subscribe immediately and return a Reg
                // whose `:remove()` unsubscribes. Host not live (e.g.
                // pre-TUI plugin pass) → silently no-op; the module
                // body re-runs inside `bring_up_lua` where the bind
                // takes effect on the second pass.
                let handle = LuaHandle::from_func(lua, handler)?;
                let sub_id = crate::host::try_with_core(|core| {
                    core.cells
                        .subscribe_kind(&name, SubscriberKind::Lua(Rc::new(handle)))
                })
                .flatten();
                let Some(sub_id) = sub_id else {
                    return Ok(LuaReg::new(|| false));
                };
                let name_for_reg = name;
                Ok(LuaReg::new(move || {
                    crate::host::try_with_core(|core| core.cells.unsubscribe(&name_for_reg, sub_id))
                        .unwrap_or(false)
                }))
            },
        );

        methods.add_method("name", |_, this, _: ()| -> LuaResult<String> {
            Ok(this.name.clone())
        });
    }
}
