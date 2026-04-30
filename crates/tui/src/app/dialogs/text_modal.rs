//! Simple read-only text dialog. One viewer Window inside a centered
//! Overlay; Esc / q / Ctrl+C dismiss.

use super::super::TuiApp;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::BufCreateOpts;
use ui::layout::Anchor;
use ui::{Border, Callback, CallbackResult, Constraint, KeyBind, LayoutTree, Overlay, SplitConfig};

/// Open a centered modal overlay showing `body`. Esc dismisses via
/// the `Ui` built-in; `q` and `Ctrl+C` dismiss via leaf-callbacks
/// registered on the viewer window.
pub fn open(app: &mut TuiApp, title: impl Into<String>, body: &str) {
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

    let dismiss: Callback = Callback::Rust(Box::new(|ctx| {
        if let Some(overlay_id) = ctx.ui.overlay_for_leaf(ctx.win) {
            let _ = ctx.ui.overlay_close(overlay_id);
            CallbackResult::Consumed
        } else {
            CallbackResult::Pass
        }
    }));
    let dismiss_ctrl_c: Callback = Callback::Rust(Box::new(|ctx| {
        if let Some(overlay_id) = ctx.ui.overlay_for_leaf(ctx.win) {
            let _ = ctx.ui.overlay_close(overlay_id);
            CallbackResult::Consumed
        } else {
            CallbackResult::Pass
        }
    }));
    let _ = app.ui.win_set_keymap(
        win,
        KeyBind::new(KeyCode::Char('q'), KeyModifiers::NONE),
        dismiss,
    );
    let _ = app.ui.win_set_keymap(
        win,
        KeyBind::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        dismiss_ctrl_c,
    );

    let layout = LayoutTree::vbox(vec![(
        Constraint::Percentage(60),
        LayoutTree::hbox(vec![(Constraint::Percentage(70), LayoutTree::leaf(win))]),
    )])
    .with_border(Border::Single)
    .with_title(title.into());

    app.ui
        .overlay_open(Overlay::new(layout, Anchor::ScreenCenter).modal(true));
}
