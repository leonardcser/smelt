//! `smelt.statusline` — register/unregister sources and expose a `snapshot()` of all
//! state the Lua composer needs in one table per refresh.

use crate::lua::{LuaHandle, LuaShared, StatusSource};
use lua_doc_derive::{lua_module, LuaOpts};
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};
use smelt_core::lua::lua_type::LuaCallback;
use std::sync::Arc;

/// Options accepted by `smelt.statusline.register`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.statusline.RegisterOpts")]
pub struct LuaStatuslineRegisterOpts {
    /// `"right"` makes the source's segments default to the right strip. Defaults to left.
    pub align: Option<String>,
}

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let statusline_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.statusline",
        "Register/unregister statusline sources and snapshot composer state. UiHost-only.",
    );
    {
        let s = shared.clone();
        register_ui_fn(
            &statusline_tbl,
            "smelt.statusline",
            "register",
            "Register a Lua statusline source named `name`. The handler is called once per refresh with the snapshot table and returns segments. `opts.align = \"right\"` makes its segments default to the right strip; later registrations replace earlier ones with the same name.",
            &["name", "handler", "opts"],
            lua,
            move |lua,
                  (name, handler, opts): (
                String,
                LuaCallback<mlua::Table, mlua::Table>,
                Option<LuaStatuslineRegisterOpts>,
            )|
                  -> LuaResult<()> {
                let default_align_right = opts
                    .and_then(|o| o.align)
                    .map(|s| s == "right")
                    .unwrap_or(false);
                let key = lua.create_registry_value(handler.into_inner())?;
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
        )?;
    }
    {
        let s = shared.clone();
        register_ui_fn(
            &statusline_tbl,
            "smelt.statusline",
            "unregister",
            "Drop the statusline source registered under `name`. No-op if no such source exists.",
            &["name"],
            lua,
            move |_, name: String| -> LuaResult<()> {
                if let Ok(mut sources) = s.statusline_sources.lock() {
                    sources.retain(|(n, _)| n != &name);
                }
                Ok(())
            },
        )?;
    }
    register_ui_fn(
        &statusline_tbl,
        "smelt.statusline",
        "snapshot",
        "Return the full statusline state in one table per refresh: theme colors, working/throbber state, vim mode, agent mode, indicators, and cursor position. Returns an empty table when the app pointer is unavailable.",
        &[],
        lua,
        |lua, ()| -> LuaResult<mlua::Table> {
            match crate::lua::try_with_app(|app| build_snapshot(app, lua)) {
                None => lua.create_table(), // no app pointer — status.lua short-circuits
                Some(result) => result,
            }
        },
    )?;
    smelt.set("statusline", statusline_tbl)?;
    Ok(())
}

/// Build the full snapshot the Lua composer consumes once per refresh.
fn build_snapshot(app: &mut crate::app::TuiApp, lua: &Lua) -> LuaResult<mlua::Table> {
    use crate::smelt_term::text::byte_to_cell;
    use smelt_core::style::Color;

    let t = lua.create_table()?;

    // Theme colors as ANSI u8s; nil means fall back to the terminal default.
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

    // Working state + throbber spans (cheaper to project than re-export the state machine).
    let working = lua.create_table()?;
    working.set("animating", app.working.is_animating())?;
    working.set("compacting", app.working.is_compacting())?;
    if let Some(c) = app.working.spinner_char() {
        working.set("spinner_char", c)?;
    }
    let muted = app.ui.theme().get("Comment").fg.unwrap_or(Color::Reset);
    let muted_ansi = super::color_to_ansi(muted);
    let throbber_arr = lua.create_table()?;
    let show_tps = app.core.config.settings.show_tps;
    for (i, item) in app.working.throbber_data(show_tps).iter().enumerate() {
        let st = lua.create_table()?;
        st.set("text", item.text.as_str())?;
        if item.is_muted {
            if let Some(c) = muted_ansi {
                st.set("fg", c)?;
            }
        }
        st.set("bold", item.bold)?;
        st.set("dim", item.dim)?;
        st.set("priority", item.priority)?;
        throbber_arr.set(i + 1, st)?;
    }
    working.set("throbber", throbber_arr)?;
    t.set("working", working)?;

    // Vim mode: focused overlay-leaf with vim wins; non-vim overlay leaf yields no label.
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
                let drag = matches!(
                    app.ui.capture(),
                    Some(crate::smelt_term::HitTarget::Window(_))
                );
                if drag {
                    mode = Some(crate::smelt_term::VimMode::Visual);
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
            Some(crate::smelt_term::VimMode::Insert) => "insert",
            Some(crate::smelt_term::VimMode::Visual)
            | Some(crate::smelt_term::VimMode::VisualLine) => "visual",
            _ => "normal",
        };
        vim_tbl.set("kind", kind)?;
    }
    t.set("vim", vim_tbl)?;

    // AgentMode: name only; icon resolves in Lua so plugin-defined modes pick up their glyph.
    let mode_tbl = lua.create_table()?;
    mode_tbl.set("name", app.core.config.mode.as_str())?;
    t.set("mode", mode_tbl)?;

    // Right-strip indicators.
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

    // Cursor position; nil for empty transcript.
    let position = match app.app_focus {
        crate::app::AppFocus::Prompt => {
            let buf = &app.input.source;
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
