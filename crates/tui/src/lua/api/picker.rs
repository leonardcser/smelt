//! `smelt.picker` - Picker handle. UiHost-only.
//!
//! `smelt.picker.new(opts)` opens a picker overlay and returns a `Picker`
//! userdata whose methods drive selection and item replacement. The
//! yield-until-pick wrapper lives in pure Lua (see
//! `runtime/lua/smelt/picker.lua`).

use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaType};
use smelt_core::lua::module::LuaMod;

/// Lua-side handle for a picker. Backed by a `WinId` - the picker is
/// just a list-style win wrapped in an overlay, so methods delegate to
/// the same ui_ops helpers the bundled picker plugin already uses.
#[derive(Clone, Copy, Debug)]
pub struct LuaPicker {
    pub(crate) win: crate::smelt_term::WinId,
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
            crate::lua::with_app(|app| {
                app.close_overlay_leaf(this.win);
            });
            Ok(())
        });

        // ── items(items, selected?) - replace items + land cursor atomically
        methods.add_function(
            "items",
            |_,
             (this_ud, items_tbl, selected): (mlua::AnyUserData, mlua::Table, Option<i64>)|
             -> LuaResult<mlua::AnyUserData> {
                let this = *this_ud.borrow::<LuaPicker>()?;
                let mut items = Vec::new();
                for v in items_tbl.sequence_values::<mlua::Value>() {
                    let value = v?;
                    let it = crate::lua::ui_ops::parse_picker_item(&value)
                        .map_err(LuaError::RuntimeError)?;
                    items.push(it);
                }
                let sel = selected.map(|i| i.max(0) as usize).unwrap_or(0);
                crate::lua::with_app(|app| {
                    crate::picker::set_items(app, this.win, items, sel);
                });
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
                        crate::lua::with_app(|app| {
                            crate::picker::set_selected(app, this.win, index);
                        });
                        Ok(mlua::Value::UserData(this_ud))
                    }
                    None => {
                        let i = crate::lua::try_with_app(|app| {
                            crate::picker::selected_index(app, this.win)
                        })
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
                crate::lua::with_app(|app| {
                    crate::picker::move_selected(app, this.win, delta as isize);
                });
                Ok(this_ud)
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "picker",
        "Picker handle constructor. `smelt.picker.new(opts)` opens a picker overlay and returns a `Picker` userdata. \
The picker is non-blocking; the yield-until-pick wrapper lives in pure Lua as `smelt.picker.choose(opts)`.",
        Tier::UiHost,
    )?;

    record_class(LuaClassDecl {
        name: "smelt.picker.Picker",
        doc: "Picker handle returned by `smelt.picker.new(opts)`. Setter methods return the same handle for chaining.",
        fields: smelt_core::class_methods! {
            "win" => fn() -> super::win::LuaWin, "Return the underlying Win handle (use `win:key(...)`, `win:on(...)` to bind input).",
            "close" => fn() -> (), "Close the picker overlay. No-op if already closed.",
            "items" => fn(items: mlua::Table, selected: Option<i64>) -> LuaPicker, "Replace the picker's items. Each entry is a string or `{ label, description?, ansi_color?, prefix?, ... }`. `selected` is the 0-based logical index to land the cursor on (default 0 - top of the new list); pass the current selection here to avoid a flash to row 0 followed by a separate `:selected()` call. Returns the handle for chaining.",
            "selected" => fn(idx: Option<i64>) -> mlua::Value, "Read or write the current logical selection (0-based). Without arg returns the index (`nil` if the picker is empty); with arg sets the selection and returns the handle for chaining.",
            "move" => fn(delta: i64) -> LuaPicker, "Move the picker's cursor by `delta` rows (clamped to the buffer's line count). Returns the handle for chaining.",
        },
    });

    m.fn_(
        "new",
        "Open a picker overlay and return a `Picker` userdata. The picker is non-blocking; the yield-until-pick wrapper lives in pure Lua as `smelt.picker.choose(opts)`.",
        &["opts"],
        |_, opts: mlua::Table| -> LuaResult<LuaPicker> {
            let win = crate::lua::with_app(|app| crate::lua::ui_ops::open_picker(app, opts))
                .map_err(|e| LuaError::RuntimeError(format!("picker: {e}")))?;
            Ok(LuaPicker { win })
        },
    )?;

    Ok(())
}
