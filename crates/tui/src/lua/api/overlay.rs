//! `smelt.overlay` — Overlay handle. UiHost-only.
//!
//! `smelt.overlay.new(opts)` opens an overlay rendered from `opts.layout`
//! (a `smelt.overlay.layout` userdata tree) and returns an `Overlay`
//! userdata. `opts.name` opts the overlay into hot-reload survival.

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;

/// Lua-side handle for an `OverlayId`.
#[derive(Clone, Copy, Debug)]
pub struct LuaOverlay {
    pub(crate) id: crate::smelt_term::OverlayId,
}

impl LuaType for LuaOverlay {
    fn lua_type() -> String {
        "smelt.overlay.Overlay".into()
    }
}

impl mlua::UserData for LuaOverlay {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Overlay#{}", this.id.0))
        });

        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                // Closing an overlay = closing its first leaf; the
                // overlay registry tears down automatically when its
                // leaves are gone.
                if let Some(leaf) = app
                    .ui
                    .overlay(this.id)
                    .and_then(|ov| ov.layout.leaves_in_order().into_iter().next())
                {
                    app.close_overlay_leaf(crate::smelt_term::WinId(leaf.0));
                }
            });
            Ok(())
        });
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "overlay",
        "Overlay handle constructor. `smelt.overlay.new(opts)` opens an overlay from `opts.layout` (a `smelt.overlay.layout` userdata) and returns an `Overlay` userdata. \
`opts.name` opts the overlay into hot-reload survival. UiHost-only.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.overlay.Overlay",
        doc: "Overlay handle returned by `smelt.overlay.new(opts)`.",
        fields: smelt_core::class_methods! {
            "close" => fn() -> (), "Close the overlay. No-op if already closed.",
        },
    });

    m.fn_(
        "new",
        "Open an overlay rendered from `opts.layout` (a `smelt.overlay.layout` userdata) and return an `Overlay` userdata. `opts.name` opts the overlay into hot-reload survival.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaOverlay> {
            let id = crate::lua::with_app(|app| crate::lua::ui_ops::open_overlay(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("overlay: {e}")))?;
            Ok(LuaOverlay {
                id: crate::smelt_term::OverlayId(id as u32),
            })
        },
    )?;

    super::overlay_layout::register(&m)?;
    Ok(())
}
