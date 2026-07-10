//! Host primitives for root-docked `smelt.dialog` layouts.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::lua_type::LuaCallback;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;

use crate::lua::parse_keybind;

struct LuaDialogOpenOpts(mlua::Table);

impl smelt_core::lua::lua_type::LuaType for LuaDialogOpenOpts {
    fn lua_type() -> String {
        "table".into()
    }
}

impl FromLua for LuaDialogOpenOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(Self(mlua::Table::from_lua(value, lua)?))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LuaDockedDialog {
    id: crate::smelt_edit::ContainerId,
    modal: crate::smelt_edit::ModalId,
}

impl mlua::UserData for LuaDockedDialog {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("close", |_, this, ()| {
            crate::lua::with_app(|app| app.close_docked_dialog(this.id));
            Ok(())
        });
        methods.add_method("toggle_expanded", |_, this, ()| {
            crate::lua::with_app(|app| app.toggle_docked_dialog_expanded(this.id));
            Ok(())
        });
        methods.add_function(
            "key",
            |lua,
             (this_ud, chord, func): (mlua::AnyUserData, String, LuaCallback<mlua::Table, ()>)|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaDockedDialog>()?;
                install_modal_key(lua, this.modal, chord, func.into_inner())
            },
        );
    }
}

fn optional_constraint(
    opts: &mlua::Table,
    key: &str,
) -> LuaResult<Option<crate::smelt_edit::Constraint>> {
    let value = opts.get::<mlua::Value>(key)?;
    if matches!(value, mlua::Value::Nil) {
        return Ok(None);
    }
    crate::lua::parse::constraint(Some(value), &format!("smelt.dialog.{key}"))
        .map(Some)
        .map_err(mlua::Error::external)
}

fn install_modal_key(
    lua: &Lua,
    modal: crate::smelt_edit::ModalId,
    chord: String,
    func: mlua::Function,
) -> LuaResult<LuaReg> {
    let Some(key) = parse_keybind(&chord) else {
        return Err(mlua::Error::RuntimeError(format!(
            "dialog:key: unknown chord `{chord}`"
        )));
    };
    let shared = lua
        .named_registry_value::<mlua::AnyUserData>("__smelt_shared")?
        .borrow::<super::win::SharedHandle>()?
        .0
        .clone();
    let callback = crate::lua::register_callback_handle(&shared, lua, func)?;
    crate::lua::with_app(|app| {
        let previous = app.ui.modal_set_keymap(
            modal,
            key,
            crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(callback)),
        );
        crate::lua::drop_displaced_lua_handle(app, previous);
    });
    Ok(LuaReg::new(move || {
        let mut removed = false;
        crate::lua::with_app(|app| {
            let previous = app.ui.modal_clear_keymap(modal, key);
            removed = previous.is_some();
            crate::lua::drop_displaced_lua_handle(app, previous);
        });
        removed
    }))
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let module = LuaMod::under(
        lua,
        smelt,
        "dialog",
        "Root-docked modal dialog primitives. UiHost-only.",
        Tier::UiHost,
    )?;
    module.private_fn(
        "__open",
        &["opts"],
        |_, (opts,): (LuaDialogOpenOpts,)| -> LuaResult<LuaDockedDialog> {
            let opts = opts.0;
            let layout = opts
                .get::<mlua::AnyUserData>("layout")?
                .borrow::<super::overlay_layout::LuaUiLayout>()?
                .0
                .clone();
            let height = crate::lua::parse::constraint(
                opts.get::<mlua::Value>("height").ok(),
                "smelt.dialog.height",
            )
            .map_err(mlua::Error::external)?;
            let min_height = optional_constraint(&opts, "min_height")?;
            let max_height = optional_constraint(&opts, "max_height")?;
            let blocks_agent = opts.get::<bool>("blocks_agent").unwrap_or(false);
            let resizable = opts.get::<Option<bool>>("resizable")?.unwrap_or(true);
            let (id, modal) = crate::lua::with_app(|app| {
                app.open_docked_dialog(
                    layout,
                    height,
                    min_height,
                    max_height,
                    blocks_agent,
                    resizable,
                )
            })
            .map_err(mlua::Error::external)?;
            Ok(LuaDockedDialog { id, modal })
        },
    )?;
    Ok(())
}
