//! `smelt.win` - Win handle. UiHost-only.
//!
//! `smelt.win.new(buf, opts?)` opens a split window over `buf` (a `Buf`
//! handle) and returns a `Win` userdata. `opts.name` opts the window
//! into hot-reload survival.
//!
//! Keymap / event registrations return a `Reg` userdata with a
//! single `:remove()` method that frees the binding.

use crate::lua::{parse_keybind, LuaShared};
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use smelt_core::lua::LuaHandle;
use std::sync::{Arc, OnceLock};

/// Window-event names accepted by `win:on(event, fn)`. Maps onto the
/// internal `WinEvent` enum.
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
    /// Mouse-down landed on the window. Payload: `{ x, y, button }`
    /// (leaf-relative cell coords, `button` ∈ `"left"|"right"|"middle"`).
    /// Non-focusable, non-selectable windows still receive it.
    Press,
    /// Mouse-up after a `Press` on this window. Fires on the leaf that owned
    /// the press, even if the pointer drifted out (capture). Same payload as
    /// `Press`.
    Release,
    /// Mouse drag (motion with button held) while this window owns the press.
    /// Same payload as `Press`; coords are leaf-relative for the new position.
    Drag,
    /// The pointer moved onto the window with no button held. Payload:
    /// `{ x, y }` (leaf-relative).
    Enter,
    /// The pointer left the window with no button held. Payload: `{ x, y }`
    /// relative to this window, so it may lie outside it.
    Leave,
    /// The pointer moved within the window with no button held. Payload: `{ x, y }`.
    Move,
    /// Window's scroll state changed. Payload: `{ top, follow }` where
    /// `top` is the new `scroll_top` and `follow` is the pin-to-tail flag.
    Scrolled,
    /// Leaf's viewport rect changed. Payload: `{ x, y, width, height,
    /// content_width }` for the new outer rect and inner cell budget.
    /// Fires once after the first paint and on every later resize/reflow.
    Resized,
    /// User accepted a placeholder via one of its `accept_keys`. Payload:
    /// `{ text = <accepted text> }`.
    #[lua(rename = "placeholder_accepted")]
    PlaceholderAccepted,
    /// User dismissed a placeholder via one of its `dismiss_keys`. Payload:
    /// `{ text = <dismissed text> }`.
    #[lua(rename = "placeholder_dismissed")]
    PlaceholderDismissed,
}

impl From<LuaWinEvent> for crate::smelt_edit::WinEvent {
    fn from(e: LuaWinEvent) -> Self {
        use crate::smelt_edit::WinEvent;
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
            LuaWinEvent::Press => WinEvent::Press,
            LuaWinEvent::Release => WinEvent::Release,
            LuaWinEvent::Drag => WinEvent::Drag,
            LuaWinEvent::Enter => WinEvent::PointerEnter,
            LuaWinEvent::Leave => WinEvent::PointerLeave,
            LuaWinEvent::Move => WinEvent::PointerMove,
            LuaWinEvent::Scrolled => WinEvent::Scrolled,
            LuaWinEvent::Resized => WinEvent::Resized,
            LuaWinEvent::PlaceholderAccepted => WinEvent::PlaceholderAccepted,
            LuaWinEvent::PlaceholderDismissed => WinEvent::PlaceholderDismissed,
        }
    }
}

/// Lua-side handle for a `WinId`.
#[derive(Clone, Copy, Debug)]
pub struct LuaWin {
    pub(crate) id: crate::smelt_edit::WinId,
}

impl LuaType for LuaWin {
    fn lua_type() -> String {
        "smelt.win.Win".into()
    }
}

impl FromLua for LuaWin {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::UserData(ud) => Ok(*ud.borrow::<LuaWin>()?),
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "smelt.win.Win".into(),
                message: Some("expected a Win userdata (built via `smelt.win.new(...)`)".into()),
            }),
        }
    }
}

/// Built-in window ids are owned by the app shell; closing them through Lua
/// would tear down the chrome the host depends on, so `Win:close()` no-ops
/// for these. Plugin-owned overlay leaves close normally.
fn is_builtin_win(id: crate::smelt_edit::WinId) -> bool {
    matches!(id, crate::app::TRANSCRIPT_WIN | crate::app::PROMPT_WIN)
}

/// Lua-side handle for a window-owned decoration.
#[derive(Clone, Copy, Debug)]
pub struct LuaDecoration {
    pub(crate) id: crate::smelt_edit::DecorationId,
}

impl LuaType for LuaDecoration {
    fn lua_type() -> String {
        "smelt.win.Decoration".into()
    }
}

impl mlua::UserData for LuaDecoration {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Decoration#{}", this.id.0))
        });
        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.close_decoration(this.id));
            Ok(())
        });
    }
}

