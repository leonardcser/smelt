//! Lua paint registry - maps `PaintId`s to Lua callbacks invoked by the renderer.
//!
//! The slice borrow is lent through scoped TLS only while the paint callback is on the
//! stack. Methods called outside a paint callback return a clean Lua error.

use crate::smelt_edit::layout::PaintId;
use crate::smelt_edit::{DrawContext, Grid, GridSlice, Rect};
use mlua::prelude::*;
use scoped_tls_hkt::scoped_thread_local;
use smelt_core::lua::doc::record_class;
use smelt_core::lua::lua_type::LuaClassDecl;
use smelt_edit::NamedSlots;
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
    Window(crate::smelt_edit::WinId),
    Paint(PaintId),
}

/// Maps `PaintId` → callback handle id (a `u64` into `LuaShared::callbacks`).
///
/// Named slots opt the `PaintId` into hot-reload survival: re-registering
/// with the same name reuses the existing id and atomically swaps in a
/// new handle. Anonymous slots are reaped on `/reload`.
pub(crate) struct PaintRegistry {
    handles: HashMap<PaintId, u64>,
    names: NamedSlots<PaintId>,
    next_id: AtomicU64,
}

impl Default for PaintRegistry {
    fn default() -> Self {
        Self {
            handles: HashMap::new(),
            names: NamedSlots::new(),
            next_id: AtomicU64::new(PAINT_ID_BASE),
        }
    }
}

impl PaintRegistry {
    /// Preserve stable names and ids for a candidate generation without
    /// carrying callback handles owned by the committed Lua runtime.
    pub(crate) fn fork_for_lua_generation(&self) -> Self {
        Self {
            handles: HashMap::new(),
            names: self.names.clone(),
            next_id: AtomicU64::new(self.next_id.load(Ordering::Relaxed)),
        }
    }

    pub(crate) fn finish_lua_generation(&mut self) {
        let stale: Vec<_> = self
            .names
            .bindings()
            .into_iter()
            .filter_map(|(_, id)| (!self.handles.contains_key(&id)).then_some(id))
            .collect();
        for id in stale {
            self.names.unbind_by_id(id);
        }
    }

    /// Register `handle_id` as a paint callback and return its stable
    /// `PaintId`. When `name` is `Some`, the slot survives `/reload`:
    /// a subsequent `register(.., Some(same_name))` reuses the existing
    /// `PaintId` and atomically swaps in the new handle, returning the
    /// previous handle so the caller can release the old callback.
    /// Anonymous (`None`) slots always allocate a fresh id and never
    /// return a previous handle.
    pub(crate) fn register(
        &mut self,
        handle_id: u64,
        name: Option<String>,
    ) -> (PaintId, Option<u64>) {
        if let Some(ref n) = name {
            if let Some(existing) = self.names.lookup(n) {
                let prev = self.handles.insert(existing, handle_id);
                return (existing, prev);
            }
        }
        let id = PaintId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(
            id.0 >= PAINT_ID_BASE,
            "PaintRegistry exhausted paint half of u64 namespace"
        );
        self.handles.insert(id, handle_id);
        if let Some(n) = name {
            self.names.bind(n, id);
        }
        (id, None)
    }

    /// Remove the binding and return the handle id so the caller can release the Lua callback.
    pub(crate) fn unregister(&mut self, id: PaintId) -> Option<u64> {
        self.names.unbind_by_id(id);
        self.handles.remove(&id)
    }

    pub(crate) fn lookup(&self, id: PaintId) -> Option<u64> {
        self.handles.get(&id).copied()
    }

    pub(crate) fn contains(&self, id: PaintId) -> bool {
        self.handles.contains_key(&id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Count of named paint slots. Anonymous slots have no entry in
    /// `names`, so this excludes them - exactly what reload-survival
    /// post-checks want. Harness-only: production code reads names
    /// directly via `id_by_name`.
    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn named_count(&self) -> usize {
        self.names.names().count()
    }

    #[cfg(test)]
    pub(crate) fn id_by_name(&self, name: &str) -> Option<PaintId> {
        self.names.lookup(name)
    }

    #[cfg(test)]
    pub(crate) fn all_ids(&self) -> Vec<PaintId> {
        self.handles.keys().copied().collect()
    }

    /// Drop every anonymous paint-to-handle mapping while preserving named
    /// slot ids. The registry unit test uses this to verify the ownership rule;
    /// production candidate generations use [`Self::fork_for_lua_generation`].
    #[cfg(test)]
    pub(crate) fn clear_anonymous(&mut self) -> Vec<u64> {
        let mut released = Vec::new();
        let names = &self.names;
        self.handles.retain(|id, handle| {
            if names.contains_id(*id) {
                true
            } else {
                released.push(*handle);
                false
            }
        });
        released
    }
}

trait PaintSurface {
    fn width(&self) -> u16;
    fn height(&self) -> u16;
    fn set(&mut self, col: u16, row: u16, symbol: char, style: crate::smelt_edit::Style);
    fn put_str(&mut self, col: u16, row: u16, text: &str, style: crate::smelt_edit::Style);
    fn fill(
        &mut self,
        rect: crate::smelt_edit::layout::Rect,
        symbol: char,
        style: crate::smelt_edit::Style,
    );
}

impl PaintSurface for GridSlice<'_> {
    fn width(&self) -> u16 {
        GridSlice::width(self)
    }

    fn height(&self) -> u16 {
        GridSlice::height(self)
    }

    fn set(&mut self, col: u16, row: u16, symbol: char, style: crate::smelt_edit::Style) {
        GridSlice::set(self, col, row, symbol, style);
    }

    fn put_str(&mut self, col: u16, row: u16, text: &str, style: crate::smelt_edit::Style) {
        GridSlice::put_str(self, col, row, text, style);
    }

    fn fill(
        &mut self,
        rect: crate::smelt_edit::layout::Rect,
        symbol: char,
        style: crate::smelt_edit::Style,
    ) {
        GridSlice::fill(self, rect, symbol, style);
    }
}

/// Off-screen output from one Lua paint callback. Untouched cells remain
/// transparent when the layer is composited at its layout position.
pub(crate) struct PaintLayer {
    grid: Grid,
    touched: Vec<bool>,
}

impl PaintLayer {
    fn new(width: u16, height: u16) -> Self {
        Self {
            grid: Grid::new(width, height),
            touched: vec![false; width as usize * height as usize],
        }
    }

