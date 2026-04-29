//! Panel descriptor types consumed by `tui::lua::ui_ops` when it
//! translates a `smelt.ui.dialog.open(opts)` call into an `Overlay`
//! containing one buffer-backed `Window` per panel. There is no
//! `Dialog` widget any more — every panel is a real `ui::Window` over
//! a `Buffer`, composed via `LayoutTree` + `Overlay`.

use crate::id::BufId;

/// How tall a panel wants to be inside the overlay's vbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHeight {
    /// Exact row count.
    Fixed(u16),
    /// Shrink to content (capped by remaining space).
    Fit,
    /// Consume whatever remains after Fixed/Fit panels are allocated.
    Fill,
}

/// Description of one panel in a `smelt.ui.dialog.open` call. The
/// translator (`tui::lua::ui_ops::open_dialog_via_overlay`) opens a
/// buffer-backed `Window` per spec and slots it into the overlay's
/// `LayoutTree` with the resolved height constraint.
pub struct PanelSpec {
    pub buf: BufId,
    pub height: PanelHeight,
    /// Whether this panel participates in focus cycling. Title /
    /// summary panels usually don't.
    pub focusable: bool,
    /// Take initial focus on overlay open. When no panel opts in, the
    /// translator focuses the first focusable leaf.
    pub focus_initial: bool,
    /// Hide the panel (zero-row leaf) when its buffer has no
    /// non-blank content. Mirrors the legacy panel-collapse rule so
    /// dialogs ride a hidden summary / preview row without the leaf
    /// taking up space.
    pub collapse_when_empty: bool,
    /// Buffer panels only: route mouse + nav keys through the panel's
    /// `Window` so the user gets transcript-grade interaction
    /// (click-to-position, double/triple-click word/line select,
    /// drag-extend, vim Visual modes, theme selection bg).
    pub interactive: bool,
}

impl PanelSpec {
    /// Buffer-backed read-only content (preview, header, body text).
    /// Defaults to non-focusable; flip with [`PanelSpec::focusable`]
    /// or [`PanelSpec::interactive`].
    pub fn content(buf: BufId, height: PanelHeight) -> Self {
        Self {
            buf,
            height,
            focusable: false,
            focus_initial: false,
            collapse_when_empty: false,
            interactive: false,
        }
    }

    /// Buffer panel that behaves like the transcript pane: focusable,
    /// click-to-position cursor, double/triple click word/line select,
    /// drag-extend with theme selection background, vim Visual modes
    /// when the host has vim enabled.
    pub fn interactive_content(buf: BufId, height: PanelHeight) -> Self {
        Self {
            buf,
            height,
            focusable: true,
            focus_initial: false,
            collapse_when_empty: false,
            interactive: true,
        }
    }

    pub fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }

    pub fn with_initial_focus(mut self, focus: bool) -> Self {
        self.focus_initial = focus;
        self
    }
}
