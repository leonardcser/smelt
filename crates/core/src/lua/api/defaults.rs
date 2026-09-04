//! `smelt.defaults` - startup fallbacks for new sessions.
//!
//! `smelt.defaults.set({ model, mode, reasoning_effort })` sets the model /
//! mode / reasoning-effort that fresh sessions land on. Every field is
//! a fallback: CLI flags (`--model`, `--mode`, `--reasoning-effort`)
//! and resumed-session state win.

use std::sync::Arc;

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;

/// Spec accepted by `smelt.defaults`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.defaults.Config")]
pub struct LuaDefaults {
    /// Starting model reference (`"provider/model"` or bare model name).
    pub model: Option<String>,
    /// Starting agent mode. Must name a registered mode.
    pub mode: Option<String>,
    /// Starting reasoning effort. Known labels are `"off"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`, and `"ultra"`; provider-defined labels are also accepted.
    pub reasoning_effort: Option<String>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "defaults",
        "Startup fallbacks for new sessions. `smelt.defaults.set({ model, mode, reasoning_effort })` sets each field; CLI flags and resumed-session state still win.",
        Tier::Host,
    )?;

    let shared_for_set = Arc::clone(shared);
    m.fn_(
        "set",
        "Set startup defaults for fresh sessions. Accepts `{ model?, mode?, reasoning_effort? }`; CLI flags and resumed-session state still win.",
        &["cfg"],
        move |_, cfg: LuaDefaults| -> LuaResult<()> {
            let mut d = shared_for_set
                .defaults
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(model) = cfg.model {
                d.model = Some(model);
            }
            if let Some(mode) = cfg.mode {
                d.mode = Some(mode);
            }
            if let Some(re) = cfg.reasoning_effort {
                d.reasoning_effort = Some(re);
            }
            Ok(())
        },
    )?;

    Ok(())
}
