use super::super::App;
use super::DialogState;
use crossterm::event::{KeyCode, KeyModifiers};

pub struct Ps {
    registry: engine::tools::ProcessRegistry,
    killed: Vec<String>,
    list_buf: ui::BufId,
}

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;

    let registry = app.engine.processes.clone();
    let procs = registry.list();

    let list_lines: Vec<String> = procs.iter().map(format_proc).collect();

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["processes".into(), String::new()]);
        buf.add_highlight(0, 0, 9, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(list_lines);
    }

    let hint_text = hints::join(&[hints::CLOSE, hints::KILL_PROC]);
    let dialog_config = app.builtin_dialog_config(
        Some(hint_text),
        vec![(KeyCode::Char('q'), KeyModifiers::NONE)],
    );

    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(50)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fill),
        ],
    );

    if let Some(win_id) = win_id {
        app.float_states.insert(
            win_id,
            Box::new(Ps {
                registry,
                killed: Vec::new(),
                list_buf,
            }),
        );
    }
}

impl DialogState for Ps {
    fn handle_key(
        &mut self,
        app: &mut App,
        win: ui::WinId,
        code: KeyCode,
        _mods: KeyModifiers,
    ) -> Option<ui::KeyResult> {
        if code == KeyCode::Backspace {
            let idx = app.ui.dialog_mut(win).and_then(|d| d.selected_index());
            if let Some(idx) = idx {
                let procs: Vec<_> = self
                    .registry
                    .list()
                    .into_iter()
                    .filter(|p| !self.killed.contains(&p.id))
                    .collect();
                if let Some(p) = procs.get(idx) {
                    self.killed.push(p.id.clone());
                    let fresh: Vec<_> = self
                        .registry
                        .list()
                        .into_iter()
                        .filter(|p| !self.killed.contains(&p.id))
                        .collect();
                    let lines: Vec<String> = fresh.iter().map(format_proc).collect();
                    if let Some(buf) = app.ui.buf_mut(self.list_buf) {
                        buf.set_all_lines(lines);
                    }
                }
            }
            return Some(ui::KeyResult::Consumed);
        }
        None
    }
}

fn format_proc(p: &engine::tools::ProcessInfo) -> String {
    let time = crate::utils::format_duration(p.started_at.elapsed().as_secs());
    format!("{} — {time} {}", p.command, p.id)
}
