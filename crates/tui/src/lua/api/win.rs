//! `smelt.win` — focus, keymap/event registration, buf resolution, window config. UiHost-only.

use crate::lua::{parse_keybind, LuaShared};
use lua_doc_derive::lua_module;
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;
use smelt_core::lua::lua_type::LuaCallback;
use std::sync::Arc;

/// Window-event names accepted by `smelt.win.on_event` and
/// `smelt.win.clear_event`. Maps onto the internal `WinEvent` enum.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.win.Event")]
pub enum LuaWinEvent {
    /// Window was created.
    Open,
    /// Window was closed.
    Close,
    /// Window gained focus.
    #[lua(rename = "focus")]
    FocusGained,
    /// Window lost focus.
    #[lua(rename = "blur")]
    FocusLost,
    /// Selection within a list-style window changed.
    SelectionChanged,
    /// User confirmed input (Enter, button click, etc.).
    Submit,
    /// Buffer text changed.
    TextChanged,
    /// User cancelled / dismissed the window.
    Dismiss,
    /// Periodic tick for animation/refresh.
    Tick,
}

impl From<LuaWinEvent> for crate::smelt_term::WinEvent {
    fn from(e: LuaWinEvent) -> Self {
        use crate::smelt_term::WinEvent;
        match e {
            LuaWinEvent::Open => WinEvent::Open,
            LuaWinEvent::Close => WinEvent::Close,
            LuaWinEvent::FocusGained => WinEvent::FocusGained,
            LuaWinEvent::FocusLost => WinEvent::FocusLost,
            LuaWinEvent::SelectionChanged => WinEvent::SelectionChanged,
            LuaWinEvent::Submit => WinEvent::Submit,
            LuaWinEvent::TextChanged => WinEvent::TextChanged,
            LuaWinEvent::Dismiss => WinEvent::Dismiss,
            LuaWinEvent::Tick => WinEvent::Tick,
        }
    }
}

