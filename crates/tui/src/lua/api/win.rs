//! `smelt.win` — Win handle. UiHost-only.
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
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use std::sync::Arc;

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
    /// Mouse-down landed on the window. Payload: `{ row, col, button }`
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
    /// Window's scroll state changed. Payload: `{ top, follow }` where
    /// `top` is the new `scroll_top` and `follow` is the pin-to-tail flag.
    Scrolled,
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
            LuaWinEvent::Press => WinEvent::Press,
            LuaWinEvent::Release => WinEvent::Release,
            LuaWinEvent::Drag => WinEvent::Drag,
            LuaWinEvent::Scrolled => WinEvent::Scrolled,
        }
    }
}

/// Lua-side handle for a `WinId`.
#[derive(Clone, Copy, Debug)]
pub struct LuaWin {
    pub(crate) id: crate::smelt_term::WinId,
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
fn is_builtin_win(id: crate::smelt_term::WinId) -> bool {
    matches!(
        id,
        crate::app::TRANSCRIPT_WIN
            | crate::app::PROMPT_WIN
            | crate::app::PROMPT_ABOVE_WIN
            | crate::app::PROMPT_BELOW_WIN
    )
}

impl mlua::UserData for LuaWin {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Win#{}", this.id.0))
        });

        // ── close / focus ──────────────────────────────────────────
        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            if is_builtin_win(this.id) {
                return Ok(());
            }
            crate::lua::with_app(|app| {
                app.close_overlay_leaf(this.id);
            });
            Ok(())
        });

        methods.add_method("focus", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                app.ui.set_focus(this.id);
            });
            Ok(())
        });

        // ── buf — return the backing Buf handle ────────────────────
        methods.add_method(
            "buf",
            |_, this, ()| -> LuaResult<Option<super::buf::LuaBuf>> {
                let bid =
                    crate::lua::try_with_app(|app| app.ui.win(this.id).map(|w| w.buf)).flatten();
                Ok(bid.map(|id| super::buf::LuaBuf { id }))
            },
        );

        // ── rect — viewport bounds ─────────────────────────────────
        methods.add_method("rect", |lua, this, ()| -> LuaResult<mlua::Value> {
            let rect = crate::lua::try_with_app(|app| {
                app.ui
                    .win(this.id)
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
        });

        // ── cursor: get / set ──────────────────────────────────────
        methods.add_function(
            "cursor",
            |_, (this_ud, row): (mlua::AnyUserData, Option<u64>)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaWin>()?;
                match row {
                    Some(r) => {
                        crate::lua::with_app(|app| {
                            crate::lua::ui_ops::set_cursor_row(
                                app,
                                this.id,
                                r.min(u16::MAX as u64) as u16,
                            );
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let row = crate::lua::try_with_app(|app| {
                            crate::lua::ui_ops::cursor_row(app, this.id)
                        })
                        .flatten();
                        Ok(match row {
                            Some(r) => mlua::Value::Integer(r as i64),
                            None => mlua::Value::Nil,
                        })
                    }
                }
            },
        );

        // ── move_cursor(delta) — chainable ─────────────────────────
        methods.add_function(
            "move_cursor",
            |_, (this_ud, delta): (mlua::AnyUserData, i64)| -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                crate::lua::with_app(|app| {
                    crate::lua::ui_ops::move_cursor(app, this.id, delta as isize);
                });
                Ok(this_ud)
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
                crate::lua::with_app(|app| {
                    let prev = app.ui.win_set_keymap(
                        this.id,
                        key,
                        crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
                    );
                    crate::lua::drop_displaced_lua_handle(app, prev);
                });
                let win = this.id;
                Ok(LuaReg::new(move || {
                    let mut removed = false;
                    crate::lua::with_app(|app| {
                        let prev = app.ui.win_clear_keymap(win, key);
                        removed = prev.is_some();
                        crate::lua::drop_displaced_lua_handle(app, prev);
                    });
                    removed
                }))
            },
        );

        // ── on(event, fn) → Reg ────────────────────────────────────
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
                let event: crate::smelt_term::WinEvent = event.into();
                crate::lua::with_app(|app| {
                    app.ui.win_on_event(
                        this.id,
                        event,
                        crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
                    );
                });
                let win = this.id;
                Ok(LuaReg::new(move || {
                    let mut removed = false;
                    crate::lua::with_app(|app| {
                        let prev = app.ui.win_clear_event_by_id(win, event, id);
                        removed = prev.is_some();
                        crate::lua::drop_displaced_lua_handle(app, prev);
                    });
                    removed
                }))
            },
        );

        // ── scroll: get / set / pin-to-tail ────────────────────────
        // `win:scroll()` returns `{ top, follow, total, viewport }`.
        // `win:scroll(integer)` sets `scroll_top` and clears `follow_tail`.
        // `win:scroll("tail")` re-pins (`follow_tail = true`).
        methods.add_function(
            "scroll",
            |lua, (this_ud, arg): (mlua::AnyUserData, mlua::Value)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaWin>()?;
                match arg {
                    mlua::Value::Nil => {
                        let info = crate::lua::try_with_app(|app| {
                            let win = app.ui.win(this.id)?;
                            let total = app.ui.buf(win.buf).map(|b| b.lines().len()).unwrap_or(0);
                            let viewport = win.viewport.map(|v| v.rect.height).unwrap_or(0);
                            Some((win.scroll_top, win.follow_tail, total as u64, viewport))
                        })
                        .flatten();
                        match info {
                            Some((top, follow, total, viewport)) => {
                                let t = lua.create_table()?;
                                t.set("top", top)?;
                                t.set("follow", follow)?;
                                t.set("total", total)?;
                                t.set("viewport", viewport)?;
                                Ok(mlua::Value::Table(t))
                            }
                            None => Ok(mlua::Value::Nil),
                        }
                    }
                    mlua::Value::Integer(n) => {
                        crate::lua::with_app(|app| {
                            let Some(win) = app.ui.win(this.id) else {
                                return;
                            };
                            let buf_id = win.buf;
                            let viewport_rows = win.viewport.map(|v| v.rect.height).unwrap_or(0);
                            let target = n.max(0).min(u16::MAX as i64) as u16;
                            let (w, buf) = app.ui.win_and_buf_mut(this.id, buf_id);
                            if let (Some(w), Some(buf)) = (w, buf) {
                                // Match mouse-wheel semantics: keep the cursor on
                                // the same screen row across the pan.
                                w.scroll_to_preserving_cursor_screen_row(
                                    target,
                                    buf,
                                    viewport_rows,
                                );
                                w.follow_tail = false;
                            }
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    mlua::Value::String(s) if s.to_str()?.as_ref() == "tail" => {
                        crate::lua::with_app(|app| {
                            if let Some(w) = app.ui.win_mut(this.id) {
                                w.follow_tail = true;
                            }
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

        // ── link_scroll(...) — chainable; variadic over Win ────────
        methods.add_function(
            "link_scroll",
            |_,
             (this_ud, others): (mlua::AnyUserData, mlua::Variadic<LuaWin>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaWin>()?;
                let mut ids: Vec<crate::smelt_term::WinId> = vec![this.id];
                for w in others {
                    ids.push(w.id);
                }
                crate::lua::with_app(|app| {
                    app.ui.link_scroll(&ids);
                });
                Ok(this_ud)
            },
        );
    }
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
    let m = LuaMod::under(
        lua,
        smelt,
        "win",
        "Window handle constructor. `smelt.win.new(buf, opts?)` opens a split window over `buf` and returns a `Win` userdata. \
`opts.name` opts the window into hot-reload survival. UiHost-only — windows are layout leaves that render a buffer onto the screen.",
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
        doc: "Window handle returned by `smelt.win.new(buf, opts?)`. Setter methods return the same handle for chaining.",
        fields: smelt_core::class_methods! {
            "close" => fn() -> (), "Close the overlay leaf. No-op if the window is already closed.",
            "focus" => fn() -> (), "Move keyboard focus to this window. No-op if the window is not focusable.",
            "buf" => fn() -> Option<super::buf::LuaBuf>, "Return the backing Buf handle, or `nil` if the window is gone.",
            "rect" => fn() -> mlua::Value, "Return the window's current viewport rect as `{ row, col, width, height }`, or `nil` until the first render lays it out.",
            "cursor" => fn(row: Option<u64>) -> mlua::Value, "Read or write the cursor row (0-based). Without arg returns the row; with arg sets and returns the handle for chaining.",
            "move_cursor" => fn(delta: i64) -> LuaWin, "Move the cursor by `delta` rows (clamped to the buffer's line count). Returns the handle for chaining.",
            "key" => fn(chord: String, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Bind `func` to `chord` on this window. Returns a Reg handle whose `:remove()` undoes the binding. Raises on unknown chords.",
            "on" => fn(event: LuaWinEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe `func` to `event` on this window. Returns a Reg handle whose `:remove()` undoes the subscription.",
            "link_scroll" => fn(others: mlua::Variadic<LuaWin>) -> LuaWin, "Link `scroll_top` between this window and the variadic `others`. Closing any member auto-removes it. Returns the handle for chaining.",
            "scroll" => fn(arg: mlua::Value) -> mlua::Value, "Read or write the window's scroll state. No arg returns `{ top, follow, total, viewport }` (`total` is the buffer's line count; `viewport` is the leaf's height). An integer sets `scroll_top` and clears the pin-to-tail flag. The literal string `\"tail\"` re-pins the viewport to the buffer's tail.",
        },
    });

    // Doc text is built at registration time so per-opt defaults stay in
    // lockstep with the Rust source (`Gutters::default()`) — no hand-kept
    // duplication that could drift on a default change.
    let new_doc: &'static str = Box::leak(format!(
        "Open a split window over `buf` and return a `Win` userdata. `opts.name` opts the window into hot-reload survival; omitted from a module body, a stable per-(plugin, declaration-index) name is auto-assigned. `opts.kind = \"input\"` (`opts.placeholder?`) marks the window as a single-line text input; `opts.kind = \"list\"` (`opts.initial_cursor?`) marks it as a navigable list leaf. `opts.scrollbar` reserves the rightmost column for an overflow scrollbar (default `{}`); pass `false` on 1-row pills / dialog chrome to reclaim that cell.",
        crate::smelt_term::layout::Gutters::default().scrollbar
    ).into_boxed_str());
    m.fn_(
        "new",
        new_doc,
        &["buf", "opts"],
        |lua,
         (buf, opts): (super::buf::LuaBuf, Option<mlua::Table>)|
         -> LuaResult<Option<LuaWin>> {
            // Auto-name from active plugin scope when caller didn't.
            let opts = match opts {
                Some(t) => {
                    let has_name = t.get::<Option<String>>("name").ok().flatten().is_some();
                    if !has_name {
                        if let Some(auto) = crate::lua::auto_name_for_scope(lua, "win") {
                            t.set("name", auto)?;
                        }
                    }
                    Some(t)
                }
                None => {
                    if let Some(auto) = crate::lua::auto_name_for_scope(lua, "win") {
                        let t = lua.create_table()?;
                        t.set("name", auto)?;
                        Some(t)
                    } else {
                        None
                    }
                }
            };
            Ok(open_or_refresh(buf.id, opts.as_ref())?.map(|id| LuaWin { id }))
        },
    )?;

    m.fn_(
        "transcript",
        "Return a `Win` handle for the built-in transcript window. Useful as an `anchor = \"win\"` / `\"win_center\"` target so plugins can float overlays over the transcript without hard-coding its id.",
        &[],
        |_, ()| -> LuaResult<LuaWin> { Ok(LuaWin { id: crate::app::TRANSCRIPT_WIN }) },
    )?;

    Ok(())
}

/// Implementation of `smelt.win.new(buf, opts?)`. If `opts.name` matches an
/// open window, refresh its mutable opts and return the existing id;
/// otherwise open a fresh split.
fn open_or_refresh(
    buf_id: crate::smelt_term::BufId,
    opts: Option<&mlua::Table>,
) -> LuaResult<Option<crate::smelt_term::WinId>> {
    let win = crate::lua::with_app(|app| -> Option<crate::smelt_term::WinId> {
        let name: Option<String> = opts
            .and_then(|t| t.get::<Option<String>>("name").ok())
            .flatten();
        // Named window already exists — refresh and return.
        if let Some(ref n) = name {
            if let Some(existing) = app.ui.named_win(n) {
                if let Some(t) = opts {
                    apply_window_opts(app, existing, t);
                }
                return Some(existing);
            }
        }
        let region = opts
            .and_then(|t| t.get::<String>("region").ok())
            .unwrap_or_else(|| "lua_overlay".to_string());
        let pad_left = opts
            .and_then(|t| t.get::<u64>("pad_left").ok())
            .unwrap_or(0) as u16;
        let pad_right = opts
            .and_then(|t| t.get::<u64>("pad_right").ok())
            .unwrap_or(0) as u16;
        let scrollbar = opts
            .and_then(|t| t.get::<Option<bool>>("scrollbar").ok().flatten())
            .unwrap_or_else(|| crate::smelt_term::layout::Gutters::default().scrollbar);
        let win = app.ui.win_open_split(
            buf_id,
            crate::smelt_term::SplitConfig {
                region,
                gutters: crate::smelt_term::layout::Gutters {
                    pad_left,
                    pad_right,
                    scrollbar,
                },
            },
        );
        if let Some(win_id) = win {
            if let Some(w) = app.ui.win_mut(win_id) {
                // Default gutter is `LineNumberGutter` (strict): buffers
                // without `SourceLine` stamps get a zero-width column.
                w.gutter = Some(std::sync::Arc::new(
                    crate::smelt_term::gutter::LineNumberGutter::new(),
                ));
                w.wrap = true;
            }
            if let Some(t) = opts {
                apply_window_opts(app, win_id, t);
                // Apply input/list kind if requested.
                if let Ok(Some(kind)) = t.get::<Option<String>>("kind") {
                    match kind.as_str() {
                        "input" => {
                            let placeholder = t
                                .get::<Option<String>>("placeholder")
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                            crate::lua::ui_ops::configure_input_leaf(app, win_id, placeholder);
                        }
                        "list" => {
                            let initial = t.get::<Option<u64>>("initial_cursor").ok().flatten();
                            crate::lua::ui_ops::configure_list_leaf(
                                app,
                                win_id,
                                initial.unwrap_or(0).min(u16::MAX as u64) as u16,
                            );
                        }
                        _ => {}
                    }
                }
            }
            if let Some(ref n) = name {
                app.ui.name_win(n.clone(), win_id);
            }
        }
        win
    });
    Ok(win)
}

/// Apply the mutable subset of window opts to an existing window. Used
/// by `smelt.win.new()` on both the create and named-refresh paths.
fn apply_window_opts(
    app: &mut crate::app::TuiApp,
    win_id: crate::smelt_term::WinId,
    opts: &mlua::Table,
) {
    let Some(w) = app.ui.win_mut(win_id) else {
        return;
    };
    if let Ok(wrap) = opts.get::<bool>("wrap") {
        w.wrap = wrap;
    }
    if let Ok(focusable) = opts.get::<bool>("focusable") {
        w.focusable = focusable;
    }
    if let Ok(cursor_line) = opts.get::<bool>("cursor_line") {
        w.cursor_line = cursor_line;
    }
    if let Ok(selection_highlight) = opts.get::<bool>("selection_highlight") {
        w.selection_highlight = selection_highlight;
    }
    if let Ok(vim_enabled) = opts.get::<bool>("vim_enabled") {
        w.set_vim_enabled(vim_enabled);
    }
    if let Ok(selectable) = opts.get::<bool>("selectable") {
        w.selectable = selectable;
    }
    if let Ok(Some(gutter)) = opts.get::<Option<String>>("gutter") {
        w.gutter = match gutter.as_str() {
            "line_numbers" => Some(std::sync::Arc::new(
                crate::smelt_term::gutter::LineNumberGutter::new(),
            )),
            "none" | "" => None,
            _ => None,
        };
    }
}
