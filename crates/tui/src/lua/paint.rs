//! Lua paint registry - maps `PaintId`s to Lua callbacks invoked by the renderer.
//!
//! The slice borrow is lent through scoped TLS only while the paint callback is on the
//! stack. Methods called outside a paint callback return a clean Lua error.

use crate::smelt_edit::layout::PaintId;
use crate::smelt_edit::{Cell, DrawContext, GridSlice, Rect, Style};
use mlua::prelude::*;
use scoped_tls_hkt::scoped_thread_local;
use smelt_buffer::cell_width;
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

/// Where a paint callback draws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PaintStage {
    /// A layout leaf: the callback paints its own rect, in layer order.
    #[default]
    Leaf,
    /// The whole screen, after everything else has painted and before flush.
    Frame,
}

/// Per-paint options set at registration and adjustable on the handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaintOptions {
    pub stage: PaintStage,
    /// Repaint at this rate while the paint is on screen. `None` repaints
    /// only when something else changes.
    pub fps: Option<u16>,
    /// Repaint when the pointer moves, and expose `ctx.pointer`.
    pub pointer: bool,
}

impl PaintOptions {
    /// Frame interval this paint asks for while it is on screen.
    pub(crate) fn interval(&self) -> Option<std::time::Duration> {
        self.fps
            .filter(|fps| *fps > 0)
            .map(|fps| std::time::Duration::from_secs(1) / u32::from(fps))
    }
}

/// A registered paint callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaintSlot {
    /// Callback handle id (a `u64` into `LuaShared::callbacks`).
    pub handle: u64,
    pub options: PaintOptions,
}

/// Maps `PaintId` → registered paint callback.
///
/// Named slots opt the `PaintId` into hot-reload survival: re-registering
/// with the same name reuses the existing id and atomically swaps in a
/// new handle. Anonymous slots are reaped on `/reload`.
pub(crate) struct PaintRegistry {
    handles: HashMap<PaintId, PaintSlot>,
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
        options: PaintOptions,
    ) -> (PaintId, Option<u64>) {
        let slot = PaintSlot {
            handle: handle_id,
            options,
        };
        if let Some(ref n) = name {
            if let Some(existing) = self.names.lookup(n) {
                let prev = self.handles.insert(existing, slot);
                return (existing, prev.map(|prev| prev.handle));
            }
        }
        let id = PaintId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(
            id.0 >= PAINT_ID_BASE,
            "PaintRegistry exhausted paint half of u64 namespace"
        );
        self.handles.insert(id, slot);
        if let Some(n) = name {
            self.names.bind(n, id);
        }
        (id, None)
    }

    /// Remove the binding and return the handle id so the caller can release the Lua callback.
    pub(crate) fn unregister(&mut self, id: PaintId) -> Option<u64> {
        self.names.unbind_by_id(id);
        self.handles.remove(&id).map(|slot| slot.handle)
    }

    /// The leaf paint for a layout leaf id. Frame paints never draw as leaves,
    /// even when their handle is placed in a layout.
    pub(crate) fn leaf(&self, id: PaintId) -> Option<PaintSlot> {
        self.handles
            .get(&id)
            .copied()
            .filter(|slot| slot.options.stage == PaintStage::Leaf)
    }

    /// Change a paint's options. Returns `false` for an unknown id.
    pub(crate) fn update(&mut self, id: PaintId, f: impl FnOnce(&mut PaintOptions)) -> bool {
        self.handles
            .get_mut(&id)
            .map(|slot| f(&mut slot.options))
            .is_some()
    }

    /// Frame-stage paints in registration order. Re-registering a named slot
    /// keeps its id, so it keeps its place.
    pub(crate) fn frame_paints(&self) -> Vec<(PaintId, PaintSlot)> {
        let mut paints: Vec<_> = self
            .handles
            .iter()
            .filter(|(_, slot)| slot.options.stage == PaintStage::Frame)
            .map(|(id, slot)| (*id, *slot))
            .collect();
        paints.sort_by_key(|(id, _)| id.0);
        paints
    }

    /// Whether any paint asked to repaint on pointer motion.
    pub(crate) fn tracks_pointer(&self) -> bool {
        self.handles.values().any(|slot| slot.options.pointer)
    }

    pub(crate) fn contains(&self, id: PaintId) -> bool {
        self.handles.contains_key(&id)
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
        self.handles.retain(|id, slot| {
            if names.contains_id(*id) {
                true
            } else {
                released.push(slot.handle);
                false
            }
        });
        released
    }
}

