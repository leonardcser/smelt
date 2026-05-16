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
    doc = "Alias namespace for [`smelt.cell`](cell.md). \
`au.subscribe(name, handler)` is `smelt.cell.subscribe`; \
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
        "subscribe",
        "Alias of [`smelt.cell.subscribe`](cell.md#smeltcellsubscribe). Returns an `off()` function that removes the subscription.",
        &["name", "handler"],
        lua,
        |lua,
         (name, handler): (LuaCellName, LuaCallback<mlua::Value, ()>)|
         -> LuaResult<Option<mlua::Function>> {
            let key = lua.create_registry_value(handler.into_inner())?;
            let id = crate::host::try_with_core(|core| {
                core.cells
                    .subscribe_kind(&name, SubscriberKind::Lua(Rc::new(LuaHandle { key })))
            })
            .flatten();
            let Some(id) = id else { return Ok(None) };
            let name_owned = name.0.clone();
            let off = lua.create_function(move |_, ()| -> LuaResult<bool> {
                Ok(
                    crate::host::try_with_core(|core| core.cells.unsubscribe(&name_owned, id))
                        .unwrap_or(false),
                )
            })?;
            Ok(Some(off))
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

    register_fn(
        &au_tbl,
        "smelt.au",
        "unsubscribe",
        "Alias of [`smelt.cell.unsubscribe`](cell.md#smeltcellunsubscribe). Drop the subscription with id `id` from `name`. Prefer the `off()` function returned by `subscribe`.",
        &["name", "id"],
        lua,
        |_, (name, id): (LuaCellName, u64)| -> LuaResult<bool> {
            Ok(
                crate::host::try_with_core(|core| core.cells.unsubscribe(&name, id))
                    .unwrap_or(false),
            )
        },
    )?;

    smelt.set("au", au_tbl)?;
    Ok(())
}
