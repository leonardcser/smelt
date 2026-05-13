//! `smelt.trust` — query and mutate the per-project content trust store.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
use mlua::prelude::*;

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let trust_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.trust",
        "Query and mutate the per-project content trust store.",
    );

    register_fn(
        &trust_tbl,
        "smelt.trust",
        "status",
        "Return the trust state of the current working directory: `\"trusted\"`, `\"untrusted\"`, or `\"no_content\"`. Raises if the cwd cannot be read.",
        &[],
        lua,
        |_, ()|  -> LuaResult<&'static str>{
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.status: cwd: {e}")))?;
            Ok(match crate::trust::project_trust_state(&cwd) {
                crate::trust::TrustState::Trusted { .. } => "trusted",
                crate::trust::TrustState::Untrusted { .. } => "untrusted",
                crate::trust::TrustState::NoContent => "no_content",
            })
        },
    )?;

    register_fn(
        &trust_tbl,
        "smelt.trust",
        "mark",
        "Mark the current working directory as trusted, persisting it in the user's trust store.",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            let cwd = std::env::current_dir()
                .map_err(|e| LuaError::RuntimeError(format!("trust.mark: cwd: {e}")))?;
            crate::trust::mark_trusted(&cwd).map_err(LuaError::RuntimeError)
        },
    )?;

    smelt.set("trust", trust_tbl)?;
    Ok(())
}