/// The slice a paint callback draws into, lent through scoped TLS for the
/// duration of one call. The trait object erases the slice's lifetime, which
/// a concrete `&mut GridSlice<'_>` cannot do in a thread-local.
trait Canvas {
    fn width(&self) -> u16;
    fn height(&self) -> u16;
    fn cell(&self, x: u16, y: u16) -> Option<&Cell>;
    fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style);
    fn restyle(&mut self, x: u16, y: u16, style: Style);
}

impl Canvas for GridSlice<'_> {
    fn width(&self) -> u16 {
        GridSlice::width(self)
    }

    fn height(&self) -> u16 {
        GridSlice::height(self)
    }

    fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        GridSlice::cell(self, x, y)
    }

    fn set_symbol(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        GridSlice::set_symbol(self, x, y, symbol, style);
    }

    fn restyle(&mut self, x: u16, y: u16, style: Style) {
        if let Some(cell) = self.cell_mut(x, y) {
            cell.style = style;
        }
    }
}

/// Writes keep the background under them when the style leaves `bg` unset,
/// so text painted over existing content blends in.
fn over(canvas: &dyn Canvas, x: u16, y: u16, style: Style) -> Style {
    Style {
        bg: style
            .bg
            .or_else(|| canvas.cell(x, y).and_then(|cell| cell.style.bg)),
        ..style
    }
}

/// Write `text` from `(x, y)`, clipping at the right edge. Returns the end column.
fn put_str(canvas: &mut dyn Canvas, x: u16, y: u16, text: &str, style: Style) -> u16 {
    if y >= canvas.height() {
        return x.min(canvas.width());
    }
    let mut col = x;
    for grapheme in cell_width::graphemes(text) {
        let width = cell_width::text_width_u16(grapheme);
        if col.saturating_add(width) > canvas.width() {
            break;
        }
        let style = over(canvas, col, y, style);
        canvas.set_symbol(col, y, grapheme, style);
        col = col.saturating_add(width);
    }
    col.min(canvas.width())
}

fn fill(canvas: &mut dyn Canvas, rect: Rect, symbol: &str, style: Style) {
    let bounds = Rect::new(0, 0, canvas.width(), canvas.height());
    let rect = rect.clip_to(bounds);
    let step = cell_width::text_width_u16(symbol).max(1);
    for y in rect.y..rect.bottom() {
        let mut x = rect.x;
        while x < rect.right() {
            let style = over(canvas, x, y, style);
            canvas.set_symbol(x, y, symbol, style);
            x = x.saturating_add(step);
        }
    }
}

scoped_thread_local!(static mut CURRENT_CANVAS: for<'a> &'a mut dyn Canvas);

fn scope_canvas<R>(canvas: &mut dyn Canvas, body: impl FnOnce() -> R) -> R {
    CURRENT_CANVAS.set(canvas, body)
}

/// Run `f` against the active paint canvas, or return a Lua error if not in a paint callback.
fn with_canvas<R>(f: impl FnOnce(&mut dyn Canvas) -> LuaResult<R>) -> LuaResult<R> {
    if !CURRENT_CANVAS.is_set() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: slice not in scope (call from a paint callback)".into(),
        ));
    }
    CURRENT_CANVAS.with(f)
}

fn one_grapheme_or_space(text: Option<&str>) -> LuaResult<&str> {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return Ok(" ");
    };
    let mut graphemes = cell_width::graphemes(text);
    let symbol = graphemes.next().unwrap_or(" ");
    if graphemes.next().is_some() {
        return Err(LuaError::RuntimeError(
            "smelt.paint: a cell symbol must be one grapheme cluster".into(),
        ));
    }
    Ok(symbol)
}

fn style_arg(style: Option<mlua::Table>) -> LuaResult<Style> {
    match style {
        Some(t) => crate::lua::parse::style(&t).map_err(LuaError::RuntimeError),
        None => Ok(Style::new()),
    }
}

fn cell_to_lua(lua: &Lua, cell: &Cell) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    if cell.symbol.is_continuation() {
        t.set("ch", "")?;
        t.set("width", 0)?;
    } else {
        t.set("ch", cell.symbol.as_str())?;
        t.set("width", cell.symbol.width())?;
    }
    t.set("style", crate::lua::parse::style_to_lua(lua, cell.style)?)?;
    if cell.image.is_some() {
        t.set("image", true)?;
    }
    Ok(t)
}

