use super::super::App;
use super::{DialogState, TurnState};

pub struct Export;

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["export".into(), String::new()]);
        buf.add_highlight(0, 0, 6, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(vec![
            "1. Copy to clipboard".into(),
            "2. Write to file".into(),
        ]);
    }

    let hint_text = hints::join(&[hints::SELECT, hints::CANCEL]);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Fixed(8)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fit),
        ],
    );

    if let Some(win_id) = win_id {
        app.float_states.insert(win_id, Box::new(Export));
    }
}

impl DialogState for Export {
    fn on_select(
        &mut self,
        app: &mut App,
        _win: ui::WinId,
        idx: usize,
        _agent: &mut Option<TurnState>,
    ) {
        match idx {
            0 => app.export_to_clipboard(),
            1 => app.export_to_file(),
            _ => {}
        }
    }
}
