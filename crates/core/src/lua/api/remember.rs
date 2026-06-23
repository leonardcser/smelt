//! `smelt.remember` - per-key opt-in to last-used recall on launch.
//!
//! `smelt.remember.set({ model, mode, reasoning_effort })` flips whether
//! each pick survives across restarts. Every field defaults to `true`.
//! Setting one to `false` makes that key always start from
//! `smelt.defaults` (or the hardcoded fallback), ignoring whatever the
//! user picked last session.
//!
//! Pairs with `smelt.defaults`: defaults set the cold-start value,
//! `recent.json` overrides them on each launch when remember is on.

use std::sync::Arc;

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;

/// Spec accepted by `smelt.remember`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.remember.Config")]
pub struct LuaRemember {
    /// When true (default), restore the last-used model on launch.
    pub model: Option<bool>,
    /// When true (default), restore the last-used agent mode on launch.
    pub mode: Option<bool>,
    /// When true (default), restore the last-used reasoning effort on launch.
    pub reasoning_effort: Option<bool>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "remember",
        "Per-key opt-in to last-used recall on launch. `smelt.remember.set({ model = false })` makes that key always start from `smelt.defaults`, ignoring `recent.json`. Defaults to `true` for every key.",
        Tier::Host,
    )?;

    let shared_for_set = Arc::clone(shared);
    m.fn_(
        "set",
        "Set which startup choices are remembered across launches. Accepts `{ model?, mode?, reasoning_effort? }`; omitted fields keep their current policy.",
        &["cfg"],
        move |_, cfg: LuaRemember| -> LuaResult<()> {
            let mut r = shared_for_set
                .remember
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(v) = cfg.model {
                r.model = v;
            }
            if let Some(v) = cfg.mode {
                r.mode = v;
            }
            if let Some(v) = cfg.reasoning_effort {
                r.reasoning_effort = v;
            }
            Ok(())
        },
    )?;

    Ok(())
}
