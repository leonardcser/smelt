//! `smelt.au` — nvim-shaped aliases over `smelt.cell` subscribe/set.
//!
//! `au.on(name, handler)` subscribes `handler` to a named cell;
//! `au.fire(name, payload)` publishes a payload that every subscriber
//! sees. Both round-trip through `Cells`, so the same registry powers
//! `smelt.cell` and `smelt.au` — they're surface aliases, not
//! parallel mechanisms.

use crate::lua::api::cell::LuaCellName;
use crate::lua::doc::register_fn;
use crate::lua::lua_type::LuaCallback;
use crate::lua::LuaHandle;
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module(
    name = "smelt.au",
    doc = "Nvim-shaped surface aliases for [`smelt.cell`](cell.md). \
`au.on(name, handler)` is `smelt.cell.subscribe`; \
`au.fire(name, payload)` is `smelt.cell.set`. Both share the same \
underlying registry — pick whichever name fits your plugin."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    use crate::cells::{LuaCellValue, SubscriberKind};
    use std::rc::Rc;

    let au_tbl = lua.create_table()?;
    register_fn(
        &au_tbl,
        "smelt.au",
        "on",
        "Alias of [`smelt.cell.subscribe`](cell.md#smeltcellsubscribe).",
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
        &au_tbl,
        "smelt.au",
        "fire",
        "Alias of [`smelt.cell.set`](cell.md#smeltcellset).",
        &["name", "payload"],
        lua,
        |lua, (name, payload): (LuaCellName, mlua::Value)| -> LuaResult<bool> {
            let key = lua.create_registry_value(payload)?;
            Ok(crate::host::try_with_core(|core| {
                core.cells.set_dyn(&name, Rc::new(LuaCellValue { key }))
            })
            .unwrap_or(false))
        },
    )?;

    smelt.set("au", au_tbl)?;
    Ok(())
}
