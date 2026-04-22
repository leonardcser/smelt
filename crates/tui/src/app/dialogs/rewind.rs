use super::super::App;
use crate::app::ops::AppOp;
use ui::{Callback, CallbackResult, Payload, WinEvent};

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

    let Some(win_id) = app.ui.dialog_open(
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
    ) else {
        return;
    };

    let ops = app.lua.ops_handle();
    let ops_submit = ops.clone();
    let turn_blocks: Vec<usize> = turns.iter().map(|(idx, _)| *idx).collect();
    let turns_len = turns.len();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            if let Payload::Selection { index } = ctx.payload {
                let block_idx = if index < turns_len {
                    turn_blocks.get(index).copied()
                } else {
                    None
                };
                ops_submit.push(AppOp::RewindToBlock {
                    block_idx,
                    restore_vim_insert,
                });
            }
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
}
