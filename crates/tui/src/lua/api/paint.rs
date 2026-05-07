//! `smelt.paint` bindings — register Lua callbacks against custom
//! paint regions.
//!
//! Returns a paint id (u64) usable anywhere a window id is accepted in
//! the layout / overlay APIs (`overlay item.win`). Per frame the leaf
//! is visible, the renderer fires the registered callback with a slice
//! userdata + a context table; the callback writes cells via slice
//! methods (`set` / `put_str` / `fill_rect`).
//!
//! Lifetime / safety: see `crate::lua::paint` — the slice is exposed
//! via TLS-stashed pointer for the duration of one paint call, and
//! out-of-scope method calls fail cleanly with a Lua runtime error.

use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let paint_tbl = lua.create_table()?;

    {
        let s = shared.clone();
        paint_tbl.set(
            "register",
            lua.create_function(move |lua, func: mlua::Function| {
                let handle_id = crate::lua::register_callback_handle(&s, lua, func)?;
                let paint_id = crate::lua::with_app(|app| app.paint_registry.register(handle_id));
                Ok(paint_id.0)
            })?,
        )?;
    }

    paint_tbl.set(
        "unregister",
        lua.create_function(|_, id: u64| {
            crate::lua::with_app(|app| {
                if let Some(handle_id) = app
                    .paint_registry
                    .unregister(crate::smelt_term::layout::PaintId(id))
                {
                    app.lua.remove_callback(handle_id);
                }
            });
            Ok(())
        })?,
    )?;

    smelt.set("paint", paint_tbl)?;
    Ok(())
}
