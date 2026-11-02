//! Lua paint registry - maps `PaintId`s to Lua callbacks invoked by the renderer.
//!
//! The slice borrow is lent through scoped TLS only while the paint callback is on the
//! stack. Methods called outside a paint callback return a clean Lua error.

mod slice;

use crate::smelt_edit::layout::PaintId;
use crate::smelt_edit::{DrawContext, Grid, GridSlice, Rect};
use mlua::prelude::*;
use smelt_edit::NamedSlots;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) use slice::{register_paint_slice_docs, PaintCtx};

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
    Frame {
        /// Frame paints run from the lowest order up; ties run in registration order.
        order: i32,
        /// Keep the previous frame for `ctx.prev` and `ctx.damage`.
        history: bool,
        /// Callback handle that maps a screen cell to the cell smelt laid out,
        /// applied to mouse input before hit-testing.
        map_pointer: Option<u64>,
    },
}

/// Per-paint options set at registration and adjustable on the handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaintOptions {
    pub stage: PaintStage,
    /// Repaint when the pointer moves, and expose `ctx.pointer`.
    pub pointer: bool,
}

impl PaintOptions {
    pub(crate) fn history(&self) -> bool {
        matches!(self.stage, PaintStage::Frame { history: true, .. })
    }
}

/// A registered paint callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaintSlot {
    pub id: PaintId,
    /// Callback handle id (a `u64` into `LuaShared::callbacks`).
    pub handle: u64,
    pub options: PaintOptions,
}

impl PaintSlot {
    pub(crate) fn map_pointer(&self) -> Option<u64> {
        match self.options.stage {
            PaintStage::Frame { map_pointer, .. } => map_pointer,
            PaintStage::Leaf => None,
        }
    }

    /// Every callback handle the slot owns, to release when it goes away.
    pub(crate) fn callbacks(&self) -> impl Iterator<Item = u64> {
        std::iter::once(self.handle).chain(self.map_pointer())
    }
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
    /// previous slot so the caller can release its callbacks.
    /// Anonymous (`None`) slots always allocate a fresh id and never
    /// return a previous slot.
    pub(crate) fn register(
        &mut self,
        handle_id: u64,
        name: Option<String>,
        options: PaintOptions,
    ) -> (PaintId, Option<PaintSlot>) {
        let slot = |id| PaintSlot {
            id,
            handle: handle_id,
            options,
        };
        if let Some(ref n) = name {
            if let Some(existing) = self.names.lookup(n) {
                return (existing, self.handles.insert(existing, slot(existing)));
            }
        }
        let id = PaintId(self.next_id.fetch_add(1, Ordering::Relaxed));
        debug_assert!(
            id.0 >= PAINT_ID_BASE,
            "PaintRegistry exhausted paint half of u64 namespace"
        );
        self.handles.insert(id, slot(id));
        if let Some(n) = name {
            self.names.bind(n, id);
        }
        (id, None)
    }

    /// Remove the binding and return the slot so the caller can release its callbacks.
    pub(crate) fn unregister(&mut self, id: PaintId) -> Option<PaintSlot> {
        self.names.unbind_by_id(id);
        self.handles.remove(&id)
    }

    /// The leaf paint for a layout leaf id. Frame paints never draw as leaves,
    /// even when their handle is placed in a layout.
    pub(crate) fn leaf(&self, id: PaintId) -> Option<PaintSlot> {
        self.handles
            .get(&id)
            .copied()
            .filter(|slot| slot.options.stage == PaintStage::Leaf)
    }

    /// Frame-stage paints in run order: by `order`, then registration.
    /// Re-registering a named slot keeps its id, so it keeps its place.
    pub(crate) fn frame_paints(&self) -> Vec<PaintSlot> {
        let mut paints: Vec<_> = self
            .handles
            .iter()
            .filter_map(|(id, slot)| match slot.options.stage {
                PaintStage::Frame { order, .. } => Some(((order, id.0), *slot)),
                PaintStage::Leaf => None,
            })
            .collect();
        paints.sort_by_key(|(key, _)| *key);
        paints.into_iter().map(|(_, slot)| slot).collect()
    }

    /// Frame paints with a pointer map, top effect first: the order a
    /// screen position unwinds through them.
    pub(crate) fn pointer_maps(&self) -> Vec<PaintSlot> {
        let mut maps: Vec<_> = self
            .frame_paints()
            .into_iter()
            .filter(|slot| slot.map_pointer().is_some())
            .collect();
        maps.reverse();
        maps
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
    pub(crate) fn all_ids(&self) -> impl Iterator<Item = PaintId> + '_ {
        self.handles.keys().copied()
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
                released.extend(slot.callbacks());
                false
            }
        });
        released
    }
}

