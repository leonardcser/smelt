//! `Overlay` — the P1.c replacement for `Float` / `FloatConfig`.
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

use crate::layout::{Anchor, LayoutTree};

/// Stable handle for an overlay. Distinct from `WinId` so chrome
/// hit-testing (`HitTarget::Chrome { owner: OverlayId }`) doesn't
/// collide with content hit-testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OverlayId(pub u32);

#[derive(Clone, Debug)]
pub struct Overlay {
    pub layout: LayoutTree,
    pub anchor: Anchor,
    /// Stacking order. Higher draws on top. Same `z` falls back to
    /// insertion order.
    pub z: u16,
    /// When `true`, the host pauses engine-event drain while focus
    /// is inside this overlay (permission prompts, lua dialogs
    /// gating a parked task). When `false`, the overlay coexists
    /// with a running turn (read-only viewers, completers).
    pub modal: bool,
}

impl Overlay {
    pub fn new(layout: LayoutTree, anchor: Anchor) -> Self {
        Self {
            layout,
            anchor,
            z: 50,
            modal: false,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Anchor, Constraint, Corner};
    use crate::WinId;

    #[test]
    fn overlay_defaults_are_sensible() {
        let layout =
            LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(layout, Anchor::ScreenCenter);
        assert_eq!(ov.z, 50);
        assert!(!ov.modal);
        assert_eq!(ov.anchor, Anchor::ScreenCenter);
    }

    #[test]
    fn overlay_builders_compose() {
        let layout =
            LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(
            layout,
            Anchor::Win {
                target: WinId(7),
                attach: Corner::NW,
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
}
