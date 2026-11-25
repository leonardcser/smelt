//! Host primitives for root-docked `smelt.dialog` layouts.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

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

impl smelt_core::lua::lua_type::LuaType for LuaDockedDialog {
    fn lua_type() -> String {
        "any".into()
    }
}

impl mlua::UserData for LuaDockedDialog {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("close", |_, this, ()| {
            crate::lua::with_ui_host(|host| host.close_docked_dialog(this.id));
            Ok(())
        });
        methods.add_method("toggle_expanded", |_, this, ()| {
            crate::lua::with_ui_host(|host| host.toggle_docked_dialog_expanded(this.id));
            Ok(())
        });
        methods.add_method(
            "scope",
            |lua, this, (name, actions): (String, Option<mlua::Table>)| {
                super::keymap::install_scope(
                    lua,
                    crate::smelt_edit::ScopeOwner::Modal(this.modal),
                    name,
                    actions,
                )
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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let module = LuaMod::supported(
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
            if smelt_core::lua::reg::RegistrationScope::active() {
                return Err(mlua::Error::external(
                    "view factories return layouts; open dialogs outside construction",
                ));
            }
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
            let (id, modal) = crate::lua::with_ui_host(|host| {
                host.open_docked_dialog(
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
