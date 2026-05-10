//! `smelt.win` — focus, keymap/event registration, buf resolution, window config. UiHost-only.

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
            app.focused_vim_mode_label().unwrap_or_default()
        }),
    )?;
    win_tbl.set(
        "close",
        lua.create_function(|_, id: u64| {
            crate::lua::with_app(|app| {
                app.close_overlay_leaf(crate::smelt_term::WinId(id));
            });
            Ok(())
        })?,
    )?;
    win_tbl.set(
        "open",
        lua.create_function(|_, (buf_id, opts): (u64, Option<mlua::Table>)| {
            let win = crate::lua::with_app(|app| {
                let region = opts
                    .as_ref()
                    .and_then(|t| t.get::<String>("region").ok())
                    .unwrap_or_else(|| "lua_overlay".to_string());
                let win = app.ui.win_open_split(
                    crate::smelt_term::BufId(buf_id),
                    crate::smelt_term::SplitConfig {
                        region,
                        gutters: Default::default(),
                    },
                );
                if let Some(win_id) = win {
                    if let Some(w) = app.ui.win_mut(win_id) {
                        if let Some(opts) = opts.as_ref() {
                            if let Ok(focusable) = opts.get::<bool>("focusable") {
                                w.focusable = focusable;
                            }
                            if let Ok(cursor_line_highlight) =
                                opts.get::<bool>("cursor_line_highlight")
                            {
                                w.cursor_line_highlight = cursor_line_highlight;
                            }
                            if let Ok(vim_enabled) = opts.get::<bool>("vim_enabled") {
                                w.set_vim_enabled(vim_enabled);
                            }
                        }
                    }
                }
                win.map(|w| w.0)
            });
            Ok(win)
        })?,
    )?;
    win_tbl.set(
        "configure_list",
        lua.create_function(|_, (win_id, initial_cursor): (u64, Option<u64>)| {
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::configure_list_leaf(
                    app,
                    crate::smelt_term::WinId(win_id),
                    initial_cursor.unwrap_or(0).min(u16::MAX as u64) as u16,
                );
            });
            Ok(())
        })?,
    )?;
    win_tbl.set(
        "configure_input",
        lua.create_function(|_, win_id: u64| {
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::configure_input_leaf(app, crate::smelt_term::WinId(win_id));
            });
            Ok(())
        })?,
    )?;
    // `smelt.win.buf(win_id) -> buf_id | nil`
    win_tbl.set(
        "buf",
        lua.create_function(|_, id: u64| {
            let buf = crate::lua::try_with_app(|app| {
                app.ui.win(crate::smelt_term::WinId(id)).map(|w| w.buf.0)
            })
            .flatten();
            Ok(buf)
        })?,
    )?;
    // `smelt.win.rect(win_id) -> {row, col, width, height} | nil` — nil until first render.
    win_tbl.set(
        "rect",
        lua.create_function(|lua, id: u64| {
            let rect = crate::lua::try_with_app(|app| {
                app.ui
                    .win(crate::smelt_term::WinId(id))
                    .and_then(|w| w.viewport)
                    .map(|vp| vp.rect)
            })
            .flatten();
            match rect {
                Some(r) => {
                    let t = lua.create_table()?;
                    t.set("row", r.top)?;
                    t.set("col", r.left)?;
                    t.set("width", r.width)?;
                    t.set("height", r.height)?;
                    Ok(mlua::Value::Table(t))
                }
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;
    // `smelt.win.set_focus(win_id)`
    win_tbl.set(
        "set_focus",
        lua.create_function(|_, id: u64| {
            crate::lua::with_app(|app| {
                app.ui.set_focus(crate::smelt_term::WinId(id));
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
                            crate::smelt_term::WinId(win_id),
                            key,
                            crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
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
                            crate::smelt_term::WinId(win_id),
                            event,
                            crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
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
                let prev = app
                    .ui
                    .win_clear_keymap(crate::smelt_term::WinId(win_id), key);
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
                let prev = app.ui.win_clear_event_by_id(
                    crate::smelt_term::WinId(win_id),
                    event,
                    callback_id,
                );
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            Ok(())
        })?,
    )?;
    smelt.set("win", win_tbl)?;
    Ok(())
}
