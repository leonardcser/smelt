use super::super::App;
use crate::app::ops::AppOp;
use crate::render::{AgentSnapshot, SharedSnapshots};
use crate::utils::format_duration;
use crossterm::event::{KeyCode, KeyModifiers};
use engine::registry::{AgentStatus, RegistryEntry};
use std::cell::RefCell;
use std::rc::Rc;
use ui::{Callback, CallbackResult, KeyBind, Payload, WinEvent};

struct AgentsListState {
    my_pid: u32,
    snapshots: SharedSnapshots,
    list_buf: ui::BufId,
    agents: Vec<RegistryEntry>,
}

struct AgentsDetailState {
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

pub(in crate::app) fn open_list(app: &mut App, initial_selected: usize) {
    use crate::keymap::hints;

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

    if initial_selected > 0 {
        if let Some(dialog) = app.ui.dialog_mut(win_id) {
            dialog.set_selected_index(initial_selected);
        }
    }

    let state = Rc::new(RefCell::new(AgentsListState {
        my_pid,
        snapshots,
        list_buf,
        agents,
    }));

    let ops = app.lua.ops_handle();

    // Backspace: kill the selected subagent if it belongs to our tree.
    let state_bs = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Backspace),
        Callback::Rust(Box::new(move |ctx| {
            let idx = ctx.ui.dialog_mut(ctx.win).and_then(|d| d.selected_index());
            let Some(idx) = idx else {
                return CallbackResult::Consumed;
            };
            let mut s = state_bs.borrow_mut();
            if let Some(agent) = s.agents.get(idx) {
                let pid = agent.pid;
                if engine::registry::is_in_tree(pid, s.my_pid) {
                    engine::registry::kill_agent(pid);
                    s.agents = engine::registry::children_of(s.my_pid);
                    let s_ref: &AgentsListState = &s;
                    refresh_list(ctx.ui, s_ref.list_buf, &s_ref.agents, &s_ref.snapshots);
                }
            }
            CallbackResult::Consumed
        })),
    );

    // Submit (Enter): swap list for detail view of the selected agent.
    let state_submit = state.clone();
    let ops_submit = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            if let Payload::Selection { index } = ctx.payload {
                let s = state_submit.borrow();
                if let Some(entry) = s.agents.get(index) {
                    ops_submit.push(AppOp::OpenAgentsDetail {
                        agent_id: entry.agent_id.clone(),
                        parent_selected: index,
                    });
                }
            }
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Dismiss: refresh agent counts + close.
    let ops_dismiss = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            ops_dismiss.push(AppOp::RefreshAgentCounts);
            ops_dismiss.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Tick: re-read registry and refresh the buffer when anything changed.
    let state_tick = state;
    app.ui.win_on_event(
        win_id,
        WinEvent::Tick,
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_tick.borrow_mut();
            let fresh = engine::registry::children_of(s.my_pid);
            let changed = fresh.len() != s.agents.len()
                || fresh.iter().zip(s.agents.iter()).any(|(a, b)| {
                    a.agent_id != b.agent_id
                        || a.status != b.status
                        || a.task_slug != b.task_slug
                        || a.pid != b.pid
                });
            if changed {
                s.agents = fresh;
                let s_ref: &AgentsListState = &s;
                refresh_list(ctx.ui, s_ref.list_buf, &s_ref.agents, &s_ref.snapshots);
            }
            CallbackResult::Consumed
        })),
    );
    let _ = ops;
}

pub(in crate::app) fn open_detail(app: &mut App, agent_id: String, parent_selected: usize) {
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
            ui::PanelSpec::content(detail_buf, ui::PanelHeight::Fill)
                .with_pad_left(2)
                .focusable(true),
        ],
    ) else {
        return;
    };

    let state = Rc::new(RefCell::new(AgentsDetailState {
        my_pid,
        agent_id,
        snapshots,
        title_buf,
        detail_buf,
        parent_selected,
    }));

    let ops = app.lua.ops_handle();

    // Dismiss: back-nav to the list view.
    let state_dismiss = state.clone();
    let ops_dismiss = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            let initial = state_dismiss.borrow().parent_selected;
            ops_dismiss.push(AppOp::CloseFloat(ctx.win));
            ops_dismiss.push(AppOp::OpenAgentsList {
                initial_selected: initial,
            });
            CallbackResult::Consumed
        })),
    );

    // Tick: refresh title + body.
    app.ui.win_on_event(
        win_id,
        WinEvent::Tick,
        Callback::Rust(Box::new(move |ctx| {
            let s = state.borrow();
            refresh_detail_title(ctx.ui, s.title_buf, &s.agent_id, s.my_pid, &s.snapshots);
            refresh_detail_body(ctx.ui, s.detail_buf, &s.agent_id, &s.snapshots);
            CallbackResult::Consumed
        })),
    );
    let _ = ops;
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
