//! `Overlay` — z-stacked window groups positioned via an `Anchor`
//! over a `LayoutTree` of leaves.
//!
//! An overlay is a rectangle of windows positioned on top of the
//! main editor area. Its layout is a regular `LayoutTree` (so an
//! overlay can contain a Vbox/Hbox of one or more `Leaf(WinId)`s
//! with chrome on the container itself); positioning is by
//! [`Anchor`] (screen / cursor / another window); stacking is by
//! `z`; modality controls whether the host pauses engine drain
//! while focus is here.
//!
//! `OverlayId` is a stable opaque handle for chrome hit-testing
//! (border drag, title-bar grab) — distinct from any `WinId`s
//! contained in the overlay's layout.

use crate::layout::{Anchor, Corner, LayoutTree, Rect};
use crate::WinId;
use std::collections::HashMap;

/// Stable handle for an overlay. Distinct from `WinId` so chrome
/// hit-testing (`HitTarget::Chrome { owner: OverlayId }`) doesn't
/// collide with content hit-testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OverlayId(pub u32);

/// What inside an overlay a hit landed on. `Window` carries the
/// specific leaf `WinId`; `Chrome` is anywhere else inside the
/// overlay's resolved rect (border, title row, gap, padding) — the
/// host treats chrome hits as drag handles, close-button targets,
/// or focus-promote on click depending on the overlay's policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayHitTarget {
    Window(crate::WinId),
    Chrome,
}

/// Result of a global mouse hit-test against the open Ui surface.
/// Composes `OverlayHitTarget` (overlay paths) with split-window
/// hits, so callers don't need separate code per surface kind.
/// `Scrollbar { owner }` is reserved for the eventual split-render
/// path where Window publishes its scrollbar rect; the variant
/// exists so callers can pattern-match the full target shape today,
/// but `Ui::hit_test` doesn't return it yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Window(crate::WinId),
    Scrollbar { owner: crate::WinId },
    Chrome { owner: OverlayId },
}

#[derive(Clone, Debug)]
pub struct Overlay {
    pub layout: LayoutTree,
    pub anchor: Anchor,
    /// Stacking order. Higher draws on top. Same `z` falls back to
    /// insertion order.
    pub z: u16,
    /// When `true`, focus + Tab cycling stay inside this overlay and
    /// Esc on the focused leaf fires `WinEvent::Dismiss` on every
    /// leaf before closing. Independent of [`Self::blocks_agent`] —
    /// passive viewers (`/help`, `/btw`) are modal but do not pause
    /// the engine.
    pub modal: bool,
    /// When `true`, the host pauses engine-event drain and queues
    /// new user input while focus is inside this overlay. Set on
    /// permission prompts and other dialogs that gate a pending tool
    /// call.
    pub blocks_agent: bool,
}

impl Overlay {
    pub fn new(layout: LayoutTree, anchor: Anchor) -> Self {
        Self {
            layout,
            anchor,
            z: 50,
            modal: false,
            blocks_agent: false,
        }
    }

    pub fn with_z(mut self, z: u16) -> Self {
        self.z = z;
        self
    }

    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    pub fn blocks_agent(mut self, b: bool) -> Self {
        self.blocks_agent = b;
        self
    }
}

/// Inputs the anchor resolver needs from the rest of `Ui`.
pub struct AnchorContext<'a> {
    pub term_width: u16,
    pub term_height: u16,
    /// Where the text cursor currently is, in terminal cells.
    /// `None` if the cursor isn't visible / placed.
    pub cursor: Option<(u16, u16)>,
    /// Per-window rects for the host's split windows. Looked up by
    /// `Anchor::Win { target, .. }`.
    pub win_rects: &'a HashMap<WinId, Rect>,
}

