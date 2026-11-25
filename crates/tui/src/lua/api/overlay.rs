//! `smelt.overlay` - Overlay handle. UiHost-only.
//!
//! `smelt.overlay.new(opts)` opens an overlay rendered from `opts.layout`
//! (a `smelt.ui.layout` userdata tree) and returns an `Overlay`
//! userdata. `opts.name` opts the overlay into hot-reload survival.
//! `opts.scope` puts the overlay in a keymap scope, whose bindings fire when
//! any leaf of the overlay holds focus, with `opts.actions` implementing it.

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;

/// Options accepted by `smelt.overlay.new(opts)`.
struct LuaOverlayNewOpts(mlua::Table);

impl LuaType for LuaOverlayNewOpts {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.overlay.NewOpts",
            classification: smelt_core::lua::doc::classification_for_type("smelt.overlay.NewOpts"),
            doc: "Options for `smelt.overlay.new(opts)`. The overlay body comes from a `smelt.ui.layout` tree.",
            fields: vec![
                LuaClassField { name: "layout", ty: "smelt.ui.layout".into(), optional: false, doc: "Layout tree to render inside the overlay." },
                LuaClassField { name: "name", ty: "string".into(), optional: true, doc: "Stable name used to hot-reload this overlay in place." },
                LuaClassField { name: "title", ty: "string | smelt.Span | (string | smelt.Span)[]".into(), optional: true, doc: "Optional title rendered in the overlay border." },
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
                LuaClassField { name: "modal", ty: "boolean".into(), optional: true, doc: "Whether the overlay blocks input behind it. Defaults to true, or false when `transparent`." },
                LuaClassField { name: "blocks_agent", ty: "boolean".into(), optional: true, doc: "Whether the overlay should block agent progress while open." },
                LuaClassField { name: "reserve", ty: "boolean".into(), optional: true, doc: "Keep navigation targets below this top-anchored window overlay. Uses its clipped bottom edge; overlapping reservations are not added. Does not move or stack overlays. Defaults to false." },
                LuaClassField { name: "transparent", ty: "boolean".into(), optional: true, doc: "Draw the overlay as a see-through layer: its rect is not cleared, so only the cells its leaves draw cover what is under it, and the mouse passes through. Not modal by default; cannot be modal, draggable or resizable." },
                LuaClassField { name: "z", ty: "integer".into(), optional: true, doc: "Z-index. Higher overlays render above lower overlays." },
                LuaClassField { name: "draggable", ty: "boolean | smelt.overlay.DragConfig".into(), optional: true, doc: "Enable or configure mouse dragging." },
                LuaClassField { name: "resizable", ty: "boolean | smelt.overlay.ResizeConfig".into(), optional: true, doc: "Enable or configure mouse resize handles." },
                LuaClassField { name: "scope", ty: "string".into(), optional: true, doc: "Keymap scope of the overlay. Its bindings fire while any leaf of the overlay has focus, after the focused window's own scope." },
                LuaClassField { name: "actions", ty: "table<string, fun(ctx: table): boolean?>".into(), optional: true, doc: "Implementations of the scope's actions, by action name. Requires `scope`." },
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

impl FromLua for LuaOverlay {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(*mlua::AnyUserData::from_lua(value, lua)?.borrow::<Self>()?)
    }
}

impl mlua::UserData for LuaOverlay {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Overlay#{}", this.id.0))
        });

        methods.add_method("rect", |lua, this, ()| -> LuaResult<Option<mlua::Table>> {
            crate::lua::with_ui_host(|host| host.with_ui(|ui| ui.resolved_overlay_rect(this.id)))
                .map(|rect| crate::lua::parse::rect_to_lua(lua, rect))
                .transpose()
        });

        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.close_overlay(this.id));
            Ok(())
        });

        methods.add_method(
            "scope",
            |lua, this, (name, actions): (String, Option<mlua::Table>)| {
                super::keymap::install_scope(
                    lua,
                    crate::smelt_edit::ScopeOwner::Overlay(this.id),
                    name,
                    actions,
                )
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "overlay",
        "Overlay handle constructor. `smelt.overlay.new(opts)` opens an overlay from `opts.layout` (a `smelt.ui.layout` userdata) and returns an `Overlay` userdata. \
`opts.name` opts the overlay into hot-reload survival. `opts.width` and `opts.height` size the overlay rect (per axis) using the same constraint vocabulary as `layout.vbox`/`hbox` slots - integer cells, `\"N%\"`, `\"fit\"` (default; read the layout's natural size), `\"fill\"`, `\"max:N\"`, etc. `opts.max_width`/`opts.max_height` cap the resolved size from above; `opts.min_width`/`opts.min_height` floor it from below - pair either with `\"fit\"` to express \"shrink to content, clamped between floor and cap\". `opts.draggable` accepts a boolean or `smelt.overlay.DragConfig`; `true` enables title drag plus inert-body drag. `opts.resizable` accepts a boolean or `smelt.overlay.ResizeConfig`; `true` enables floating-safe left/right/bottom handles without stealing the title row. `opts.scope` names the overlay's keymap scope and `opts.actions` implements its actions. UiHost-only.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.overlay.DragConfig",
        classification: smelt_core::lua::doc::classification_for_type("smelt.overlay.DragConfig"),
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
        classification: smelt_core::lua::doc::classification_for_type("smelt.overlay.ResizeConfig"),
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
        classification: smelt_core::lua::doc::classification_for_type("smelt.overlay.Overlay"),
        doc: "Overlay handle returned by `smelt.overlay.new(opts)`.",
        fields: smelt_core::class_methods! {
            "rect" => fn() -> Option<mlua::Table>, "Return the current compositor-resolved rectangle, including chrome, or nil when closed or its anchor is unavailable.",
            "close" => fn() -> (), "Close the overlay. No-op if already closed.",
            "scope" => fn(name: String, actions: Option<mlua::Table>) -> (), "Put the overlay in keymap scope `name`, with `actions` mapping the scope's action names to functions. Replaces its previous scope.",
        },
    });

    m.fn_(
        "new",
        "Open an overlay rendered from `opts.layout` (a `smelt.ui.layout` userdata) and return an `Overlay` userdata. `opts.name` opts the overlay into hot-reload survival. `opts.width` and `opts.height` size the overlay rect (per axis) using the same constraint vocabulary as `layout.vbox`/`hbox` slots - integer cells, `\"N%\"`, `\"fit\"` (default; read the layout's natural size), `\"fill\"`, `\"max:N\"`, etc. `opts.max_width`/`opts.max_height` cap the resolved size from above; `opts.min_width`/`opts.min_height` floor it from below - pair either with `\"fit\"` to express \"shrink to content, clamped between floor and cap\". `opts.draggable` accepts a boolean or `smelt.overlay.DragConfig`; `opts.resizable` accepts a boolean or `smelt.overlay.ResizeConfig`. `opts.scope` names the overlay's keymap scope and `opts.actions` implements its actions.",
        &["opts"],
        |lua, (opts,): (LuaOverlayNewOpts,)| -> LuaResult<LuaOverlay> {
            if smelt_core::lua::reg::RegistrationScope::active() {
                return Err(LuaError::external("view factories return layouts; open overlays outside construction"));
            }
            let opts = opts.0;
            let scope: Option<String> = opts.get("scope")?;
            let actions: Option<mlua::Table> = opts.get("actions")?;
            if scope.is_none() && actions.is_some() {
                return Err(LuaError::RuntimeError("overlay: `actions` requires `scope`".into()));
            }
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

            if let Some(scope) = scope {
                super::keymap::install_scope(
                    lua,
                    crate::smelt_edit::ScopeOwner::Overlay(overlay_id),
                    scope,
                    actions,
                )?;
            }

            Ok(LuaOverlay { id: overlay_id })
        },
    )?;

    Ok(())
}