/// Marker userdata enabling idiomatic `slice:set(...)` call syntax.
/// Methods delegate to [`with_canvas`] at call time rather than carrying the borrow.
pub(crate) struct PaintSliceUd;

impl mlua::UserData for PaintSliceUd {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("width", |_, _, ()| with_canvas(|c| Ok(c.width())));
        methods.add_method("height", |_, _, ()| with_canvas(|c| Ok(c.height())));
        methods.add_method("get", |lua, _, (x, y): (u16, u16)| {
            with_canvas(|c| c.cell(x, y).map(|cell| cell_to_lua(lua, cell)).transpose())
        });
        methods.add_method(
            "set",
            |_, _, (x, y, ch, style): (u16, u16, Option<String>, Option<mlua::Table>)| {
                let symbol = one_grapheme_or_space(ch.as_deref())?;
                let style = style_arg(style)?;
                with_canvas(|c| {
                    if x < c.width() && y < c.height() {
                        let style = over(c, x, y, style);
                        c.set_symbol(x, y, symbol, style);
                    }
                    Ok(())
                })
            },
        );
        methods.add_method(
            "put_str",
            |_, _, (x, y, text, style): (u16, u16, String, Option<mlua::Table>)| {
                let style = style_arg(style)?;
                with_canvas(|c| Ok(put_str(c, x, y, &text, style)))
            },
        );
        methods.add_method(
            "fill",
            |_,
             _,
             (x, y, w, h, ch, style): (
                u16,
                u16,
                u16,
                u16,
                Option<String>,
                Option<mlua::Table>,
            )| {
                let symbol = one_grapheme_or_space(ch.as_deref())?;
                let style = style_arg(style)?;
                with_canvas(|c| {
                    fill(c, Rect::new(x, y, w, h), symbol, style);
                    Ok(())
                })
            },
        );
        methods.add_method(
            "set_style",
            |_, _, (x, y, w, h, patch): (u16, u16, u16, u16, mlua::Table)| {
                let patch =
                    crate::lua::parse::style_patch(&patch).map_err(LuaError::RuntimeError)?;
                with_canvas(|c| {
                    let rect =
                        Rect::new(x, y, w, h).clip_to(Rect::new(0, 0, c.width(), c.height()));
                    for y in rect.y..rect.bottom() {
                        for x in rect.x..rect.right() {
                            let Some(cell) = c.cell(x, y) else {
                                continue;
                            };
                            let style = patch.apply(cell.style);
                            c.restyle(x, y, style);
                        }
                    }
                    Ok(())
                })
            },
        );
        methods.add_method("map", |lua, _, f: mlua::Function| {
            with_canvas(|c| {
                for y in 0..c.height() {
                    let mut x = 0;
                    while x < c.width() {
                        let Some(cell) = c.cell(x, y).cloned() else {
                            break;
                        };
                        let width = cell.symbol.width().max(1);
                        if cell.symbol.is_continuation() {
                            x += 1;
                            continue;
                        }
                        let out: Option<mlua::Table> = f.call((cell_to_lua(lua, &cell)?, x, y))?;
                        if let Some(out) = out {
                            let style = match out.get::<Option<mlua::Table>>("style")? {
                                Some(t) => {
                                    crate::lua::parse::style(&t).map_err(LuaError::RuntimeError)?
                                }
                                None => cell.style,
                            };
                            match out.get::<Option<String>>("ch")? {
                                Some(ch) => {
                                    let symbol = one_grapheme_or_space(Some(&ch))?;
                                    c.set_symbol(x, y, symbol, style);
                                }
                                None => c.restyle(x, y, style),
                            }
                        }
                        x += width;
                    }
                }
                Ok(())
            })
        });
    }
}

