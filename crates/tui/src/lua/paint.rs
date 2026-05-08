//! Lua paint registry — Lua-side custom paint regions.
//!
//! Phase A of P12. Registers Lua callbacks against `PaintId`s allocated
//! above the WinId range, so the same `Ui::render_with_paints`
//! dispatcher that routes WinIds to `Window::render` can route paint
//! ids to a Lua function. The Lua callback receives a slice userdata
//! and a context table; cell writes go through TLS-stashed access to
//! the live `GridSlice` for the duration of one paint call.
//!
//! Lifetime model: the slice borrow is alive only while the
//! dispatcher's paint closure is on the stack. We install the slice
//! pointer in TLS via [`SliceGuard`] just before invoking the Lua
//! function and clear it on drop (panic-safe). Slice methods read TLS
//! at call time; calling them outside a paint callback errors cleanly
//! with `"smelt.paint: slice not in scope"` rather than dereferencing
//! a dangling pointer.

use crate::smelt_term::layout::PaintId;
use crate::smelt_term::{DrawContext, GridSlice};
use mlua::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Paint ids allocated by the Lua registry start above this base so a
/// single dispatcher table can route both WinIds (low) and paint ids
/// (high) without collision.
///
/// **Namespace partition.** WinIds (`smelt-edit`) and PaintIds (this
/// registry) share the underlying `u64` space at the layout layer
/// (`LayoutTree::Leaf` is opaque over the id type). Splitting the
/// space at `PAINT_ID_BASE = 1<<32` lets a single dispatcher
/// (`Ui::render_with_paints`) and a single Lua-side `win` field on
/// overlay items disambiguate which subsystem should resolve a
/// given id. The contract:
///
/// - `id <  PAINT_ID_BASE` ⇒ WinId, owned by `Ui` (`smelt-edit`)
/// - `id >= PAINT_ID_BASE` ⇒ PaintId, owned by [`PaintRegistry`]
///
/// The WinId allocator in `smelt-edit` increments from 0 monotonically
/// per session and would need 4 billion windows to collide — well
/// beyond any plausible workload. [`PaintRegistry::register`] preserves
/// the lower bound by initialising `next_id` to `PAINT_ID_BASE`.
/// [`super::resolve_leaf_id`] / [`crate::app::TuiApp::resolve_leaf_id`]
/// rely on this partition.
pub(crate) const PAINT_ID_BASE: u64 = 1u64 << 32;

/// Result of resolving a Lua-supplied leaf id (a raw `u64` from an
/// overlay `items[*].win` field). The two namespaces (WinId / PaintId)
/// share the `u64` space — see [`PAINT_ID_BASE`] for the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeafKind {
    Window(crate::smelt_term::WinId),
    Paint(PaintId),
}

/// Per-host registry mapping `PaintId` → Lua callback handle id. The
/// handle id is the same `u64` produced by
/// [`super::register_callback_handle`] — the actual `mlua::Function`
/// lives in `LuaShared::callbacks` keyed by that id, so we don't clone
/// the registry key.
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
    /// Reserve a fresh paint id and bind it to `handle_id`. Asserts the
    /// freshly-allocated id stays in the paint half of the partition
    /// (>= `PAINT_ID_BASE`); allocator init guarantees this for any
    /// reasonable session length, but the assert pins the contract.
    pub(crate) fn register(&mut self, handle_id: u64) -> PaintId {
        let id = PaintId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(
            id.0 >= PAINT_ID_BASE,
            "PaintRegistry exhausted paint half of u64 namespace"
        );
        self.handles.insert(id, handle_id);
        id
    }

    /// Drop the binding, returning the previously-registered handle id
    /// so the caller can release the underlying Lua callback (via
    /// `LuaRuntime::remove_callback`).
    pub(crate) fn unregister(&mut self, id: PaintId) -> Option<u64> {
        self.handles.remove(&id)
    }

    pub(crate) fn lookup(&self, id: PaintId) -> Option<u64> {
        self.handles.get(&id).copied()
    }

    pub(crate) fn contains(&self, id: PaintId) -> bool {
        self.handles.contains_key(&id)
    }
}

thread_local! {
    /// Active `GridSlice` pointer for the in-flight Lua paint callback,
    /// or null when no paint is in progress. Set by [`SliceGuard::new`],
    /// cleared on guard drop (including panic unwind).
    static CURRENT_SLICE: Cell<*mut GridSlice<'static>> =
        const { Cell::new(std::ptr::null_mut()) };
}

/// RAII guard installed for the duration of one Lua paint callback.
/// Stashes the slice pointer in TLS on creation; clears it on drop so
/// out-of-scope `slice:set` etc. error out instead of touching a stale
/// borrow.
struct SliceGuard;

impl SliceGuard {
    fn new(slice: &mut GridSlice<'_>) -> Self {
        // SAFETY: lifetime-erased to `'static` for storage. The guard's
        // Drop impl runs before `invoke_paint` returns to its caller —
        // who still owns the real `&mut GridSlice<'_>` borrow — so the
        // pointer is valid for the entire window in which `with_slice`
        // can read it back.
        // SAFETY: lifetime-erase via raw pointer. Same allocation, only
        // the lifetime parameter changes. The Drop impl clears TLS
        // before our caller's borrow ends.
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

/// Run `f` against the active paint slice. Returns `Err` (Lua-facing
/// "not in paint" message) when no callback is on the stack, so slice
/// userdata methods can be called from any Lua context and fail
/// cleanly rather than crash.
pub(crate) fn with_slice<R>(f: impl FnOnce(&mut GridSlice<'_>) -> R) -> LuaResult<R> {
    let ptr = CURRENT_SLICE.with(|cell| cell.get());
    if ptr.is_null() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: slice not in scope (call from a paint callback)".into(),
        ));
    }
    // SAFETY: pointer set by the live `SliceGuard` on the dispatcher's
    // stack; the guard hasn't dropped or we wouldn't be inside a Lua
    // callback. Lifetime-restoration to a fresh borrow is sound for
    // the same reason.
    let slice: &mut GridSlice<'_> = unsafe { &mut *ptr };
    Ok(f(slice))
}

/// Empty marker userdata. The methods on this userdata don't carry the
/// slice borrow themselves — they read [`with_slice`] at call time —
/// but the userdata exists so plugins write idiomatic
/// `slice:set(...)` calls instead of free `smelt.paint.set(...)`.
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

/// Build the per-frame `ctx` table handed to the Lua callback.
/// Currently exposes `focused` + `terminal_width` + `terminal_height`.
/// `theme` access from the paint callback isn't wired yet — plugins
/// pull palette colours from `smelt.theme` instead, which already
/// reads the same theme object.
fn build_ctx_table(lua: &Lua, ctx: &DrawContext) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("focused", ctx.focused)?;
    t.set("terminal_width", ctx.terminal_width)?;
    t.set("terminal_height", ctx.terminal_height)?;
    Ok(t)
}

/// Fire the registered Lua paint callback for `handle_id` against the
/// live `slice`. Stashes the slice pointer for the duration of the
/// call so userdata methods can write through it; records errors
/// rather than propagating (a broken painter doesn't crash the
/// renderer — it just fails to paint that leaf for the frame).
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
