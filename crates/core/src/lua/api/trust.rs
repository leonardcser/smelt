//! `smelt.trust` — query and mutate the per-project content trust
//! store backing `<cwd>/.smelt/{init.lua, plugins/*.lua,
//! commands/*.md}` autoload.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let trust_tbl = lua.create_table()?;

    // Returns one of `"trusted" / "untrusted" / "no_content"` for
    // the current working directory.
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

    // Marks the current `.smelt/` content trusted by hashing it and
    // writing the digest to the trust store. Returns the recorded
    // hash on success; raises a Lua error otherwise.
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
