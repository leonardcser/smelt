//! `smelt.statusline` bindings — register / unregister statusline
//! sources by name. Each source is a Lua callback the renderer invokes
//! when its segment refreshes; `align = "right"` flips the default
//! alignment for the registration.

use crate::lua::{LuaHandle, LuaShared, StatusSource};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let statusline_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        statusline_tbl.set(
            "register",
            lua.create_function(
                move |lua, (name, handler, opts): (String, mlua::Function, Option<mlua::Table>)| {
                    let default_align_right = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<String>>("align").ok().flatten())
                        .map(|s| s == "right")
                        .unwrap_or(false);
                    let key = lua.create_registry_value(handler)?;
                    let source = StatusSource {
                        handle: LuaHandle { key },
                        default_align_right,
                    };
                    if let Ok(mut sources) = s.statusline_sources.lock() {
                        if let Some(existing) = sources.iter_mut().find(|(n, _)| n == &name) {
                            existing.1 = source;
                        } else {
                            sources.push((name, source));
                        }
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let s = shared.clone();
        statusline_tbl.set(
            "unregister",
            lua.create_function(move |_, name: String| {
                if let Ok(mut sources) = s.statusline_sources.lock() {
                    sources.retain(|(n, _)| n != &name);
                }
                Ok(())
            })?,
        )?;
    }
    smelt.set("statusline", statusline_tbl)?;
    Ok(())
}
