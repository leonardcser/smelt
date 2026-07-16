//! `smelt.trust` - query and mutate the per-project content trust store.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "trust",
        "Query and mutate the per-project content trust store.",
        Tier::Host,
    )?;
    let status_context = Arc::clone(shared);
    m.fn_(
        "status",
        "Return the trust state of the current working directory: `\"trusted\"`, `\"untrusted\"`, or `\"no_content\"`.",
        &[],
        move |_, ()| -> LuaResult<&'static str> {
            Ok(match crate::trust::project_trust_state(&status_context.evaluation_cwd()) {
                crate::trust::TrustState::Trusted { .. } => "trusted",
                crate::trust::TrustState::Untrusted { .. } => "untrusted",
                crate::trust::TrustState::NoContent => "no_content",
            })
        },
    )?;

    let mark_context = Arc::clone(shared);
    m.fn_(
        "mark",
        "Mark the current working directory as trusted, persisting it in the user's trust store.",
        &[],
        move |_, ()| -> LuaResult<String> {
            crate::trust::mark_trusted(&mark_context.evaluation_cwd())
                .map_err(LuaError::RuntimeError)
        },
    )?;

    Ok(())
}
