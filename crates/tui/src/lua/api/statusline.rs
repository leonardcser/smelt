//! `smelt.statusline` bindings — register / unregister statusline
//! sources by name + a `snapshot()` of all state the core composer
//! reads. The Rust side stays the layout engine (priority dropping,
//! truncation, alignment) and `smelt.statusline.snapshot()` shovels
//! every input the bottom-row composition needs into one Lua table so
//! `runtime/lua/smelt/status.lua` can build the items without reaching
//! across half a dozen narrower bindings every refresh.

use crate::lua::{LuaHandle, LuaShared, StatusSource};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let statusline_tbl = lua.create_table()?;
    {
        let s = shared.clone();
        statusline_tbl.set(
            "register",
            lua.create_function(
                move |lua, (name, handler, opts): (String, mlua::Function, Option<mlua::Table>)| {
                    let default_align_right = opts
                        .as_ref()
                        .and_then(|t| t.get::<Option<String>>("align").ok().flatten())
                        .map(|s| s == "right")
                        .unwrap_or(false);
                    let key = lua.create_registry_value(handler)?;
                    let source = StatusSource {
                        handle: LuaHandle { key },
                        default_align_right,
                    };
                    if let Ok(mut sources) = s.statusline_sources.lock() {
                        if let Some(existing) = sources.iter_mut().find(|(n, _)| n == &name) {
                            existing.1 = source;
                        } else {
                            sources.push((name, source));
                        }
                    }
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let s = shared.clone();
        statusline_tbl.set(
            "unregister",
            lua.create_function(move |_, name: String| {
                if let Ok(mut sources) = s.statusline_sources.lock() {
                    sources.retain(|(n, _)| n != &name);
                }
                Ok(())
            })?,
        )?;
    }
    statusline_tbl.set(
        "snapshot",
        lua.create_function(|lua, ()| {
            match crate::lua::try_with_app(|app| build_snapshot(app, lua)) {
                // No app pointer (cold-start unit-test path); return an
                // empty table so status.lua can short-circuit cleanly.
                None => lua.create_table(),
                Some(result) => result,
            }
        })?,
    )?;
    smelt.set("statusline", statusline_tbl)?;
    Ok(())
}

/// Build the full snapshot the Lua composer consumes once per refresh.
/// Mirrors the inputs the retired Rust composition read off `TuiApp`;
/// see `runtime/lua/smelt/status.lua` for the segment shape.
fn build_snapshot(app: &mut crate::app::TuiApp, lua: &Lua) -> LuaResult<mlua::Table> {
    use crossterm::style::Color;
    use ui::text::byte_to_cell;

    let t = lua.create_table()?;

    // Theme colors as ANSI u8s. `nil` slots mean the highlight group
    // has no fg/bg and the segment should fall back to the default.
    let theme = lua.create_table()?;
    let theme_ref = app.ui.theme();
    let fg_of = |group: &str| theme_ref.get(group).fg.and_then(super::color_to_ansi);
    let bg_of = |group: &str| theme_ref.get(group).bg.and_then(super::color_to_ansi);
    if let Some(c) = fg_of("SmeltAccent") {
        theme.set("accent_fg", c)?;
    }
    if let Some(c) = fg_of("Comment") {
        theme.set("muted_fg", c)?;
    }
    if let Some(c) = fg_of("SmeltModePlan") {
        theme.set("plan_fg", c)?;
    }
    if let Some(c) = fg_of("SmeltModeApply") {
        theme.set("apply_fg", c)?;
    }
    if let Some(c) = fg_of("SmeltModeYolo") {
        theme.set("yolo_fg", c)?;
    }
    if let Some(c) = bg_of("SmeltSlug") {
        theme.set("slug_bg", c)?;
    }
    t.set("theme", theme)?;

    // Working state + Rust-rendered throbber sub-spans (the throbber
    // walks `LiveTurn` / `LastTurn` state machines tracked entirely on
    // `WorkingState`; cheaper to project the spans than to re-export
    // the state machine itself).
    let working = lua.create_table()?;
    working.set("animating", app.working.is_animating())?;
    working.set("compacting", app.working.is_compacting())?;
    if let Some(c) = app.working.spinner_char() {
        working.set("spinner_char", c)?;
    }
    let muted = app.ui.theme().get("Comment").fg.unwrap_or(Color::Reset);
    let throbber_arr = lua.create_table()?;
    let show_tps = app.core.config.settings.show_tps;
    for (i, span) in app
        .working
        .throbber_spans(show_tps, muted)
        .iter()
        .enumerate()
    {
        let st = lua.create_table()?;
        st.set("text", span.text.as_str())?;
        if let Some(fg) = super::color_to_ansi(span.color) {
            st.set("fg", fg)?;
        }
        st.set("bold", span.bold)?;
        st.set("dim", span.dim)?;
        st.set("priority", span.priority)?;
        throbber_arr.set(i + 1, st)?;
    }
    working.set("throbber", throbber_arr)?;
    t.set("working", working)?;

    // Vim mode resolution mirrors the keymap dispatcher: focused
    // overlay-leaf with vim wins, then split under `app_focus`. A
    // non-vim overlay leaf yields no label (those windows have no
    // buffer cursor — same model nvim uses).
    let vim_tbl = lua.create_table()?;
    let focused_window_has_vim = app
        .ui
        .focused_window()
        .map(|w| w.vim_enabled)
        .unwrap_or(false);
    let (vim_enabled, vim_mode) = if focused_window_has_vim {
        (true, Some(app.vim_mode))
    } else if app.ui.focused_overlay().is_some() {
        (false, None)
    } else {
        match app.app_focus {
            crate::app::AppFocus::Content => {
                let has = app.transcript_window.vim_enabled;
                (has, has.then_some(app.vim_mode))
            }
            crate::app::AppFocus::Prompt => {
                let mut mode = app.input.vim_enabled().then_some(app.vim_mode);
                let drag = matches!(app.ui.capture(), Some(ui::HitTarget::Window(_)));
                if drag {
                    mode = Some(ui::VimMode::Visual);
                }
                (app.input.vim_enabled() || drag, mode)
            }
        }
    };
    vim_tbl.set("enabled", vim_enabled)?;
    if vim_enabled {
        let label = crate::content::status::vim_mode_label(vim_mode).unwrap_or("NORMAL");
        vim_tbl.set("label", label)?;
        let kind = match vim_mode {
            Some(ui::VimMode::Insert) => "insert",
            Some(ui::VimMode::Visual) | Some(ui::VimMode::VisualLine) => "visual",
            _ => "normal",
        };
        vim_tbl.set("kind", kind)?;
    }
    t.set("vim", vim_tbl)?;

    // AgentMode (Plan/Apply/Yolo/Normal) — icon + name; the Lua composer
    // reads `theme.{plan,apply,yolo,muted}_fg` to colorize.
    let mode_tbl = lua.create_table()?;
    let (icon, name) = match app.core.config.mode {
        protocol::AgentMode::Plan => ("◇ ", "plan"),
        protocol::AgentMode::Apply => ("→ ", "apply"),
        protocol::AgentMode::Yolo => ("⚡", "yolo"),
        protocol::AgentMode::Normal => ("○ ", "normal"),
    };
    mode_tbl.set("icon", icon)?;
    mode_tbl.set("name", name)?;
    t.set("mode", mode_tbl)?;

    // Indicators that flip the "permission pending" / "N procs" / "N
    // agents" trio on the right-of-throbber strip.
    let blocked = app.focused_overlay_blocks_agent();
    t.set("permission_pending", app.pending_dialog && !blocked)?;
    t.set("running_procs", app.core.processes.running_count() as i64)?;
    t.set("running_agents", 0i64)?;
    if let Some(label) = &app.task_label {
        t.set("task_label", label.as_str())?;
    }

    let settings = lua.create_table()?;
    settings.set("show_slug", app.core.config.settings.show_slug)?;
    settings.set("show_tps", show_tps)?;
    t.set("settings", settings)?;

    // Position — mirrors the retired `compute_status_position` for
    // both prompt and content focus. `nil` for an empty transcript.
    let position = match app.app_focus {
        crate::app::AppFocus::Prompt => {
            let buf = &app.input.buf;
            let cpos = app.input.win.cpos.min(buf.len());
            let line_idx = buf[..cpos].bytes().filter(|&b| b == b'\n').count();
            let line_start = buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let col_cells = byte_to_cell(&buf[line_start..], cpos - line_start);
            let total_lines = buf.bytes().filter(|&b| b == b'\n').count() + 1;
            let pct = if total_lines <= 1 {
                100u8
            } else {
                ((line_idx as u64 * 100) / (total_lines.saturating_sub(1) as u64)) as u8
            };
            Some(((line_idx as u32) + 1, col_cells as u32 + 1, pct.min(100)))
        }
        crate::app::AppFocus::Content => {
            let total = app
                .full_transcript_display_text(app.core.config.settings.show_thinking)
                .len();
            if total == 0 {
                None
            } else {
                let line_idx = app.transcript_window.cursor_abs_row();
                let pct = if total <= 1 {
                    100u8
                } else {
                    ((line_idx as u64 * 100) / (total.saturating_sub(1) as u64)) as u8
                };
                Some((
                    (line_idx as u32) + 1,
                    app.transcript_window.cursor_col as u32 + 1,
                    pct.min(100),
                ))
            }
        }
    };
    if let Some((line, col, scroll_pct)) = position {
        let p = lua.create_table()?;
        p.set("line", line)?;
        p.set("col", col)?;
        p.set("scroll_pct", scroll_pct as i64)?;
        p.set("text", format!("{line}:{col} {scroll_pct}%"))?;
        t.set("position", p)?;
    }

    Ok(t)
}