    fn mark_rect(&mut self, rect: Rect) {
        let right = rect.right().min(self.grid.width());
        let bottom = rect.bottom().min(self.grid.height());
        for row in rect.top.min(self.grid.height())..bottom {
            for col in rect.left.min(self.grid.width())..right {
                self.touched[row as usize * self.grid.width() as usize + col as usize] = true;
            }
        }
    }

    pub(crate) fn composite_into(&self, slice: &mut GridSlice<'_>) {
        let width = self.grid.width().min(slice.width());
        let height = self.grid.height().min(slice.height());
        for row in 0..height {
            for col in 0..width {
                if !self.touched[row as usize * self.grid.width() as usize + col as usize] {
                    continue;
                }
                let cell = self.grid.cell(col, row);
                if cell.symbol != '\0' {
                    slice.set(col, row, cell.symbol, cell.style);
                }
            }
        }
    }
}

impl PaintSurface for PaintLayer {
    fn width(&self) -> u16 {
        self.grid.width()
    }

    fn height(&self) -> u16 {
        self.grid.height()
    }

    fn set(&mut self, col: u16, row: u16, symbol: char, style: crate::smelt_edit::Style) {
        if col >= self.grid.width() || row >= self.grid.height() {
            return;
        }
        self.grid.set(col, row, symbol, style);
        self.mark_rect(Rect::new(row, col, 1, 1));
        if col + 1 < self.grid.width() && self.grid.cell(col + 1, row).symbol == '\0' {
            self.mark_rect(Rect::new(row, col + 1, 1, 1));
        }
    }

    fn put_str(&mut self, col: u16, row: u16, text: &str, style: crate::smelt_edit::Style) {
        let end = self.grid.put_str(col, row, text, style);
        self.mark_rect(Rect::new(row, col, end.saturating_sub(col), 1));
    }