/// Tracks frame count, animation time, the pointer, and the previous frame
/// across frames.
#[derive(Default)]
pub(crate) struct PaintTimeline {
    frame: u64,
    /// Last pointer position in screen cells.
    pub pointer: Option<(u16, u16)>,
    history: FrameHistory,
}

impl PaintTimeline {
    /// Start a new frame at `now_ms` on the runtime's monotonic clock and
    /// return the values its paint callbacks see.
    pub(crate) fn advance(&mut self, now_ms: u64, terminal_focused: bool) -> PaintClock {
        self.frame = self.frame.wrapping_add(1);
        PaintClock {
            now_ms,
            frame: self.frame,
            terminal_focused,
            pointer: self.pointer,
            cursor: None,
        }
    }

    /// Compare the finished screen `base`, with its windows' content at
    /// `areas`, to the last one recorded, before frame paints change it.
    /// Pass the result to [`Self::end_history`].
    pub(crate) fn begin_history(&mut self, base: &Grid, areas: Vec<ScrollArea>) -> FrameChanges {
        self.history.begin(base, areas)
    }

    pub(crate) fn end_history(&mut self, changes: FrameChanges) {
        self.history.end(changes);
    }

    /// Forget the previous frame while no paint asks for it.
    pub(crate) fn drop_history(&mut self) {
        self.history = FrameHistory::default();
    }
}

/// Where one window's content sat on a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScrollArea {
    pub win: crate::smelt_edit::WinId,
    /// The window's text area.
    pub area: Rect,
    /// `(anchor id, screen row of the anchor's row 0)`, which is above the
    /// area for an anchor whose start is scrolled off. A window's buffer is
    /// one anchor; the transcript has one per visible block, so a block
    /// growing above the view does not read as a scroll.
    pub anchors: Vec<(u64, i64)>,
}

impl ScrollArea {
    /// Rows the content moved up since `prev` (down when negative), told by
    /// an anchor on screen in both frames. Zero when nothing moved, or when
    /// the area changed and rows no longer line up.
    fn scrolled_since(&self, prev: &ScrollArea) -> i32 {
        if self.area != prev.area {
            return 0;
        }
        self.anchors
            .iter()
            .find_map(|(id, origin)| {
                let (_, was) = prev.anchors.iter().find(|(prev_id, _)| prev_id == id)?;
                i32::try_from(was - origin).ok()
            })
            .unwrap_or(0)
    }
}

/// The screen as smelt drew it on the previous frame, before frame paints.
#[derive(Default)]
struct FrameHistory {
    prev: Option<(Arc<Grid>, Vec<ScrollArea>)>,
}

impl FrameHistory {
    fn begin(&mut self, base: &Grid, areas: Vec<ScrollArea>) -> FrameChanges {
        let prev = self.prev.take();
        let moves: Vec<(Rect, i32)> = areas
            .iter()
            .map(|now| {
                let was = prev
                    .as_ref()
                    .and_then(|(_, was)| was.iter().find(|was| was.win == now.win));
                (now.area, was.map_or(0, |was| now.scrolled_since(was)))
            })
            .collect();
        let damage = match &prev {
            Some((grid, _)) => base.damage_scrolled(grid, &moves),
            None => base.rows(),
        };
        let scrolls = areas
            .iter()
            .zip(&moves)
            .filter(|(_, (_, dy))| *dy != 0)
            .map(|(area, &(rect, dy))| (area.win, rect, dy))
            .collect();
        FrameChanges {
            prev: prev.map(|(grid, _)| grid),
            damage,
            scrolls,
            base: (Arc::new(base.clone()), areas),
        }
    }

    fn end(&mut self, changes: FrameChanges) {
        self.prev = Some(changes.base);
    }
}

/// What a frame paint with `history` sees as `ctx.prev`, `ctx.damage` and
/// `ctx.scrolls`.
pub(crate) struct FrameChanges {
    prev: Option<Arc<Grid>>,
    damage: Vec<Rect>,
    /// Each window whose text moved, its text area, and by how many rows up.
    scrolls: Vec<(crate::smelt_edit::WinId, Rect, i32)>,
    base: (Arc<Grid>, Vec<ScrollArea>),
}

