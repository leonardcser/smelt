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
use lua_doc_derive::{LuaAlias, LuaOpts};
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

/// Options for `smelt.paint.register`.
#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.paint.Opts")]
pub struct LuaPaintOpts {
    /// Keep the paint id across `/reload` and swap in the new callback.
    pub name: Option<String>,
    /// Repaint when the mouse moves and expose `ctx.pointer`.
    pub pointer: Option<bool>,
}

/// `map_pointer(x, y, width, height) -> x?, y?` for `smelt.paint.FrameOpts`.
pub type LuaPointerMap = LuaCallback<(u16, u16, u16, u16), (Option<i64>, Option<i64>)>;

/// Options for `smelt.paint.frame`.
#[derive(Debug, Default, LuaOpts)]
#[lua(name = "smelt.paint.FrameOpts")]
pub struct LuaFrameOpts {
    /// Keep the paint id across `/reload` and swap in the new callback.
    pub name: Option<String>,
    /// Repaint when the mouse moves and expose `ctx.pointer`.
    pub pointer: Option<bool>,
    /// Frame paints run from the lowest `order` up, so a higher order draws
    /// over the effects below it. Ties run in registration order. Default 0.
    pub order: Option<i32>,
    /// Expose `ctx.prev`, the screen as smelt drew it on the previous frame
    /// before any frame paint, `ctx.damage`, the cells that changed since,
    /// and `ctx.scrolls`, the windows whose text scrolled.
    pub history: Option<bool>,
    /// For an effect that moves cells, `map_pointer(x, y, width, height)`
    /// returns the cell smelt laid out at screen cell `(x, y)` of a
    /// `width` x `height` screen, so clicks, drags, and wheel scrolls land
    /// where the user sees them. Return `nil` to keep the position. The top
    /// effect maps first. `ctx.pointer` stays in screen cells.
    pub map_pointer: Option<LuaPointerMap>,
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

#[derive(Clone, Copy, Debug)]
pub struct LuaEffectReg {
    id: crate::smelt_edit::layout::PaintId,
}

impl LuaType for LuaEffectReg {
    fn lua_type() -> String {
        "smelt.paint.Effect".into()
    }
}

fn lifecycle_methods<T: 'static, M: mlua::UserDataMethods<T>>(
    methods: &mut M,
    id: fn(&T) -> crate::smelt_edit::layout::PaintId,
) {
    methods.add_method("remove", move |_, this, ()| {
        Ok(crate::lua::with_ui_host(|host| host.remove_paint(id(this))))
    });
    methods.add_method("invalidate", move |_, this, ()| {
        Ok(crate::lua::with_ui_host(|host| {
            host.invalidate_paint(id(this), None)
        }))
    });
    methods.add_method("invalidate_after", move |_, this, ms: u64| {
        Ok(crate::lua::with_ui_host(|host| {
            host.invalidate_paint(id(this), Some(std::time::Duration::from_millis(ms)))
        }))
    });
}

impl mlua::UserData for LuaEffectReg {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        lifecycle_methods(methods, |this| this.id);
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Effect#{}", this.id.0))
        });
    }
}