impl mlua::UserData for LuaWin {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Win#{}", this.id.0))
        });
        methods.add_meta_method(mlua::MetaMethod::Eq, |_, this, other: mlua::AnyUserData| {
            Ok(other
                .borrow::<LuaWin>()
                .is_ok_and(|other| other.id == this.id))
        });

        // ── close / focus ──────────────────────────────────────────
        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            if is_builtin_win(this.id) {
                return Ok(());
            }
            crate::lua::with_ui_host(|host| host.close_overlay_leaf(this.id));
            Ok(())
        });

        methods.add_method("focus", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.focus_window(this.id));
            Ok(())
        });

        methods.add_method("focused", |_, this, ()| -> LuaResult<bool> {
            Ok(crate::lua::with_ui_host(|host| {
                host.is_window_focused(this.id)
            }))
        });

        methods.add_method(
            "resize",
            |_, this, (axis, delta): (String, i32)| -> LuaResult<bool> {
                let axis = match axis.as_str() {
                    "width" => crate::smelt_edit::layout::Axis::Horizontal,
                    "height" => crate::smelt_edit::layout::Axis::Vertical,
                    _ => return Err(LuaError::external("resize axis must be width or height")),
                };
                Ok(crate::lua::with_ui_host(|host| {
                    host.resize_window(this.id, axis, delta)
                }))
            },
        );
        methods.add_method("equalize", |_, this, ()| -> LuaResult<bool> {
            Ok(crate::lua::with_ui_host(|host| {
                host.with_ui(|ui| ui.equalize_window(this.id))
            }))
        });

        // ── buf - return the backing Buf handle ────────────────────
        methods.add_method(
            "buf",
            |_, this, ()| -> LuaResult<Option<super::buf::LuaBuf>> {
                let bid = crate::lua::try_with_ui_host(|host| {
                    host.with_ui(|ui| ui.win(this.id).map(|window| window.backing_buffer()))
                })
                .flatten();
                Ok(bid.map(|id| super::buf::LuaBuf { id }))
            },
        );

        methods.add_function(
            "document",
            |_,
             (this_ud, source): (
                mlua::AnyUserData,
                Option<smelt_core::lua::api::document::LuaDocument>,
            )|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                if is_builtin_win(this.id) {
                    return Err(LuaError::external(
                        "cannot replace a built-in window's document",
                    ));
                }
                crate::lua::with_ui_host(|host| {
                    host.with_ui(|ui| {
                        ui.set_window_document(this.id, source.map(|source| source.0));
                    })
                });
                Ok(this_ud)
            },
        );

        methods.add_function(
            "pan",
            |_, (this_ud, delta): (mlua::AnyUserData, i16)| -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                crate::lua::with_ui_host(|host| {
                    host.with_ui(|ui| {
                        if let Some(win) = ui.win_mut(this.id) {
                            if !win.wrap {
                                win.scroll_left = win.scroll_left.saturating_add_signed(delta);
                            }
                        }
                    })
                });
                Ok(this_ud)
            },
        );

        // ── rect - current layout-resolved bounds ──────────────────
        // Prefer `paint_rect` from the current layout tree so renderers and
        // resize handlers don't observe the previous painted viewport for one
        // frame after a layout change.
        methods.add_method("rect", |lua, this, ()| -> LuaResult<mlua::Value> {
            let rect = crate::lua::try_with_ui_host(|host| {
                host.with_ui(|ui| {
                    let window = ui.win(this.id)?;
                    ui.paint_rect(this.id.into())
                        .or_else(|| window.viewport.map(|viewport| viewport.rect))
                })
            })
            .flatten();
            match rect {
                Some(r) => Ok(mlua::Value::Table(crate::lua::parse::rect_to_lua(lua, r)?)),
                None => Ok(mlua::Value::Nil),
            }
        });

        // Inner-content width in cells, with gutter and pad_left/pad_right
        // already subtracted. Prefer the layout-resolved rect so renderers see
        // the current terminal size during the first frame of a resize, before
        // the window viewport has been refreshed by painting.
        methods.add_method("content_width", |_, this, ()| -> LuaResult<mlua::Value> {
            let w = crate::lua::try_with_ui_host(|host| {
                host.with_ui(|ui| ui.win_content_width(this.id))
            })
            .flatten();
            Ok(match w {
                Some(n) => mlua::Value::Integer(n as i64),
                None => mlua::Value::Nil,
            })
        });

        // ── decorate(opts) - owner-scoped surface ─────────────────────
        methods.add_method(
            "decorate",
            |_, this, opts: mlua::Table| -> LuaResult<LuaDecoration> {
                let id = crate::lua::with_ui_host(|host| host.open_decoration(this.id, opts))
                    .map_err(|e| LuaError::RuntimeError(format!("win:decorate: {e}")))?;
                smelt_core::lua::reg::RegistrationScope::resource(
                    "decoration",
                    u64::from(id.0),
                    move || {
                        crate::lua::try_with_ui_host(|host| {
                            host.close_decoration(id);
                            true
                        })
                        .unwrap_or(false)
                    },
                );
                Ok(LuaDecoration { id })
            },
        );

        // ── cursor: get / set ──────────────────────────────────────
        methods.add_function(
            "cursor",
            |_, (this_ud, row): (mlua::AnyUserData, Option<u64>)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaWin>()?;
                match row {
                    Some(r) => {
                        if this.id != crate::app::PROMPT_WIN {
                            crate::lua::with_ui_host(|host| host.set_cursor_row(this.id, r));
                        }
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let row =
                            crate::lua::try_with_ui_host(|host| host.cursor_row(this.id)).flatten();
                        Ok(match row {
                            Some(r) => mlua::Value::Integer(r as i64),
                            None => mlua::Value::Nil,
                        })
                    }
                }
            },
        );

        // ── move_cursor(delta) - chainable ─────────────────────────
        methods.add_function(
            "move_cursor",
            |_, (this_ud, delta): (mlua::AnyUserData, i64)| -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                if this.id != crate::app::PROMPT_WIN {
                    crate::lua::with_ui_host(|host| host.move_cursor(this.id, delta as isize));
                }
                Ok(this_ud)
            },
        );

        // ── reveal(row, opts?) - chainable ────────────────────────
        methods.add_function(
            "reveal",
            |_,
             (this_ud, row, opts): (mlua::AnyUserData, u64, Option<mlua::Table>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let top_padding = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("top_padding").ok().flatten())
                    .unwrap_or(0);
                let bottom_padding = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("bottom_padding").ok().flatten())
                    .unwrap_or(0);
                let cursor = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("cursor").ok().flatten())
                    .unwrap_or(true);
                let jump = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("jump").ok().flatten())
                    .unwrap_or(false);
                if this.id != crate::app::PROMPT_WIN {
                    crate::lua::with_ui_host(|host| {
                        if jump && cursor {
                            host.record_window_jump(this.id);
                        }
                        host.reveal_position(this.id, row, top_padding, bottom_padding, cursor)
                    });
                }
                Ok(this_ud)
            },
        );

        // ── jump(opts) → bool ─────────────────────────────────────
        methods.add_function(
            "jump",
            |_, (this_ud, opts): (mlua::AnyUserData, mlua::Table)| -> LuaResult<bool> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let count = |key: &str| -> LuaResult<Option<usize>> {
                    Ok(opts
                        .get::<Option<u64>>(key)?
                        .map(|count| (count as usize).max(1)))
                };
                let previous = opts.get::<Option<bool>>("previous")?.unwrap_or(false);
                let jump = match (count("older")?, count("newer")?, previous) {
                    (Some(count), None, false) => crate::smelt_edit::JumpMove::Older(count),
                    (None, Some(count), false) => crate::smelt_edit::JumpMove::Newer(count),
                    (None, None, true) => crate::smelt_edit::JumpMove::Previous,
                    _ => {
                        return Err(mlua::Error::runtime(
                            "win:jump: pass exactly one of `older`, `newer` or `previous`",
                        ))
                    }
                };
                if this.id == crate::app::PROMPT_WIN {
                    return Ok(false);
                }
                Ok(crate::lua::with_ui_host(|host| {
                    host.jump_window(this.id, jump)
                }))
            },
        );

        // ── key(chord, fn) → Reg ───────────────────────────────────
        methods.add_function(
            "key",
            |lua,
             (this_ud, chord, func): (mlua::AnyUserData, String, LuaCallback<mlua::Table, ()>)|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let Some(key) = parse_keybind(&chord) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "win:key: unknown chord `{chord}`"
                    )));
                };
                let shared = current_shared(lua)?;
                let id = crate::lua::register_callback_handle(&shared, lua, func.into_inner())?;
                crate::lua::with_ui_host(|host| {
                    host.set_window_keymap(
                        this.id,
                        key,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                });
                let win = this.id;
                Ok(LuaReg::new(move || {
                    crate::lua::app_ref::defer_registered_lua_operation(
                        &shared,
                        id,
                        crate::lua::app_ref::DeferredLuaOperation::WindowKeymap {
                            window: win,
                            key,
                        },
                    )
                }))
            },
        );

        // ── placeholder(text, opts?) → LuaWin ──────────────────────
        methods.add_function(
            "placeholder",
            |_,
             (this_ud, text, opts): (mlua::AnyUserData, String, Option<mlua::Table>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                if text.contains('\n') {
                    return Err(mlua::Error::RuntimeError(
                        "Win:placeholder: text must be a single line; split before calling \
                         (e.g. `text:match(\"[^\\n]+\")`)"
                            .into(),
                    ));
                }
                let accept_keys = parse_chord_list(opts.as_ref(), "accept_keys")?;
                let dismiss_keys =
                    parse_chord_list_with_default(opts.as_ref(), "dismiss_keys", &["esc", "c-c"])?;
                crate::lua::with_ui_host(|host| {
                    host.set_placeholder(
                        this.id,
                        text,
                        crate::app::PlaceholderOpts {
                            accept_keys,
                            dismiss_keys,
                        },
                    );
                });
                Ok(this_ud)
            },
        );

        methods.add_method("clear_placeholder", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.clear_placeholder(this.id));
            Ok(())
        });

        methods.add_method(
            "placeholder_text",
            |_, this, ()| -> LuaResult<Option<String>> {
                Ok(crate::lua::try_with_ui_host(|host| host.placeholder_text(this.id)).flatten())
            },
        );

        // ── configure(opts) - chainable ────────────────────────────
        methods.add_function(
            "configure",
            |lua,
             (this_ud, opts): (mlua::AnyUserData, mlua::Table)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let mut reader = smelt_core::lua::options::LuaOptions::new(opts, "win");
                let opts = parse_window_opts(lua, &mut reader)?;
                reader.finish()?;
                opts.check_configuration()?;
                crate::lua::with_ui_host(|host| {
                    host.with_ui(|ui| opts.apply(ui, this.id));
                });
                Ok(this_ud)
            },
        );

        // ── row_highlights(specs?) - chainable ─────────────────────
        methods.add_function(
            "row_highlights",
            |_,
             (this_ud, specs): (mlua::AnyUserData, Option<mlua::Table>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let highlights = parse_row_highlights(specs.as_ref())?;
                crate::lua::with_ui_host(|host| {
                    host.with_ui(|ui| {
                        if let Some(window) = ui.win_mut(this.id) {
                            window.set_row_highlights(highlights);
                        }
                    });
                });
                Ok(this_ud)
            },
        );

        // ── on(event, fn) → Reg ────────────────────────────────────
        //
        // Headless-safe: when no frontend host is in scope (e.g. autoload
        // running before `bring_up_lua`, or a unit test driving the Lua
        // runtime directly), there is no `Ui` to register against and no
        // event source to ever fire. The call silently no-ops and the
        // returned Reg is inert. Callers that need a guaranteed live
        // subscription should re-call after `lifecycle.on_ready`.
        methods.add_function(
            "on",
            |lua,
             (this_ud, event, func): (
                mlua::AnyUserData,
                LuaWinEvent,
                LuaCallback<mlua::Table, ()>,
            )|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let shared = current_shared(lua)?;
                let id = crate::lua::register_callback_handle(&shared, lua, func.into_inner())?;
                let event: crate::smelt_edit::WinEvent = event.into();
                let installed = crate::lua::try_with_ui_host(|host| {
                    host.register_window_event(
                        this.id,
                        event,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                })
                .is_some();
                if !installed {
                    // Drop the orphan callback so the handle table doesn't leak.
                    if let Ok(mut cbs) = shared.callbacks.lock() {
                        cbs.remove(&id);
                    }
                    return Ok(LuaReg::new(|| false));
                }
                let win = this.id;
                Ok(LuaReg::new(move || {
                    crate::lua::app_ref::defer_registered_lua_operation(
                        &shared,
                        id,
                        crate::lua::app_ref::DeferredLuaOperation::WindowEvent {
                            window: win,
                            event,
                            callback_id: id,
                        },
                    )
                }))
            },
        );

        // ── scroll: get / set / jump-to-tail ───────────────────────
        // `win:scroll()` returns `{ top, left, follow, total, viewport, max, overflow, at_top, at_bottom, needs_tail_repin }`.
        // `needs_tail_repin` is true whenever content overflows and the viewport
        // is not positioned at the current bottom, regardless of follow state.
        // `win:scroll(integer)` pins the viewport at that `scroll_top`.
        // `win:scroll("tail")` jumps the viewport to the buffer tail while
        // keeping the cursor on the same screen row, then enables tail-follow.
        methods.add_function(
            "scroll",
            |lua, (this_ud, arg): (mlua::AnyUserData, mlua::Value)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaWin>()?;
                match arg {
                    mlua::Value::Nil => {
                        let info = crate::lua::try_with_ui_host(|host| {
                            host.window_scroll_snapshot(this.id)
                        })
                        .flatten();
                        match info {
                            Some(info) => {
                                let t = lua.create_table()?;
                                t.set("top", info.top)?;
                                let left = crate::lua::try_with_ui_host(|host| {
                                    host.with_ui(|ui| ui.win(this.id).map(|win| win.scroll_left))
                                })
                                .flatten()
                                .unwrap_or(0);
                                t.set("left", left)?;
                                t.set("follow", info.follow)?;
                                t.set("total", info.total)?;
                                t.set("viewport", info.viewport)?;
                                t.set("max", info.max)?;
                                t.set("overflow", info.overflow)?;
                                t.set("at_top", info.top == 0)?;
                                t.set("at_bottom", info.at_bottom)?;
                                t.set("needs_tail_repin", info.needs_tail_repin)?;
                                Ok(mlua::Value::Table(t))
                            }
                            None => Ok(mlua::Value::Nil),
                        }
                    }
                    mlua::Value::Integer(n) => {
                        crate::lua::with_ui_host(|host| {
                            host.scroll_window(
                                this.id,
                                crate::app::transcript_scroll::WindowScrollCommand::Pin(
                                    n.max(0) as u64
                                ),
                            );
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    mlua::Value::String(s) if s.to_str()?.as_ref() == "tail" => {
                        crate::lua::with_ui_host(|host| {
                            host.scroll_window(
                                this.id,
                                crate::app::transcript_scroll::WindowScrollCommand::Tail,
                            );
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    other => Err(mlua::Error::RuntimeError(format!(
                        "win:scroll: expected nil, integer, or \"tail\"; got {}",
                        other.type_name()
                    ))),
                }
            },
        );

        // ── link_scroll(...) - chainable; variadic over Win ────────
        methods.add_function(
            "link_scroll",
            |_,
             (this_ud, others): (mlua::AnyUserData, mlua::Variadic<LuaWin>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let mut ids: Vec<crate::smelt_edit::WinId> = vec![this.id];
                for w in others {
                    ids.push(w.id);
                }
                crate::lua::with_ui_host(|host| host.with_ui(|ui| ui.link_scroll(&ids)));
                Ok(this_ud)
            },
        );

        // ── set_renderer(fn) - register/clear retained window renderer ──
        methods.add_function(
            "set_renderer",
            |lua,
             (this_ud, func): (mlua::AnyUserData, Option<mlua::Function>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let shared = current_shared(lua)?;
                let handle = match func {
                    Some(f) => Some(LuaHandle::from_func(lua, f)?),
                    None => None,
                };
                let installed = handle.is_some();
                if let Ok(mut map) = shared.win_renderers.lock() {
                    match handle {
                        Some(handle) => {
                            map.insert(
                                this.id.0,
                                smelt_core::lua::RegisteredWinRenderer {
                                    handle,
                                    dirty: true,
                                    rendered_size: None,
                                },
                            );
                        }
                        None => {
                            map.remove(&this.id.0);
                        }
                    }
                }
                if installed && smelt_core::lua::reg::RegistrationScope::owns("window", this.id.0) {
                    let weak = Arc::downgrade(&shared.core);
                    let win = this.id;
                    smelt_core::lua::reg::RegistrationScope::resource(
                        "renderer",
                        win.0,
                        move || {
                            weak.upgrade().is_some_and(|shared| {
                                shared
                                    .win_renderers
                                    .lock()
                                    .is_ok_and(|mut renderers| renderers.remove(&win.0).is_some())
                            })
                        },
                    );
                }
                Ok(this_ud)
            },
        );

        methods.add_method(
            "invalidate_after",
            |lua, this, ms: u64| -> LuaResult<LuaReg> {
                let shared = current_shared(lua)?;
                let weak = Arc::downgrade(&shared.core);
                let win = this.id;
                let callback = lua.create_function(move |_, ()| {
                    if let Some(shared) = weak.upgrade() {
                        if let Ok(mut renderers) = shared.win_renderers.lock() {
                            if let Some(renderer) = renderers.get_mut(&win.0) {
                                renderer.dirty = true;
                            }
                        }
                    }
                    Ok(())
                })?;
                smelt_core::lua::api::set_timer(lua, &shared.core, ms, callback)
            },
        );

        // ── invalidate() - repaint retained window content ──
        methods.add_function(
            "invalidate",
            |lua, this_ud: mlua::AnyUserData| -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let shared = current_shared(lua)?;
                if let Ok(mut map) = shared.win_renderers.lock() {
                    if let Some(renderer) = map.get_mut(&this.id.0) {
                        renderer.dirty = true;
                    }
                }
                Ok(this_ud)
            },
        );
    }
}

/// Parse `opts.<field>` as a list of chord strings (e.g. `{"tab", "right"}`).
/// Returns the empty list when the field is missing or `nil`.
fn parse_chord_list(
    opts: Option<&mlua::Table>,
    field: &str,
) -> LuaResult<Vec<crate::smelt_edit::KeyBind>> {
    let Some(t) = opts else {
        return Ok(Vec::new());
    };
    match t.get::<Option<mlua::Value>>(field)? {
        None | Some(mlua::Value::Nil) => Ok(Vec::new()),
        Some(mlua::Value::Table(arr)) => collect_chords(&arr, field),
        Some(other) => Err(mlua::Error::RuntimeError(format!(
            "Win:placeholder: opts.{field} must be an array of chord strings; got {}",
            other.type_name()
        ))),
    }
}

/// Same as [`parse_chord_list`] but substitutes `defaults` when the field
/// is omitted (vs. explicitly `nil` or empty, which still mean "none").
fn parse_chord_list_with_default(
    opts: Option<&mlua::Table>,
    field: &str,
    defaults: &[&str],
) -> LuaResult<Vec<crate::smelt_edit::KeyBind>> {
    let Some(t) = opts else {
        return parse_default(defaults, field);
    };
    match t.get::<Option<mlua::Value>>(field)? {
        None => parse_default(defaults, field),
        Some(mlua::Value::Nil) => Ok(Vec::new()),
        Some(mlua::Value::Table(arr)) => collect_chords(&arr, field),
        Some(other) => Err(mlua::Error::RuntimeError(format!(
            "Win:placeholder: opts.{field} must be an array of chord strings; got {}",
            other.type_name()
        ))),
    }
}

fn parse_default(defaults: &[&str], field: &str) -> LuaResult<Vec<crate::smelt_edit::KeyBind>> {
    defaults
        .iter()
        .map(|c| {
            parse_keybind(c).ok_or_else(|| {
                mlua::Error::RuntimeError(format!(
                    "Win:placeholder: default opts.{field} contains unknown chord `{c}`"
                ))
            })
        })
        .collect()
}

fn collect_chords(arr: &mlua::Table, field: &str) -> LuaResult<Vec<crate::smelt_edit::KeyBind>> {
    let mut out = Vec::new();
    for pair in arr.clone().pairs::<mlua::Value, String>() {
        let (_, chord) = pair?;
        let kb = parse_keybind(&chord).ok_or_else(|| {
            mlua::Error::RuntimeError(format!(
                "Win:placeholder: opts.{field} contains unknown chord `{chord}`"
            ))
        })?;
        out.push(kb);
    }
    Ok(out)
}

fn parse_row_highlights(
    specs: Option<&mlua::Table>,
) -> LuaResult<Vec<crate::smelt_edit::RowHighlight>> {
    let Some(specs) = specs else {
        return Ok(Vec::new());
    };
    let mut highlights = Vec::new();
    for spec in specs.clone().sequence_values::<mlua::Table>() {
        let spec = spec?;
        let hl_group = smelt_buffer::theme::intern(
            &spec
                .get::<Option<String>>("hl_group")?
                .unwrap_or_else(|| "CursorLine".into()),
        );
        let mode = match spec
            .get::<Option<String>>("mode")?
            .unwrap_or_else(|| "always".into())
            .as_str()
        {
            "always" => crate::smelt_edit::RowHighlightMode::Always,
            "focused" => crate::smelt_edit::RowHighlightMode::WhenFocused,
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "Win:row_highlights: mode must be 'always' or 'focused', got `{other}`"
                )));
            }
        };
        let width = match spec
            .get::<Option<String>>("width")?
            .unwrap_or_else(|| "full".into())
            .as_str()
        {
            "full" => crate::smelt_edit::RowHighlightWidth::FullWindow,
            "content" => crate::smelt_edit::RowHighlightWidth::Content,
            other => {
                return Err(mlua::Error::RuntimeError(format!(
                    "Win:row_highlights: width must be 'full' or 'content', got `{other}`"
                )));
            }
        };
        if spec.get::<Option<bool>>("cursor")?.unwrap_or(false) {
            highlights.push(crate::smelt_edit::RowHighlight::cursor(
                hl_group, mode, width,
            ));
            continue;
        }
        let start = spec.get::<Option<u64>>("start")?.ok_or_else(|| {
            mlua::Error::RuntimeError("Win:row_highlights: range highlight requires start".into())
        })? as crate::smelt_edit::RowIndex;
        let end = spec
            .get::<Option<u64>>("end")?
            .unwrap_or_else(|| start.saturating_add(1))
            as crate::smelt_edit::RowIndex;
        if start < end {
            highlights.push(crate::smelt_edit::RowHighlight::range(
                start..end,
                hl_group,
                mode,
                width,
            ));
        }
    }
    Ok(highlights)
}

