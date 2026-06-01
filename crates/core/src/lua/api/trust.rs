//! `smelt.trust` - query and mutate the per-project content trust store.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "trust",
        "Query and mutate the per-project content trust store.",
        Tier::Host,
    )?;
    m.fn_(
        "status",
        "Return the trust state of the current working directory: `\"trusted\"`, `\"untrusted\"`, or `\"no_content\"`. Raises if the cwd cannot be read.",
        &[],
        |_, ()| -> LuaResult<&'static str> {
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.status: cwd: {e}")))?;
            Ok(match crate::trust::project_trust_state(&cwd) {
                crate::trust::TrustState::Trusted { .. } => "trusted",
                crate::trust::TrustState::Untrusted { .. } => "untrusted",
                crate::trust::TrustState::NoContent => "no_content",
            })
        },
    )?;

    m.fn_(
        "mark",
        "Mark the current working directory as trusted, persisting it in the user's trust store.",
        &[],
        |_, ()| -> LuaResult<String> {
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.mark: cwd: {e}")))?;
            crate::trust::mark_trusted(&cwd).map_err(LuaError::RuntimeError)
        },
    )?;

    Ok(())
}
