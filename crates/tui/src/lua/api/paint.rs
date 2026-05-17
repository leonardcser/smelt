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
use smelt_core::lua::doc::Tier;
use smelt_core::lua::lua_type::{LuaCallback, LuaType};
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// Paint-slice placeholder used only to surface the `smelt.paint.Slice`
/// type name in the paint-callback signature. The actual userdata is
/// passed through; this type is never marshalled.
pub struct LuaPaintSlice;

impl LuaType for LuaPaintSlice {
    fn lua_type() -> String {
        "smelt.paint.Slice".into()
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "paint",
        "Register Lua callbacks against custom paint regions. UiHost-only.",
        Tier::UiHost,
    )?;
    crate::lua::paint::register_paint_slice_docs();

    {
        let s = shared.clone();
        m.fn_(
            "register",
            "Register `func` as a paint callback and return a stable paint id usable anywhere a window id is accepted (overlay item `win`, layout leaves). The callback fires per frame the leaf is visible with a slice + context table.",
            &["func"],
            move |lua, func: LuaCallback<(LuaPaintSlice, mlua::Table), ()>| {
                let handle_id =
                    crate::lua::register_callback_handle(&s, lua, func.into_inner())?;
                let paint_id = crate::lua::with_app(|app| app.paint_registry.register(handle_id));
                Ok(paint_id.0)
            },
        )?;
    }

    m.fn_(
        "unregister",
        "Drop a previously registered paint callback by `id`. The associated Lua handle is freed; subsequent paints of that id no-op.",
        &["id"],
        |_, id: u64| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if let Some(handle_id) = app
                    .paint_registry
                    .unregister(crate::smelt_term::layout::PaintId(id))
                {
                    app.lua.remove_callback(handle_id);
                }
            });
            Ok(())
        },
    )?;

    Ok(())
}