/// Frame-wide values every paint callback sees in `ctx`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaintClock {
    /// Milliseconds since smelt started, the clock of `smelt.time.monotonic_ms()`.
    pub now_ms: u64,
    /// Count of frames painted since startup.
    pub frame: u64,
    /// Whether the terminal window has OS focus.
    pub terminal_focused: bool,
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
    options: PaintOptions,
    changes: Option<&FrameChanges>,
) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    // Whether keystrokes reach this paint: a leaf needs its window focused, and
    // the whole screen only needs the terminal.
    let area_focused = matches!(options.stage, PaintStage::Frame { .. }) || ctx.focused;
    t.set("focused", clock.terminal_focused && area_focused)?;
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
    if let Some(at) = clock.pointer.filter(|_| options.pointer) {
        t.set("pointer", relative(at)?)?;
    }
    if let Some(at) = clock.cursor {
        t.set("cursor", relative(at)?)?;
    }
    if let Some(changes) = changes.filter(|_| options.history()) {
        if let Some(prev) = &changes.prev {
            t.set("prev", slice::PaintSnapshot(Arc::clone(prev)))?;
        }
        let damage = lua.create_table_with_capacity(changes.damage.len(), 0)?;
        for (i, rect) in changes.damage.iter().enumerate() {
            damage.set(i + 1, crate::lua::parse::rect_to_lua(lua, *rect)?)?;
        }
        t.set("damage", damage)?;
        let scrolls = lua.create_table_with_capacity(changes.scrolls.len(), 0)?;
        for (i, &(id, rect, dy)) in changes.scrolls.iter().enumerate() {
            let scroll = crate::lua::parse::rect_to_lua(lua, rect)?;
            scroll.set("win", crate::lua::api::win::LuaWin { id })?;
            scroll.set("dy", dy)?;
            scrolls.set(i + 1, scroll)?;
        }
        t.set("scrolls", scrolls)?;
    }
    Ok(t)
}

/// The Lua function behind callback `handle`, if it is still registered.
fn callback(runtime: &super::LuaExecution, handle: u64) -> Option<mlua::Function> {
    let cbs = runtime.shared().callbacks.lock().ok()?;
    runtime.lua.registry_value(&cbs.get(&handle)?.key).ok()
}

/// Run one Lua paint callback on `slice`. Cells the callback wrote before
/// failing stay painted.
pub(crate) fn paint(
    runtime: &super::LuaExecution,
    slot: PaintSlot,
    slice: &mut GridSlice<'_>,
    ctx: &DrawContext,
    clock: &PaintClock,
    changes: Option<&FrameChanges>,
) -> LuaResult<()> {
    let Some(func) = callback(runtime, slot.handle) else {
        return Ok(());
    };
    let ctx_tbl = build_ctx_table(
        &runtime.lua,
        slice.grid_rect(),
        ctx,
        clock,
        slot.options,
        changes,
    )?;
    let _perf = smelt_perf::perf::begin("lua:paint");
    slice::scope_slice(slice, || {
        runtime.lua.scope(|scope| {
            let ud = scope.create_userdata(slice::PaintSliceUd)?;
            func.call::<()>((ud, ctx_tbl))
        })
    })
}

