pub(crate) use ui::Rect;

#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutState {
    pub(crate) transcript: Rect,
    pub(crate) prompt: Rect,
    pub(crate) status: Rect,
}

#[derive(Debug)]
pub(crate) struct LayoutInput {
    pub(crate) term_height: u16,
    /// Natural height claimed by the prompt area, including the row
    /// reserved for the status line. The tree splits this between a
    /// prompt leaf (`prompt_height - 1` rows) and a status leaf
    /// (1 row) sharing an inner vbox.
    pub(crate) prompt_height: u16,
}

/// Build the splits tree for the main TUI layout. Outer vbox:
/// transcript fills the top, with a 1-row gap, then an inner vbox
/// holding the prompt and status leaves stacked tightly. The host
/// publishes this tree to `Ui` via `Ui::set_layout` once per frame;
/// `Ui` resolves rects against the current terminal area.
pub(crate) fn build_layout_tree(input: &LayoutInput, status_win: ui::WinId) -> ui::LayoutTree {
    let LayoutInput {
        term_height,
        prompt_height,
    } = *input;

    let max_prompt = (term_height / 2).max(3);
    let prompt_height = prompt_height.min(max_prompt).max(2);
    let prompt_leaf_height = prompt_height.saturating_sub(1).max(1);

    ui::LayoutTree::vbox(vec![
        (
            ui::Constraint::Fill,
            ui::LayoutTree::leaf(ui::TRANSCRIPT_WIN),
        ),
        (
            ui::Constraint::Length(prompt_height),
            ui::LayoutTree::vbox(vec![
                (
                    ui::Constraint::Length(prompt_leaf_height),
                    ui::LayoutTree::leaf(ui::PROMPT_WIN),
                ),
                (ui::Constraint::Length(1), ui::LayoutTree::leaf(status_win)),
            ]),
        ),
    ])
    .with_gap(1)
}

impl LayoutState {
    /// Read the resolved rects out of `ui` after the host published
    /// the splits tree via `Ui::set_layout`. Missing leaves fall back
    /// to `Rect::default` (zero-sized) — practically only happens
    /// before the first frame.
    pub(crate) fn from_ui(ui: &ui::Ui, status_win: ui::WinId) -> Self {
        Self {
            transcript: ui.split_rect(ui::TRANSCRIPT_WIN).unwrap_or_default(),
            prompt: ui.split_rect(ui::PROMPT_WIN).unwrap_or_default(),
            status: ui.split_rect(status_win).unwrap_or_default(),
        }
    }

    pub(crate) fn viewport_rows(&self) -> u16 {
        self.transcript.height
    }

    pub(crate) fn hit_test(&self, row: u16, col: u16) -> HitRegion {
        if self.status.height > 0 && self.status.contains(row, col) {
            return HitRegion::Status;
        }
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
    Status,
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::{Constraint, LayoutTree, WinId};

    fn open_split(ui: &mut ui::Ui, win: WinId, region: &str) {
        let buf = ui.buf_create(ui::buffer::BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win,
            buf,
            ui::SplitConfig {
                region: region.into(),
                gutters: ui::Gutters::default(),
            },
        ));
    }

    fn set_up_layout(prompt_height: u16, term_width: u16, term_height: u16) -> (ui::Ui, WinId) {
        let mut ui = ui::Ui::new();
        ui.set_terminal_size(term_width, term_height);
        open_split(&mut ui, ui::TRANSCRIPT_WIN, "transcript");
        open_split(&mut ui, ui::PROMPT_WIN, "prompt");
        let status_buf = ui.buf_create(ui::buffer::BufCreateOpts::default());
        let status_win = ui
            .win_open_split(
                status_buf,
                ui::SplitConfig {
                    region: "status".into(),
                    gutters: ui::Gutters::default(),
                },
            )
            .unwrap();
        let tree = build_layout_tree(
            &LayoutInput {
                term_height,
                prompt_height,
            },
            status_win,
        );
        ui.set_layout(tree);
        (ui, status_win)
    }

    #[test]
    fn normal_layout_splits_term() {
        let (ui, status_win) = set_up_layout(5, 80, 40);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert_eq!(layout.transcript.top, 0);
        assert_eq!(layout.transcript.height, 34);
        assert_eq!(layout.prompt.top, 35);
        assert_eq!(layout.prompt.height, 4);
        assert_eq!(layout.status.top, 39);
        assert_eq!(layout.status.height, 1);
    }

    #[test]
    fn prompt_capped_at_half_height() {
        let (ui, status_win) = set_up_layout(15, 80, 20);
        let layout = LayoutState::from_ui(&ui, status_win);
        // Capped at term_h / 2 = 10 → prompt leaf 9, status 1.
        assert_eq!(layout.prompt.height, 9);
        assert_eq!(layout.status.height, 1);
        assert_eq!(layout.transcript.height, 9);
    }

    #[test]
    fn hit_test_routes_correctly() {
        let (ui, status_win) = set_up_layout(5, 80, 40);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert_eq!(layout.hit_test(0, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(33, 0), HitRegion::Transcript);
        assert_eq!(layout.hit_test(34, 0), HitRegion::Outside);
        assert_eq!(layout.hit_test(35, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(38, 0), HitRegion::Prompt);
        assert_eq!(layout.hit_test(39, 0), HitRegion::Status);
    }

    #[test]
    fn tiny_terminal_still_works() {
        let (ui, status_win) = set_up_layout(10, 40, 3);
        let layout = LayoutState::from_ui(&ui, status_win);
        assert!(layout.transcript.height <= 3);
        assert!(layout.prompt.height <= 3);
        assert!(layout.status.height <= 1);
    }

    #[test]
    fn build_layout_tree_includes_status_as_leaf() {
        let mut tree_leaves: Vec<WinId> = LayoutTree::vbox(Vec::new()).leaves_in_order();
        assert!(tree_leaves.is_empty());
        let status = WinId(99);
        let tree = build_layout_tree(
            &LayoutInput {
                term_height: 40,
                prompt_height: 5,
            },
            status,
        );
        tree_leaves = tree.leaves_in_order();
        assert_eq!(
            tree_leaves,
            vec![ui::TRANSCRIPT_WIN, ui::PROMPT_WIN, status]
        );
        let _ = Constraint::Fill;
    }
}
