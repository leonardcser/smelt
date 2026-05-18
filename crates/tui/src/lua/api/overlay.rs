//! `smelt.overlay` — Overlay handle. UiHost-only.
//!
//! `smelt.overlay.new(opts)` opens an overlay rendered from `opts.layout`
//! (a `smelt.overlay.layout` userdata tree) and returns an `Overlay`
//! userdata. `opts.name` opts the overlay into hot-reload survival.
//! `opts.keymaps` installs overlay-scoped bindings that fire when any
//! leaf of the overlay holds focus, without each leaf re-registering.

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use std::sync::Arc;

use crate::lua::{parse_keybind, LuaShared};

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
                app.close_overlay(this.id);
            });
            Ok(())
        });

        // ── key(chord, fn) → Reg ───────────────────────────────────
        methods.add_function(
            "key",
            |lua,
             (this_ud, chord, func): (mlua::AnyUserData, String, LuaCallback<mlua::Table, ()>)|
             -> LuaResult<LuaReg> {
                let this = *this_ud.borrow::<LuaOverlay>()?;
                install_overlay_key(lua, this.id, chord, func.into_inner())
            },
        );
    }
}

/// Register one overlay-scoped key binding. Returns a `Reg` whose `:remove()`
/// undoes the binding (and drops the Lua callback handle).
pub(crate) fn install_overlay_key(
    lua: &Lua,
    overlay: crate::smelt_term::OverlayId,
    chord: String,
    func: mlua::Function,
) -> LuaResult<LuaReg> {
    let Some(key) = parse_keybind(&chord) else {
        return Err(mlua::Error::RuntimeError(format!(
            "overlay:key: unknown chord `{chord}`"
        )));
    };
    let shared = current_shared(lua)?;
    let id = crate::lua::register_callback_handle(&shared, lua, func)?;
    crate::lua::with_app(|app| {
        let prev = app.ui.overlay_set_keymap(
            overlay,
            key,
            crate::smelt_term::Callback::Lua(crate::smelt_term::LuaHandle(id)),
        );
        crate::lua::drop_displaced_lua_handle(app, prev);
    });
    Ok(LuaReg::new(move || {
        let mut removed = false;
        crate::lua::with_app(|app| {
            let prev = app.ui.overlay_clear_keymap(overlay, key);
            removed = prev.is_some();
            crate::lua::drop_displaced_lua_handle(app, prev);
        });
        removed
    }))
}

fn current_shared(lua: &Lua) -> LuaResult<Arc<LuaShared>> {
    lua.named_registry_value::<mlua::AnyUserData>("__smelt_shared")?
        .borrow::<super::win::SharedHandle>()
        .map(|h| h.0.clone())
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "overlay",
        "Overlay handle constructor. `smelt.overlay.new(opts)` opens an overlay from `opts.layout` (a `smelt.overlay.layout` userdata) and returns an `Overlay` userdata. \
`opts.name` opts the overlay into hot-reload survival. `opts.keymaps` (list of `{key, on_press, hint?}`) installs overlay-scoped bindings. UiHost-only.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.overlay.Overlay",
        doc: "Overlay handle returned by `smelt.overlay.new(opts)`.",
        fields: smelt_core::class_methods! {
            "close" => fn() -> (), "Close the overlay. No-op if already closed.",
            "key" => fn(chord: String, func: LuaCallback<mlua::Table, ()>) -> LuaReg, "Bind `func` to `chord` on this overlay. Fires when any leaf of the overlay holds focus, after a per-window keymap miss but before global Lua keymaps. Returns a Reg whose `:remove()` undoes the binding.",
        },
    });

    m.fn_(
        "new",
        "Open an overlay rendered from `opts.layout` (a `smelt.overlay.layout` userdata) and return an `Overlay` userdata. `opts.name` opts the overlay into hot-reload survival. `opts.keymaps` (list of `{key, on_press, hint?}`) installs overlay-scoped bindings.",
        &["opts"],
        |lua, opts: mlua::Table| -> LuaResult<LuaOverlay> {
            let keymaps: Option<mlua::Table> = opts.get("keymaps").ok().flatten();
            // Auto-name overlays declared without `opts.name` inside a
            // module body so they survive `/reload` keyed by their
            // declaration site. Skipped when the caller already passed
            // a name or when no plugin scope is active.
            let has_name = opts
                .get::<Option<String>>("name")
                .ok()
                .flatten()
                .is_some();
            if !has_name {
                if let Some(auto) = crate::lua::auto_name_for_scope(lua, "overlay") {
                    opts.set("name", auto)?;
                }
            }
            let id = crate::lua::with_app(|app| crate::lua::ui_ops::open_overlay(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("overlay: {e}")))?;
            let overlay_id = crate::smelt_term::OverlayId(id as u32);

            if let Some(kms) = keymaps {
                // Clear any prior overlay-scoped bindings on a named re-open so the
                // freshly parsed list fully replaces the old one (no stale chords
                // hanging around when a plugin hot-reloads).
                crate::lua::with_app(|app| {
                    for stale in app.ui.overlay_clear_callbacks(overlay_id) {
                        app.lua.remove_callback(stale);
                    }
                });
                for pair in kms.sequence_values::<mlua::Table>() {
                    let entry = pair?;
                    let chord: String = entry
                        .get("key")
                        .map_err(|e| LuaError::RuntimeError(format!("overlay keymaps: {e}")))?;
                    let func: mlua::Function = entry.get("on_press").map_err(|e| {
                        LuaError::RuntimeError(format!(
                            "overlay keymaps: missing on_press for `{chord}`: {e}"
                        ))
                    })?;
                    install_overlay_key(lua, overlay_id, chord, func)?;
                }
            }

            Ok(LuaOverlay { id: overlay_id })
        },
    )?;

    super::overlay_layout::register(&m)?;
    Ok(())
}