/// Register the `smelt.paint.Slice` class docs. Keep in sync with the
/// `impl UserData for PaintSliceUd` block above.
pub fn register_paint_slice_docs() {
    record_class(LuaClassDecl {
        name: "smelt.paint.Slice",
        classification: smelt_core::lua::doc::classification_for_type("smelt.paint.Slice"),
        doc: "The area a paint callback draws into: its layout leaf, or the whole screen for `smelt.paint.frame`. Coordinates are `(x, y)` from the top-left, in cells. The slice already holds what was painted below it, so a callback can read cells and draw over them. Writes whose style leaves `bg` unset keep the background underneath.",
        fields: smelt_core::class_methods! {
            "width" => fn() -> u16, "Return the slice width in cells.",
            "height" => fn() -> u16, "Return the slice height in cells.",
            "get" => fn(x: u16, y: u16) -> Option<mlua::Table>, "Return the cell at `(x, y)` as `{ ch, width, style, image? }`, or `nil` outside the slice. The right half of a wide glyph has `ch = \"\"` and `width = 0`.",
            "set" => fn(x: u16, y: u16, ch: Option<String>, style: Option<mlua::Table>) -> (), "Write one grapheme cluster (default a space) at `(x, y)`.",
            "put_str" => fn(x: u16, y: u16, text: String, style: Option<mlua::Table>) -> u16, "Write `text` from `(x, y)`, clipped at the right edge. Returns the column after the last cell written.",
            "fill" => fn(x: u16, y: u16, w: u16, h: u16, ch: Option<String>, style: Option<mlua::Table>) -> (), "Fill a rectangle with one grapheme cluster (default a space).",
            "set_style" => fn(x: u16, y: u16, w: u16, h: u16, patch: mlua::Table) -> (), "Change only the style of a rectangle, keeping its glyphs. Keys present in `patch` override the current style; `fg = false` / `bg = false` clear a color.",
            "map" => fn(f: mlua::Function) -> (), "Call `f(cell, x, y)` for every cell, skipping the right halves of wide glyphs. Return `nil` to keep the cell, or `{ ch?, style? }` to replace its glyph and/or full style.",
        },
    });
}

/// Tracks frame count, animation time, and the pointer across frames.
#[derive(Debug, Default)]
pub(crate) struct PaintTimeline {
    epoch: Option<std::time::Instant>,
    frame: u64,
    /// Last pointer position in screen cells.
    pub pointer: Option<(u16, u16)>,
}

impl PaintTimeline {
    /// Start a new frame and return the values its paint callbacks see.
    pub(crate) fn advance(&mut self, now: std::time::Instant) -> PaintClock {
        let epoch = *self.epoch.get_or_insert(now);
        self.frame = self.frame.wrapping_add(1);
        PaintClock {
            now_ms: now.saturating_duration_since(epoch).as_millis() as u64,
            frame: self.frame,
            pointer: self.pointer,
            cursor: None,
        }
    }
}

/// Frame-wide values every paint callback sees in `ctx`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaintClock {
    /// Milliseconds since the first painted frame, for animation.
    pub now_ms: u64,
    /// Count of frames painted since startup.
    pub frame: u64,
    /// Last pointer position in screen cells.
    pub pointer: Option<(u16, u16)>,
    /// Where the visible cursor is painted, in screen cells. Known only once
    /// the frame is complete, so only frame paints see it.
    pub cursor: Option<(u16, u16)>,
}

/// Build the per-frame `ctx` table handed to the Lua paint callback.
fn build_ctx_table(
    lua: &Lua,
    area: Rect,
    ctx: &DrawContext,
    clock: &PaintClock,
    pointer: bool,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("focused", ctx.focused)?;
    t.set("terminal_width", ctx.terminal_width)?;
    t.set("terminal_height", ctx.terminal_height)?;
    t.set("x", area.x)?;
    t.set("y", area.y)?;
    t.set("now_ms", clock.now_ms)?;
    t.set("frame", clock.frame)?;
    let relative = |(x, y): (u16, u16)| -> LuaResult<mlua::Table> {
        let p = lua.create_table()?;
        p.set("x", i32::from(x) - i32::from(area.x))?;
        p.set("y", i32::from(y) - i32::from(area.y))?;
        Ok(p)
    };
    if let Some(at) = clock.pointer.filter(|_| pointer) {
        t.set("pointer", relative(at)?)?;
    }
    if let Some(at) = clock.cursor {
        t.set("cursor", relative(at)?)?;
    }
    Ok(t)
}