/// Resolve an anchor + overlay size to a screen rect.
///
/// `size` is the `(width, height)` the overlay wants to render at
/// (typically computed from its layout's natural size + chrome).
/// The returned rect is clamped to the terminal — overflowing
/// overlays shrink and shift to remain on-screen.
///
/// Cursor anchors flip to the opposite corner if the natural
/// placement would overflow (canonical popup behavior). Win
/// anchors return `None` when the target window has no rect (yet).
pub fn resolve_anchor(anchor: &Anchor, size: (u16, u16), ctx: &AnchorContext<'_>) -> Option<Rect> {
    let (w, h) = size;
    let term_w = ctx.term_width;
    let term_h = ctx.term_height;
    let w = w.min(term_w);
    let h = h.min(term_h);
    let (top, left) = match anchor {
        Anchor::ScreenCenter => (term_h.saturating_sub(h) / 2, term_w.saturating_sub(w) / 2),
        Anchor::ScreenAt { row, col, corner } => {
            let (r, c) = corner_to_topleft(*corner, *row, *col, w, h);
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::Cursor {
            corner,
            row_offset,
            col_offset,
        } => {
            let (cy, cx) = ctx.cursor?;
            let r = cy as i32 + row_offset;
            let c = cx as i32 + col_offset;
            let (r, c) = corner_to_topleft(*corner, r, c, w, h);
            // Flip: if natural placement overflows the screen, swap
            // to the opposite corner relative to the cursor.
            let r = if r + h as i32 > term_h as i32 || r < 0 {
                let opposite = flip_vert(*corner);
                let (r2, _) = corner_to_topleft(
                    opposite,
                    cy as i32 + row_offset,
                    cx as i32 + col_offset,
                    w,
                    h,
                );
                r2
            } else {
                r
            };
            let c = if c + w as i32 > term_w as i32 || c < 0 {
                let opposite = flip_horiz(*corner);
                let (_, c2) = corner_to_topleft(
                    opposite,
                    cy as i32 + row_offset,
                    cx as i32 + col_offset,
                    w,
                    h,
                );
                c2
            } else {
                c
            };
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::Win {
            target,
            attach,
            row_offset,
            col_offset,
        } => {
            let target_rect = ctx.win_rects.get(target)?;
            let (r, c) = match attach {
                Corner::NW => (target_rect.top as i32, target_rect.left as i32),
                Corner::NE => (
                    target_rect.top as i32,
                    target_rect.right() as i32 - w as i32,
                ),
                Corner::SW => (
                    target_rect.bottom() as i32 - h as i32,
                    target_rect.left as i32,
                ),
                Corner::SE => (
                    target_rect.bottom() as i32 - h as i32,
                    target_rect.right() as i32 - w as i32,
                ),
            };
            let r = r + row_offset;
            let c = c + col_offset;
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::ScreenBottom { above_rows } => {
            let avail_h = term_h.saturating_sub(*above_rows);
            let h = h.min(avail_h);
            let top = avail_h.saturating_sub(h);
            let left = term_w.saturating_sub(w) / 2;
            return Some(Rect::new(top, left, w, h));
        }
    };
    Some(Rect::new(top, left, w, h))
}

/// Translate a corner-anchored point `(row, col)` into the
/// rectangle's top-left given its `(w, h)`.
fn corner_to_topleft(corner: Corner, row: i32, col: i32, w: u16, h: u16) -> (i32, i32) {
    match corner {
        Corner::NW => (row, col),
        Corner::NE => (row, col - w as i32 + 1),
        Corner::SW => (row - h as i32 + 1, col),
        Corner::SE => (row - h as i32 + 1, col - w as i32 + 1),
    }
}

fn flip_vert(c: Corner) -> Corner {
    match c {
        Corner::NW => Corner::SW,
        Corner::NE => Corner::SE,
        Corner::SW => Corner::NW,
        Corner::SE => Corner::NE,
    }
}

fn flip_horiz(c: Corner) -> Corner {
    match c {
        Corner::NW => Corner::NE,
        Corner::NE => Corner::NW,
        Corner::SW => Corner::SE,
        Corner::SE => Corner::SW,
    }
}

fn clamp_axis(pos: i32, term: u16, span: u16) -> u16 {
    let max_start = term.saturating_sub(span) as i32;
    pos.clamp(0, max_start) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Anchor, Constraint, Corner};
    use crate::WinId;

    #[test]
    fn overlay_defaults_are_sensible() {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(layout, Anchor::ScreenCenter);
        assert_eq!(ov.z, 50);
        assert!(!ov.modal);
        assert_eq!(ov.anchor, Anchor::ScreenCenter);
    }

    #[test]
    fn overlay_builders_compose() {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(
            layout,
            Anchor::Win {
                target: WinId(7),
                attach: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
        )
        .with_z(100)
        .modal(true);
        assert_eq!(ov.z, 100);
        assert!(ov.modal);
        assert!(matches!(ov.anchor, Anchor::Win { .. }));
    }

    #[test]
    fn overlay_id_round_trips() {
        let id = OverlayId(7);
        assert_eq!(id, OverlayId(7));
        assert_ne!(id, OverlayId(8));
    }

    fn ctx<'a>(w: u16, h: u16, win_rects: &'a HashMap<WinId, Rect>) -> AnchorContext<'a> {
        AnchorContext {
            term_width: w,
            term_height: h,
            cursor: None,
            win_rects,
        }
    }

    #[test]
    fn screen_center_centers() {
        let rects = HashMap::new();
        let r = resolve_anchor(&Anchor::ScreenCenter, (40, 10), &ctx(80, 24, &rects)).unwrap();
        assert_eq!(r, Rect::new(7, 20, 40, 10));
    }

    #[test]
    fn screen_center_clamps_to_terminal() {
        let rects = HashMap::new();
        let r = resolve_anchor(&Anchor::ScreenCenter, (200, 50), &ctx(80, 24, &rects)).unwrap();
        assert_eq!(r, Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn screen_at_nw_places_at_origin() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenAt {
                row: 5,
                col: 10,
                corner: Corner::NW,
            },
            (20, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(5, 10, 20, 5));
    }

    #[test]
    fn screen_at_se_places_with_corner_at_target() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenAt {
                row: 10,
                col: 30,
                corner: Corner::SE,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        // SE corner of overlay sits at (10, 30) → top-left at (7, 21).
        assert_eq!(r, Rect::new(7, 21, 10, 4));
    }

    #[test]
    fn cursor_anchor_flips_when_overflowing() {
        let rects = HashMap::new();
        let mut c = ctx(80, 24, &rects);
        c.cursor = Some((22, 5)); // near bottom
        let r = resolve_anchor(
            &Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 8),
            &c,
        )
        .unwrap();
        // NW at row 22 + height 8 would overflow row 24; flips to SW.
        // SW corner at (22, 5) → top-left at (15, 5).
        assert_eq!(r.top, 15);
        assert_eq!(r.left, 5);
    }

    #[test]
    fn cursor_anchor_returns_none_without_cursor() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 5),
            &ctx(80, 24, &rects),
        );
        assert!(r.is_none());
    }

    #[test]
    fn win_anchor_attaches_to_target_corner() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7),
                attach: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (15, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(10, 20, 15, 5));
    }

    #[test]
    fn win_anchor_se_aligns_bottom_right() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        // target's SE = (18, 60), overlay's SE corner sits there →
        // top-left = (14, 50).
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7),
                attach: Corner::SE,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(14, 50, 10, 4));
    }

    #[test]
    fn win_anchor_returns_none_for_unknown_target() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(999),
                attach: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        );
        assert!(r.is_none());
    }

    #[test]
    fn win_anchor_offsets_shift_position() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        // NW corner at (10, 20) shifted up 1, right 3 → (9, 23).
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7),
                attach: Corner::NW,
                row_offset: -1,
                col_offset: 3,
            },
            (15, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(9, 23, 15, 5));
    }

    #[test]
    fn screen_bottom_docks_full_height_above_statusline() {
        let rects = HashMap::new();
        // term 80x24, above_rows=1 (statusline). Layout reports
        // natural (40, 24) — wants full height. The anchor clamps
        // height to 23 (term_h - above_rows) and pins it to the
        // bottom of the available area.
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 1 },
            (40, 24),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(0, 20, 40, 23));
    }

    #[test]
    fn screen_bottom_docks_short_layout_at_bottom() {
        let rects = HashMap::new();
        // Layout's natural (60, 8) — short content. Anchor sits
        // it at the bottom of the available area, centered.
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 1 },
            (60, 8),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        // top = (24 - 1) - 8 = 15; left = (80 - 60)/2 = 10.
        assert_eq!(r, Rect::new(15, 10, 60, 8));
    }

    #[test]
    fn screen_bottom_with_no_reserved_rows_uses_full_screen() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 0 },
            (80, 24),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(0, 0, 80, 24));
    }
}
