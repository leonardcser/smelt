//! `smelt.win` bindings — focus state, keymap / event registration,
//! buf resolution, overlay leaf close. UiHost-only.

use super::app_read;
use crate::lua::{parse_keybind, parse_win_event, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let win_tbl = lua.create_table()?;
    win_tbl.set(
        "focus",
        app_read!(lua, |app| match app.app_focus {
            crate::app::AppFocus::Content => "transcript".to_string(),
            crate::app::AppFocus::Prompt => "prompt".to_string(),
        }),
    )?;
    win_tbl.set(
        "mode",
        app_read!(lua, |app| {
            let has_vim = match app.app_focus {
                crate::app::AppFocus::Content => app.transcript_window.vim_enabled,
                crate::app::AppFocus::Prompt => app.input.vim_enabled(),
            };
            if has_vim {
                format!("{:?}", app.vim_mode)
            } else {
                String::new()
            }
        }),
    )?;
    win_tbl.set(
        "close",
        lua.create_function(|_, id: u64| {
            crate::lua::with_app(|app| {
                app.close_overlay_leaf(ui::WinId(id));
            });
            Ok(())
        })?,
    )?;
    // `smelt.win.buf(win_id) -> buf_id | nil` — resolve the
    // Buffer backing a Window. Used by Lua-side dialog
    // orchestration (e.g. `dialog.lua` reading text from an
    // input leaf at submit time).
    win_tbl.set(
        "buf",
        lua.create_function(|_, id: u64| {
            let buf =
                crate::lua::try_with_ui_host(|host| host.ui().win(ui::WinId(id)).map(|w| w.buf.0))
                    .flatten();
            Ok(buf)
        })?,
    )?;
    // `smelt.win.set_focus(win_id)` — give keyboard focus to a Window.
    // Wraps `Ui::set_focus` so Lua-side dialog orchestration can move
    // focus between leaves (e.g. confirm.lua's `e` keymap that focuses
    // the reason input).
    win_tbl.set(
        "set_focus",
        lua.create_function(|_, id: u64| {
            crate::lua::with_app(|app| {
                app.ui.set_focus(ui::WinId(id));
            });
            Ok(())
        })?,
    )?;
    {
        let s = shared.clone();
        win_tbl.set(
            "set_keymap",
            lua.create_function(
                move |lua, (win_id, key_str, func): (u64, String, mlua::Function)| {
                    let Some(key) = parse_keybind(&key_str) else {
                        return Err(mlua::Error::RuntimeError(format!(
                            "win.set_keymap: unknown key `{key_str}`"
                        )));
                    };
                    let id = crate::lua::register_callback_handle(&s, lua, func)?;
                    crate::lua::with_app(|app| {
                        let prev = app.ui.win_set_keymap(
                            ui::WinId(win_id),
                            key,
                            ui::Callback::Lua(ui::LuaHandle(id)),
                        );
                        crate::lua::drop_displaced_lua_handle(app, prev);
                    });
                    Ok(())
                },
            )?,
        )?;
    }
    {
        let s = shared.clone();
        win_tbl.set(
            "on_event",
            lua.create_function(
                move |lua, (win_id, ev_str, func): (u64, String, mlua::Function)| {
                    let Some(event) = parse_win_event(&ev_str) else {
                        return Err(mlua::Error::RuntimeError(format!(
                            "win.on_event: unknown event `{ev_str}`"
                        )));
                    };
                    let id = crate::lua::register_callback_handle(&s, lua, func)?;
                    crate::lua::with_app(|app| {
                        app.ui.win_on_event(
                            ui::WinId(win_id),
                            event,
                            ui::Callback::Lua(ui::LuaHandle(id)),
                        );
                    });
                    Ok(id)
                },
            )?,
        )?;
    }
    win_tbl.set(
        "clear_keymap",
        lua.create_function(|_, (win_id, key_str): (u64, String)| {
            let Some(key) = parse_keybind(&key_str) else {
                return Err(mlua::Error::RuntimeError(format!(
                    "win.clear_keymap: unknown key `{key_str}`"
                )));
            };
            crate::lua::with_app(|app| {
                let prev = app.ui.win_clear_keymap(ui::WinId(win_id), key);
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            Ok(())
        })?,
    )?;
    win_tbl.set(
        "clear_event",
        lua.create_function(|_, (win_id, ev_str, callback_id): (u64, String, u64)| {
            let Some(event) = parse_win_event(&ev_str) else {
                return Err(mlua::Error::RuntimeError(format!(
                    "win.clear_event: unknown event `{ev_str}`"
                )));
            };
            crate::lua::with_app(|app| {
                let prev = app
                    .ui
                    .win_clear_event_by_id(ui::WinId(win_id), event, callback_id);
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            Ok(())
        })?,
    )?;
    smelt.set("win", win_tbl)?;
    Ok(())
}
