use super::{OverlayId, WinId};

/// Stable handle for a modal focus scope. Modality is independent of whether
/// its windows are mounted in the root layout or painted as an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModalId(pub u32);

#[derive(Clone, Debug)]
pub(crate) struct Modal {
    pub leaves: Vec<WinId>,
    pub blocks_agent: bool,
    pub overlay: Option<OverlayId>,
}

impl Modal {
    pub fn contains(&self, win: WinId) -> bool {
        self.leaves.contains(&win)
    }
}
