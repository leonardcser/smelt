pub(crate) use crate::smelt_term::Rect;

/// Rows reserved for the bottom statusline in the main splits layout. Overlays
/// docked at the bottom (`Anchor::ScreenBottom { above_rows: STATUSLINE_ROWS }`)
/// use this to avoid covering it.
pub(crate) const STATUSLINE_ROWS: u16 = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutState {
    pub(crate) transcript: Rect,
    pub(crate) prompt_above: Rect,
    pub(crate) prompt: Rect,
    pub(crate) prompt_below: Rect,
    pub(crate) status: Rect,
}

#[derive(Debug)]
pub(crate) struct LayoutInput {
    pub(crate) term_height: u16,
    /// Chrome rows above input (queued + stash + top bar); always `>= 1`.
    pub(crate) prompt_above_rows: u16,
    /// Input rows before clamping; always `>= 1`.
    pub(crate) prompt_input_rows: u16,
}

/// Build the splits tree for the main TUI layout.
pub(crate) fn build_layout_tree(
    input: &LayoutInput,
    status_win: crate::smelt_term::WinId,
) -> crate::smelt_term::LayoutTree {
    let LayoutInput {
        term_height,
        prompt_above_rows,
        prompt_input_rows,
    } = *input;

    let above = prompt_above_rows.max(1);
    let below = 1u16;
    let chrome = above + below + STATUSLINE_ROWS;
    // Cap prompt block at half the terminal so the transcript always has room.
    let max_block = (term_height / 2).max(chrome + 1);
    let input_rows = prompt_input_rows
        .max(1)
        .min(max_block.saturating_sub(chrome))
        .max(1);
    let total = chrome + input_rows;

    crate::smelt_term::LayoutTree::vbox(vec![
        (
            crate::smelt_term::Constraint::Fill,
            crate::smelt_term::LayoutTree::leaf(crate::app::TRANSCRIPT_WIN),
        ),
        (
            crate::smelt_term::Constraint::Length(total),
            crate::smelt_term::LayoutTree::vbox(vec![
                (
                    crate::smelt_term::Constraint::Length(above),
                    crate::smelt_term::LayoutTree::leaf(crate::app::PROMPT_ABOVE_WIN),
                ),
                (
                    crate::smelt_term::Constraint::Length(input_rows),
                    crate::smelt_term::LayoutTree::leaf(crate::app::PROMPT_WIN),
                ),
                (
                    crate::smelt_term::Constraint::Length(below),
                    crate::smelt_term::LayoutTree::leaf(crate::app::PROMPT_BELOW_WIN),
                ),
                (
                    crate::smelt_term::Constraint::Length(STATUSLINE_ROWS),
                    crate::smelt_term::LayoutTree::leaf(status_win),
                ),
            ]),
        ),
    ])
    .with_gap(1)
}

impl LayoutState {
    pub(crate) fn from_ui(
        ui: &crate::smelt_term::Ui,
        status_win: crate::smelt_term::WinId,
    ) -> Self {
        Self {
            transcript: ui
                .split_rect(crate::app::TRANSCRIPT_WIN)
                .unwrap_or_default(),
            prompt_above: ui
                .split_rect(crate::app::PROMPT_ABOVE_WIN)
                .unwrap_or_default(),
            prompt: ui.split_rect(crate::app::PROMPT_WIN).unwrap_or_default(),
            prompt_below: ui
                .split_rect(crate::app::PROMPT_BELOW_WIN)
                .unwrap_or_default(),
            status: ui.split_rect(status_win).unwrap_or_default(),
        }
    }

    pub(crate) fn viewport_rows(&self) -> u16 {
        self.transcript.height
    }

    /// Rect spanning the whole prompt block; used by mouse-click routing.
    pub(crate) fn prompt_block(&self) -> Rect {
        let top = self.prompt_above.top;
        let bottom = self.prompt_below.bottom();
        Rect::new(
            top,
            self.prompt.left,
            self.prompt.width,
            bottom.saturating_sub(top),
        )
    }

