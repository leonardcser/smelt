//! `smelt.picker` - Picker handle. UiHost-only.
//!
//! `smelt.picker.new(opts)` opens a picker overlay and returns a `Picker`
//! userdata whose methods drive selection and item replacement. The
//! yield-until-pick wrapper lives in pure Lua (see
//! `runtime/lua/smelt/picker.lua`).

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;

/// Options accepted by `smelt.picker.new(opts)`.
struct LuaPickerNewOpts(mlua::Table);

impl LuaType for LuaPickerNewOpts {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.picker.Item",
            classification: smelt_core::lua::doc::classification_for_type("smelt.picker.Item"),
            doc: "Row accepted by picker constructors. A bare string is also accepted and is treated as `{ label = string }`.",
            fields: vec![
                LuaClassField { name: "label", ty: "string".into(), optional: false, doc: "Primary text shown for this row." },
                LuaClassField { name: "description", ty: "string".into(), optional: true, doc: "Secondary text shown next to the label." },
                LuaClassField { name: "prefix", ty: "string".into(), optional: true, doc: "Small prefix rendered before the label." },
                LuaClassField { name: "ansi_color", ty: "integer".into(), optional: true, doc: "ANSI color slot for the prefix." },
                LuaClassField { name: "label_color", ty: "integer".into(), optional: true, doc: "ANSI color slot for the label." },
                LuaClassField { name: "search_terms", ty: "string[]".into(), optional: true, doc: "Extra strings considered by fuzzy pickers." },
            ],
        });
        record_class(LuaClassDecl {
            name: "smelt.picker.NewOpts",
            classification: smelt_core::lua::doc::classification_for_type("smelt.picker.NewOpts"),
            doc: "Options for the low-level non-blocking picker handle constructor.",
            fields: vec![
                LuaClassField {
                    name: "items",
                    ty: "(string | smelt.picker.Item)[]".into(),
                    optional: false,
                    doc: "Initial picker rows. Must be non-empty.",
                },
                LuaClassField {
                    name: "placement",
                    ty: "\"center\"|\"bottom\"|\"cursor\"|\"prompt_docked\"".into(),
                    optional: true,
                    doc: "Where to place the picker. Defaults to `center`.",
                },
            ],
        });
        "smelt.picker.NewOpts".into()
    }
}

impl FromLua for LuaPickerNewOpts {
    fn from_lua(value: mlua::Value, lua: &Lua) -> LuaResult<Self> {
        Ok(Self(mlua::Table::from_lua(value, lua)?))
    }
}

/// Lua-side handle for a picker. Backed by a `WinId` - the picker is
/// just a list-style win wrapped in an overlay, so methods delegate to
/// the same ui_ops helpers the bundled picker plugin already uses.
#[derive(Clone, Copy, Debug)]
pub struct LuaPicker {
    pub(crate) win: crate::smelt_edit::WinId,
}

impl LuaType for LuaPicker {
    fn lua_type() -> String {
        "smelt.picker.Picker".into()
    }
}

impl mlua::UserData for LuaPicker {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Picker#{}", this.win.0))
        });

        // `win()` exposes the underlying Win handle so callers can
        // bind keys / events without duplicating the surface here.
        methods.add_method("win", |_, this, ()| Ok(super::win::LuaWin { id: this.win }));

        methods.add_method("close", |_, this, ()| -> LuaResult<()> {
            crate::lua::with_ui_host(|host| host.close_overlay_leaf(this.win));
            Ok(())
        });

        // ── items(items, selected?) - replace items + land cursor atomically
        methods.add_function(
            "items",
            |_,
             (this_ud, items_tbl, selected): (mlua::AnyUserData, mlua::Table, Option<i64>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaPicker>()?;
                let sel = selected.map(|i| i.max(0) as usize).unwrap_or(0);
                crate::lua::with_ui_host(|host| host.set_picker_items(this.win, &items_tbl, sel))
                    .map_err(LuaError::RuntimeError)?;
                Ok(this_ud)
            },
        );

        // ── selected(idx?) - get / set current logical row ─────────
        methods.add_function(
            "selected",
            |_, (this_ud, idx): (mlua::AnyUserData, Option<i64>)| -> LuaResult<mlua::Value> {
                let this = *this_ud.borrow::<LuaPicker>()?;
                match idx {
                    Some(i) => {
                        let index = if i < 0 { 0 } else { i as usize };
                        crate::lua::with_ui_host(|host| {
                            host.set_picker_selected(this.win, index);
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let i = crate::lua::try_with_ui_host(|host| host.picker_selected(this.win))
                            .flatten();
                        Ok(match i {
                            Some(v) => mlua::Value::Integer(v as i64),
                            None => mlua::Value::Nil,
                        })
                    }
                }
            },
        );

        // ── move(delta) - chainable; relative cursor move ──────────
        methods.add_function(
            "move",
            |_, (this_ud, delta): (mlua::AnyUserData, i64)| -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaPicker>()?;
                crate::lua::with_ui_host(|host| {
                    host.move_picker_selected(this.win, delta as isize);
                });
                Ok(this_ud)
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "picker",
        "Picker handle constructor. `smelt.picker.new(opts)` opens a picker overlay and returns a `Picker` userdata. \
The picker is non-blocking; the yield-until-pick wrapper lives in pure Lua as `smelt.picker.choose(opts)`.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.picker.Picker",
        classification: smelt_core::lua::doc::classification_for_type("smelt.picker.Picker"),
        doc: "Picker handle returned by `smelt.picker.new(opts)`. Setter methods return the same handle for chaining.",
        fields: smelt_core::class_methods! {
            "win" => fn() -> super::win::LuaWin, "Return the underlying Win handle (use `win:key(...)`, `win:on(...)` to bind input).",
            "close" => fn() -> (), "Close the picker overlay. No-op if already closed.",
            "items" => fn(items: mlua::Table, selected: Option<i64>) -> LuaPicker, "Replace the picker's items. Each entry is a string or `{ label, description?, ansi_color?, label_color?, prefix?, icon?, ... }`. `icon = { kind = \"file\"|\"dir\", path = string }` renders file-list icons unless `prefix` is set. `selected` is the 0-based logical index to land the cursor on (default 0 - top of the new list); pass the current selection here to avoid a flash to row 0 followed by a separate `:selected()` call. Returns the handle for chaining.",
            "selected" => fn(idx: Option<i64>) -> mlua::Value, "Read or write the current logical selection (0-based). Without arg returns the index (`nil` if the picker is empty); with arg sets the selection and returns the handle for chaining.",
            "move" => fn(delta: i64) -> LuaPicker, "Move the picker's cursor by `delta` rows (clamped to the buffer's line count). Returns the handle for chaining.",
        },
    });

    m.fn_(
        "new",
        "Open a picker overlay and return a `Picker` userdata. The picker is non-blocking; the yield-until-pick wrapper lives in pure Lua as `smelt.picker.choose(opts)`.",
        &["opts"],
        |_, (opts,): (LuaPickerNewOpts,)| -> LuaResult<LuaPicker> {
            let win = crate::lua::with_ui_host(|host| host.open_picker(opts.0))
                .map_err(|e| LuaError::RuntimeError(format!("picker: {e}")))?;
            Ok(LuaPicker { win })
        },
    )?;

    Ok(())
}
