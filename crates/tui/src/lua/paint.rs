//! Lua paint registry — maps `PaintId`s to Lua callbacks invoked by the renderer.
//!
//! The slice borrow is live only while the dispatcher's paint closure is on the stack.
//! [`SliceGuard`] installs the pointer in TLS before the Lua call and clears it on drop
//! (panic-safe). Methods called outside a paint callback return a clean Lua error instead
//! of touching a dangling pointer.

use crate::smelt_term::layout::PaintId;
use crate::smelt_term::{DrawContext, GridSlice};
use mlua::prelude::*;
use smelt_core::lua::doc::record_class;
use smelt_core::lua::lua_type::LuaClassDecl;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Paint ids start above this base to partition the shared `u64` space:
/// - `id <  PAINT_ID_BASE` → WinId (owned by `Ui`)
/// - `id >= PAINT_ID_BASE` → PaintId (owned by [`PaintRegistry`])
///
/// WinIds increment from 0; reaching `1<<32` would require 4 billion windows.
/// [`PaintRegistry::register`] initializes `next_id` to this value to uphold the contract.
pub(crate) const PAINT_ID_BASE: u64 = 1u64 << 32;

/// Resolved kind of a raw `u64` leaf id from an overlay `items[*].win` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeafKind {
    Window(crate::smelt_term::WinId),
    Paint(PaintId),
}

/// Maps `PaintId` → callback handle id (a `u64` into `LuaShared::callbacks`).
pub(crate) struct PaintRegistry {
    handles: HashMap<PaintId, u64>,
    next_id: AtomicU64,
}

impl Default for PaintRegistry {
    fn default() -> Self {
        Self {
            handles: HashMap::new(),
            next_id: AtomicU64::new(PAINT_ID_BASE),
        }
    }
}

impl PaintRegistry {
    /// Reserve a fresh paint id and bind it to `handle_id`.
    /// Asserts the id stays in the paint half (`>= PAINT_ID_BASE`).
    pub(crate) fn register(&mut self, handle_id: u64) -> PaintId {
        let id = PaintId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(
            id.0 >= PAINT_ID_BASE,
            "PaintRegistry exhausted paint half of u64 namespace"
        );
        self.handles.insert(id, handle_id);
        id
    }

    /// Remove the binding and return the handle id so the caller can release the Lua callback.
    pub(crate) fn unregister(&mut self, id: PaintId) -> Option<u64> {
        self.handles.remove(&id)
    }

    pub(crate) fn lookup(&self, id: PaintId) -> Option<u64> {
        self.handles.get(&id).copied()
    }

    pub(crate) fn contains(&self, id: PaintId) -> bool {
        self.handles.contains_key(&id)
    }

    /// Drop every paint→handle mapping. Used by `/reload` so extmarks
    /// don't dispatch to handle ids whose Lua function was wiped from
    /// `LuaShared::callbacks`. Plugins re-register painters on re-load.
    pub(crate) fn clear(&mut self) {
        self.handles.clear();
    }
}

thread_local! {
    /// Active `GridSlice` pointer for the in-flight paint callback; null otherwise.
    /// Set by [`SliceGuard::new`], cleared on drop (including panic unwind).
    static CURRENT_SLICE: Cell<*mut GridSlice<'static>> =
        const { Cell::new(std::ptr::null_mut()) };
}

/// RAII guard that stashes the slice pointer in TLS for one paint callback, clearing on drop.
struct SliceGuard;

impl SliceGuard {
    fn new(slice: &mut GridSlice<'_>) -> Self {
        // SAFETY: lifetime-erased to `'static` for TLS storage. The Drop impl clears TLS
        // before `invoke_paint` returns to its caller, who still holds the real borrow.
        // The pointer is valid for the entire window in which `with_slice` can read it.
        #[allow(clippy::unnecessary_cast)]
        let ptr: *mut GridSlice<'static> = slice as *mut GridSlice<'_> as *mut GridSlice<'static>;
        CURRENT_SLICE.with(|cell| cell.set(ptr));
        Self
    }
}

impl Drop for SliceGuard {
    fn drop(&mut self) {
        CURRENT_SLICE.with(|cell| cell.set(std::ptr::null_mut()));
    }
}

/// Run `f` against the active paint slice, or return a Lua error if not in a paint callback.
pub(crate) fn with_slice<R>(f: impl FnOnce(&mut GridSlice<'_>) -> R) -> LuaResult<R> {
    let ptr = CURRENT_SLICE.with(|cell| cell.get());
    if ptr.is_null() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: slice not in scope (call from a paint callback)".into(),
        ));
    }
    // SAFETY: pointer set by the live `SliceGuard`; guard hasn't dropped
    // (we're inside a Lua callback). Lifetime-restoration is sound for the same reason.
    let slice: &mut GridSlice<'_> = unsafe { &mut *ptr };
    Ok(f(slice))
}

/// Marker userdata enabling idiomatic `slice:set(...)` call syntax.
/// Methods delegate to [`with_slice`] at call time rather than carrying the borrow.
pub(crate) struct PaintSliceUd;

