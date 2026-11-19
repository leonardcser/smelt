//! `smelt.mode` - the agent modes. `smelt.mode.register` adds a mode,
//! `smelt.mode.current()` reads the active one, `smelt.mode.set(v)` sets it
//! in a TUI session, and `smelt.mode.cycle_list()` returns the cycle.

use super::permissions::LuaAccess;
use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaType;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::modes::{Above, Mode};
use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

/// What happens to a call that needs more access than its mode allows.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.mode.Above")]
pub enum LuaAbove {
    Ask,
    Deny,
}

/// A mode passed to `smelt.mode.register`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.mode.Spec")]
pub struct LuaModeSpec {
    /// Mode name: lowercase letters, digits, `-` and `_`.
    pub name: String,
    /// Status bar label. Default: the name.
    pub label: Option<String>,
    /// Status bar icon.
    pub icon: Option<String>,
    /// Status bar highlight group. Default: `SmeltModeDefault`.
    pub hl_group: Option<String>,
    /// What the model is told when the session enters the mode.
    pub note: Option<String>,
    /// The most a call may do without asking. Default: `"read"`.
    pub access: Option<LuaAccess>,
    /// What happens to calls that need more. Default: `"ask"`.
    pub above: Option<LuaAbove>,
    /// Insert the new mode after this one in the cycle.
    pub after: Option<String>,
}

/// A registered mode, as a Lua table.
pub(crate) struct LuaMode(pub Mode);

impl LuaType for LuaMode {
    fn lua_type() -> String {
        "smelt.mode.Mode".into()
    }
}

impl IntoLua for LuaMode {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let mode = self.0;
        let table = lua.create_table()?;
        table.set("name", mode.name.as_str())?;
        table.set("label", mode.label)?;
        table.set("icon", mode.icon)?;
        table.set("hl_group", mode.hl_group)?;
        table.set("note", mode.note)?;
        table.set("access", mode.access.as_str())?;
        table.set("above", mode.above.as_str())?;
        Ok(LuaValue::Table(table))
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "mode",
        "Agent modes. A mode sets the most a tool call may do without asking (see `smelt.permissions`). `smelt.mode.current()` reads the active mode, `smelt.mode.set(v)` sets it in a TUI session, and `smelt.mode.cycle_list()` lists the cycle.",
        Tier::Host,
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "register",
            "Register an agent mode. A new name goes at the end of the cycle, or after `spec.after`. Registering an existing name replaces it in place.",
            &["spec"],
            move |_, spec: LuaModeSpec| {
                let name = protocol::AgentMode::parse(&spec.name).ok_or_else(|| {
                    LuaError::RuntimeError(format!(
                        "smelt.mode.register: invalid mode name `{}`",
                        spec.name
                    ))
                })?;
                let mode = Mode {
                    label: spec.label.unwrap_or_else(|| spec.name.clone()),
                    icon: spec.icon.unwrap_or_default(),
                    hl_group: spec.hl_group.unwrap_or_else(|| "SmeltModeDefault".into()),
                    note: spec
                        .note
                        .unwrap_or_else(|| format!("now in {} mode", spec.name)),
                    access: spec.access.map_or(crate::permissions::Access::Read, Into::into),
                    above: match spec.above {
                        Some(LuaAbove::Deny) => Above::Deny,
                        Some(LuaAbove::Ask) | None => Above::Ask,
                    },
                    name,
                };
                shared
                    .modes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .register(mode, spec.after.as_deref());
                Ok(())
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "get",
            "Return the mode named `name`, or nil when none is registered.",
            &["name"],
            move |_, name: String| -> LuaResult<Option<LuaMode>> {
                Ok(shared
                    .modes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&name)
                    .cloned()
                    .map(LuaMode))
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "list",
            "Return the registered modes in cycle order.",
            &[],
            move |_, ()| -> LuaResult<Vec<LuaMode>> {
                Ok(shared
                    .modes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .cloned()
                    .map(LuaMode)
                    .collect())
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "cycle_list",
            "Return the modes `smelt.mode.cycle()` steps through: the configured cycle, or else every registered mode.",
            &[],
            move |_, ()| -> LuaResult<Vec<String>> {
                let cycle = crate::host::try_with_core(|core| core.config.mode_cycle.clone())
                    .filter(|cycle| !cycle.is_empty())
                    .unwrap_or_else(|| {
                        shared
                            .modes
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .names()
                    });
                Ok(cycle.iter().map(|mode| mode.as_str().to_string()).collect())
            },
        )?;
    }
    m.fn_(
        "current",
        "Return the active agent mode name.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::host::try_with_core(|core| core.config.mode.as_str().to_string())
                    .unwrap_or_else(|| protocol::AgentMode::read().to_string()),
            )
        },
    )?;
    Ok(())
}
