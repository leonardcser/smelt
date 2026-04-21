use super::super::App;
use super::{DialogState, TurnState};

pub struct Rewind {
    turns: Vec<(usize, String)>,
    restore_vim_insert: bool,
}

pub(in crate::app) fn open(app: &mut App, turns: Vec<(usize, String)>, restore_vim_insert: bool) {
    use crate::keymap::hints;

    let total = turns.len() + 1;
    let mut lines: Vec<String> = turns
        .iter()
        .enumerate()
        .map(|(i, (_, label))| {
            let line = label.lines().next().unwrap_or("");
            format!("{}. {line}", i + 1)
        })
        .collect();
    lines.push(format!("{}. (current)", total));

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["rewind".into(), String::new()]);
        buf.add_highlight(0, 0, 6, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(lines);
    }

    let hint_text = hints::join(&[hints::SELECT, hints::CANCEL]);
    let footer_h = (total as u16).min(10);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Fixed(footer_h + 4)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fit),
        ],
    );

    if let Some(win_id) = win_id {
        app.float_states.insert(
            win_id,
            Box::new(Rewind {
                turns,
                restore_vim_insert,
            }),
        );
    }
}

impl DialogState for Rewind {
    fn on_select(
        &mut self,
        app: &mut App,
        _win: ui::WinId,
        idx: usize,
        agent: &mut Option<TurnState>,
    ) {
        let block_idx = if idx < self.turns.len() {
            Some(self.turns[idx].0)
        } else {
            None
        };
        if let Some(bidx) = block_idx {
            if agent.is_some() {
                app.cancel_agent();
                *agent = None;
            }
            if let Some((text, images)) = app.rewind_to(bidx) {
                app.input.restore_from_rewind(text, images);
            }
            while app.engine.try_recv().is_ok() {}
            app.save_session();
        } else if self.restore_vim_insert {
            app.input.set_vim_mode(crate::vim::ViMode::Insert);
        }
    }
}