    fn fill(&mut self, rect: Rect, symbol: char, style: crate::smelt_edit::Style) {
        self.grid.fill(rect, symbol, style);
        self.mark_rect(rect);
    }
}

scoped_thread_local!(static mut CURRENT_SLICE: for<'a> &'a mut dyn PaintSurface);

fn scope_slice<R>(slice: &mut dyn PaintSurface, body: impl FnOnce() -> R) -> R {
    CURRENT_SLICE.set(slice, body)
}

/// Run `f` against the active paint slice, or return a Lua error if not in a paint callback.
fn with_slice<R>(f: impl FnOnce(&mut dyn PaintSurface) -> R) -> LuaResult<R> {
    if !CURRENT_SLICE.is_set() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: slice not in scope (call from a paint callback)".into(),
        ));
    }
    Ok(CURRENT_SLICE.with(f))
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
                    None => crate::smelt_edit::Style::new(),
                };
                with_slice(|s| s.set(col, row, symbol, resolved_style))
            },
        );
        methods.add_method(
            "put_str",
            |_, _, (row, col, text, style): (u16, u16, String, Option<mlua::Table>)| {
                let resolved_style = match style {
                    Some(t) => crate::lua::parse::style(&t).map_err(LuaError::RuntimeError)?,
                    None => crate::smelt_edit::Style::new(),
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
                    None => crate::smelt_edit::Style::new(),
                };
                let rect = crate::smelt_edit::layout::Rect::new(row, col, w, h);
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

/// Render one Lua paint callback into a transparent off-screen layer. Errors are
/// recorded rather than propagated - a broken painter leaves the layer empty.
pub(crate) fn render_paint(
    runtime: &super::LuaExecution,
    handle_id: u64,
    width: u16,
    height: u16,
    ctx: &DrawContext,
) -> PaintLayer {
    let mut layer = PaintLayer::new(width, height);
    invoke_paint(runtime, handle_id, &mut layer, ctx);
    layer
}

fn invoke_paint(
    runtime: &super::LuaExecution,
    handle_id: u64,
    surface: &mut dyn PaintSurface,
    ctx: &DrawContext,
) {
    let lua = &runtime.lua;
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
    scope_slice(surface, || {
        if let Err(e) = func.call::<()>((ud, ctx_tbl)) {
            runtime.record_error(format!("smelt.paint: {e}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anon(reg: &mut PaintRegistry, handle: u64) -> PaintId {
        reg.register(handle, None).0
    }

    #[test]
    fn allocator_starts_above_winid_range() {
        let mut reg = PaintRegistry::default();
        let id = anon(&mut reg, 7);
        assert!(id.0 >= PAINT_ID_BASE);
    }

    #[test]
    fn register_unregister_roundtrips_handle_id() {
        let mut reg = PaintRegistry::default();
        let id = anon(&mut reg, 42);
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
        let a = anon(&mut reg, 1);
        let b = anon(&mut reg, 2);
        let c = anon(&mut reg, 3);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn with_slice_errors_outside_paint() {
        let r: LuaResult<()> = with_slice(|_| ());
        assert!(r.is_err());
    }

    #[test]
    fn paint_layer_preserves_untouched_cells_and_applies_explicit_blanks() {
        let mut layer = PaintLayer::new(3, 1);
        PaintSurface::set(&mut layer, 1, 0, ' ', crate::smelt_edit::Style::default());
        let mut grid = Grid::new(3, 1);
        grid.fill(
            Rect::new(0, 0, 3, 1),
            'X',
            crate::smelt_edit::Style::default(),
        );

        layer.composite_into(&mut grid.slice_mut(Rect::new(0, 0, 3, 1)));

        assert_eq!(grid.cell(0, 0).symbol, 'X');
        assert_eq!(grid.cell(1, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).symbol, 'X');
    }

    #[test]
    fn paint_layer_composites_wide_glyphs_with_valid_continuations() {
        let mut layer = PaintLayer::new(4, 1);
        PaintSurface::put_str(&mut layer, 1, 0, "語", crate::smelt_edit::Style::default());
        let mut grid = Grid::new(4, 1);

        layer.composite_into(&mut grid.slice_mut(Rect::new(0, 0, 4, 1)));

        assert_eq!(grid.cell(1, 0).symbol, '語');
        assert_eq!(grid.cell(2, 0).symbol, '\0');
    }

    #[test]
    fn scoped_slice_rejects_aliases_and_restores_nested_scope() {
        let mut outer_grid = crate::smelt_edit::Grid::new(3, 1);
        let mut outer = outer_grid.slice_mut(crate::smelt_edit::Rect::new(0, 0, 3, 1));
        let mut inner_grid = crate::smelt_edit::Grid::new(2, 1);
        let mut inner = inner_grid.slice_mut(crate::smelt_edit::Rect::new(0, 0, 2, 1));

        scope_slice(&mut outer, || {
            assert_eq!(with_slice(|slice| slice.width()).unwrap(), 3);
            with_slice(|slice| {
                assert!(with_slice(|_| ()).is_err());
                slice.set(0, 0, 'a', crate::smelt_edit::Style::new());
            })
            .unwrap();

            scope_slice(&mut inner, || {
                assert_eq!(with_slice(|slice| slice.width()).unwrap(), 2);
            });
            assert_eq!(with_slice(|slice| slice.width()).unwrap(), 3);
        });

        assert!(with_slice(|_| ()).is_err());
    }

    #[test]
    fn register_named_reuses_id_and_returns_old_handle() {
        let mut reg = PaintRegistry::default();
        let (id1, old1) = reg.register(11, Some("plugin.banner".into()));
        assert!(old1.is_none());
        assert_eq!(reg.lookup(id1), Some(11));
        let (id2, old2) = reg.register(22, Some("plugin.banner".into()));
        assert_eq!(
            id1, id2,
            "named slot must keep stable id across re-register"
        );
        assert_eq!(
            old2,
            Some(11),
            "previous handle must be returned for release"
        );
        assert_eq!(reg.lookup(id2), Some(22));
    }

    #[test]
    fn clear_anonymous_keeps_named_drops_anonymous() {
        let mut reg = PaintRegistry::default();
        let anon_id = anon(&mut reg, 100);
        let (named, _) = reg.register(200, Some("plugin.x".into()));
        let released = reg.clear_anonymous();
        assert_eq!(released, vec![100], "anonymous handle ids must be released");
        assert!(!reg.contains(anon_id), "anonymous slot must be dropped");
        assert!(reg.contains(named), "named slot must survive");
        assert_eq!(reg.lookup(named), Some(200));
    }

    #[test]
    fn unregister_named_removes_name_binding() {
        let mut reg = PaintRegistry::default();
        let (id, _) = reg.register(7, Some("plugin.x".into()));
        assert_eq!(reg.unregister(id), Some(7));
        let (id2, old) = reg.register(8, Some("plugin.x".into()));
        assert!(old.is_none(), "name binding must be cleared on unregister");
        assert_ne!(
            id, id2,
            "re-registering after unregister allocates fresh id"
        );
    }
}
