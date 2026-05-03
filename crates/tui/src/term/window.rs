#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowGutters {
    pub(crate) pad_left: u16,
    pub(crate) scrollbar: bool,
}

pub(crate) const TRANSCRIPT_GUTTERS: WindowGutters = WindowGutters {
    pad_left: 1,
    scrollbar: true,
};

impl WindowGutters {
    pub(crate) fn scrollbar_width(&self) -> u16 {
        if self.scrollbar {
            1
        } else {
            0
        }
    }

    pub(crate) fn layer_width(&self, term_width: u16) -> u16 {
        term_width.saturating_sub(self.pad_left)
    }

    pub(crate) fn content_width(&self, term_width: u16) -> u16 {
        self.layer_width(term_width)
            .saturating_sub(self.scrollbar_width())
    }
}
