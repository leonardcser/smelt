//! `smelt.busy(label)` — push a busy-stack token. Returns a `Reg`
//! whose `:remove()` pops it. UiHost-only.
//!
//! Runs in parallel with `smelt.spinner.busy` while the reactive
//! `work_*` cells become the canonical work-state surface; both APIs
//! push onto the same per-app `BusyStack`. The cell publisher running
//! on the main tick republishes `work_state` / `work_label` / `work_busy`
//! on diff, so callers should subscribe to `smelt.cell("work_state")`
//! rather than polling.
//!
//! `kind` slot in the table form is reserved for a future categorisation
//! (e.g. `"download"`, `"compaction"`); ignored at this step.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::extend(lua, smelt.clone(), "smelt", Tier::UiHost);
    m.fn_(
        "busy",
        "Push a busy token onto the per-app stack and return a `Reg` whose `:remove()` pops it. While any token is live the reactive `work_*` cells flip to the busy state (`work_state == \"busy\"`, `work_label` = top label). Mirrors `smelt.spinner.busy`; the cells are the canonical surface plugins should subscribe to.",
        &["label"],
        |_, label: BusyArg| -> LuaResult<LuaReg> {
            let id = crate::lua::with_app(|app| app.busy_stack.push(label.label));
            Ok(LuaReg::new(move || {
                crate::lua::try_with_app(|app| app.busy_stack.release(id)).unwrap_or(false)
            }))
        },
    )?;
    Ok(())
}

/// Accepts either a bare label string or a table with a `label` field
/// (and a reserved-for-future `kind`).
struct BusyArg {
    label: String,
}

impl mlua::FromLua for BusyArg {
    fn from_lua(value: mlua::Value, _lua: &mlua::Lua) -> LuaResult<Self> {
        match value {
            mlua::Value::String(s) => Ok(BusyArg {
                label: s.to_str()?.to_owned(),
            }),
            mlua::Value::Table(t) => {
                let label: String =
                    t.get("label")
                        .map_err(|_| mlua::Error::FromLuaConversionError {
                            from: "table",
                            to: "smelt.busy arg".into(),
                            message: Some("expected `label` field".into()),
                        })?;
                Ok(BusyArg { label })
            }
            other => Err(mlua::Error::FromLuaConversionError {
                from: other.type_name(),
                to: "string|{ label = string, kind? = string }".into(),
                message: Some("smelt.busy: expected string label or { label = ... } table".into()),
            }),
        }
    }
}

impl smelt_core::lua::lua_type::LuaType for BusyArg {
    fn lua_type() -> String {
        String::from("string|{ label: string, kind?: string }")
    }
}

impl smelt_core::lua::lua_type::LuaTypeTuple for BusyArg {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("label");
        format!(
            "{}: {}",
            name,
            <Self as smelt_core::lua::lua_type::LuaType>::lua_type()
        )
    }
}
