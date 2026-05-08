//! `smelt.trust` — query and mutate the per-project content trust store.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let trust_tbl = lua.create_table()?;

    trust_tbl.set(
        "status",
        lua.create_function(|_, ()| {
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.status: cwd: {e}")))?;
            Ok(match crate::trust::project_trust_state(&cwd) {
                crate::trust::TrustState::Trusted { .. } => "trusted",
                crate::trust::TrustState::Untrusted { .. } => "untrusted",
                crate::trust::TrustState::NoContent => "no_content",
            })
        })?,
    )?;

    trust_tbl.set(
        "mark",
        lua.create_function(|_, ()| {
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.mark: cwd: {e}")))?;
            crate::trust::mark_trusted(&cwd).map_err(LuaError::RuntimeError)
        })?,
    )?;

    smelt.set("trust", trust_tbl)?;
    Ok(())
}