/// Map screen cell `at` through the pointer-map callback `handle`, on a
/// `size` screen. A `nil` return keeps the position, and the result is
/// clamped to the screen.
pub(crate) fn map_pointer(
    runtime: &super::LuaExecution,
    handle: u64,
    at: (u16, u16),
    size: (u16, u16),
) -> LuaResult<(u16, u16)> {
    let Some(func) = callback(runtime, handle) else {
        return Ok(at);
    };
    let _perf = smelt_perf::perf::begin("lua:paint:map_pointer");
    let clamp = |v: i64, len: u16| v.clamp(0, i64::from(len.saturating_sub(1))) as u16;
    Ok(
        match func.call::<(Option<i64>, Option<i64>)>((at.0, at.1, size.0, size.1))? {
            (Some(x), Some(y)) => (clamp(x, size.0), clamp(y, size.1)),
            _ => at,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anon(reg: &mut PaintRegistry, handle: u64) -> PaintId {
        reg.register(handle, None, PaintOptions::default()).0
    }

    fn frame(order: i32) -> PaintOptions {
        PaintOptions {
            stage: PaintStage::Frame {
                order,
                history: false,
                map_pointer: None,
            },
            ..PaintOptions::default()
        }
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
        let removed = reg.unregister(id).map(|slot| slot.handle);
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
    fn frame_paints_keep_registration_order_and_skip_leaves() {
        let mut reg = PaintRegistry::default();
        reg.register(1, Some("a".into()), frame(0));
        anon(&mut reg, 2);
        reg.register(3, None, frame(0));
        reg.register(4, Some("a".into()), frame(0));

        let order: Vec<_> = reg.frame_paints().iter().map(|slot| slot.handle).collect();
        assert_eq!(order, [4, 3]);
    }

    #[test]
    fn frame_paints_run_by_order_before_registration() {
        let mut reg = PaintRegistry::default();
        reg.register(1, None, frame(10));
        reg.register(2, None, frame(-5));
        reg.register(3, None, frame(0));
        reg.register(4, None, frame(0));

        let order: Vec<_> = reg.frame_paints().iter().map(|slot| slot.handle).collect();
        assert_eq!(order, [2, 3, 4, 1]);
    }

    #[test]
    fn history_reports_damage_and_keeps_the_previous_base() {
        let mut history = FrameHistory::default();
        let mut base = Grid::new(4, 1);

        let first = history.begin(&base, Vec::new());
        assert!(first.prev.is_none());
        assert_eq!(first.damage, [Rect::new(0, 0, 4, 1)]);
        history.end(first);

        base.as_mut_slice()
            .put_str(1, 0, "x", crate::smelt_edit::Style::default());
        let second = history.begin(&base, Vec::new());
        let prev = second.prev.as_ref().expect("previous frame");
        assert_eq!(prev.cell(1, 0).symbol, ' ');
        assert_eq!(second.damage, [Rect::new(1, 0, 1, 1)]);
        history.end(second);

        let third = history.begin(&base, Vec::new());
        assert!(third.damage.is_empty());
        assert_eq!(third.prev.as_ref().unwrap().cell(1, 0).symbol, 'x');
    }

    #[test]
    fn history_takes_the_scroll_out_of_damage() {
        let content = |origin| {
            vec![ScrollArea {
                win: crate::smelt_edit::WinId(1),
                area: Rect::new(0, 0, 3, 3),
                anchors: vec![(7, origin)],
            }]
        };
        let rows = |lines: [&str; 3]| {
            let mut grid = Grid::new(3, 3);
            for (y, line) in lines.iter().enumerate() {
                grid.as_mut_slice()
                    .put_str(0, y as u16, line, crate::smelt_edit::Style::default());
            }
            grid
        };
        let mut history = FrameHistory::default();
        let first = history.begin(&rows(["a", "b", "c"]), content(0));
        history.end(first);

        let scrolled = history.begin(&rows(["b", "c", "d"]), content(-1));
        assert_eq!(
            scrolled.scrolls,
            [(crate::smelt_edit::WinId(1), Rect::new(0, 0, 3, 3), 1)]
        );
        assert_eq!(scrolled.damage, [Rect::new(0, 2, 3, 1)], "only the new row");
        history.end(scrolled);

        let still = history.begin(&rows(["b", "c", "e"]), content(-1));
        assert!(still.scrolls.is_empty());
        assert_eq!(still.damage, [Rect::new(0, 2, 1, 1)]);
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
            old2.map(|slot| slot.handle),
            Some(11),
            "previous handle must be returned for release"
        );
        assert_eq!(reg.leaf(id2).map(|slot| slot.handle), Some(22));
    }

    #[test]
    fn pointer_maps_unwind_from_the_top_effect_and_are_released() {
        let mapped = |order, map| PaintOptions {
            stage: PaintStage::Frame {
                order,
                history: false,
                map_pointer: Some(map),
            },
            ..PaintOptions::default()
        };
        let mut reg = PaintRegistry::default();
        reg.register(1, None, mapped(0, 10));
        let top = reg.register(2, None, mapped(5, 20)).0;
        reg.register(3, None, frame(9));
        let maps = |reg: &PaintRegistry| -> Vec<_> {
            reg.pointer_maps()
                .iter()
                .filter_map(PaintSlot::map_pointer)
                .collect()
        };

        assert_eq!(maps(&reg), [20, 10]);
        let released: Vec<_> = reg.unregister(top).unwrap().callbacks().collect();
        assert_eq!(released, [2, 20]);
        assert_eq!(maps(&reg), [10]);
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
        assert_eq!(reg.unregister(id).map(|slot| slot.handle), Some(7));
        let (id2, old) = reg.register(8, Some("plugin.x".into()), PaintOptions::default());
        assert!(old.is_none(), "name binding must be cleared on unregister");
        assert_ne!(
            id, id2,
            "re-registering after unregister allocates fresh id"
        );
    }
}