/// Run one Lua paint callback on `slice`. Errors are recorded rather than
/// propagated; cells the callback wrote before failing stay painted.
pub(crate) fn paint(
    runtime: &super::LuaExecution,
    slot: PaintSlot,
    slice: &mut GridSlice<'_>,
    ctx: &DrawContext,
    clock: &PaintClock,
) {
    let lua = &runtime.lua;
    let func: mlua::Function = {
        let Ok(cbs) = runtime.shared().callbacks.lock() else {
            return;
        };
        let Some(handle) = cbs.get(&slot.handle) else {
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
    let ctx_tbl = match build_ctx_table(lua, slice.grid_rect(), ctx, clock, slot.options.pointer) {
        Ok(t) => t,
        Err(e) => {
            runtime.record_error(format!("smelt.paint: ctx: {e}"));
            return;
        }
    };
    let _perf = smelt_perf::perf::begin("lua:paint");
    scope_canvas(slice, || {
        if let Err(e) = func.call::<()>((ud, ctx_tbl)) {
            runtime.record_error(format!("smelt.paint: {e}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anon(reg: &mut PaintRegistry, handle: u64) -> PaintId {
        reg.register(handle, None, PaintOptions::default()).0
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
        assert_eq!(reg.leaf(id).map(|slot| slot.handle), Some(42));
        assert!(reg.contains(id));
        let removed = reg.unregister(id);
        assert_eq!(removed, Some(42));
        assert_eq!(reg.leaf(id), None);
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
    fn canvas_errors_outside_paint() {
        let r: LuaResult<()> = with_canvas(|_| Ok(()));
        assert!(r.is_err());
    }

    /// Run `script` against a fresh `grid`, with `slice` bound to the whole grid.
    fn run(grid: &mut crate::smelt_edit::Grid, script: &str) -> LuaResult<()> {
        let lua = Lua::new();
        lua.globals()
            .set("slice", lua.create_userdata(PaintSliceUd)?)?;
        let mut slice = grid.as_mut_slice();
        scope_canvas(&mut slice, || lua.load(script).exec())
    }

    #[test]
    fn set_writes_one_complete_grapheme() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let mut grid = crate::smelt_edit::Grid::new(4, 1);
            run(&mut grid, &format!("slice:set(0, 0, {grapheme:?})")).unwrap();
            assert_eq!(grid.cell(0, 0).symbol, grapheme);
        }
    }

    #[test]
    fn writes_reject_ambiguous_symbols() {
        let mut grid = crate::smelt_edit::Grid::new(4, 1);
        assert!(run(&mut grid, "slice:set(0, 0, 'ab')").is_err());
        assert!(run(&mut grid, "slice:fill(0, 0, 2, 1, 'ab')").is_err());
    }

    #[test]
    fn writes_keep_the_background_under_them_unless_set() {
        use crate::smelt_edit::{Color, Style};
        let mut grid = crate::smelt_edit::Grid::new(3, 1);
        grid.as_mut_slice().fill(
            Rect::new(0, 0, 3, 1),
            'X',
            Style::default().bg(Color::Green),
        );
        run(
            &mut grid,
            "slice:put_str(0, 0, '@', { fg = 'red' }); slice:set(1, 0, '@', { bg = 'blue' })",
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(0, 0).style.bg, Some(Color::Green));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Blue));
        assert_eq!(grid.cell(2, 0).symbol, 'X');
    }

    #[test]
    fn get_reads_what_was_painted_below() {
        use crate::smelt_edit::{Color, Style};
        let mut grid = crate::smelt_edit::Grid::new(4, 1);
        grid.as_mut_slice()
            .put_str(0, 0, "a語", Style::default().fg(Color::Red));
        run(
            &mut grid,
            r#"
            local a, wide, half = slice:get(0, 0), slice:get(1, 0), slice:get(2, 0)
            assert(a.ch == "a" and a.width == 1 and a.style.fg.ansi == 9)
            assert(wide.ch == "語" and wide.width == 2)
            assert(half.ch == "" and half.width == 0)
            assert(slice:get(4, 0) == nil)
            "#,
        )
        .unwrap();
    }

    #[test]
    fn set_style_patches_style_and_keeps_glyphs() {
        use crate::smelt_edit::{Color, Style};
        let mut grid = crate::smelt_edit::Grid::new(3, 1);
        grid.as_mut_slice()
            .put_str(0, 0, "abc", Style::default().fg(Color::Red).bold());
        run(
            &mut grid,
            "slice:set_style(1, 0, 5, 1, { bg = 'blue', bold = false })",
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).style.bg, None);
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Blue));
        assert!(!grid.cell(2, 0).style.bold);
    }

    #[test]
    fn map_visits_each_glyph_once_and_applies_replacements() {
        let mut grid = crate::smelt_edit::Grid::new(4, 1);
        grid.as_mut_slice()
            .put_str(0, 0, "a語b", crate::smelt_edit::Style::default());
        run(
            &mut grid,
            r#"
            local seen = {}
            slice:map(function(cell, x)
              seen[#seen + 1] = x .. cell.ch
              if cell.ch == "a" then return { ch = "z" } end
              if cell.ch == "b" then return { style = { underline = true } } end
            end)
            assert(table.concat(seen, ",") == "0a,1語,3b", table.concat(seen, ","))
            "#,
        )
        .unwrap();

        assert_eq!(grid.cell(0, 0).symbol, 'z');
        assert_eq!(grid.cell(1, 0).symbol, '語');
        assert_eq!(grid.cell(3, 0).symbol, 'b');
        assert!(grid.cell(3, 0).style.underline);
    }

    #[test]
    fn scoped_canvas_rejects_aliases_and_restores_nested_scope() {
        let mut outer_grid = crate::smelt_edit::Grid::new(3, 1);
        let mut outer = outer_grid.as_mut_slice();
        let mut inner_grid = crate::smelt_edit::Grid::new(2, 1);
        let mut inner = inner_grid.as_mut_slice();

        scope_canvas(&mut outer, || {
            assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 3);
            with_canvas(|c| {
                assert!(with_canvas(|_| Ok(())).is_err());
                put_str(c, 0, 0, "a", Style::new());
                Ok(())
            })
            .unwrap();

            scope_canvas(&mut inner, || {
                assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 2);
            });
            assert_eq!(with_canvas(|c| Ok(c.width())).unwrap(), 3);
        });

        assert!(with_canvas(|_| Ok(())).is_err());
    }

    #[test]
    fn frame_paints_keep_registration_order_and_skip_leaves() {
        let mut reg = PaintRegistry::default();
        let frame = PaintOptions {
            stage: PaintStage::Frame,
            ..PaintOptions::default()
        };
        let (first, _) = reg.register(1, Some("a".into()), frame);
        anon(&mut reg, 2);
        let (second, _) = reg.register(3, None, frame);
        reg.register(4, Some("a".into()), frame);

        let order: Vec<_> = reg
            .frame_paints()
            .into_iter()
            .map(|(id, slot)| (id, slot.handle))
            .collect();
        assert_eq!(order, [(first, 4), (second, 3)]);
    }

    #[test]
    fn fps_sets_the_animation_interval() {
        let options = |fps| PaintOptions {
            fps,
            ..PaintOptions::default()
        };
        assert_eq!(options(None).interval(), None);
        assert_eq!(options(Some(0)).interval(), None);
        assert_eq!(
            options(Some(20)).interval(),
            Some(std::time::Duration::from_millis(50))
        );
    }

    #[test]
    fn register_named_reuses_id_and_returns_old_handle() {
        let mut reg = PaintRegistry::default();
        let (id1, old1) = reg.register(11, Some("plugin.banner".into()), PaintOptions::default());
        assert!(old1.is_none());
        assert_eq!(reg.leaf(id1).map(|slot| slot.handle), Some(11));
        let (id2, old2) = reg.register(22, Some("plugin.banner".into()), PaintOptions::default());
        assert_eq!(
            id1, id2,
            "named slot must keep stable id across re-register"
        );
        assert_eq!(
            old2,
            Some(11),
            "previous handle must be returned for release"
        );
        assert_eq!(reg.leaf(id2).map(|slot| slot.handle), Some(22));
    }

    #[test]
    fn clear_anonymous_keeps_named_drops_anonymous() {
        let mut reg = PaintRegistry::default();
        let anon_id = anon(&mut reg, 100);
        let (named, _) = reg.register(200, Some("plugin.x".into()), PaintOptions::default());
        let released = reg.clear_anonymous();
        assert_eq!(released, vec![100], "anonymous handle ids must be released");
        assert!(!reg.contains(anon_id), "anonymous slot must be dropped");
        assert!(reg.contains(named), "named slot must survive");
        assert_eq!(reg.leaf(named).map(|slot| slot.handle), Some(200));
    }

    #[test]
    fn unregister_named_removes_name_binding() {
        let mut reg = PaintRegistry::default();
        let (id, _) = reg.register(7, Some("plugin.x".into()), PaintOptions::default());
        assert_eq!(reg.unregister(id), Some(7));
        let (id2, old) = reg.register(8, Some("plugin.x".into()), PaintOptions::default());
        assert!(old.is_none(), "name binding must be cleared on unregister");
        assert_ne!(
            id, id2,
            "re-registering after unregister allocates fresh id"
        );
    }
}
