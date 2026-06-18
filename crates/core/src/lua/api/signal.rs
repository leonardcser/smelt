//! `smelt.signal` - named reactive values. `smelt.signal(name)` returns a
//! sticky `Signal` handle whose `:get` / `:set` / `:subscribe` methods observe
//! and update the signal. `smelt.signal.new(name, initial)` declares a new
//! signal. `smelt.signal.glob(pattern, handler)` subscribes across every signal
//! name matching `pattern`; both subscriptions return a `Reg` userdata whose
//! `:remove()` drops the subscription.

use crate::lua::doc::{record_alias, record_class, Tier};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaClassDecl, LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaHandle;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

/// Lua-facing string type for signal names. Renders as
/// `string | "vim_mode" | "agent_mode" | ...` in the generated LuaCATS so
/// plugin authors get autocomplete for the well-known runtime signals while
/// custom names declared via `smelt.signal.new` still type-check.
#[derive(Clone, Debug)]
pub struct LuaSignalName(pub String);

impl LuaType for LuaSignalName {
    fn lua_type() -> String {
        record_alias(LuaAliasDecl {
            name: "smelt.signal.Name",
            doc: "Name of a reactive signal. Open alias - plugin-defined signals declared via `smelt.signal.new` are accepted alongside the well-known runtime signals listed here.",
            variants: crate::cells::builtin_signal_names(),
            open: true,
        });
        "smelt.signal.Name".into()
    }
}

impl LuaTypeTuple for LuaSignalName {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("name");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

impl FromLua for LuaSignalName {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        let s: String = FromLua::from_lua(value, lua)?;
        Ok(LuaSignalName(s))
    }
}

impl IntoLua for LuaSignalName {
    fn into_lua(self, lua: &Lua) -> LuaResult<mlua::Value> {
        IntoLua::into_lua(self.0, lua)
    }
}

impl std::ops::Deref for LuaSignalName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    record_class(LuaClassDecl {
        name: "smelt.signal.Signal",
        doc: "Sticky handle returned by `smelt.signal(name)`. Setters return the handle for chaining; `:subscribe` returns a `Reg`.",
        fields: crate::class_methods! {
            "get" => fn() -> mlua::Value, "Return the current signal value, or `nil` when the signal isn't declared.",
            "set" => fn(value: mlua::Value) -> LuaSignal, "Publish a new value. Returns the handle for chaining.",
            "subscribe" => fn(handler: LuaCallback<(mlua::Value, mlua::Value), ()>) -> LuaReg, "Register `handler(value, previous)` to fire on every `set`. Returns a `Reg` whose `:remove()` drops the subscription. No-op when called before the host pointer is live (e.g. the pre-TUI plugin pass). The module body re-runs inside `bring_up_lua` where the bind takes effect.",
            "name" => fn() -> String, "Return the signal name.",
        },
    });

    let _ = shared;

    let m = LuaMod::under(
        lua,
        smelt,
        "signal",
        "Named reactive values. `smelt.signal(name)` returns a sticky `Signal` handle with `:get`, `:set`, `:subscribe`, `:name`. `smelt.signal.new` declares a signal with an initial value. `smelt.signal.glob` subscribes across every name matching a glob pattern. Use `smelt.events.on` for event-shaped signals where only occurrences matter.",
        Tier::Host,
    )?;

    m.fn_(
        "new",
        "Declare a signal named `name` with `initial` as its starting value. No-op if the signal already exists.",
        &["name", "initial"],
        |lua, (name, initial): (LuaSignalName, mlua::Value)| -> LuaResult<()> {
            let key = lua.create_registry_value(initial)?;
            crate::host::try_with_core(|core| {
                core.cells.declare_if_missing(name.0, LuaCellValue { key });
            });
            Ok(())
        },
    )?;

    m.fn_(
        "glob",
        "Register `handler(name, value, previous)` for every signal whose name matches `pattern` (glob syntax). Returns a `Reg` whose `:remove()` drops the glob subscription.",
        &["pattern", "handler"],
        |lua,
         (pattern, handler): (String, LuaCallback<(String, mlua::Value, mlua::Value), ()>)|
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

    // Metatable __call so `smelt.signal(name)` returns a sticky handle.
    let mt = lua.create_table()?;
    mt.set(
        "__call",
        lua.create_function(|_, (_tbl, name): (mlua::Table, String)| Ok(LuaSignal { name }))?,
    )?;
    m.tbl.set_metatable(Some(mt))?;

    Ok(())
}

/// Sticky handle for a single signal. Returned by `smelt.signal(name)`.
pub struct LuaSignal {
    name: String,
}

impl LuaType for LuaSignal {
    fn lua_type() -> String {
        "smelt.signal.Signal".into()
    }
}

impl mlua::UserData for LuaSignal {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        use crate::cells::{LuaCellValue, SubscriberKind};
        use std::rc::Rc;

        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Signal({})", this.name))
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
                let name = this_ud.borrow::<LuaSignal>()?.name.clone();
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
