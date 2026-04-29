//! Simple read-only text dialog. One viewer Window inside a centered
//! Overlay; Esc dismisses.

use super::super::App;
use ui::buffer::BufCreateOpts;
use ui::layout::Anchor;
use ui::{Border, Constraint, LayoutTree, Overlay, SplitConfig};

/// Open a centered modal overlay showing `body`. Esc dismisses
/// (handled by the modal-Esc-dismiss built-in in `Ui::handle_key`).
pub fn open(app: &mut App, title: impl Into<String>, body: &str) {
    let buf = app.ui.buf_create(BufCreateOpts::default());
    if let Some(b) = app.ui.buf_mut(buf) {
        let lines: Vec<String> = if body.is_empty() {
            vec![String::new()]
        } else {
            body.lines().map(|s| s.to_string()).collect()
        };
        b.set_all_lines(lines);
    }

    let Some(win) = app.ui.win_open_split(
        buf,
        SplitConfig {
            region: "text_modal".into(),
            gutters: Default::default(),
        },
    ) else {
        return;
    };

    let layout = LayoutTree::vbox(vec![(
        Constraint::Percentage(60),
        LayoutTree::hbox(vec![(Constraint::Percentage(70), LayoutTree::leaf(win))]),
    )])
    .with_border(Border::Single)
    .with_title(title.into());

    app.ui
        .overlay_open(Overlay::new(layout, Anchor::ScreenCenter).modal(true));
}
