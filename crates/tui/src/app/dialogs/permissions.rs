use super::super::App;
use super::DialogState;
use crate::render::PermissionEntry;
use crate::workspace_permissions::Rule;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone)]
enum Item {
    Session(usize),
    Workspace(usize, usize),
}

pub struct Permissions {
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

    let win_id = app.ui.dialog_open(
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
    );

    if let Some(win_id) = win_id {
        app.float_states.insert(
            win_id,
            Box::new(Permissions {
                session_entries,
                workspace_rules,
                items,
                pending_d: false,
                list_buf,
            }),
        );
    }
}

impl DialogState for Permissions {
    fn handle_key(
        &mut self,
        app: &mut App,
        win: ui::WinId,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Option<ui::KeyResult> {
        if self.pending_d {
            self.pending_d = false;
            if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                if let Some(idx) = app.ui.dialog_mut(win).and_then(|d| d.selected_index()) {
                    self.delete_at(idx);
                    self.refresh_list(app);
                }
                return Some(ui::KeyResult::Consumed);
            }
        }
        if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
            self.pending_d = true;
            return Some(ui::KeyResult::Consumed);
        }
        if code == KeyCode::Backspace {
            if let Some(idx) = app.ui.dialog_mut(win).and_then(|d| d.selected_index()) {
                self.delete_at(idx);
                self.refresh_list(app);
            }
            return Some(ui::KeyResult::Consumed);
        }
        self.pending_d = false;
        None
    }

    fn on_dismiss(&mut self, app: &mut App, _win: ui::WinId) {
        app.sync_permissions(self.session_entries.clone(), self.workspace_rules.clone());
    }
}

impl Permissions {
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

    fn refresh_list(&self, app: &mut App) {
        let lines: Vec<String> = self
            .items
            .iter()
            .map(|item| format_label(&self.session_entries, &self.workspace_rules, item))
            .collect();
        if let Some(buf) = app.ui.buf_mut(self.list_buf) {
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
