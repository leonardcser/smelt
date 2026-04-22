use super::super::App;
use crate::app::ops::AppOp;
use crate::render::PermissionEntry;
use crate::workspace_permissions::Rule;
use crossterm::event::{KeyCode, KeyModifiers};
use std::cell::RefCell;
use std::rc::Rc;
use ui::{Callback, CallbackResult, KeyBind, WinEvent};

#[derive(Clone)]
enum Item {
    Session(usize),
    Workspace(usize, usize),
}

struct PermState {
    session_entries: Vec<PermissionEntry>,
    workspace_rules: Vec<Rule>,
    items: Vec<Item>,
    pending_d: bool,
    list_buf: ui::BufId,
}

pub(in crate::app) fn open(app: &mut App) {
    use crate::keymap::hints;

    let session_entries = app.session_permission_entries();
    let workspace_rules = crate::workspace_permissions::load(&app.cwd);
    let vim_enabled = app.input.vim_enabled();

    let items = build_items(&session_entries, &workspace_rules);
    let list_lines: Vec<String> = items
        .iter()
        .map(|item| format_label(&session_entries, &workspace_rules, item))
        .collect();

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["permissions".into(), String::new()]);
        buf.add_highlight(0, 0, 11, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(list_buf) {
        buf.set_all_lines(list_lines);
    }

    let hint_text = hints::join(&[hints::dd_delete(vim_enabled), hints::CLOSE]);
    let dialog_config = app.builtin_dialog_config(
        Some(hint_text),
        vec![(KeyCode::Char('q'), KeyModifiers::NONE)],
    );

    let Some(win_id) = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(2)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fill),
        ],
    ) else {
        return;
    };

    let state = Rc::new(RefCell::new(PermState {
        session_entries,
        workspace_rules,
        items,
        pending_d: false,
        list_buf,
    }));

    // `d` chord: first press arms pending_d, second press deletes. `dd`
    // mirrors vim line-delete.
    let state_d = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::char('d'),
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_d.borrow_mut();
            if s.pending_d {
                s.pending_d = false;
                let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
                if let Some(idx) = idx {
                    s.delete_at(idx);
                    s.refresh_list(ctx.ui);
                }
            } else {
                s.pending_d = true;
            }
            CallbackResult::Consumed
        })),
    );

    let state_bs = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Backspace),
        Callback::Rust(Box::new(move |ctx| {
            let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
            if let Some(idx) = idx {
                let mut s = state_bs.borrow_mut();
                s.delete_at(idx);
                s.refresh_list(ctx.ui);
            }
            CallbackResult::Consumed
        })),
    );

    let ops = app.lua.ops_handle();
    let state_dismiss = state.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            let s = state_dismiss.borrow();
            ops.push(AppOp::SyncPermissions {
                session_entries: s.session_entries.clone(),
                workspace_rules: s.workspace_rules.clone(),
            });
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );
}

impl PermState {
    fn delete_at(&mut self, idx: usize) {
        let Some(item) = self.items.get(idx).cloned() else {
            return;
        };
        match item {
            Item::Session(si) => {
                self.session_entries.remove(si);
            }
            Item::Workspace(ri, pi) => {
                let rule = &mut self.workspace_rules[ri];
                if rule.patterns.is_empty() || rule.patterns.len() == 1 {
                    self.workspace_rules.remove(ri);
                } else {
                    rule.patterns.remove(pi);
                }
            }
        }
        self.items = build_items(&self.session_entries, &self.workspace_rules);
    }

    fn refresh_list(&self, ui: &mut ui::Ui) {
        let lines: Vec<String> = self
            .items
            .iter()
            .map(|item| format_label(&self.session_entries, &self.workspace_rules, item))
            .collect();
        if let Some(buf) = ui.buf_mut(self.list_buf) {
            buf.set_all_lines(lines);
        }
    }
}

fn build_items(session_entries: &[PermissionEntry], workspace_rules: &[Rule]) -> Vec<Item> {
    let mut items = Vec::new();
    for i in 0..session_entries.len() {
        items.push(Item::Session(i));
    }
    for (ri, rule) in workspace_rules.iter().enumerate() {
        if rule.patterns.is_empty() {
            items.push(Item::Workspace(ri, 0));
        } else {
            for pi in 0..rule.patterns.len() {
                items.push(Item::Workspace(ri, pi));
            }
        }
    }
    items
}

fn format_label(
    session_entries: &[PermissionEntry],
    workspace_rules: &[Rule],
    item: &Item,
) -> String {
    match item {
        Item::Session(idx) => {
            let e = &session_entries[*idx];
            format!("{}: {}", e.tool, e.pattern)
        }
        Item::Workspace(ri, pi) => {
            let rule = &workspace_rules[*ri];
            if rule.patterns.is_empty() {
                format!("{}: *", rule.tool)
            } else {
                format!("{}: {}", rule.tool, rule.patterns[*pi])
            }
        }
    }
}
