//! Typed hit-region registry. Hosts push `(Rect, payload)` pairs during
//! paint and query them on mouse events. Reset between frames.

use crate::geometry::Rect;

/// Stores `(Rect, payload)` pairs in painter order. Later pushes shadow earlier ones.
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

    /// Drop all recorded entries. Call at the start of each paint pass.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a hit region. Empty rects are ignored.
    pub fn record(&mut self, rect: Rect, payload: P) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.entries.push((rect, payload));
    }

    /// Return the topmost payload whose rect covers `(row, col)`.
    pub fn hit(&self, row: u16, col: u16) -> Option<&P> {
        self.entries
            .iter()
            .rev()
            .find_map(|(r, p)| r.contains(row, col).then_some(p))
    }

    /// All `(Rect, &payload)` pairs in insertion order.
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
        assert_eq!(reg.hit(3, 5), Some(&Tile::File(42)));
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
