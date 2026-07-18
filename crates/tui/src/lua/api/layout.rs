//! `smelt.ui.layout` - composable layout-tree primitives for the main TUI.
//!
//! Plugins register a composer with `smelt.ui.layout.set(fn)`. The host
//! invokes it once per frame with a state table describing the current
//! prompt height, terminal size, and other inputs; the composer returns
//! a layout tree built from `vbox` / `hbox` / `leaf` that the host
//! resolves to per-window rectangles for that frame.
//!
//! The same constructors also produce the layout userdata accepted by
//! `smelt.overlay.new` via `opts.layout` - there's one namespace for both
//! screen-composition and overlay-composition cases.

use crate::lua::LuaShared;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::LuaHandle;
use std::sync::Arc;

/// Terminal size returned by `smelt.ui.size()`.
struct LuaUiSize(mlua::Table);

impl LuaType for LuaUiSize {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.ui.Size",
            doc: "Terminal size in cells.",
            fields: vec![
                LuaClassField {
                    name: "width",
                    ty: "integer".into(),
                    optional: false,
                    doc: "Terminal width in cells.",
                },
                LuaClassField {
                    name: "height",
                    ty: "integer".into(),
                    optional: false,
                    doc: "Terminal height in cells.",
                },
            ],
        });
        "smelt.ui.Size".into()
    }
}

impl IntoLua for LuaUiSize {
    fn into_lua(self, _: &Lua) -> LuaResult<mlua::Value> {
        Ok(mlua::Value::Table(self.0))
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    // `smelt.ui` is a thin grouping namespace for screen-composition primitives.
    // It's distinct from `smelt.layout` (which composes block content inside
    // transcript messages for tool render callbacks).
    let ui = LuaMod::under(
        lua,
        smelt,
        "ui",
        "Screen-composition primitives: main layout composer and per-window renderer registration.",
        Tier::UiHost,
    )?;

    ui.fn_(
        "size",
        "Return the current terminal size as `{ width, height }` in cells. Useful for choosing between compact and wide overlay layouts without relying on any particular window's current rect.",
        &[],
        |lua, ()| -> LuaResult<LuaUiSize> {
            let (width, height) = crate::lua::try_with_app(|app| (app.last_width, app.last_height))
                .unwrap_or_else(|| crossterm::terminal::size().unwrap_or((80, 24)));
            let out = lua.create_table()?;
            out.set("width", width)?;
            out.set("height", height)?;
            Ok(LuaUiSize(out))
        },
    )?;

    let m = ui.sub(
        "layout",
        "Composable layout-tree primitives (set/vbox/hbox/leaf) for the main TUI layout. \
`smelt.ui.layout.set(fn)` registers a composer invoked once per frame; the \
composer returns a tree built from these constructors describing how the \
transcript, prompt, statusline, and any plugin-added windows split the \
screen.",
    )?;

    super::overlay_layout::register_layout_constructors(&m)?;

    {
        let s = shared.clone();
        m.fn_(
            "set",
            "Register the main layout composer. The callback receives a state \
table (`term_w`, `term_h`, `prompt_input_rows`, plus `dialog` while a root dialog is active) and \
returns a layout userdata built via `smelt.ui.layout.{vbox,hbox,leaf}`. `state.dialog` is an \
opaque transcript-dialog stage with host-owned sizing and expansion behavior. While a root dialog \
is active, the returned tree must include the current stage exactly once and no retained dialog \
stages from earlier calls; otherwise the host uses the safe transcript-dialog-statusline fallback. \
Passing `nil` clears the composer and \
reverts to the engine's hardcoded layout. Only the most recent registration is active; later calls \
replace earlier ones.",
            &["composer"],
            move |lua, composer: Option<mlua::Function>| -> LuaResult<()> {
                let handle = match composer {
                    Some(f) => Some(LuaHandle::from_func(lua, f)?),
                    None => None,
                };
                if let Ok(mut slot) = s.main_layout_composer.lock() {
                    *slot = handle;
                }
                if s.core.external_effects_active() {
                    let _ = crate::lua::try_with_app(|app| {
                        app.refresh_main_layout();
                    });
                }
                Ok(())
            },
        )?;
    }

    Ok(())
}
