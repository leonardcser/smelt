//! `smelt.statusline.snapshot()` — feed the Lua statusline composer the
//! per-refresh slice of host state it can't read from cells (vim mode,
//! mode pill, permission/proc counts, settings, cursor position).
//!
//! Registration of sources moved to pure Lua in
//! `runtime/lua/smelt/statusline.lua`; this module no longer exposes
//! `register`/`unregister`.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

use crate::lua::LuaShared;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, _shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "statusline",
        "Snapshot host state for the Lua statusline composer. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "snapshot",
        "Return the statusline state in one table per refresh: `tps` \
(tokens-per-second from the live or just-archived turn, when \
available), `vim`, `mode`, `permission_pending`, `running_procs`, \
`running_agents`, `task_label`, `settings`, and `position`. The \
working pill lives in the prompt top bar now; plugins that need work \
state read the `work_*` cells instead. Styles are not projected — \
look colors up by theme group in the composer. Returns an empty \
table when the app pointer is unavailable.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            match crate::lua::try_with_app(|app| build_snapshot(app, lua)) {
                None => lua.create_table(),
                Some(result) => result,
            }
        },
    )?;
    Ok(())
}

fn vim_mode_label(mode: Option<crate::smelt_term::VimMode>) -> Option<&'static str> {
    match mode {
        Some(crate::smelt_term::VimMode::Insert) => Some("INSERT"),
        Some(crate::smelt_term::VimMode::Visual) => Some("VISUAL"),
        Some(crate::smelt_term::VimMode::VisualLine) => Some("VISUAL LINE"),
        _ => None,
    }
}

fn build_snapshot(app: &mut crate::app::TuiApp, lua: &Lua) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;

    if let Some(tps) = app.working.turn_meta().and_then(|m| m.avg_tps) {
        t.set("tps", tps)?;
    }

    let vim_tbl = lua.create_table()?;
    let focused_window = app.ui.focused_window();
    let focused_window_has_vim = focused_window.map(|w| w.vim_enabled).unwrap_or(false);
    let (vim_enabled, vim_mode) = if focused_window_has_vim {
        (true, focused_window.map(|w| w.vim_mode))
    } else if app.ui.focused_overlay().is_some() {
        (false, None)
    } else {
        match app.app_focus {
            crate::app::AppFocus::Content => {
                let has = app.transcript_win().vim_enabled;
                (has, has.then_some(app.transcript_win().vim_mode))
            }
            crate::app::AppFocus::Prompt => {
                let prompt_win = app.prompt_win();
                let mut mode = app
                    .input
                    .vim_enabled(prompt_win)
                    .then_some(prompt_win.vim_mode);
                let drag = matches!(
                    app.ui.capture(),
                    Some(crate::smelt_term::HitTarget::Window(_))
                );
                if drag {
                    mode = Some(crate::smelt_term::VimMode::Visual);
                }
                (app.input.vim_enabled(prompt_win) || drag, mode)
            }
        }
    };
    vim_tbl.set("enabled", vim_enabled)?;
    if vim_enabled {
        let label = vim_mode_label(vim_mode).unwrap_or("NORMAL");
        vim_tbl.set("label", label)?;
        let kind = match vim_mode {
            Some(crate::smelt_term::VimMode::Insert) => "insert",
            Some(crate::smelt_term::VimMode::Visual)
            | Some(crate::smelt_term::VimMode::VisualLine) => "visual",
            _ => "normal",
        };
        vim_tbl.set("kind", kind)?;
    }
    t.set("vim", vim_tbl)?;

    let mode_tbl = lua.create_table()?;
    mode_tbl.set("name", app.core.config.mode.as_str())?;
    t.set("mode", mode_tbl)?;

    let blocked = app.focused_overlay_blocks_agent();
    t.set("permission_pending", app.pending_dialog && !blocked)?;
    t.set("running_procs", app.core.processes.running_count() as i64)?;
    t.set("running_agents", 0i64)?;
    if let Some(label) = &app.task_label {
        t.set("task_label", label.as_str())?;
    }

    let settings = lua.create_table()?;
    settings.set("show_slug", app.core.config.settings.show_slug)?;
    settings.set("show_tps", app.core.config.settings.show_tps)?;
    t.set("settings", settings)?;

    let position = app.ui.focused_window().and_then(|w| {
        let total = app.ui.buf(w.buf).map(|b| b.line_count()).unwrap_or(0);
        if total == 0 {
            return None;
        }
        let line_idx = w.cursor_abs_row();
        let col = w.cursor_col() as usize;
        let pct = if total <= 1 {
            100u8
        } else {
            ((line_idx as u64 * 100) / (total.saturating_sub(1) as u64)) as u8
        };
        Some(((line_idx as u32) + 1, (col as u32) + 1, pct.min(100)))
    });
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
