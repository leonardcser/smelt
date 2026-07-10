//! `smelt.signal` - named reactive values. `smelt.signal.get(name)` reads the
//! current value, `smelt.signal.set(name, value)` publishes a new value,
//! `smelt.signal.subscribe(name, handler)` observes changes, and
//! `smelt.signal.new(name, initial)` declares a new signal.
//! `smelt.signal.glob(pattern, handler)` subscribes across every signal name
//! matching `pattern`; both subscriptions return a `Reg` userdata whose
//! `:remove()` drops the subscription.

use crate::lua::doc::{record_alias, Tier};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaType, LuaTypeTuple};
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
            variants: crate::signals::builtin_signal_names(),
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
    use crate::signals::{LuaSignalValue, SubscriberKind};
    use std::rc::Rc;

    let m = LuaMod::under(
        lua,
        smelt,
        "signal",
        "Named reactive values. `smelt.signal.get(name)` reads the current value, `smelt.signal.set(name, value)` publishes a new value, `smelt.signal.subscribe(name, handler)` observes changes, and `smelt.signal.new(name, initial)` declares a new signal. `smelt.signal.glob` subscribes across every name matching a glob pattern. Use `smelt.events.on` for event-shaped signals where only occurrences matter.",
        Tier::Host,
    )?;

    m.fn_(
        "get",
        "Return the current value of signal `name`, or `nil` when the signal is not declared.",
        &["name"],
        |lua, name: LuaSignalName| -> LuaResult<mlua::Value> { signal_get(lua, &name.0) },
    )?;

    {
        let shared = Arc::clone(shared);
        m.fn_(
            "set",
            "Publish `value` for signal `name`.",
            &["name", "value"],
            move |lua, (name, value): (LuaSignalName, mlua::Value)| -> LuaResult<()> {
                signal_set(lua, &name.0, value, shared.generation_id())
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        m.fn_(
            "subscribe",
            "Register `handler(value, previous)` for signal `name`. Returns a `Reg` whose `:remove()` drops the subscription.",
            &["name", "handler"],
            move |lua,
                  (name, handler): (
                LuaSignalName,
                LuaCallback<(mlua::Value, mlua::Value), ()>,
            )|
                  -> LuaResult<LuaReg> {
                signal_subscribe(
                    lua,
                    name.0,
                    handler.into_inner(),
                    shared.generation_id(),
                )
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        m.fn_(
            "new",
            "Declare a signal named `name` with `initial` as its starting value. No-op if the signal already exists.",
            &["name", "initial"],
            move |lua, (name, initial): (LuaSignalName, mlua::Value)| -> LuaResult<()> {
                let key = lua.create_registry_value(initial)?;
                let generation = shared.generation_id();
                crate::host::try_with_core(|core| {
                    core.signals.declare_if_missing_for_generation(
                        name.0,
                        LuaSignalValue { key },
                        generation,
                    );
                });
                Ok(())
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        m.fn_(
            "glob",
            "Register `handler(name, value, previous)` for every signal whose name matches `pattern` (glob syntax). Returns a `Reg` whose `:remove()` drops the glob subscription.",
            &["pattern", "handler"],
            move |lua,
                  (pattern, handler): (
                String,
                LuaCallback<(String, mlua::Value, mlua::Value), ()>,
            )|
                  -> LuaResult<LuaReg> {
                let pat = glob::Pattern::new(&pattern).map_err(|error| {
                    LuaError::RuntimeError(format!("invalid glob `{pattern}`: {error}"))
                })?;
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                let generation = shared.generation_id();
                let id = crate::host::try_with_core(|core| {
                    core.signals.glob_subscribe_for_generation(
                        pat,
                        SubscriberKind::Lua(Rc::new(handle)),
                        generation,
                    )
                })
                .unwrap_or(0);
                Ok(LuaReg::new(move || {
                    crate::host::try_with_core(|core| core.signals.unsubscribe_glob(id))
                        .unwrap_or(false)
                }))
            },
        )?;
    }

    Ok(())
}

pub(super) fn subscribe_lua_signal(name: String, handle: LuaHandle, generation: u64) -> LuaReg {
    let sub_id = crate::host::try_with_core(|core| {
        core.signals.subscribe_kind_for_generation(
            &name,
            crate::signals::SubscriberKind::Lua(std::rc::Rc::new(handle)),
            generation,
        )
    })
    .flatten();
    let Some(sub_id) = sub_id else {
        return LuaReg::new(|| false);
    };
    LuaReg::new(move || {
        crate::host::try_with_core(|core| core.signals.unsubscribe(&name, sub_id)).unwrap_or(false)
    })
}

pub(super) fn subscribe_lua_event(name: String, handle: LuaHandle, generation: u64) -> LuaReg {
    crate::host::try_with_core(|core| {
        core.signals.declare_if_missing_for_generation(
            name.clone(),
            crate::signals::EventStub,
            generation,
        );
    });
    subscribe_lua_signal(name, handle, generation)
}

fn signal_get(lua: &Lua, name: &str) -> LuaResult<mlua::Value> {
    Ok(
        crate::host::try_with_core(|core| core.signals.get_lua(name, lua))
            .unwrap_or(mlua::Value::Nil),
    )
}

fn signal_set(lua: &Lua, name: &str, value: mlua::Value, generation: u64) -> LuaResult<()> {
    let key = lua.create_registry_value(value)?;
    crate::host::try_with_core(|core| {
        if core.lua_generation == generation {
            core.signals.set_dyn(
                name,
                std::rc::Rc::new(crate::signals::LuaSignalValue { key }),
            );
        }
    });
    Ok(())
}

fn signal_subscribe(
    lua: &Lua,
    name: String,
    handler: mlua::Function,
    generation: u64,
) -> LuaResult<LuaReg> {
    let handle = LuaHandle::from_func(lua, handler)?;
    Ok(subscribe_lua_signal(name, handle, generation))
}
