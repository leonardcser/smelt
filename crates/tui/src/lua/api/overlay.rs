//! `smelt.overlay` - Overlay handle. UiHost-only.
//!
//! `smelt.overlay.new(opts)` opens an overlay rendered from `opts.layout`
//! (a `smelt.ui.layout` userdata tree) and returns an `Overlay`
//! userdata. `opts.name` opts the overlay into hot-reload survival.
//! `opts.keymaps` installs overlay-scoped bindings that fire when any
//! leaf of the overlay holds focus, without each leaf re-registering.

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaCallback, LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;
use std::sync::Arc;

use crate::lua::{parse_keybind, LuaShared};

/// Options accepted by `smelt.overlay.new(opts)`.
struct LuaOverlayNewOpts(mlua::Table);

impl LuaType for LuaOverlayNewOpts {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.overlay.Keymap",
            doc: "One overlay-scoped key binding installed by `smelt.overlay.new({ keymaps = ... })`.",
            fields: vec![
                LuaClassField {
                    name: "key",
                    ty: "string".into(),
                    optional: false,
                    doc: "Key chord such as `<Esc>`, `<C-j>`, or `q`.",
                },
                LuaClassField {
                    name: "on_press",
                    ty: "fun(ctx: table)".into(),
                    optional: false,
                    doc: "Handler invoked when the key fires while any overlay leaf has focus.",
                },
                LuaClassField {
                    name: "hint",
                    ty: "string".into(),
                    optional: true,
                    doc: "Human-readable hint for key-discovery plugins.",
                },
            ],
        });
        record_class(LuaClassDecl {
            name: "smelt.overlay.NewOpts",
            doc: "Options for `smelt.overlay.new(opts)`. The overlay body comes from a `smelt.ui.layout` tree.",
            fields: vec![
                LuaClassField { name: "layout", ty: "smelt.ui.layout".into(), optional: false, doc: "Layout tree to render inside the overlay." },
                LuaClassField { name: "name", ty: "string".into(), optional: true, doc: "Stable name used to hot-reload this overlay in place." },
                LuaClassField { name: "title", ty: "string | table".into(), optional: true, doc: "Optional title rendered in the overlay border." },
                LuaClassField { name: "border", ty: "table".into(), optional: true, doc: "Border style override; parsed with the shared border vocabulary." },
                LuaClassField { name: "anchor", ty: "\"dock_bottom\"|\"dock_top\"|\"dock_left\"|\"dock_right\"|\"center\"|\"screen_at\"|\"win\"".into(), optional: true, doc: "Where to place the overlay. Defaults to `dock_bottom`." },
                LuaClassField { name: "above_rows", ty: "integer".into(), optional: true, doc: "Rows to keep clear above the bottom dock, typically the statusline height." },
                LuaClassField { name: "target", ty: "smelt.win.Win | integer".into(), optional: true, doc: "Target window for `anchor = \"win\"`." },
                LuaClassField { name: "attach", ty: "string".into(), optional: true, doc: "Alignment point for `anchor = \"win\"` such as `nw`, `center`, or `se`." },
                LuaClassField { name: "row", ty: "integer".into(), optional: true, doc: "Screen row offset for `anchor = \"screen_at\"`." },
                LuaClassField { name: "col", ty: "integer".into(), optional: true, doc: "Screen column offset for `anchor = \"screen_at\"`." },
                LuaClassField { name: "row_offset", ty: "integer".into(), optional: true, doc: "Row offset for `anchor = \"win\"`." },
                LuaClassField { name: "col_offset", ty: "integer".into(), optional: true, doc: "Column offset for `anchor = \"win\"`." },
                LuaClassField { name: "corner", ty: "string".into(), optional: true, doc: "Corner used by `anchor = \"screen_at\"` (`nw`, `ne`, `sw`, or `se`)." },
                LuaClassField { name: "width", ty: "integer | string | table".into(), optional: true, doc: "Overlay width constraint. Accepts cells, `\"N%\"`, `\"fit\"`, `\"fill\"`, `\"min:N\"`, `\"max:N\"`, `\"ratio:N/M\"`, or long table form." },
                LuaClassField { name: "height", ty: "integer | string | table".into(), optional: true, doc: "Overlay height constraint. Same vocabulary as `width`." },
                LuaClassField { name: "max_width", ty: "integer | string | table".into(), optional: true, doc: "Optional upper bound applied after width resolves." },
                LuaClassField { name: "max_height", ty: "integer | string | table".into(), optional: true, doc: "Optional upper bound applied after height resolves." },
                LuaClassField { name: "min_width", ty: "integer | string | table".into(), optional: true, doc: "Optional lower bound applied after width resolves." },
                LuaClassField { name: "min_height", ty: "integer | string | table".into(), optional: true, doc: "Optional lower bound applied after height resolves." },
                LuaClassField { name: "modal", ty: "boolean".into(), optional: true, doc: "Whether the overlay blocks input behind it. Defaults to true." },
                LuaClassField { name: "blocks_agent", ty: "boolean".into(), optional: true, doc: "Whether the overlay should block agent progress while open." },
                LuaClassField { name: "z", ty: "integer".into(), optional: true, doc: "Z-index. Higher overlays render above lower overlays." },
                LuaClassField { name: "draggable", ty: "boolean | smelt.overlay.DragConfig".into(), optional: true, doc: "Enable or configure mouse dragging." },
                LuaClassField { name: "resizable", ty: "boolean | smelt.overlay.ResizeConfig".into(), optional: true, doc: "Enable or configure mouse resize handles." },
                LuaClassField { name: "keymaps", ty: "smelt.overlay.Keymap[]".into(), optional: true, doc: "Overlay-scoped key bindings." },
            ],
        });
        "smelt.overlay.NewOpts".into()
    }
}