impl mlua::UserData for LuaPaintReg {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Paint#{}", this.id.0))
        });

        lifecycle_methods(methods, |this| this.id);

        methods.add_method("rect", |lua, this, ()| -> LuaResult<mlua::Value> {
            let rect =
                crate::lua::try_with_ui_host(|host| host.with_ui(|ui| ui.paint_rect(this.id)))
                    .flatten();
            match rect {
                Some(r) => Ok(mlua::Value::Table(crate::lua::parse::rect_to_lua(lua, r)?)),
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
            "invalidate" => fn() -> bool, "Repaint on the next frame, for example after a timer or command changes what the callback draws. Returns `false` once the paint is removed.",
            "invalidate_after" => fn(ms: u64) -> bool, "Repaint `ms` milliseconds after the current frame. Nothing repaints on its own, so an animated paint asks for its next frame here on every paint, and goes idle when it stops asking. The earliest pending request wins. Returns `false` once the paint is removed.",
            "on" => fn(event: LuaPaintEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe `func` to `event` on this paint leaf. Returns a Reg handle whose `:remove()` undoes the subscription.",
        },
    });

    record_class(LuaClassDecl {
        name: "smelt.paint.Effect",
        classification: smelt_core::lua::doc::classification_for_type("smelt.paint.Effect"),
        doc: "Screen-wide effect returned by smelt.paint.frame. Not a layout leaf and has no leaf events or rectangle.",
        fields: smelt_core::class_methods! {
            "remove" => fn() -> bool, "Remove the effect. Returns false if already removed.",
            "invalidate" => fn() -> bool, "Request a frame. Returns false after removal.",
            "invalidate_after" => fn(ms: u64) -> bool, "Request another frame after ms milliseconds. The earliest pending request wins; stops when the effect no longer asks.",
        },
    });

    let s = shared.clone();
    m.fn_(
        "register",
        "Register `func(slice, ctx)` as a paint leaf and return its `Paint` handle. Use the handle anywhere a window id is accepted in the layout / overlay APIs; the callback runs each frame the leaf is visible, in its layer order, so the slice already holds what was painted below it. See `smelt.paint.Ctx` for what `ctx` holds.",
        &["func", "opts"],
        move |lua, (func, opts): (LuaCallback<(LuaPaintSlice, crate::lua::paint::PaintCtx), ()>, Option<LuaPaintOpts>)| -> LuaResult<LuaPaintReg> {
            let opts = opts.unwrap_or_default();
            let options = crate::lua::paint::PaintOptions {
                stage: crate::lua::paint::PaintStage::Leaf,
                pointer: opts.pointer.unwrap_or(false),
            };
            register_paint(lua, &s, func, opts.name, options)
        },
    )?;
    let s = shared.clone();
    m.fn_(
        "frame",
        "Register `func(slice, ctx)` over the finished screen and return an `Effect` handle. It can read, recolor or move cells before the frame is sent. `ctx.cursor` follows the caret; opt-in `history` exposes `prev`, `damage` and `scrolls`. An Effect is not a layout leaf. Remove it to stop.",
        &["func", "opts"],
        move |lua, (func, opts): (LuaCallback<(LuaPaintSlice, crate::lua::paint::PaintCtx), ()>, Option<LuaFrameOpts>)| -> LuaResult<LuaEffectReg> {
            let opts = opts.unwrap_or_default();
            let map_pointer = opts
                .map_pointer
                .map(|map| crate::lua::register_callback_handle(&s, lua, map.into_inner()))
                .transpose()?;
            let options = crate::lua::paint::PaintOptions {
                stage: crate::lua::paint::PaintStage::Frame {
                    order: opts.order.unwrap_or(0),
                    history: opts.history.unwrap_or(false),
                    map_pointer,
                },
                pointer: opts.pointer.unwrap_or(false),
            };
            register_paint(lua, &s, func, opts.name, options).map(|paint| LuaEffectReg { id: paint.id })
        },
    )?;

    Ok(())
}

fn register_paint(
    lua: &Lua,
    shared: &Arc<LuaShared>,
    func: LuaCallback<(LuaPaintSlice, crate::lua::paint::PaintCtx), ()>,
    name: Option<String>,
    options: crate::lua::paint::PaintOptions,
) -> LuaResult<LuaPaintReg> {
    let name = smelt_core::lua::reg::RegistrationScope::name(
        name.or_else(|| crate::lua::auto_name_for_scope(lua, "paint")),
    );
    let handle_id = crate::lua::register_callback_handle(shared, lua, func.into_inner())?;
    let paint_id = crate::lua::with_ui_host(|host| host.register_paint(handle_id, name, options));
    smelt_core::lua::reg::RegistrationScope::resource("paint", paint_id.0, move || {
        crate::lua::try_with_ui_host(|host| host.remove_paint(paint_id)).unwrap_or(false)
    });
    Ok(LuaPaintReg { id: paint_id })
}
