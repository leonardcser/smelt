//! Typed hit-region registry. Hosts that paint custom regions
//! (treemap tiles, picker rows, drag handles) push `(Rect, payload)`
//! pairs during paint and query them on mouse events. The registry
//! is generic over the payload so each host attaches its own
//! domain type — no `dyn Any`, no boxing.
//!
//! This is the "hit table" smelt's `Ui` already maintains internally
//! for windows + overlays, exposed as a primitive for paint-leaf
//! callers (`LayoutTree::Paint`) and standalone renderer consumers.
//! Keep the registry separate per surface (one per `Ui::render_*`
//! call); resetting between frames is the host's responsibility.

use crate::geometry::Rect;

/// Stores `(Rect, payload)` pairs in painter order. Later pushes win
/// hit lookups, matching the painter's overdraw semantics — the
/// rect painted last sits visually on top.
#[derive(Clone, Debug)]
pub struct HitRegistry<P> {
    entries: Vec<(Rect, P)>,
}

impl<P> Default for HitRegistry<P> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<P> HitRegistry<P> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every recorded entry. Call at the start of each paint
    /// pass so stale rects from the previous frame don't survive.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a hit region for the given payload. Empty rects
    /// (zero width or height) are ignored.
    pub fn record(&mut self, rect: Rect, payload: P) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.entries.push((rect, payload));
    }

    /// Find the topmost entry whose rect covers `(col, row)`. Walks
    /// in reverse-insertion order so later `record` calls shadow
    /// earlier ones.
    pub fn hit(&self, row: u16, col: u16) -> Option<&P> {
        self.entries
            .iter()
            .rev()
            .find_map(|(r, p)| r.contains(row, col).then_some(p))
    }

    /// Iterate all `(Rect, &payload)` pairs in insertion order.
    pub fn entries(&self) -> impl Iterator<Item = (Rect, &P)> {
        self.entries.iter().map(|(r, p)| (*r, p))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Tile {
        File(u32),
        Dir(u32),
    }

    #[test]
    fn hit_returns_topmost_payload() {
        let mut reg = HitRegistry::<Tile>::new();
        reg.record(Rect::new(0, 0, 20, 10), Tile::Dir(1));
        reg.record(Rect::new(2, 5, 5, 3), Tile::File(42));
        // (5, 3) is inside the inner File(42) region, recorded last.
        assert_eq!(reg.hit(3, 5), Some(&Tile::File(42)));
        // Outside the inner region but inside the outer Dir(1).
        assert_eq!(reg.hit(0, 0), Some(&Tile::Dir(1)));
    }

    #[test]
    fn hit_outside_all_rects_returns_none() {
        let mut reg = HitRegistry::<u32>::new();
        reg.record(Rect::new(0, 0, 5, 5), 7);
        assert_eq!(reg.hit(10, 10), None);
    }

    #[test]
    fn empty_rect_is_not_recorded() {
        let mut reg = HitRegistry::<u32>::new();
        reg.record(Rect::new(0, 0, 0, 5), 1);
        reg.record(Rect::new(0, 0, 5, 0), 2);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn clear_drops_entries() {
        let mut reg = HitRegistry::<u32>::new();
        reg.record(Rect::new(0, 0, 5, 5), 1);
        reg.clear();
        assert_eq!(reg.hit(0, 0), None);
    }
}