/// Recover the `LuaShared` from the Lua state's app registry. Used by
/// methods that need to register Lua callback handles.
pub(super) fn current_shared(lua: &Lua) -> LuaResult<Arc<LuaShared>> {
    // Stored under the named registry value `__smelt_shared` by
    // `register_api` below. mlua's named-registry API is the cheapest
    // pin: same key, same Arc clone, no per-call lookup overhead worth
    // measuring next to the kbd handler itself.
    lua.named_registry_value::<mlua::AnyUserData>("__smelt_shared")?
        .borrow::<SharedHandle>()
        .map(|h| h.0.clone())
}

/// Userdata wrapper so an `Arc<LuaShared>` can sit in the Lua registry.
pub(crate) struct SharedHandle(pub(crate) Arc<LuaShared>);

impl mlua::UserData for SharedHandle {}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "win",
        "Window handle constructor. `smelt.win.new(buf, opts?)` opens a split window over `buf` and returns a `Win` userdata. \
`opts.name` opts the window into hot-reload survival. UiHost-only - windows are layout leaves that render a buffer onto the screen.",
        Tier::UiHost,
    )?;

    // Stash the shared state so per-method registrations (key/on) can
    // recover it without each method capturing its own clone.
    lua.set_named_registry_value(
        "__smelt_shared",
        lua.create_userdata(SharedHandle(shared.clone()))?,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.win.Win",
        classification: smelt_core::lua::doc::classification_for_type("smelt.win.Win"),
        doc: "Window handle returned by `smelt.win.new(buf, opts?)`. Handles to the same window compare equal with `==`. Most setters return the same handle for chaining; resize operations report whether a resizable owner exists.",
        fields: smelt_core::class_methods! {
            "close" => fn() -> (), "Close the overlay leaf. No-op if the window is already closed.",
            "focus" => fn() -> (), "Move keyboard focus to this window. No-op if the window is not focusable.",
            "focused" => fn() -> bool, "Whether this window holds keyboard focus.",
            "resize" => fn(axis: String, delta: i32) -> bool, "Grow this window along width or height by signed terminal cells; negative shrinks. Resizes the nearest matching hsplit/vsplit, or the containing overlay/docked dialog when no split applies. Shares mouse bounds and preserves focus and document state. Returns false if no resizable owner exists.",
            "equalize" => fn() -> bool, "Balance all enclosing hsplit/vsplit panes, respecting their minimum sizes. Does not change focus or document state. Returns false if the window is not in a resizable split.",
            "buf" => fn() -> Option<super::buf::LuaBuf>, "Return the original backing Buf handle, including while a native document is attached, or `nil` if the window is gone.",
            "document" => fn(source: Option<smelt_core::lua::api::document::LuaDocument>) -> LuaWin, "Attach a native indexed row source to a plugin window, or detach with nil. Each window owns private viewport-sized scratch storage and an independent highlighting subscription; the original backing buffer is retained unchanged and may be shared. Attachment resets view position, disables wrapping and makes text windows read-only, preserving list interaction. Detachment restores the original buffer, input state, surface, wrapping and scroll position. Navigation, selection and copy resolve against the entire source in bounded row batches; copying does not request highlighting. Built-in windows reject this call.",
            "pan" => fn(delta: i16) -> LuaWin, "Pan an unwrapped window horizontally by delta terminal cells. Negative pans left; clamps to the supported column range. Returns the handle.",
            "rect" => fn() -> mlua::Value, "Return the window's current viewport rect as `{ x, y, width, height }`, or `nil` until the first render lays it out.",
            "content_width" => fn() -> mlua::Value, "Return the inner-content width in cells (gutter and pad_left/pad_right already subtracted), or `nil` until the first render lays it out. Use this instead of `rect().width` when fitting text into the window's actual content budget.",
            "decorate" => fn(opts: mlua::Table) -> LuaDecoration, "Attach a decoration to this window. Decorations are clipped to and painted with their owner pane, below later layout leaves and below global overlays.",
            "cursor" => fn(row: Option<u64>) -> mlua::Value, "Read or write the absolute cursor row (0-based). Without arg returns the row; with arg sets and returns the handle for chaining. The built-in prompt window ignores row-cursor writes; use `smelt.prompt.cursor(byte_offset)` for prompt text cursor control.",
            "move_cursor" => fn(delta: i64) -> LuaWin, "Move the cursor by `delta` rows (clamped to the buffer's line count). Returns the handle for chaining. The built-in prompt window ignores row-cursor moves; use `smelt.prompt.cursor(byte_offset)` for prompt text cursor control.",
            "reveal" => fn(row: u64, opts: Option<mlua::Table>) -> LuaWin, "Reveal row `row` (0-based) and return the handle for chaining. By default this also moves the row cursor there. `opts.top_padding` reserves rows above the target after the jump; `opts.bottom_padding` reserves rows below it; `opts.cursor = false` scrolls without moving the cursor; `opts.jump = true` first leaves the current cursor in the window's jump list so `win:jump{ older = 1 }` or vim's `Ctrl-O` returns to it. This is a generic row reveal for ordinary windows and explicit row debugging; transcript message navigation should resolve a target from a committed `smelt.transcript.View` and pass it to `smelt.transcript.reveal`. The built-in prompt window ignores row reveals; use `smelt.prompt.cursor(byte_offset)` for prompt text cursor control.",
            "jump" => fn(opts: mlua::Table) -> bool, "Walk the window's jump list, the positions its vim jumps (`G`, `}`, `]u`, searches) left from; vim's `Ctrl-O`, `Ctrl-I` and `''`. Pass exactly one of `opts.older = n` (back `n` jumps), `opts.newer = n` (forward again) or `opts.previous = true` (toggle between the last two positions). Returns whether the cursor moved. The built-in prompt window has no jump list and returns false.",
            "key" => fn(chord: String, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Bind `func` to `chord` on this window. Returns a Reg handle whose `:remove()` undoes the binding. Raises on unknown chords.",
            "on" => fn(event: LuaWinEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe `func` to `event` on this window. Returns a Reg handle whose `:remove()` undoes the subscription.",
            "placeholder" => fn(text: String, opts: Option<mlua::Table>) -> LuaWin, "Set the window's placeholder - a dim suggestion rendered when the buffer is empty. Replaces any prior placeholder. `text` must be a single line (no `\\n`); split before calling. `opts.accept_keys` (array of chord strings, default `{}`) accept the placeholder into the buffer and fire `placeholder_accepted`. `opts.dismiss_keys` (default `{ \"esc\", \"c-c\" }`) clear the placeholder and fire `placeholder_dismissed`. Typing does not destroy the placeholder; the stored text survives so an undo back to an empty buffer makes it visible again. The prompt renders placeholders as wrapped ghost text; other windows render a single virtual-text row. Returns the handle for chaining.",
            "clear_placeholder" => fn() -> (), "Clear the window's placeholder text and opts. Idempotent.",
            "placeholder_text" => fn() -> Option<String>, "Return the current placeholder text, or `nil` if none is set.",
            "configure" => fn(opts: mlua::Table) -> LuaWin, "Atomically change live options, including on built-in windows. Unknown keys, wrong types and invalid enum values raise before any change. Absent keys stay unchanged. `style` is a shared style value filling every row including padding; false clears it. `prefix` is a string or `{ first, rest?, style? }`, clipped to pad_left; false clears it. `focus_on_click` is a Win or false. Also accepts pad_left, pad_right, scrollbar, wrap, surface, cursor_line, selection_highlight, hide_cursor, vim_enabled and gutter. Prompt resize gestures are configured through smelt.prompt.set_resize_handle, not window options.",
            "row_highlights" => fn(specs: Option<mlua::Table>) -> LuaWin, "Replace window-owned row background highlights and return the handle. Specs are `smelt.win.RowHighlight` tables. Pass nil or `{}` to clear. Use this for selection/cursor backgrounds that belong to a window view rather than buffer text.",
            "link_scroll" => fn(others: mlua::Variadic<LuaWin>) -> LuaWin, "Link `scroll_top` between this window and the variadic `others`. Closing any member auto-removes it. Returns the handle for chaining.",
            "scroll" => fn(arg: mlua::Value) -> mlua::Value, "Read or write the window's scroll state. No arg returns `{ top, left, follow, total, viewport, max, overflow, at_top, at_bottom, needs_tail_repin }` (`total` is the buffer's line count; `viewport` is the leaf's height; `max` is the largest valid `top`; `needs_tail_repin` means content overflows and the viewport is not already at bottom). An integer sets `scroll_top` and clears the pin-to-tail flag. The literal string `\"tail\"` jumps the viewport to the buffer's tail while keeping the cursor on the same screen row, then enables tail-follow.",
            "set_renderer" => fn(renderer: Option<LuaCallback<(LuaWin,), ()>>) -> LuaWin, "Register a retained renderer for this window, or clear it with nil. While mounted, it runs before paint after registration, `invalidate`, or a change in resolved content width or window height. Its backing buffer remains authoritative between runs. An unmounted window stays dirty and runs when a layout mounts it.",
            "invalidate" => fn() -> LuaWin, "Mark this window's retained renderer dirty. It repaints during the next compositor frame in which the window is mounted. Returns the handle for chaining.",
            "invalidate_after" => fn(ms: u64) -> LuaReg, "Invalidate this window after ms milliseconds. Returns a cancellable one-shot registration; timers are allocated only when requested.",
        },
    });

    record_class(LuaClassDecl {
        name: "smelt.win.Decoration",
        classification: smelt_core::lua::doc::classification_for_type("smelt.win.Decoration"),
        doc: "Handle returned by `Win:decorate(opts)` for a window-owned decoration.",
        fields: smelt_core::class_methods! {
            "close" => fn() -> (), "Remove the decoration and any window leaves it owns.",
        },
    });

    record_class(LuaClassDecl {
        name: "smelt.win.RowHighlight",
        classification: smelt_core::lua::doc::classification_for_type("smelt.win.RowHighlight"),
        doc: "Window-owned row background highlight. Ranges use absolute visual rows and an exclusive `end`; `{ cursor = true }` follows the window cursor row. `mode` is `always` (default) or `focused`; `width` is `full` (default) or `content`.",
        fields: vec![
            LuaClassField { name: "start", ty: "integer".into(), optional: true, doc: "First absolute visual row to highlight (0-based). Required unless `cursor = true`." },
            LuaClassField { name: "end", ty: "integer".into(), optional: true, doc: "Exclusive absolute visual row end. Defaults to `start + 1`." },
            LuaClassField { name: "cursor", ty: "boolean".into(), optional: true, doc: "When true, highlight the current cursor row instead of a fixed range." },
            LuaClassField { name: "hl_group", ty: "string".into(), optional: true, doc: "Theme highlight group to resolve at render time. Default `CursorLine`." },
            LuaClassField { name: "mode", ty: "\"always\"|\"focused\"".into(), optional: true, doc: "Paint always or only while the window is focused. Default `always`." },
            LuaClassField { name: "width", ty: "\"full\"|\"content\"".into(), optional: true, doc: "Paint the full window row, including gutter and padding, or only the content region. Default `full`." },
        ],
    });

    record_class(LuaClassDecl {
        name: "smelt.win.DecorationOpts",
        classification: smelt_core::lua::doc::classification_for_type("smelt.win.DecorationOpts"),
        doc: "Options accepted by `Win:decorate(opts)`. The required `layout` is a `smelt.ui.layout` tree. `align` is one of `nw|n|ne|w|center|e|sw|s|se` and defaults to `center`. Width/height constraints use the same vocabulary as overlays but resolve against the owner window rect.",
        fields: vec![
            LuaClassField { name: "layout", ty: "smelt.ui.layout.Layout".into(), optional: false, doc: "Decoration layout tree." },
            LuaClassField { name: "align", ty: "string".into(), optional: true, doc: "Owner-relative alignment point; default `center`." },
            LuaClassField { name: "row_offset", ty: "integer".into(), optional: true, doc: "Rows to offset after alignment." },
            LuaClassField { name: "col_offset", ty: "integer".into(), optional: true, doc: "Columns to offset after alignment." },
            LuaClassField { name: "z", ty: "integer".into(), optional: true, doc: "Owner-local stacking order." },
            LuaClassField { name: "transparent", ty: "boolean".into(), optional: true, doc: "Leave the decoration's rect uncleared, so the owner window shows through every cell its leaves do not draw." },
            LuaClassField { name: "width", ty: "any".into(), optional: true, doc: "Width constraint; defaults to `fit`." },
            LuaClassField { name: "height", ty: "any".into(), optional: true, doc: "Height constraint; defaults to `fit`." },
            LuaClassField { name: "max_width", ty: "any".into(), optional: true, doc: "Optional width cap." },
            LuaClassField { name: "max_height", ty: "any".into(), optional: true, doc: "Optional height cap." },
            LuaClassField { name: "min_width", ty: "any".into(), optional: true, doc: "Optional width floor." },
            LuaClassField { name: "min_height", ty: "any".into(), optional: true, doc: "Optional height floor." },
        ],
    });

    // Doc text is built at registration time so per-opt defaults stay in
    // lockstep with the Rust source (`Gutters::default()`) - no hand-kept
    // duplication that could drift on a default change.
    static NEW_DOC: OnceLock<String> = OnceLock::new();
    let new_doc = NEW_DOC.get_or_init(|| {
        format!(
            "Open a split window over `buf` and return a `Win` userdata. `opts.name` opts the window into hot-reload survival; omitted from a module body, a stable per-(plugin, declaration-index) name is auto-assigned. `opts.surface = \"editable_text\"|\"readonly_text\"|\"selectable_text\"|\"inert\"|\"list\"|\"list_inert\"` sets the leaf interaction role. `opts.hide_cursor = true` suppresses the block caret while keeping the leaf focusable. `opts.kind = \"input\"` (`opts.placeholder?`) marks the window as a single-line text input; `opts.kind = \"list\"` (`opts.initial_cursor?`) marks it as a navigable list leaf. `opts.scrollbar` reserves the rightmost column for an overflow scrollbar (default `{}`); pass `false` on 1-row pills / dialog chrome to reclaim that cell. `opts.pad_left` / `opts.pad_right` inset the content. `opts.style` accepts a shared style value layered over `Normal` as the window's base style. `opts.prefix` paints text into the left padding (see `Win:configure`). `Win:configure(opts)` changes these later.",
            crate::smelt_edit::Gutters::default().scrollbar
        )
    });
    m.fn_(
        "new",
        new_doc,
        &["buf", "opts"],
        |lua,
         (buf, opts): (super::buf::LuaBuf, Option<mlua::Table>)|
         -> LuaResult<Option<LuaWin>> {
            let mut reader = smelt_core::lua::options::LuaOptions::new(
                opts.map_or_else(|| lua.create_table(), Ok)?,
                "win",
            );
            let mut opts = parse_window_opts(lua, &mut reader)?;
            reader.finish()?;
            opts.name = opts
                .name
                .or_else(|| crate::lua::auto_name_for_scope(lua, "win"));
            Ok(opts.open(buf.id).map(|id| LuaWin { id }))
        },
    )?;

    Ok(())
}

