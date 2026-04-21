//! Dialog built from a Lua `opts` table and driven by a parked
//! `LuaTask`. Opened when a plugin tool / `smelt.task` coroutine yields
//! `smelt.api.dialog.open({...})`. When the user resolves the dialog
//! (selects an option, dismisses), the task is resumed with a result
//! table and continues executing.
//!
//! Supported panel kinds (from `opts.panels[]`):
//! - `{ kind = "content",  text = "..." }`      plain lines
//! - `{ kind = "markdown", text = "..." }`      rendered via `render_markdown_inner`
//! - `{ kind = "options",  items = [{label, action?, shortcut?, on_select?}] }`
//! - `{ kind = "input",    name = "x", placeholder? = "..." }`

use super::super::{App, TurnState};
use super::{ActionResult, DialogState};
use crate::keymap::hints;
use mlua::prelude::*;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{BufId, OptionItem, OptionList, PanelHeight, PanelSpec};

/// Per-option data the DialogState needs to build the result table on
/// selection. Index matches the `OptionList` row.
struct OptionEntry {
    /// Action string reported back to the task (`result.action`).
    /// Defaults to `"select"` when the plugin didn't specify one.
    action: String,
    /// Optional `on_select` callback fired *before* the task resumes.
    on_select: Option<mlua::RegistryKey>,
}

/// Per-input panel, to gather final text on resolution.
struct InputEntry {
    name: String,
    panel_index: usize,
}

/// DialogState for a Lua-driven dialog. Resolves its parked task on
/// option selection or dismiss.
pub struct LuaDialog {
    dialog_id: u64,
    options: Vec<OptionEntry>,
    inputs: Vec<InputEntry>,
    /// Index of the `OptionList` panel (if any). Shortcut routing
    /// reads the list's current selection via this index.
    options_panel: Option<usize>,
}

impl DialogState for LuaDialog {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn on_action(
        &mut self,
        app: &mut App,
        win: ui::WinId,
        action: &str,
        _agent: &mut Option<TurnState>,
    ) -> ActionResult {
        if let Some(idx_str) = action.strip_prefix("select:") {
            if let Ok(idx) = idx_str.parse::<usize>() {
                self.resolve_with_option(app, win, idx);
                return ActionResult::Close;
            }
        }
        if let Some(ch) = action.strip_prefix("shortcut:") {
            if let Some(idx) = self.find_shortcut_index(app, win, ch) {
                self.resolve_with_option(app, win, idx);
                return ActionResult::Close;
            }
        }
        if action == "dismiss" {
            self.resolve_dismissed(app);
            return ActionResult::Close;
        }
        ActionResult::Pass
    }

    fn on_dismiss(&mut self, app: &mut App, _win: ui::WinId) {
        self.resolve_dismissed(app);
    }
}

impl LuaDialog {
    fn find_shortcut_index(&self, app: &mut App, win: ui::WinId, ch_str: &str) -> Option<usize> {
        let panel_idx = self.options_panel?;
        let list = app
            .ui
            .dialog_mut(win)
            .and_then(|d| d.panel_widget_mut::<OptionList>(panel_idx))?;
        let ch = ch_str.chars().next()?;
        list.items().iter().position(|it| it.shortcut == Some(ch))
    }

    fn resolve_with_option(&self, app: &mut App, win: ui::WinId, idx: usize) {
        // Fire on_select side effect before resuming the task so
        // Lua callbacks see the pre-resume state.
        if let Some(entry) = self.options.get(idx) {
            if let Some(ref key) = entry.on_select {
                if let Ok(func) = app.lua.lua().registry_value::<mlua::Function>(key) {
                    if let Err(e) = func.call::<()>(()) {
                        app.screen.notify_error(format!("dialog on_select: {e}"));
                    }
                    app.apply_lua_ops();
                }
            }
        }
        let action = self
            .options
            .get(idx)
            .map(|e| e.action.clone())
            .unwrap_or_else(|| "select".into());
        let inputs = self.collect_inputs(app, win);
        let result = build_result(app.lua.lua(), &action, Some(idx + 1), inputs);
        match result {
            Ok(v) => {
                app.lua.resolve_dialog(self.dialog_id, v);
            }
            Err(e) => {
                app.screen.notify_error(format!("dialog resolve: {e}"));
                app.lua.resolve_dialog(self.dialog_id, mlua::Value::Nil);
            }
        }
    }

    fn resolve_dismissed(&self, app: &mut App) {
        let result = build_result(app.lua.lua(), "dismiss", None, Vec::new());
        match result {
            Ok(v) => {
                app.lua.resolve_dialog(self.dialog_id, v);
            }
            Err(_) => {
                app.lua.resolve_dialog(self.dialog_id, mlua::Value::Nil);
            }
        }
    }

