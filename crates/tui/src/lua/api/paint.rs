//! `smelt.paint` bindings - register Lua callbacks against custom
//! paint regions.
//!
//! `smelt.paint.register(func, opts?)` returns an opaque `Paint`
//! userdata with `:remove()`. The handle is usable directly anywhere a
//! window id is accepted in the layout / overlay APIs (`overlay item.win`,
//! `smelt.ui.layout.leaf`). Per frame the leaf is visible, the
//! renderer fires the registered callback with a slice userdata +
//! context table; the callback writes cells via slice methods (`set` /
//! `put_str` / `fill_rect`).
//!
//! Lifetime / safety: see `crate::lua::paint` - the slice is exposed
//! via TLS-stashed pointer for the duration of one paint call, and
//! out-of-scope method calls fail cleanly with a Lua runtime error.

use crate::lua::LuaShared;
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use std::sync::Arc;

use super::win::current_shared;

/// Paint-slice placeholder used only to surface the `smelt.paint.Slice`
/// type name in the paint-callback signature. The actual userdata is
/// passed through; this type is never marshalled.
pub struct LuaPaintSlice;

impl LuaType for LuaPaintSlice {
    fn lua_type() -> String {
        "smelt.paint.Slice".into()
    }
}

/// Paint-leaf events accepted by `paint:on(event, fn)`.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.paint.Event")]
pub enum LuaPaintEvent {
    /// Mouse-down landed inside this paint leaf. Payload: `{ row, col, button }`
    /// with leaf-relative cell coordinates and `button` ∈ `"left"|"right"|"middle"`.
    Press,
    /// Mouse-up after a `Press` on this leaf. Fires on the leaf that owned the
    /// press, even if the pointer drifted out (capture). Same payload as `Press`.
    Release,
    /// Mouse drag (motion with button held) while this leaf owns the press.
    /// Same payload as `Press`; coords are leaf-relative for the new position.
    Drag,
}

impl From<LuaPaintEvent> for crate::smelt_edit::WinEvent {
    fn from(e: LuaPaintEvent) -> Self {
        match e {
            LuaPaintEvent::Press => crate::smelt_edit::WinEvent::Press,
            LuaPaintEvent::Release => crate::smelt_edit::WinEvent::Release,
            LuaPaintEvent::Drag => crate::smelt_edit::WinEvent::Drag,
        }
    }
}

/// Handle returned by `smelt.paint.register`. Carries the `PaintId`
/// directly so it can stand in for a Win userdata in layout leaves,
/// and exposes `:remove()`. The `PaintId` is intentionally not exposed
/// to Lua - names are the stable identity surface.
#[derive(Clone, Copy, Debug)]
pub struct LuaPaintReg {
    pub(crate) id: crate::smelt_edit::layout::PaintId,
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

        methods.add_method("remove", |_, this, ()| -> LuaResult<bool> {
            let removed = crate::lua::with_app(|app| {
                for id in app.ui.leaf_clear_callbacks(this.id) {
                    app.lua.remove_callback(id);
                }
                if let Some(handle_id) = app.paint_registry.unregister(this.id) {
                    app.lua.remove_callback(handle_id);
                    true
                } else {
                    false
                }
            });
            Ok(removed)
        });

        methods.add_method("rect", |lua, this, ()| -> LuaResult<mlua::Value> {
            let rect = crate::lua::try_with_app(|app| app.ui.paint_rect(this.id)).flatten();
            match rect {
                Some(r) => {
                    let t = lua.create_table()?;
                    t.set("row", r.top)?;
                    t.set("col", r.left)?;
                    t.set("width", r.width)?;
                    t.set("height", r.height)?;
                    Ok(mlua::Value::Table(t))
                }
                None => Ok(mlua::Value::Nil),
            }
        });

        methods.add_function(
            "on",
            |lua,
             (this_ud, event, func): (
                mlua::AnyUserData,
                LuaPaintEvent,
                LuaCallback<mlua::Table, ()>,
            )|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaPaintReg>()?;
                let shared = current_shared(lua)?;
                let id = crate::lua::register_callback_handle(&shared, lua, func.into_inner())?;
                let event: crate::smelt_edit::WinEvent = event.into();
                crate::lua::with_app(|app| {
                    app.ui.leaf_on_event(
                        this.id,
                        event,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                });
                let leaf = this.id;
                Ok(LuaReg::new(move || {
                    let mut removed = false;
                    crate::lua::with_app(|app| {
                        let prev = app.ui.leaf_clear_event_by_id(leaf, event, id);
                        removed = prev.is_some();
                        crate::lua::drop_displaced_lua_handle(app, prev);
                    });
                    removed
                }))
            },
        );
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
        doc: "Opaque handle returned by `smelt.paint.register`. Usable directly in `smelt.ui.layout.leaf(handle, opts)` (it stands in for a Win in layout leaves).",
        fields: smelt_core::class_methods! {
            "remove" => fn() -> bool, "Drop the paint callback. Returns `true` if it was still registered. Subsequent paints of this id no-op.",
            "rect" => fn() -> mlua::Value, "Return the paint leaf's current screen rect as `{ row, col, width, height }`, or `nil` until the first render lays it out.",
            "on" => fn(event: LuaPaintEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe `func` to `event` on this paint leaf. Returns a Reg handle whose `:remove()` undoes the subscription.",
        },
    });

    {
        let s = shared.clone();
        m.fn_(
            "register",
            "Register `func` as a paint callback and return an opaque `Paint` handle (userdata with `:remove()`). The handle is accepted anywhere a window id is in the layout / overlay APIs. The callback fires per frame the leaf is visible with a slice + context table. `opts.name` opts the slot into hot-reload survival: re-registering with the same name keeps the paint id stable and atomically swaps the callback, so surviving overlays/layouts referencing the handle keep painting with the new code.",
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
                    for stale in app.ui.leaf_clear_callbacks(id) {
                        app.lua.remove_callback(stale);
                    }
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