impl FromLua for LuaOverlayNewOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(Self(mlua::Table::from_lua(value, lua)?))
    }
}

/// Lua-side handle for an `OverlayId`.
#[derive(Clone, Copy, Debug)]
pub struct LuaOverlay {
    pub(crate) id: crate::smelt_edit::OverlayId,
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
            crate::lua::with_ui_host(|host| host.close_overlay(this.id));
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
    overlay: crate::smelt_edit::OverlayId,
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
    crate::lua::with_ui_host(|host| {
        host.set_overlay_keymap(
            overlay,
            key,
            crate::smelt_edit::Callback::Lua(crate::smelt_edit::LuaHandle(id)),
        );
    });
    Ok(LuaReg::new(move || {
        crate::lua::app_ref::defer_registered_lua_operation(
            &shared,
            id,
            crate::lua::app_ref::DeferredLuaOperation::OverlayKeymap { overlay, key },
        )
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
        "Overlay handle constructor. `smelt.overlay.new(opts)` opens an overlay from `opts.layout` (a `smelt.ui.layout` userdata) and returns an `Overlay` userdata. \
`opts.name` opts the overlay into hot-reload survival. `opts.width` and `opts.height` size the overlay rect (per axis) using the same constraint vocabulary as `layout.vbox`/`hbox` slots - integer cells, `\"N%\"`, `\"fit\"` (default; read the layout's natural size), `\"fill\"`, `\"max:N\"`, etc. `opts.max_width`/`opts.max_height` cap the resolved size from above; `opts.min_width`/`opts.min_height` floor it from below - pair either with `\"fit\"` to express \"shrink to content, clamped between floor and cap\". `opts.draggable` accepts a boolean or `smelt.overlay.DragConfig`; `true` enables title drag plus inert-body drag. `opts.resizable` accepts a boolean or `smelt.overlay.ResizeConfig`; `true` enables floating-safe left/right/bottom handles without stealing the title row. `opts.keymaps` (list of `{key, on_press, hint?}`) installs overlay-scoped bindings. UiHost-only.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.overlay.DragConfig",
        doc: "Overlay drag configuration table. Use `true` for the floating default: title chrome plus inert-body drag.",
        fields: vec![
            LuaClassField {
                name: "title",
                ty: "boolean".into(),
                optional: true,
                doc: "When true, the overlay chrome moves the overlay unless a resize handle owns the cell.",
            },
            LuaClassField {
                name: "body",
                ty: "boolean | \"inert\"".into(),
                optional: true,
                doc: "`true` moves from any body leaf; `\"inert\"` moves only from non-focusable, non-selectable leaves.",
            },
        ],
    });

    record_class(LuaClassDecl {
        name: "smelt.overlay.ResizeConfig",
        doc: "Overlay resize configuration table. Use `true` for the floating default: left/right/bottom edges and corners, leaving the top chrome for drag.",
        fields: vec![
            LuaClassField {
                name: "top",
                ty: "boolean".into(),
                optional: true,
                doc: "Enable top-edge resize.",
            },
            LuaClassField {
                name: "right",
                ty: "boolean".into(),
                optional: true,
                doc: "Enable right-edge resize.",
            },
            LuaClassField {
                name: "bottom",
                ty: "boolean".into(),
                optional: true,
                doc: "Enable bottom-edge resize.",
            },
            LuaClassField {
                name: "left",
                ty: "boolean".into(),
                optional: true,
                doc: "Enable left-edge resize.",
            },
            LuaClassField {
                name: "corners",
                ty: "boolean".into(),
                optional: true,
                doc: "Upgrade cells where two enabled edges meet into diagonal resize handles.",
            },
        ],
    });

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
        "Open an overlay rendered from `opts.layout` (a `smelt.ui.layout` userdata) and return an `Overlay` userdata. `opts.name` opts the overlay into hot-reload survival. `opts.width` and `opts.height` size the overlay rect (per axis) using the same constraint vocabulary as `layout.vbox`/`hbox` slots - integer cells, `\"N%\"`, `\"fit\"` (default; read the layout's natural size), `\"fill\"`, `\"max:N\"`, etc. `opts.max_width`/`opts.max_height` cap the resolved size from above; `opts.min_width`/`opts.min_height` floor it from below - pair either with `\"fit\"` to express \"shrink to content, clamped between floor and cap\". `opts.draggable` accepts a boolean or `smelt.overlay.DragConfig`; `opts.resizable` accepts a boolean or `smelt.overlay.ResizeConfig`. `opts.keymaps` (list of `{key, on_press, hint?}`) installs overlay-scoped bindings.",
        &["opts"],
        |lua, (opts,): (LuaOverlayNewOpts,)| -> LuaResult<LuaOverlay> {
            let opts = opts.0;
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
            let id = crate::lua::with_ui_host(|host| host.open_overlay(opts))
                .map_err(|e| LuaError::RuntimeError(format!("overlay: {e}")))?;
            let overlay_id = crate::smelt_edit::OverlayId(id as u32);

            if let Some(kms) = keymaps {
                // Clear any prior overlay-scoped bindings on a named re-open so the
                // freshly parsed list fully replaces the old one (no stale chords
                // hanging around when a plugin hot-reloads).
                crate::lua::with_ui_host(|host| host.clear_overlay_callbacks(overlay_id));
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

    Ok(())
}