    fn collect_inputs(&self, app: &mut App, win: ui::WinId) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(self.inputs.len());
        for entry in &self.inputs {
            let text = app
                .ui
                .dialog_mut(win)
                .and_then(|d| d.panel_widget_mut::<TextInput>(entry.panel_index))
                .map(|w| w.text().to_string())
                .unwrap_or_default();
            out.push((entry.name.clone(), text));
        }
        out
    }
}

fn build_result(
    lua: &Lua,
    action: &str,
    option_index: Option<usize>,
    inputs: Vec<(String, String)>,
) -> LuaResult<mlua::Value> {
    let t = lua.create_table()?;
    t.set("action", action)?;
    if let Some(i) = option_index {
        t.set("option_index", i)?;
    }
    let inputs_tbl = lua.create_table()?;
    for (k, v) in inputs {
        inputs_tbl.set(k, v)?;
    }
    t.set("inputs", inputs_tbl)?;
    Ok(mlua::Value::Table(t))
}

/// Open the dialog described by the Lua `opts_key` table. On success,
/// registers a `LuaDialog` state so the task will be resumed on
/// user resolution. Returns `Err` when the table is malformed — the
/// caller should resolve the parked task with an error string.
pub fn open(app: &mut App, dialog_id: u64, opts_key: mlua::RegistryKey) -> Result<(), String> {
    let opts: mlua::Table = app
        .lua
        .lua()
        .registry_value(&opts_key)
        .map_err(|e| format!("dialog opts: {e}"))?;
    let title: Option<String> = opts.get("title").ok();
    let panels_tbl: mlua::Table = opts
        .get("panels")
        .map_err(|e| format!("dialog panels: {e}"))?;

    // Build per-panel state + PanelSpecs.
    let mut panel_specs: Vec<PanelSpec> = Vec::new();
    let mut options: Vec<OptionEntry> = Vec::new();
    let mut inputs: Vec<InputEntry> = Vec::new();
    let mut options_panel: Option<usize> = None;

    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let panel_index = panel_specs.len();
        match kind.as_str() {
            "content" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_text_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, PanelHeight::Fit));
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_markdown_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, PanelHeight::Fit));
            }
            "options" => {
                let items_tbl: mlua::Table = panel
                    .get("items")
                    .map_err(|e| format!("options.items: {e}"))?;
                let mut list_items = Vec::new();
                for it_pair in items_tbl.sequence_values::<mlua::Table>() {
                    let it = it_pair.map_err(|e| format!("option item: {e}"))?;
                    let label: String = it.get("label").unwrap_or_default();
                    let shortcut: Option<char> = it
                        .get::<String>("shortcut")
                        .ok()
                        .and_then(|s| s.chars().next());
                    let mut item = OptionItem::new(label);
                    if let Some(c) = shortcut {
                        item = item.with_shortcut(c);
                    }
                    let action: String = it.get("action").unwrap_or_else(|_| "select".into());
                    let on_select = it
                        .get::<mlua::Function>("on_select")
                        .ok()
                        .and_then(|f| app.lua.lua().create_registry_value(f).ok());
                    list_items.push(item);
                    options.push(OptionEntry { action, on_select });
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let widget = Box::new(
                    OptionList::new(list_items)
                        .multi(multi)
                        .with_cursor_style(accent_style())
                        .with_shortcut_style(accent_style()),
                );
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
                options_panel = Some(panel_index);
            }
            "input" => {
                let name: String = panel.get("name").unwrap_or_default();
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
                inputs.push(InputEntry { name, panel_index });
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    let dialog_config =
        app.builtin_dialog_config(Some(hints::join(&[hints::CONFIRM, hints::CANCEL])), vec![]);
    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        panel_specs,
    );
    let win_id = win_id.ok_or_else(|| "failed to open dialog window".to_string())?;
    app.float_states.insert(
        win_id,
        Box::new(LuaDialog {
            dialog_id,
            options,
            inputs,
            options_panel,
        }),
    );
    Ok(())
}

fn accent_style() -> ui::grid::Style {
    ui::grid::Style {
        fg: Some(crate::theme::accent()),
        ..Default::default()
    }
}

fn make_text_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|s| s.to_string()).collect()
        };
        buf.set_all_lines(lines);
    }
    id
}

fn make_markdown_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    let theme = crate::theme::snapshot();
    // Width is unknown at buffer-build time (the float hasn't been
    // placed yet). Use a conservative default; markdown wrapping will
    // reflow when the dialog picks its rect on first draw.
    let width: u16 = 80;
    if let Some(buf) = app.ui.buf_mut(id) {
        crate::render::to_buffer::render_into_buffer(buf, width, &theme, |sink| {
            crate::render::blocks::render_markdown_inner(
                sink,
                text,
                width as usize,
                " ",
                false,
                None,
            );
        });
    }
    id
}
