//! `smelt.paint` bindings — register Lua callbacks against custom
//! paint regions.
//!
//! `smelt.paint.register(func, opts?)` returns a `Paint` userdata with
//! `:id()` and `:remove()`. The handle is usable directly anywhere a
//! window id is accepted in the layout / overlay APIs (`overlay item.win`,
//! `smelt.overlay.layout.leaf`). Per frame the leaf is visible, the
//! renderer fires the registered callback with a slice userdata +
//! context table; the callback writes cells via slice methods (`set` /
//! `put_str` / `fill_rect`).
//!
//! Lifetime / safety: see `crate::lua::paint` — the slice is exposed
//! via TLS-stashed pointer for the duration of one paint call, and
//! out-of-scope method calls fail cleanly with a Lua runtime error.

use crate::lua::LuaShared;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
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

/// Handle returned by `smelt.paint.register`. Carries the `PaintId`
/// directly so it can stand in for a Win userdata in layout leaves,
/// and exposes `:id()` / `:remove()`.
#[derive(Clone, Copy, Debug)]
pub struct LuaPaintReg {
    pub(crate) id: crate::smelt_term::layout::PaintId,
}

impl LuaType for LuaPaintReg {
    fn lua_type() -> String {
        "smelt.paint.Paint".into()
    }
}

impl mlua::UserData for LuaPaintReg {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Paint#{}", this.id.0))
        });

        methods.add_method("id", |_, this, ()| -> LuaResult<u64> { Ok(this.id.0) });

        methods.add_method("remove", |_, this, ()| -> LuaResult<bool> {
            let removed = crate::lua::with_app(|app| {
                if let Some(handle_id) = app.paint_registry.unregister(this.id) {
                    app.lua.remove_callback(handle_id);
                    true
                } else {
                    false
                }
            });
            Ok(removed)
        });
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

    record_class(LuaClassDecl {
        name: "smelt.paint.Paint",
        doc: "Handle returned by `smelt.paint.register`. Usable directly in `smelt.overlay.layout.leaf(handle, opts)` (it stands in for a Win or raw paint id).",
        fields: smelt_core::class_methods! {
            "id" => fn() -> u64, "Return the underlying paint id (a u64 usable anywhere a Win id / paint id is accepted).",
            "remove" => fn() -> bool, "Drop the paint callback. Returns `true` if it was still registered. Subsequent paints of this id no-op.",
        },
    });

    {
        let s = shared.clone();
        m.fn_(
            "register",
            "Register `func` as a paint callback and return a `Paint` handle (userdata with `:id()` / `:remove()`). The handle is accepted anywhere a window id is in the layout / overlay APIs. The callback fires per frame the leaf is visible with a slice + context table. `opts.name` opts the slot into hot-reload survival: re-registering with the same name keeps the paint id stable and atomically swaps the callback, so surviving overlays/layouts referencing the handle keep painting with the new code.",
            &["func", "opts"],
            move |lua, (func, opts): (LuaCallback<(LuaPaintSlice, mlua::Table), ()>, Option<mlua::Table>)| -> LuaResult<LuaPaintReg> {
                let name: Option<String> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("name").ok().flatten())
                    .or_else(|| crate::lua::auto_name_for_scope(lua, "paint"));
                let handle_id =
                    crate::lua::register_callback_handle(&s, lua, func.into_inner())?;
                let paint_id = crate::lua::with_app(|app| {
                    let (id, prev) = app.paint_registry.register(handle_id, name);
                    if let Some(p) = prev {
                        app.lua.remove_callback(p);
                    }
                    id
                });
                Ok(LuaPaintReg { id: paint_id })
            },
        )?;
    }

    Ok(())
}