#[lua_module(
    name = "smelt.win",
    doc = "Window lifecycle, focus, keymap/event registration, and buffer resolution. UiHost-only — windows are layout leaves that render a buffer onto the screen."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let win_tbl = lua.create_table()?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "focus",
        "Return which top-level pane currently has focus: `\"transcript\"` or `\"prompt\"`.",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| match app.app_focus {
                crate::app::AppFocus::Content => "transcript".to_string(),
                crate::app::AppFocus::Prompt => "prompt".to_string(),
            })
            .unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "close",
        "Close the overlay leaf identified by `id`. No-op if the window does not exist or is not an overlay leaf.",
        &["id"],
        lua,
        |_, id: u64|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                app.close_overlay_leaf(crate::smelt_term::WinId(id));
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "open",
        "Open a split window over the buffer `buf_id`. `opts.region` picks the layout slot (default `\"lua_overlay\"`); `opts.focusable`, `opts.cursor_line_highlight`, and `opts.vim_enabled` toggle behaviour. `opts.pad_left` / `opts.pad_right` reserve gutter columns on either side. Returns the new `WinId` or `nil` if no slot was available.",
        &["buf_id", "opts"],
        lua,
        |_, (buf_id, opts): (u64, Option<mlua::Table>)| -> LuaResult<Option<u64>> {
            let win = crate::lua::with_app(|app| {
                let region = opts
                    .as_ref()
                    .and_then(|t| t.get::<String>("region").ok())
                    .unwrap_or_else(|| "lua_overlay".to_string());
                let pad_left = opts
                    .as_ref()
                    .and_then(|t| t.get::<u64>("pad_left").ok())
                    .unwrap_or(0) as u16;
                let pad_right = opts
                    .as_ref()
                    .and_then(|t| t.get::<u64>("pad_right").ok())
                    .unwrap_or(0) as u16;
                let win = app.ui.win_open_split(
                    crate::smelt_term::BufId(buf_id),
                    crate::smelt_term::SplitConfig {
                        region,
                        gutters: crate::smelt_term::layout::Gutters {
                            pad_left,
                            pad_right,
                            scrollbar: false,
                        },
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
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "configure_list",
        "Mark `win_id` as a list leaf with arrow-key/scroll handling and place the initial cursor at row `initial_cursor` (clamped to `u16`).",
        &["win_id", "initial_cursor"],
        lua,
        |_, (win_id, initial_cursor): (u64, Option<u64>)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::configure_list_leaf(
                    app,
                    crate::smelt_term::WinId(win_id),
                    initial_cursor.unwrap_or(0).min(u16::MAX as u64) as u16,
                );
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "move_cursor",
        "Move `win_id`'s cursor by `delta` rows (clamped to the buffer's line count), keep the row on-screen by adjusting `scroll_top`, and emit `selection_changed`. Lets an external panel (e.g. a docked search input) drive a list without holding focus.",
        &["win_id", "delta"],
        lua,
        |_, (win_id, delta): (u64, i64)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::move_cursor(
                    app,
                    crate::smelt_term::WinId(win_id),
                    delta as isize,
                );
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "set_cursor_row",
        "Place `win_id`'s cursor at absolute `row` (clamped to the buffer's line count). Adjusts `scroll_top` so the row stays on-screen and emits `selection_changed` if the position actually moved.",
        &["win_id", "row"],
        lua,
        |_, (win_id, row): (u64, u64)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::set_cursor_row(
                    app,
                    crate::smelt_term::WinId(win_id),
                    row.min(u16::MAX as u64) as u16,
                );
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "cursor_row",
        "Return the current cursor row (0-based) of `win_id`, or `nil` if the window doesn't exist.",
        &["win_id"],
        lua,
        |_, win_id: u64| -> LuaResult<Option<u64>> {
            let row = crate::lua::try_with_app(|app| {
                crate::lua::ui_ops::cursor_row(app, crate::smelt_term::WinId(win_id))
            })
            .flatten();
            Ok(row.map(|r| r as u64))
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "configure_input",
        "Mark `win_id` as a single-line text input leaf with the same editing keymap as the prompt. If `placeholder` is non-empty, seed the buffer with dim placeholder text; the first printable keystroke clears it and starts a fresh line.",
        &["win_id", "placeholder"],
        lua,
        |_, (win_id, placeholder): (u64, Option<String>)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                crate::lua::ui_ops::configure_input_leaf(
                    app,
                    crate::smelt_term::WinId(win_id),
                    placeholder.unwrap_or_default(),
                );
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "buf",
        "Return the buffer id backing window `id`, or `nil` if no such window exists.",
        &["id"],
        lua,
        |_, id: u64| -> LuaResult<Option<u64>> {
            let buf = crate::lua::try_with_app(|app| {
                app.ui.win(crate::smelt_term::WinId(id)).map(|w| w.buf.0)
            })
            .flatten();
            Ok(buf)
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "rect",
        "Return the window's current viewport rect as `{ row, col, width, height }`, or `nil` until the first render lays it out.",
        &["id"],
        lua,
        |lua, id: u64| -> LuaResult<mlua::Value> {
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
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "set_focus",
        "Move keyboard focus to window `id`. No-op if the window is not focusable or does not exist.",
        &["id"],
        lua,
        |_, id: u64| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                app.ui.set_focus(crate::smelt_term::WinId(id));
            });
            Ok(())
        },
    )?;
    {
        let s = shared.clone();
        register_ui_fn(
            &win_tbl,
            "smelt.win",
            "set_keymap",
            "Install `func` as the handler for `key_str` on window `win_id`. Replaces any existing binding (the displaced Lua handle is freed). Raises on unknown key strings.",
            &["win_id", "key_str", "func"],
            lua,
            move |lua,
                  (win_id, key_str, func): (u64, String, LuaCallback<mlua::Table, ()>)|
                  -> LuaResult<()> {
                let Some(key) = parse_keybind(&key_str) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "win.set_keymap: unknown key `{key_str}`"
                    )));
                };
                let id = crate::lua::register_callback_handle(&s, lua, func.into_inner())?;
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
        )?;
    }
    {
        let s = shared.clone();
        register_ui_fn(
            &win_tbl,
            "smelt.win",
            "on_event",
            "Subscribe `func` to event `event` on window `win_id`. Returns a callback id usable with `clear_event`.",
            &["win_id", "event", "func"],
            lua,
            move |lua,
                  (win_id, event, func): (
                u64,
                LuaWinEvent,
                LuaCallback<mlua::Table, ()>,
            )|
                  -> LuaResult<u64> {
                let id = crate::lua::register_callback_handle(&s, lua, func.into_inner())?;
                crate::lua::with_app(|app| {
                    app.ui.win_on_event(
                        crate::smelt_term::WinId(win_id),
                        event.into(),
                        crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
                    );
                });
                Ok(id)
            },
        )?;
    }
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "clear_keymap",
        "Remove a per-window key binding for `win_id` previously installed via `set_keymap`. The associated Lua handle is freed.",
        &["win_id", "key_str"],
        lua,
        |_, (win_id, key_str): (u64, String)|  -> LuaResult<()>{
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
        },
    )?;
    register_ui_fn(
        &win_tbl,
        "smelt.win",
        "clear_event",
        "Remove a per-window event handler by `callback_id` (returned from `on_event`). The associated Lua handle is freed.",
        &["win_id", "event", "callback_id"],
        lua,
        |_, (win_id, event, callback_id): (u64, LuaWinEvent, u64)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let prev = app.ui.win_clear_event_by_id(
                    crate::smelt_term::WinId(win_id),
                    event.into(),
                    callback_id,
                );
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            Ok(())
        },
    )?;
    smelt.set("win", win_tbl)?;
    Ok(())
}