impl mlua::UserData for PaintSliceUd {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("width", |_, _, ()| with_slice(|s| s.width() as i64));
        methods.add_method("height", |_, _, ()| with_slice(|s| s.height() as i64));
        methods.add_method(
            "set",
            |_, _, (row, col, ch, style): (u16, u16, String, Option<mlua::Table>)| {
                let symbol = ch.chars().next().unwrap_or(' ');
                let resolved_style = match style {
                    Some(t) => crate::lua::parse::style(&t).map_err(LuaError::RuntimeError)?,
                    None => crate::smelt_term::Style::new(),
                };
                with_slice(|s| s.set(col, row, symbol, resolved_style))
            },
        );
        methods.add_method(
            "put_str",
            |_, _, (row, col, text, style): (u16, u16, String, Option<mlua::Table>)| {
                let resolved_style = match style {
                    Some(t) => crate::lua::parse::style(&t).map_err(LuaError::RuntimeError)?,
                    None => crate::smelt_term::Style::new(),
                };
                with_slice(|s| s.put_str(col, row, &text, resolved_style))
            },
        );
        methods.add_method(
            "fill_rect",
            |_,
             _,
             (row, col, w, h, ch, style): (
                u16,
                u16,
                u16,
                u16,
                Option<String>,
                Option<mlua::Table>,
            )| {
                let symbol = ch.as_deref().and_then(|s| s.chars().next()).unwrap_or(' ');
                let resolved_style = match style {
                    Some(t) => crate::lua::parse::style(&t).map_err(LuaError::RuntimeError)?,
                    None => crate::smelt_term::Style::new(),
                };
                let rect = crate::smelt_term::layout::Rect::new(row, col, w, h);
                with_slice(|s| s.fill(rect, symbol, resolved_style))
            },
        );
    }
}

/// Register the `smelt.paint.Slice` class docs. Keep in sync with the
/// `impl UserData for PaintSliceUd` block above.
pub fn register_paint_slice_docs() {
    record_class(LuaClassDecl {
        name: "smelt.paint.Slice",
        doc: "Grid slice passed to paint callbacks. Methods delegate to the live grid slice for the current frame; out-of-scope calls fail cleanly.",
        fields: smelt_core::class_methods! {
            "width" => fn() -> i64, "Return the slice width in cells.",
            "height" => fn() -> i64, "Return the slice height in cells.",
            "set" => fn(row: u16, col: u16, ch: String, style: Option<mlua::Table>) -> (), "Write a single character with optional style at (row, col).",
            "put_str" => fn(row: u16, col: u16, text: String, style: Option<mlua::Table>) -> (), "Write a string with optional style at (row, col).",
            "fill_rect" => fn(row: u16, col: u16, w: u16, h: u16, ch: Option<String>, style: Option<mlua::Table>) -> (), "Fill a rectangle with an optional character and style.",
        },
    });
}

/// Build the per-frame `ctx` table handed to the Lua paint callback.
fn build_ctx_table(lua: &Lua, ctx: &DrawContext) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("focused", ctx.focused)?;
    t.set("terminal_width", ctx.terminal_width)?;
    t.set("terminal_height", ctx.terminal_height)?;
    Ok(t)
}

/// Fire the Lua paint callback for `handle_id`. Errors are recorded rather than
/// propagated — a broken painter skips the leaf for the frame without crashing the renderer.
pub(crate) fn invoke_paint(
    runtime: &super::LuaRuntime,
    handle_id: u64,
    slice: &mut GridSlice<'_>,
    ctx: &DrawContext,
) {
    let _guard = SliceGuard::new(slice);
    let lua = runtime.lua();
    let func: mlua::Function = {
        let Ok(cbs) = runtime.shared().callbacks.lock() else {
            return;
        };
        let Some(handle) = cbs.get(&handle_id) else {
            return;
        };
        match lua.registry_value(&handle.key) {
            Ok(f) => f,
            Err(_) => return,
        }
    };
    let ud = match lua.create_userdata(PaintSliceUd) {
        Ok(u) => u,
        Err(e) => {
            runtime.record_error(format!("smelt.paint: userdata: {e}"));
            return;
        }
    };
    let ctx_tbl = match build_ctx_table(lua, ctx) {
        Ok(t) => t,
        Err(e) => {
            runtime.record_error(format!("smelt.paint: ctx: {e}"));
            return;
        }
    };
    let _perf = smelt_perf::perf::begin("lua:paint");
    if let Err(e) = func.call::<()>((ud, ctx_tbl)) {
        runtime.record_error(format!("smelt.paint: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_starts_above_winid_range() {
        let mut reg = PaintRegistry::default();
        let id = reg.register(7);
        assert!(id.0 >= PAINT_ID_BASE);
    }

    #[test]
    fn register_unregister_roundtrips_handle_id() {
        let mut reg = PaintRegistry::default();
        let id = reg.register(42);
        assert_eq!(reg.lookup(id), Some(42));
        assert!(reg.contains(id));
        let removed = reg.unregister(id);
        assert_eq!(removed, Some(42));
        assert_eq!(reg.lookup(id), None);
        assert!(!reg.contains(id));
    }

    #[test]
    fn fresh_ids_are_distinct() {
        let mut reg = PaintRegistry::default();
        let a = reg.register(1);
        let b = reg.register(2);
        let c = reg.register(3);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn with_slice_errors_outside_paint() {
        let r: LuaResult<()> = with_slice(|_| ());
        assert!(r.is_err());
    }
}