#[derive(Clone, Copy)]
enum WindowKind {
    Input,
    List,
}

/// Parsed before any host mutation; applying options cannot call back into Lua.
#[derive(Default)]
pub(super) struct WindowOptions {
    pub(super) name: Option<String>,
    kind: Option<WindowKind>,
    placeholder: Option<String>,
    initial_cursor: Option<u64>,
    wrap: Option<bool>,
    surface: Option<crate::smelt_edit::WindowSurface>,
    cursor_line: Option<bool>,
    selection_highlight: Option<bool>,
    hide_cursor: Option<bool>,
    vim_enabled: Option<bool>,
    line_numbers: Option<bool>,
    pad_left: Option<u16>,
    pad_right: Option<u16>,
    scrollbar: Option<bool>,
    style: Option<Option<smelt_core::theme::HlGroup>>,
    prefix: Option<Option<crate::smelt_edit::PadPrefix>>,
    focus: Option<Option<crate::smelt_edit::WinId>>,
}

impl WindowOptions {
    fn check_configuration(&self) -> LuaResult<()> {
        for (key, present) in [
            ("name", self.name.is_some()),
            ("kind", self.kind.is_some()),
            ("placeholder", self.placeholder.is_some()),
            ("initial_cursor", self.initial_cursor.is_some()),
        ] {
            if present {
                return Err(LuaError::external(format!(
                    "win.{key}: constructor-only option"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn for_input(mut self) -> Self {
        self.kind = Some(WindowKind::Input);
        self.surface
            .get_or_insert_with(crate::smelt_edit::WindowSurface::editable_text);
        self.wrap.get_or_insert(false);
        self.scrollbar.get_or_insert(false);
        self
    }

    pub(super) fn open(&self, buf: crate::smelt_edit::BufId) -> Option<crate::smelt_edit::WinId> {
        let name = smelt_core::lua::reg::RegistrationScope::name(self.name.clone());
        let gutters = crate::smelt_edit::Gutters {
            pad_left: self.pad_left.unwrap_or(0),
            pad_right: self.pad_right.unwrap_or(0),
            scrollbar: self
                .scrollbar
                .unwrap_or_else(|| crate::smelt_edit::Gutters::default().scrollbar),
        };
        // Bootstrap chunks may run before the frontend host is in scope.
        crate::lua::try_with_ui_host(|host| {
            if let Some(name) = &name {
                if let Some(existing) = host.with_ui(|ui| ui.touch_named_win(name)) {
                    host.with_ui(|ui| self.apply(ui, existing));
                    return Some(existing);
                }
            }
            let window = host.with_ui(|ui| {
                let id = ui.win_open_split(buf, crate::smelt_edit::SplitConfig { gutters })?;
                if let Some(window) = ui.win_mut(id) {
                    window.gutter = Some(std::sync::Arc::new(
                        crate::smelt_edit::gutter::LineNumberGutter::new(),
                    ));
                    window.wrap = true;
                }
                self.apply(ui, id);
                if let Some(name) = &name {
                    ui.name_win(name.clone(), id);
                }
                Some(id)
            });
            if let Some(id) = window {
                smelt_core::lua::reg::RegistrationScope::resource("window", id.0, move || {
                    crate::lua::try_with_ui_host(|host| {
                        host.close_overlay_leaf(id);
                        true
                    })
                    .unwrap_or(false)
                });
                match self.kind {
                    Some(WindowKind::Input) => {
                        host.configure_input_leaf(id, self.placeholder.clone().unwrap_or_default())
                    }
                    Some(WindowKind::List) => {
                        host.configure_list_leaf(id, self.initial_cursor.unwrap_or(0))
                    }
                    None => {}
                }
            }
            window
        })
        .flatten()
    }
}

pub(super) fn parse_window_opts(
    lua: &Lua,
    opts: &mut smelt_core::lua::options::LuaOptions,
) -> LuaResult<WindowOptions> {
    use crate::lua::parse::LuaStyle;
    let mut out = WindowOptions::default();
    for (key, slot) in [
        ("wrap", &mut out.wrap),
        ("cursor_line", &mut out.cursor_line),
        ("selection_highlight", &mut out.selection_highlight),
        ("hide_cursor", &mut out.hide_cursor),
        ("vim_enabled", &mut out.vim_enabled),
        ("scrollbar", &mut out.scrollbar),
    ] {
        *slot = match opts.get::<mlua::Value>(key)? {
            mlua::Value::Nil => None,
            mlua::Value::Boolean(value) => Some(value),
            _ => return Err(LuaError::external(format!("win.{key}: expected boolean"))),
        };
    }
    for (key, slot) in [
        ("pad_left", &mut out.pad_left),
        ("pad_right", &mut out.pad_right),
    ] {
        *slot = match opts.get::<mlua::Value>(key)? {
            mlua::Value::Nil => None,
            mlua::Value::Integer(n) if (0..=u16::MAX as i64).contains(&n) => Some(n as u16),
            _ => {
                return Err(LuaError::external(format!(
                    "win.{key}: expected integer in 0..65535"
                )))
            }
        };
    }
    out.initial_cursor = match opts.get::<mlua::Value>("initial_cursor")? {
        mlua::Value::Nil => None,
        mlua::Value::Integer(n) if n >= 0 => Some(n as u64),
        _ => {
            return Err(LuaError::external(
                "win.initial_cursor: expected non-negative integer",
            ))
        }
    };
    out.name = opts.string("name")?;
    out.placeholder = opts.string("placeholder")?;
    out.surface = match opts.string("surface")?.as_deref() {
        None => None,
        Some("editable_text") => Some(crate::smelt_edit::WindowSurface::editable_text()),
        Some("readonly_text") => Some(crate::smelt_edit::WindowSurface::readonly_text()),
        Some("selectable_text") => Some(crate::smelt_edit::WindowSurface::selectable_text()),
        Some("inert") => Some(crate::smelt_edit::WindowSurface::inert()),
        Some("list") => Some(crate::smelt_edit::WindowSurface::list(true)),
        Some("list_inert") => Some(crate::smelt_edit::WindowSurface::list(false)),
        Some(value) => {
            return Err(LuaError::external(format!(
                "win.surface: unknown value '{value}'"
            )))
        }
    };
    out.line_numbers = match opts.string("gutter")?.as_deref() {
        None => None,
        Some("line_numbers") => Some(true),
        Some("none") => Some(false),
        Some(value) => {
            return Err(LuaError::external(format!(
                "win.gutter: unknown value '{value}'"
            )))
        }
    };
    out.kind = match opts.string("kind")?.as_deref() {
        None => None,
        Some("input") => Some(WindowKind::Input),
        Some("list") => Some(WindowKind::List),
        Some(value) => {
            return Err(LuaError::external(format!(
                "win.kind: unknown value '{value}'"
            )))
        }
    };
    out.style = match opts.get::<mlua::Value>("style")? {
        mlua::Value::Nil => None,
        mlua::Value::Boolean(false) => Some(None),
        value => Some(Some(LuaStyle::from_lua(value, lua)?.group()?)),
    };
    out.prefix = match opts.get::<mlua::Value>("prefix")? {
        mlua::Value::Nil => None,
        mlua::Value::Boolean(false) => Some(None),
        mlua::Value::String(first) => Some(Some(crate::smelt_edit::PadPrefix {
            first: first.to_str()?.to_string(),
            ..Default::default()
        })),
        mlua::Value::Table(spec) => {
            let mut spec = smelt_core::lua::options::LuaOptions::new(spec, "win.prefix");
            let prefix = crate::smelt_edit::PadPrefix {
                first: spec.string("first")?.unwrap_or_default(),
                rest: spec.string("rest")?.unwrap_or_default(),
                style: spec
                    .get::<Option<LuaStyle>>("style")?
                    .map(|style| style.group())
                    .transpose()?,
            };
            spec.finish()?;
            Some(Some(prefix))
        }
        _ => {
            return Err(LuaError::external(
                "win.prefix: expected string, prefix table or false",
            ))
        }
    };
    out.focus = match opts.get::<mlua::Value>("focus_on_click")? {
        mlua::Value::Nil => None,
        mlua::Value::Boolean(false) => Some(None),
        value => Some(Some(LuaWin::from_lua(value, lua)?.id)),
    };
    Ok(out)
}

impl WindowOptions {
    fn apply(&self, ui: &mut crate::smelt_edit::Ui, win_id: crate::smelt_edit::WinId) {
        let Some(w) = ui.win_mut(win_id) else { return };
        if let Some(wrap) = self.wrap {
            w.wrap = wrap;
        }
        if let Some(surface) = self.surface {
            w.set_surface(surface);
        }
        if let Some(cursor_line) = self.cursor_line {
            w.set_cursor_line_highlight(cursor_line);
        }
        if let Some(selection_highlight) = self.selection_highlight {
            w.set_list_selection_highlight(selection_highlight);
        }
        if let Some(hide_cursor) = self.hide_cursor {
            w.hide_cursor = hide_cursor;
        }
        if let Some(vim_enabled) = self.vim_enabled {
            w.set_vim_enabled(vim_enabled);
        }
        if let Some(line_numbers) = self.line_numbers {
            w.gutter = if line_numbers {
                Some(std::sync::Arc::new(
                    crate::smelt_edit::gutter::LineNumberGutter::new(),
                ))
            } else {
                None
            };
        }
        if let Some(pad_left) = self.pad_left {
            w.config.gutters.pad_left = pad_left;
        }
        if let Some(pad_right) = self.pad_right {
            w.config.gutters.pad_right = pad_right;
        }
        if let Some(scrollbar) = self.scrollbar {
            w.config.gutters.scrollbar = scrollbar;
        }
        if let Some(style) = self.style {
            w.set_style(style);
        }
        if let Some(prefix) = &self.prefix {
            w.set_prefix(prefix.clone());
        }
        if let Some(focus) = self.focus {
            w.click_focus = focus;
        }
    }
}
