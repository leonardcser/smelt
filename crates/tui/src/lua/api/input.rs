//! `smelt.input` - first-class single-line input handle. UiHost-only.

use crate::lua::{parse_keybind, LuaShared};
use lua_doc_derive::LuaAlias;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct LuaInput {
    pub(crate) win: crate::smelt_edit::WinId,
}

impl LuaType for LuaInput {
    fn lua_type() -> String {
        "smelt.input.Input".into()
    }
}

#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.input.Event")]
pub enum LuaInputEvent {
    /// Text changed; callback payload carries `text`.
    #[lua(rename = "change")]
    Change,
    /// Enter submitted the input; callback payload carries `text`.
    Submit,
    /// Esc/Ctrl-C cancelled the input; callback payload carries `text`.
    Cancel,
}

impl LuaInputEvent {
    fn win_event(self) -> crate::smelt_edit::WinEvent {
        match self {
            LuaInputEvent::Change => crate::smelt_edit::WinEvent::TextChanged,
            LuaInputEvent::Submit => crate::smelt_edit::WinEvent::Submit,
            LuaInputEvent::Cancel => crate::smelt_edit::WinEvent::Dismiss,
        }
    }
}

impl mlua::UserData for LuaInput {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Input#{}", this.win.0))
        });

        methods.add_method("win", |_, this, ()| Ok(super::win::LuaWin { id: this.win }));

        methods.add_method(
            "buf",
            |_, this, ()| -> LuaResult<Option<super::buf::LuaBuf>> {
                let bid = crate::lua::try_with_ui_host(|host| {
                    host.with_ui(|ui| ui.win(this.win).map(|window| window.buf))
                })
                .flatten();
                Ok(bid.map(|id| super::buf::LuaBuf { id }))
            },
        );

        methods.add_method("text", |_, this, ()| -> LuaResult<String> {
            Ok(input_text(this.win).unwrap_or_default())
        });

        methods.add_method("set_text", |_, this, text: String| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.set_input_text(this.win, &text));
            Ok(())
        });

        methods.add_function(
            "on",
            |lua,
             (this_ud, event, func): (
                mlua::AnyUserData,
                LuaInputEvent,
                LuaCallback<mlua::Table, ()>,
            )|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaInput>()?;
                let shared = super::win::current_shared(lua)?;
                let id = crate::lua::register_callback_handle(&shared, lua, func.into_inner())?;
                let event = event.win_event();
                let installed = crate::lua::try_with_ui_host(|host| {
                    host.register_window_event(
                        this.win,
                        event,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                })
                .is_some();
                if !installed {
                    if let Ok(mut cbs) = shared.callbacks.lock() {
                        cbs.remove(&id);
                    }
                    return Ok(LuaReg::new(|| false));
                }
                let win = this.win;
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

        methods.add_function(
            "key",
            |lua,
             (this_ud, chord, func): (mlua::AnyUserData, String, LuaCallback<mlua::Table, ()>)|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaInput>()?;
                let Some(key) = parse_keybind(&chord) else {
                    return Err(mlua::Error::RuntimeError(format!(
                        "input:key: unknown chord `{chord}`"
                    )));
                };
                let shared = super::win::current_shared(lua)?;
                let id = crate::lua::register_callback_handle(&shared, lua, func.into_inner())?;
                crate::lua::with_ui_host(|host| {
                    host.set_window_keymap(
                        this.win,
                        key,
                        crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
                    );
                });
                let win = this.win;
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
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "input",
        "Single-line input handle constructor. `smelt.input.new(opts)` returns an `Input` userdata that owns a buffer/window pair wired to shared line-input editing semantics.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.input.Input",
        classification: smelt_core::lua::doc::classification_for_type("smelt.input.Input"),
        doc: "Single-line input handle returned by `smelt.input.new(opts)`.",
        fields: smelt_core::class_methods! {
            "win" => fn() -> super::win::LuaWin, "Return the underlying Win handle for layout, focus, and advanced event bindings.",
            "buf" => fn() -> Option<super::buf::LuaBuf>, "Return the backing buffer, or `nil` if the input window is gone.",
            "text" => fn() -> String, "Return the current input text.",
            "set_text" => fn(text: String) -> (), "Replace the current input text. Newlines are collapsed to spaces and the cursor moves to the end.",
            "on" => fn(event: LuaInputEvent, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Subscribe to `change`, `submit`, or `cancel`. Callback payload carries `ctx.text`.",
            "key" => fn(chord: String, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Bind `func` to `chord` on the underlying input window. Returns a Reg handle.",
        },
    });

    let shared = shared.clone();
    m.fn_(
        "new",
        "Create a single-line input and return an `Input` handle. Options: `text?`, `placeholder?`, plus window opts accepted by `smelt.win.new` (`region`, `name`, `pad_left`, `pad_right`, `wrap`, `scrollbar`, etc.).",
        &["opts"],
        move |lua, opts: Option<mlua::Table>| -> LuaResult<Option<LuaInput>> {
            let opts = input_opts(lua, opts)?;
            let name = opts
                .get::<Option<String>>("name")?
                .or_else(|| crate::lua::auto_name_for_scope(lua, "input"));
            if let Some(name) = name {
                opts.set("name", name.clone())?;
                opts.set("buf_name", format!("{name}.buf"))?;
            }
            let text = opts.get::<Option<String>>("text")?.unwrap_or_default();
            let buf_opts = lua.create_table()?;
            if let Some(buf_name) = opts.get::<Option<String>>("buf_name")? {
                buf_opts.set("name", buf_name)?;
            }
            buf_opts.set("editable", true)?;
            let buf = super::buf::create_or_open(&shared, Some(&buf_opts))?;
            crate::lua::try_with_ui_host(|host| {
                host.with_ui(|ui| {
                    if let Some(buffer) = ui.buf_mut(buf) {
                        buffer.set_lines(
                            0,
                            buffer.line_count(),
                            vec![crate::line_input::normalize_single_line(&text)],
                        );
                    }
                });
            });
            opts.set("surface", opts.get::<Option<String>>("surface")?.unwrap_or_else(|| "editable_text".into()))?;
            opts.set("kind", "input")?;
            opts.set("wrap", opts.get::<Option<bool>>("wrap")?.unwrap_or(false))?;
            opts.set("scrollbar", opts.get::<Option<bool>>("scrollbar")?.unwrap_or(false))?;
            opts.set("placeholder", opts.get::<Option<String>>("placeholder")?.unwrap_or_default())?;
            let win = super::win::open_or_refresh(buf, Some(&opts))?;
            Ok(win.map(|win| LuaInput { win }))
        },
    )?;

    Ok(())
}

fn input_opts(lua: &Lua, opts: Option<mlua::Table>) -> LuaResult<mlua::Table> {
    match opts {
        Some(opts) => Ok(opts),
        None => lua.create_table(),
    }
}

fn input_text(win: crate::smelt_edit::WinId) -> Option<String> {
    crate::lua::try_with_ui_host(|host| {
        host.with_ui(|ui| {
            let buffer = ui.win(win)?.buf;
            ui.buf(buffer)?.get_line(0).map(str::to_string)
        })
    })
    .flatten()
}
