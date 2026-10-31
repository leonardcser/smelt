//! `smelt.paint` bindings - register Lua callbacks against custom
//! paint regions.
//!
//! `smelt.paint.register(func, opts?)` returns an opaque `Paint`
//! userdata usable anywhere a window id is accepted in the layout / overlay
//! APIs (`overlay item.win`, `smelt.ui.layout.leaf`). Per frame the leaf is
//! visible, the renderer calls `func(slice, ctx)` in the leaf's layer order.
//! `smelt.paint.frame(func, opts?)` registers a callback over the whole
//! finished screen instead, for screen-wide effects.
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
    /// Mouse-down landed inside this paint leaf. Payload: `{ x, y, button }`
    /// with leaf-relative cell coordinates and `button` ∈ `"left"|"right"|"middle"`.
    Press,
    /// Mouse-up after a `Press` on this leaf. Fires on the leaf that owned the
    /// press, even if the pointer drifted out (capture). Same payload as `Press`.
    Release,
    /// Mouse drag (motion with button held) while this leaf owns the press.
    /// Same payload as `Press`; coords are leaf-relative for the new position.
    Drag,
    /// The pointer moved onto this leaf with no button held. Payload: `{ x, y }`.
    Enter,
    /// The pointer left this leaf with no button held. Payload: `{ x, y }`
    /// relative to this leaf, so it may lie outside it.
    Leave,
    /// The pointer moved within this leaf with no button held. Payload: `{ x, y }`.
    Move,
}

impl From<LuaPaintEvent> for crate::smelt_edit::WinEvent {
    fn from(e: LuaPaintEvent) -> Self {
        match e {
            LuaPaintEvent::Press => crate::smelt_edit::WinEvent::Press,
            LuaPaintEvent::Release => crate::smelt_edit::WinEvent::Release,
            LuaPaintEvent::Drag => crate::smelt_edit::WinEvent::Drag,
            LuaPaintEvent::Enter => crate::smelt_edit::WinEvent::PointerEnter,
            LuaPaintEvent::Leave => crate::smelt_edit::WinEvent::PointerLeave,
            LuaPaintEvent::Move => crate::smelt_edit::WinEvent::PointerMove,
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
            let removed = crate::lua::with_ui_host(|host| host.remove_paint(this.id));
            Ok(removed)
        });

        methods.add_method("rect", |lua, this, ()| -> LuaResult<mlua::Value> {
            let rect =
                crate::lua::try_with_ui_host(|host| host.with_ui(|ui| ui.paint_rect(this.id)))
                    .flatten();
            match rect {
                Some(r) => {
                    let t = lua.create_table()?;
                    t.set("x", r.x)?;
                    t.set("y", r.y)?;
                    t.set("width", r.width)?;
                    t.set("height", r.height)?;
                    Ok(mlua::Value::Table(t))
                }
                None => Ok(mlua::Value::Nil),
            }
        });

        methods.add_method("set_fps", |_, this, fps: Option<u16>| -> LuaResult<bool> {
            Ok(crate::lua::with_ui_host(|host| {
                host.set_paint_fps(this.id, fps)
            }))
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
                crate::lua::with_ui_host(|host| {
                    host.register_paint_event(
                        this.id,
                        event,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                });
                let leaf = this.id;
                Ok(LuaReg::new(move || {
                    crate::lua::app_ref::defer_registered_lua_operation(
                        &shared,
                        id,
                        crate::lua::app_ref::DeferredLuaOperation::PaintEvent {
                            paint: leaf,
                            event,
                            callback_id: id,
                        },
                    )
                }))
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "paint",
        "Register Lua callbacks against custom paint regions. UiHost-only.",
        Tier::UiHost,
    )?;
    crate::lua::paint::register_paint_slice_docs();

    record_class(LuaClassDecl {
        name: "smelt.paint.Paint",
        classification: smelt_core::lua::doc::classification_for_type("smelt.paint.Paint"),
        doc: "Opaque handle returned by `smelt.paint.register`. Usable directly in `smelt.ui.layout.leaf(handle, opts)` (it stands in for a Win in layout leaves).",
        fields: smelt_core::class_methods! {
            "remove" => fn() -> bool, "Drop the paint callback. Returns `true` if it was still registered. Subsequent paints of this id no-op.",
            "rect" => fn() -> mlua::Value, "Return the paint leaf's current screen rect as `{ x, y, width, height }`, or `nil` until the first render lays it out.",
            "set_fps" => fn(fps: Option<u16>) -> bool, "Repaint at `fps` frames per second while the paint is on screen, or only on change with `nil`. Returns `false` once the paint is removed.",
            "on" => fn(event: LuaPaintEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe `func` to `event` on this paint leaf. Returns a Reg handle whose `:remove()` undoes the subscription.",
        },
    });

    for (name, stage, doc) in [
        (
            "register",
            crate::lua::paint::PaintStage::Leaf,
            "Register `func(slice, ctx)` as a paint leaf and return its `Paint` handle. Use the handle anywhere a window id is accepted in the layout / overlay APIs; the callback runs each frame the leaf is visible, in its layer order, so the slice already holds what was painted below it. `ctx` has `focused`, `terminal_width`, `terminal_height`, `x`, `y` (the slice origin on screen), `now_ms`, `frame`, and `pointer` (`{ x, y }` relative to the slice) when `opts.pointer` is set. Options: `name` keeps the paint id across `/reload` and swaps in the new callback; `fps` repaints at that rate while visible; `pointer` repaints when the mouse moves.",
        ),
        (
            "frame",
            crate::lua::paint::PaintStage::Frame,
            "Register `func(slice, ctx)` over the whole screen and return its `Paint` handle. It runs after everything else has painted and before the frame is sent, in registration order, so it can read, recolor, move, or replace any cell: screen-wide effects such as fades, color filters, or melting text. `ctx` and options match `smelt.paint.register`, and `ctx.cursor` is `{ x, y }` of the visible cursor (absent while it is hidden), for effects that follow it such as a cursor trail. Remove the handle to stop the effect.",
        ),
    ] {
        let s = shared.clone();
        m.fn_(
            name,
            doc,
            &["func", "opts"],
            move |lua, (func, opts): (LuaCallback<(LuaPaintSlice, mlua::Table), ()>, Option<mlua::Table>)| -> LuaResult<LuaPaintReg> {
                let opts = opts.as_ref();
                let name: Option<String> = opts
                    .and_then(|t| t.get::<Option<String>>("name").ok().flatten())
                    .or_else(|| crate::lua::auto_name_for_scope(lua, "paint"));
                let options = crate::lua::paint::PaintOptions {
                    stage,
                    fps: opts.map(|t| t.get::<Option<u16>>("fps")).transpose()?.flatten(),
                    pointer: opts.map(|t| t.get::<Option<bool>>("pointer")).transpose()?.flatten().unwrap_or(false),
                };
                let handle_id =
                    crate::lua::register_callback_handle(&s, lua, func.into_inner())?;
                let paint_id =
                    crate::lua::with_ui_host(|host| host.register_paint(handle_id, name, options));
                Ok(LuaPaintReg { id: paint_id })
            },
        )?;
    }

    Ok(())
}