    pub(crate) fn hit_test(&self, row: u16, col: u16) -> HitRegion {
        if self.status.height > 0 && self.status.contains(row, col) {
            return HitRegion::Status;
        }
        if self.prompt_block().contains(row, col) {
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
    Status,
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_term::{Gutters, WinId};

    fn open_split(ui: &mut crate::smelt_term::Ui, win: WinId, region: &str, gutters: Gutters) {
        let buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win,
            buf,
            crate::smelt_term::SplitConfig {
                region: region.into(),
                gutters,
            },
        ));
    }

    fn set_up_layout(
        prompt_above_rows: u16,
        prompt_input_rows: u16,
        term_width: u16,
        term_height: u16,
    ) -> (crate::smelt_term::Ui, WinId) {
        let mut ui = crate::smelt_term::Ui::new();
        ui.set_terminal_size(term_width, term_height);
        open_split(
            &mut ui,
            crate::app::TRANSCRIPT_WIN,
            "transcript",
            Gutters::default(),
        );
        open_split(
            &mut ui,
            crate::app::PROMPT_ABOVE_WIN,
            "prompt_above",
            Gutters {
                scrollbar: false,
                ..Default::default()
            },
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
        open_split(
            &mut ui,
            crate::app::PROMPT_BELOW_WIN,
            "prompt_below",
            Gutters {
                scrollbar: false,
                ..Default::default()
            },
        );
        let status_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
        let status_win = ui
            .win_open_split(
                status_buf,
                crate::smelt_term::SplitConfig {
                    region: "status".into(),
                    gutters: Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let tree = build_layout_tree(
            &LayoutInput {
                term_height,
                prompt_above_rows,
                prompt_input_rows,
            },
            status_win,
        );
        ui.set_layout(tree);
        (ui, status_win)
    }

    #[test]
    fn prompt_rect_width_equals_terminal_width() {
        let (ui, status_win) = set_up_layout(1, 1, 80, 40);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert_eq!(
            layout.prompt.width, 80,
            "prompt rect spans full terminal width"
        );
    }

    #[test]
    fn normal_layout_splits_term() {
        let (ui, status_win) = set_up_layout(1, 1, 80, 40);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert_eq!(layout.transcript.top, 0);
        assert_eq!(layout.transcript.height, 35);
        assert_eq!(layout.prompt_above.top, 36);
        assert_eq!(layout.prompt_above.height, 1);
        assert_eq!(layout.prompt.top, 37);
        assert_eq!(layout.prompt.height, 1);
        assert_eq!(layout.prompt_below.top, 38);
        assert_eq!(layout.prompt_below.height, 1);
        assert_eq!(layout.status.top, 39);
        assert_eq!(layout.status.height, 1);
    }

    #[test]
    fn prompt_capped_at_half_height() {
        let (ui, status_win) = set_up_layout(1, 15, 80, 20);
        let layout = LayoutState::from_ui(&ui, status_win);
        let block = layout.prompt_above.height
            + layout.prompt.height
            + layout.prompt_below.height
            + layout.status.height;
        assert!(block <= 10);
    }

    #[test]
    fn hit_test_routes_correctly() {
        let (ui, status_win) = set_up_layout(1, 1, 80, 40);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(34, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(35, 0), HitRegion::Outside);
        assert_eq!(layout.hit_test(36, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(38, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(39, 0), HitRegion::Status);
    }

    #[test]
    fn tiny_terminal_still_works() {
        let (ui, status_win) = set_up_layout(1, 10, 40, 3);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert!(layout.transcript.height <= 3);
        assert!(layout.prompt.height <= 3);
        assert!(layout.status.height <= 1);
    }

    #[test]
    fn build_layout_tree_lists_all_leaves_in_order() {
        use crate::smelt_term::PaintId;
        let status = WinId(99);
        let tree = build_layout_tree(
            &LayoutInput {
                term_height: 40,
                prompt_above_rows: 1,
                prompt_input_rows: 1,
            },
            status,
        );
        let tree_leaves: Vec<PaintId> = tree.leaves_in_order();
        assert_eq!(
            tree_leaves,
            vec![
                PaintId::from(crate::app::TRANSCRIPT_WIN),
                PaintId::from(crate::app::PROMPT_ABOVE_WIN),
                PaintId::from(crate::app::PROMPT_WIN),
                PaintId::from(crate::app::PROMPT_BELOW_WIN),
                PaintId::from(status),
            ]
        );
    }
}
