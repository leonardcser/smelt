pub(crate) use crate::smelt_edit::Rect;

use crate::smelt_edit::layout::Anchor;
use crate::smelt_edit::{Align, Ui};

/// Resolve the window that represents the top edge of the prompt chrome
/// stack. Used by overlays that want to float above the prompt block.
///
/// The resolved target is the Lua-allocated top bar window when the
/// default `prompt_bar.lua` is loaded, otherwise the engine-owned
/// `PROMPT_WIN`. The host knows the *name* of the default top bar but
/// not its layout shape: a plugin that replaces the bar with a wider
/// or differently-named window will fall through to the `PROMPT_WIN`
/// branch and the picker will sit immediately above the input row.
///
/// Screen-bottom overlays that need to clear the Lua-owned statusline still
/// anchor against `require("smelt.statusline").win` directly from Lua. The
/// render loop only resolves that named window in its emergency fallback layout.
pub(crate) fn prompt_chrome_top_win(ui: &Ui) -> crate::smelt_edit::WinId {
    ui.named_win("smelt.prompt_bar.top")
        .unwrap_or(crate::app::PROMPT_WIN)
}

/// Rows between the top of the screen and the top edge of the prompt
/// chrome stack. Prompt-docked pickers use this to avoid requesting
/// more rows than fit without overlapping the prompt block.
pub(crate) fn available_rows_above_prompt_chrome(ui: &Ui) -> u16 {
    let target = prompt_chrome_top_win(ui);
    ui.split_rect(target).map(|r| r.top).unwrap_or(0)
}

/// Anchor that floats an overlay `height` rows above the prompt block.
pub(crate) fn anchor_above_prompt_chrome(ui: &Ui, height: u16) -> Anchor {
    let target = prompt_chrome_top_win(ui);
    Anchor::Win {
        target: target.into(),
        attach: Align::NW,
        row_offset: -(height as i32),
        col_offset: 0,
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutState {
    pub(crate) transcript: Rect,
    pub(crate) prompt: Rect,
}

/// Minimal splits tree used before the Lua composer runs. The Lua layer
/// (`runtime/lua/smelt/layout.lua`) replaces it on the first composed frame and
/// retains that result until dimensions or semantic layout state change. Keeping
/// a seed tree means anchored overlays resolve correctly during the brief window
/// between `Ui::new` and the first Lua-driven render.
pub(crate) fn seed_layout_tree(prompt_input_rows: u16) -> crate::smelt_edit::LayoutTree {
    let rows = prompt_input_rows.max(1);
    crate::smelt_edit::LayoutTree::vbox(vec![
        (
            crate::smelt_edit::Constraint::Fill,
            crate::smelt_edit::LayoutTree::leaf(crate::app::TRANSCRIPT_WIN),
        ),
        (
            crate::smelt_edit::Constraint::Length(rows),
            crate::smelt_edit::LayoutTree::leaf(crate::app::PROMPT_WIN),
        ),
    ])
    .with_gap(1)
}

impl LayoutState {
    pub(crate) fn from_ui(ui: &crate::smelt_edit::Ui) -> Self {
        Self {
            transcript: ui
                .split_rect(crate::app::TRANSCRIPT_WIN)
                .unwrap_or_default(),
            prompt: ui.split_rect(crate::app::PROMPT_WIN).unwrap_or_default(),
        }
    }

    pub(crate) fn viewport_rows(&self) -> u16 {
        self.transcript.height
    }

    pub(crate) fn hit_test(&self, row: u16, col: u16) -> HitRegion {
        if self.prompt.contains(row, col) {
            return HitRegion::Prompt;
        }
        if self.transcript.contains(row, col) {
            return HitRegion::Transcript;
        }
        HitRegion::Outside
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HitRegion {
    Transcript,
    Prompt,
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{Gutters, WinId};

    fn open_split(ui: &mut crate::smelt_edit::Ui, win: WinId, region: &str, gutters: Gutters) {
        let buf = ui.buf_create(crate::smelt_edit::BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win,
            buf,
            crate::smelt_edit::SplitConfig {
                region: region.into(),
                gutters,
            },
        ));
    }

    fn set_up_layout(
        prompt_input_rows: u16,
        term_width: u16,
        term_height: u16,
    ) -> crate::smelt_edit::Ui {
        let mut ui = crate::smelt_edit::Ui::new();
        ui.set_terminal_size(term_width, term_height);
        open_split(
            &mut ui,
            crate::app::TRANSCRIPT_WIN,
            "transcript",
            Gutters::default(),
        );
        open_split(
            &mut ui,
            crate::app::PROMPT_WIN,
            "prompt",
            Gutters {
                pad_left: 1,
                pad_right: 1,
                ..Default::default()
            },
        );
        ui.set_layout(seed_layout_tree(prompt_input_rows));
        ui
    }

    #[test]
    fn prompt_rect_width_equals_terminal_width() {
        let ui = set_up_layout(1, 80, 40);
        let layout = LayoutState::from_ui(&ui);
        assert_eq!(layout.prompt.width, 80);
    }

    #[test]
    fn seed_layout_splits_term() {
        let ui = set_up_layout(1, 80, 40);
        let layout = LayoutState::from_ui(&ui);
        assert_eq!(layout.transcript.top, 0);
        assert_eq!(layout.transcript.height, 38);
        assert_eq!(layout.prompt.top, 39);
        assert_eq!(layout.prompt.height, 1);
    }

    #[test]
    fn hit_test_routes_correctly() {
        let ui = set_up_layout(1, 80, 40);
        let layout = LayoutState::from_ui(&ui);
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(37, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(38, 0), HitRegion::Outside);
        assert_eq!(layout.hit_test(39, 0), HitRegion::Prompt);
    }

    #[test]
    fn seed_layout_lists_leaves_in_order() {
        use crate::smelt_edit::PaintId;
        let tree = seed_layout_tree(1);
        let leaves: Vec<PaintId> = tree.leaves_in_order();
        assert_eq!(
            leaves,
            vec![
                PaintId::from(crate::app::TRANSCRIPT_WIN),
                PaintId::from(crate::app::PROMPT_WIN),
            ]
        );
    }
}
