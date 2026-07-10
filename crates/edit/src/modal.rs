use super::{ContainerId, OverlayId, WinId};

/// Stable handle for a modal focus scope. Modality is independent of whether
/// its windows are mounted in the root layout or painted as an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModalId(pub u32);

/// Presentation that owns a modal focus scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalOwner {
    Docked(ContainerId),
    Overlay(OverlayId),
}

#[derive(Clone, Debug)]
pub(crate) struct Modal {
    pub leaves: Vec<WinId>,
    pub blocks_agent: bool,
    pub owner: ModalOwner,
}

impl Modal {
    pub fn contains(&self, win: WinId) -> bool {
        self.leaves.contains(&win)
    }
}
