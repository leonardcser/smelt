//! `smelt.signal` - named reactive values. `smelt.signal.get(name)` reads the
//! current value, `smelt.signal.set(name, value)` publishes a new value,
//! `smelt.signal.subscribe(name, handler)` observes changes, and
//! `smelt.signal.new(name, initial)` declares a new signal.
//! `smelt.signal.glob(pattern, handler)` subscribes across every signal name
//! matching `pattern`, events included; both subscriptions return a `Reg`
//! userdata whose `:remove()` drops the subscription. Named access to an event
//! belongs to `smelt.events` and is rejected here.

use crate::lua::doc::{record_alias, Tier};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaHandle;
use crate::lua::LuaShared;
use crate::signals::SignalKind;
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
            doc: "Name of a state signal. Open alias - plugin-defined signals declared via `smelt.signal.new` are accepted alongside the well-known runtime signals listed here.",
            classification: crate::lua::doc::ApiClassification::Supported,
            variants: crate::signals::builtin_state_names(),
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

    let m = LuaMod::supported(
        lua,
        smelt,
        "signal",
        "Named reactive values. `smelt.signal.get(name)` reads the current value, `smelt.signal.set(name, value)` publishes a new value, `smelt.signal.subscribe(name, handler)` observes changes, and `smelt.signal.new(name, initial)` declares a new signal. `smelt.signal.glob` subscribes across every signal matching a glob pattern, events included. Occurrences such as `turn_start` are events, not signals: use `smelt.events.on` for them. Passing an event name to `smelt.signal.get`, `set`, `subscribe` or `new` raises an error, and so does using `set` or `subscribe` on a name that was never declared.",
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
        m.live_only_fn(
            "set",
            "Publish `value` for signal `name`.",
            &["name", "value"],
            move |lua, (name, value): (LuaSignalName, mlua::Value)| -> LuaResult<()> {
                require_declared_state(&name.0)?;
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
                require_declared_state(&name.0)?;
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
                require_kind(&name.0, SignalKind::State)?;
                let key = lua.create_registry_value(initial)?;
                let generation = shared.generation_id();
                crate::host::try_with_core(|core| {
                    core.signals.declare_if_missing_for_generation(
                        name.0,
                        SignalKind::State,
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
            "Register `handler(name, value, previous)` for every signal whose name matches `pattern` (glob syntax); `previous` is `nil` for events. Returns a `Reg` whose `:remove()` drops the glob subscription.",
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
            crate::signals::SignalKind::Event,
            crate::signals::EventStub,
            generation,
        );
    });
    subscribe_lua_signal(name, handle, generation)
}

fn kind_mismatch(name: &str, expected: SignalKind) -> LuaError {
    LuaError::RuntimeError(match expected {
        SignalKind::State => {
            format!("`{name}` is an event; use smelt.events.on / smelt.events.emit")
        }
        SignalKind::Event => {
            format!("`{name}` is a state signal; use smelt.signal.get / set / subscribe")
        }
    })
}

/// Reject a name already declared as the other kind, pointing at the module that owns it.
/// Undeclared names pass: the caller decides what that means.
pub(super) fn require_kind(name: &str, expected: SignalKind) -> LuaResult<()> {
    match crate::host::try_with_core(|core| core.signals.kind(name)).flatten() {
        Some(actual) if actual != expected => Err(kind_mismatch(name, expected)),
        _ => Ok(()),
    }
}

/// Require `name` to be a declared state signal, so a typo fails loudly instead of leaving a
/// subscription that never fires or a write that is dropped.
fn require_declared_state(name: &str) -> LuaResult<()> {
    match crate::host::try_with_core(|core| core.signals.kind(name)) {
        Some(None) => Err(LuaError::RuntimeError(format!(
            "signal `{name}` is not declared; create it with smelt.signal.new(name, initial)"
        ))),
        Some(Some(SignalKind::Event)) => Err(kind_mismatch(name, SignalKind::State)),
        _ => Ok(()),
    }
}

fn signal_get(lua: &Lua, name: &str) -> LuaResult<mlua::Value> {
    match crate::host::try_with_core(|core| core.signals.get_state_lua(name, lua)) {
        None => Ok(mlua::Value::Nil),
        Some(Ok(value)) => Ok(value),
        Some(Err(_)) => Err(kind_mismatch(name, SignalKind::State)),
    }
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
