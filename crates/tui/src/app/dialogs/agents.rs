use super::super::App;
use super::DialogState;
use crate::render::{AgentSnapshot, SharedSnapshots};
use crate::utils::format_duration;
use crossterm::event::{KeyCode, KeyModifiers};
use engine::registry::{AgentStatus, RegistryEntry};

pub struct AgentsList {
    my_pid: u32,
    snapshots: SharedSnapshots,
    list_buf: ui::BufId,
    agents: Vec<RegistryEntry>,
}

pub struct AgentsDetail {
    my_pid: u32,
    agent_id: String,
    snapshots: SharedSnapshots,
    title_buf: ui::BufId,
    detail_buf: ui::BufId,
    parent_selected: usize,
}

pub(in crate::app) fn open(app: &mut App) {
    open_list(app, 0);
}

fn open_list(app: &mut App, initial_selected: usize) {
    use crate::keymap::hints;
    use crossterm::event::KeyCode;

    let my_pid = std::process::id();
    let snapshots = app.agent_snapshots.clone();
    let agents = engine::registry::children_of(my_pid);

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
        buf.set_all_lines(vec!["agents".into(), String::new()]);
        buf.add_highlight(0, 0, 6, ui::buffer::SpanStyle::dim());
    }

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    refresh_list(&mut app.ui, list_buf, &agents, &snapshots);

    let hint_text = hints::join(&["enter: view", hints::KILL_PROC, hints::CLOSE]);
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
        if initial_selected > 0 {
            if let Some(dialog) = app.ui.dialog_mut(win_id) {
                dialog.set_selected_index(initial_selected);
            }
        }
        app.float_states.insert(
            win_id,
            Box::new(AgentsList {
                my_pid,
                snapshots,
                list_buf,
                agents,
            }),
        );
    }
}

fn open_detail(app: &mut App, agent_id: String, parent_selected: usize) {
    use crate::keymap::hints;

    let my_pid = std::process::id();
    let snapshots = app.agent_snapshots.clone();

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    refresh_detail_title(&mut app.ui, title_buf, &agent_id, my_pid, &snapshots);

    let detail_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    refresh_detail_body(&mut app.ui, detail_buf, &agent_id, &snapshots);

    let hint_text = hints::join(&[hints::BACK, hints::scroll(app.input.vim_enabled())]);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

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
            ui::PanelSpec::content(detail_buf, ui::PanelHeight::Fill)
                .with_pad_left(2)
                .focusable(true),
        ],
    );

    if let Some(win_id) = win_id {
        app.float_states.insert(
            win_id,
            Box::new(AgentsDetail {
                my_pid,
                agent_id,
                snapshots,
                title_buf,
                detail_buf,
                parent_selected,
            }),
        );
    }
}

impl DialogState for AgentsList {
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
                if let Some(agent) = self.agents.get(idx) {
                    let pid = agent.pid;
                    if engine::registry::is_in_tree(pid, self.my_pid) {
                        engine::registry::kill_agent(pid);
                        self.agents = engine::registry::children_of(self.my_pid);
                        refresh_list(&mut app.ui, self.list_buf, &self.agents, &self.snapshots);
                    }
                }
            }
            return Some(ui::KeyResult::Consumed);
        }
        None
    }

    fn on_select(
        &mut self,
        app: &mut App,
        win: ui::WinId,
        idx: usize,
        _agent: &mut Option<super::TurnState>,
    ) {
        let Some(entry) = self.agents.get(idx) else {
            return;
        };
        let agent_id = entry.agent_id.clone();
        // close_float will run after on_select; open the detail view
        // from an on-close follow-up via queuing, but the simplest
        // approach is to call open_detail directly. The list window
        // will be closed by the framework immediately after this.
        let parent_selected = idx;
        // Drop the list's state first — we're about to close its window.
        self.agents.clear();
        // Schedule detail open after the framework closes this window.
        let _ = win;
        open_detail(app, agent_id, parent_selected);
    }

    fn on_dismiss(&mut self, app: &mut App, _win: ui::WinId) {
        app.refresh_agent_counts();
    }

    fn tick(&mut self, app: &mut App, _win: ui::WinId) {
        let fresh = engine::registry::children_of(self.my_pid);
        let changed = fresh.len() != self.agents.len()
            || fresh.iter().zip(self.agents.iter()).any(|(a, b)| {
                a.agent_id != b.agent_id
                    || a.status != b.status
                    || a.task_slug != b.task_slug
                    || a.pid != b.pid
            });
        if changed {
            self.agents = fresh;
            refresh_list(&mut app.ui, self.list_buf, &self.agents, &self.snapshots);
        }
    }
}

impl DialogState for AgentsDetail {
    fn on_dismiss(&mut self, app: &mut App, _win: ui::WinId) {
        let selected = self.parent_selected;
        open_list(app, selected);
    }

    fn tick(&mut self, app: &mut App, _win: ui::WinId) {
        refresh_detail_title(
            &mut app.ui,
            self.title_buf,
            &self.agent_id,
            self.my_pid,
            &self.snapshots,
        );
        refresh_detail_body(
            &mut app.ui,
            self.detail_buf,
            &self.agent_id,
            &self.snapshots,
        );
    }
}

