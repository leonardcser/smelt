//! `smelt.events` - occurrence-oriented subscriptions over event-shaped
//! signals. Use this when the current value is not meaningful and plugin code
//! only cares that something happened.

use crate::lua::doc::{record_alias, Tier};
use crate::lua::lua_type::{LuaAliasDecl, LuaCallback, LuaType, LuaTypeTuple};
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::LuaHandle;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

/// Lua-facing string type for event names.
#[derive(Clone, Debug)]
pub struct LuaEventName(pub String);

impl LuaType for LuaEventName {
    fn lua_type() -> String {
        record_alias(LuaAliasDecl {
            name: "smelt.events.Name",
            doc: "Name of an event-shaped signal. Open alias - plugin-defined event names are accepted alongside the built-in events listed here.",
            variants: crate::cells::builtin_event_names(),
            open: true,
        });
        "smelt.events.Name".into()
    }
}

impl LuaTypeTuple for LuaEventName {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("event");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

impl FromLua for LuaEventName {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        let s: String = FromLua::from_lua(value, lua)?;
        Ok(LuaEventName(s))
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    use crate::cells::SubscriberKind;
    use std::rc::Rc;

    let _ = shared;

    let m = LuaMod::under(
        lua,
        smelt,
        "events",
        "Occurrence-oriented subscriptions over event-shaped signals such as `turn_start`, `tool_start`, and `confirm_requested`. Use `smelt.signal` when the current value matters; use `smelt.events.on` when only future occurrences matter.",
        Tier::Host,
    )?;

    m.fn_(
        "new",
        "Declare an event named `event`. Existing signals are left unchanged. `smelt.events.on` and `smelt.events.emit` also declare custom events automatically, so this is only needed when a plugin wants to document its event surface explicitly.",
        &["event"],
        |_, event: LuaEventName| -> LuaResult<()> {
            crate::host::try_with_core(|core| {
                core.cells.declare_if_missing(event.0, crate::cells::EventStub);
            });
            Ok(())
        },
    )?;

    m.fn_(
        "emit",
        "Publish `payload` for the event named `event`. Custom events are declared automatically on first emit.",
        &["event", "payload"],
        |lua, (event, payload): (LuaEventName, mlua::Value)| -> LuaResult<()> {
            let key = lua.create_registry_value(payload)?;
            crate::host::try_with_core(|core| {
                core.cells
                    .declare_if_missing(event.0.clone(), crate::cells::EventStub);
                core.cells
                    .set_dyn(&event.0, Rc::new(crate::cells::LuaCellValue { key }));
            });
            Ok(())
        },
    )?;

    m.fn_(
        "on",
        "Register `handler(payload)` for the event named `event`. Custom events are declared automatically before subscribing. Returns a `Reg` whose `:remove()` unsubscribes.",
        &["event", "handler"],
        |lua, (event, handler): (LuaEventName, LuaCallback<mlua::Value, ()>)| -> LuaResult<LuaReg> {
            let handler = handler.into_inner();
            let wrapper = lua.create_function(move |_, args: mlua::MultiValue| {
                let payload = args.into_iter().next().unwrap_or(mlua::Value::Nil);
                handler.call::<()>(payload)
            })?;
            let handle = LuaHandle::from_func(lua, wrapper)?;
            let sub_id = crate::host::try_with_core(|core| {
                core.cells
                    .declare_if_missing(event.0.clone(), crate::cells::EventStub);
                core.cells
                    .subscribe_kind(&event.0, SubscriberKind::Lua(Rc::new(handle)))
            })
            .flatten();
            let Some(sub_id) = sub_id else {
                return Ok(LuaReg::new(|| false));
            };
            let name_for_reg = event.0;
            Ok(LuaReg::new(move || {
                crate::host::try_with_core(|core| core.cells.unsubscribe(&name_for_reg, sub_id))
                    .unwrap_or(false)
            }))
        },
    )?;

    Ok(())
}
