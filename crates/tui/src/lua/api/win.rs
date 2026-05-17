//! `smelt.win` — Win handle. UiHost-only.
//!
//! `smelt.win.new(buf, opts?)` opens a split window over `buf` (a `Buf`
//! handle) and returns a `Win` userdata. `opts.name` opts the window
//! into hot-reload survival.
//!
//! Keymap / event registrations return a `Reg` userdata with a
//! single `:remove()` method that frees the binding.

use crate::lua::{parse_keybind, LuaShared};
use lua_doc_derive::lua_module;
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, record_module_doc};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
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

impl mlua::UserData for LuaWin {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Win#{}", this.id.0))
        });

        // ── close / focus ──────────────────────────────────────────
        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
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
                        key.clone(),
                        crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
                    );
                    crate::lua::drop_displaced_lua_handle(app, prev);
                });
                Ok(LuaReg(RegKind::WinKey {
                    win: this.id,
                    chord,
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
                crate::lua::with_app(|app| {
                    app.ui.win_on_event(
                        this.id,
                        event.into(),
                        crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
                    );
                });
                Ok(LuaReg(RegKind::WinEvent {
                    win: this.id,
                    event: event.into(),
                    id,
                }))
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

/// What a `Reg` knows how to undo. Each variant carries enough to
/// reverse the registration regardless of which Win/event it was on.
#[derive(Debug)]
enum RegKind {
    WinKey {
        win: crate::smelt_term::WinId,
        chord: String,
    },
    WinEvent {
        win: crate::smelt_term::WinId,
        event: crate::smelt_term::WinEvent,
        id: u64,
    },
}

/// Lua-side registration handle. The result of any callback-binding
/// API call (`win:key`, `win:on`); calling `:remove()` frees the
/// binding (and the underlying Lua callback).
pub struct LuaReg(RegKind);

impl LuaType for LuaReg {
    fn lua_type() -> String {
        "smelt.Reg".into()
    }
}

impl mlua::UserData for LuaReg {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("remove", |_, this, ()| -> LuaResult<bool> {
            // Take ownership of the inner kind to make `:remove()`
            // idempotent — a second call yields `false` without
            // re-firing the underlying ui mutation.
            let kind = std::mem::replace(
                &mut this.0,
                RegKind::WinKey {
                    win: crate::smelt_term::WinId(0),
                    chord: String::new(),
                },
            );
            Ok(reg_remove(kind))
        });
    }
}

fn reg_remove(kind: RegKind) -> bool {
    match kind {
        RegKind::WinKey { win, chord } => {
            if chord.is_empty() {
                return false;
            }
            let Some(key) = parse_keybind(&chord) else {
                return false;
            };
            let mut removed = false;
            crate::lua::with_app(|app| {
                let prev = app.ui.win_clear_keymap(win, key);
                removed = prev.is_some();
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            removed
        }
        RegKind::WinEvent { win, event, id } => {
            let mut removed = false;
            crate::lua::with_app(|app| {
                let prev = app.ui.win_clear_event_by_id(win, event, id);
                removed = prev.is_some();
                crate::lua::drop_displaced_lua_handle(app, prev);
            });
            removed
        }
    }
}

/// Recover the `LuaShared` from the Lua state's app registry. Used by
/// methods that need to register Lua callback handles.
fn current_shared(lua: &Lua) -> LuaResult<Arc<LuaShared>> {
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

#[lua_module(
    name = "smelt.win",
    doc = "Window handle constructor. `smelt.win.new(buf, opts?)` opens a split window over `buf` and returns a `Win` userdata. \
`opts.name` opts the window into hot-reload survival. UiHost-only — windows are layout leaves that render a buffer onto the screen."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    record_module_doc(
        "smelt.win",
        "Window handle constructor. `smelt.win.new(buf, opts?)` returns a `Win` userdata.",
    );

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
        },
    });

    record_class(LuaClassDecl {
        name: "smelt.Reg",
        doc: "Registration handle returned by every callback-binding API. `:remove()` undoes the binding and frees the underlying Lua callback. Idempotent: subsequent calls return `false`.",
        fields: smelt_core::class_methods! {
            "remove" => fn() -> bool, "Undo the registration. Returns `true` the first time; `false` on subsequent calls or when the underlying target is already gone.",
        },
    });

    let win_tbl = lua.create_table()?;
    smelt_core::lua::doc::register_ui_fn(
        &win_tbl,
        "smelt.win",
        "new",
        "Open a split window over `buf` and return a `Win` userdata. `opts.name` opts the window into hot-reload survival. `opts.kind = \"input\"` (`opts.placeholder?`) marks the window as a single-line text input; `opts.kind = \"list\"` (`opts.initial_cursor?`) marks it as a navigable list leaf.",
        &["buf", "opts"],
        lua,
        |_,
         (buf, opts): (super::buf::LuaBuf, Option<mlua::Table>)|
         -> LuaResult<Option<LuaWin>> {
            Ok(open_or_refresh(buf.id, opts.as_ref())?.map(|id| LuaWin { id }))
        },
    )?;

    smelt.set("win", win_tbl)?;
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