fn find_snapshot(snapshots: &SharedSnapshots, agent_id: &str) -> Option<AgentSnapshot> {
    let snaps = snapshots.lock().unwrap();
    snaps.iter().find(|s| s.agent_id == agent_id).cloned()
}

fn refresh_list(
    ui: &mut ui::Ui,
    list_buf: ui::BufId,
    agents: &[RegistryEntry],
    snapshots: &SharedSnapshots,
) {
    let name_w = agents.iter().map(|a| a.agent_id.len()).max().unwrap_or(0);

    if agents.is_empty() {
        let Some(buf) = ui.buf_mut(list_buf) else {
            return;
        };
        buf.set_all_lines(vec!["  No subagents running".into()]);
        let len = buf
            .get_line(0)
            .map(|l| l.chars().count() as u16)
            .unwrap_or(0);
        buf.add_highlight(0, 0, len, ui::buffer::SpanStyle::dim());
        return;
    }

    let mut lines: Vec<String> = Vec::with_capacity(agents.len());
    let mut status_spans: Vec<(u16, u16)> = Vec::with_capacity(agents.len());
    for agent in agents {
        let status_str = match agent.status {
            AgentStatus::Working => "working",
            AgentStatus::Idle => "idle   ",
        };
        let name = format!("{:<name_w$}", agent.agent_id);
        let mut line = format!("  {name}  {status_str}");
        let status_start = (2 + name_w + 2) as u16;
        let status_end = status_start + status_str.len() as u16;
        status_spans.push((status_start, status_end));
        if let Some(slug) = &agent.task_slug {
            line.push_str("  ");
            line.push_str(slug);
        }
        if let Some(snap) = find_snapshot(snapshots, &agent.agent_id) {
            if let Some(tokens) = snap.context_tokens {
                line.push_str(&format!("  {}", crate::render::format_tokens(tokens)));
            }
            if snap.cost_usd > 0.0 {
                line.push_str(&format!("  {}", crate::metrics::format_cost(snap.cost_usd)));
            }
        }
        lines.push(line);
    }

    let Some(buf) = ui.buf_mut(list_buf) else {
        return;
    };
    buf.set_all_lines(lines);
    for (i, &(start, end)) in status_spans.iter().enumerate() {
        buf.add_highlight(i, start, end, ui::buffer::SpanStyle::dim());
    }
}

fn refresh_detail_title(
    ui: &mut ui::Ui,
    title_buf: ui::BufId,
    agent_id: &str,
    my_pid: u32,
    snapshots: &SharedSnapshots,
) {
    let Some(buf) = ui.buf_mut(title_buf) else {
        return;
    };
    let entry = engine::registry::children_of(my_pid)
        .into_iter()
        .find(|e| e.agent_id == agent_id);
    let mut line = format!(" {agent_id}");
    let id_end = line.chars().count() as u16;
    if let Some(ref e) = entry {
        if matches!(e.status, AgentStatus::Idle) {
            line.push_str(" \u{2713}");
        }
        if let Some(ref slug) = e.task_slug {
            line.push_str(&format!(" \u{00b7} {slug}"));
        }
    }
    let snap = find_snapshot(snapshots, agent_id);
    if let Some(ref s) = snap {
        if let Some(tokens) = s.context_tokens {
            line.push_str(&format!("  {}", crate::render::format_tokens(tokens)));
        }
        if s.cost_usd > 0.0 {
            line.push_str(&format!("  {}", crate::metrics::format_cost(s.cost_usd)));
        }
    }
    buf.set_all_lines(vec![line, String::new()]);
    buf.add_highlight(
        0,
        1,
        id_end,
        ui::buffer::SpanStyle {
            fg: Some(crate::theme::AGENT),
            bold: true,
            ..Default::default()
        },
    );
}

fn refresh_detail_body(
    ui: &mut ui::Ui,
    detail_buf: ui::BufId,
    agent_id: &str,
    snapshots: &SharedSnapshots,
) {
    let snap = find_snapshot(snapshots, agent_id);
    let mut lines: Vec<String> = Vec::new();
    let mut dim_lines: Vec<usize> = Vec::new();

    let Some(snap) = snap else {
        let Some(buf) = ui.buf_mut(detail_buf) else {
            return;
        };
        buf.set_all_lines(vec!["(agent not tracked)".into()]);
        return;
    };

    dim_lines.push(lines.len());
    lines.push("Prompt:".into());
    for raw_line in snap.prompt.lines() {
        lines.push(format!(" {raw_line}"));
    }
    lines.push(String::new());
    if snap.tool_calls.is_empty() {
        lines.push("(no tool calls yet)".into());
    } else {
        for entry in &snap.tool_calls {
            let elapsed = entry
                .elapsed
                .filter(|d| d.as_secs_f64() >= 0.1)
                .map(|d| format!("  {}", format_duration(d.as_secs())))
                .unwrap_or_default();
            lines.push(format!("{} {}{elapsed}", entry.tool_name, entry.summary));
        }
    }

    let Some(buf) = ui.buf_mut(detail_buf) else {
        return;
    };
    buf.set_all_lines(lines.clone());
    for i in dim_lines {
        if let Some(line) = lines.get(i) {
            let len = line.chars().count() as u16;
            buf.add_highlight(i, 0, len, ui::buffer::SpanStyle::dim());
        }
    }
}
