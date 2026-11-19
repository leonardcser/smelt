//! `smelt.sandbox` - the OS sandbox that confines commands to the workspace.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::permissions::{Access, PathRules};
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "sandbox",
        "The OS sandbox (bubblewrap on Linux, Seatbelt on macOS) that confines commands to what their access allows, so a mistake cannot change files outside the project. Every command may write the temp directories. A `write` command may also change the workspace and `allow` paths (see `smelt.permissions.paths`). The paths the access may not read are hidden. The network stays open: it is a convenience, not a security boundary.",
        Tier::Host,
    )?;
    m.fn_(
        "status",
        "Return `{ available = boolean, backend = string?, reason = string? }`. `backend` is `\"bwrap\"` or `\"seatbelt\"`; `reason` explains why the sandbox is unavailable.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let status = lua.create_table()?;
            match smelt_sandbox::backend() {
                Ok(backend) => {
                    status.set("available", true)?;
                    status.set("backend", backend.name())?;
                }
                Err(reason) => {
                    status.set("available", false)?;
                    status.set("reason", reason)?;
                }
            }
            Ok(status)
        },
    )?;
    Ok(())
}

/// The sandbox for the commands that the tool call running now starts, or
/// `None` outside a tool call. `scan_root` narrows where glob rules look for
/// files to hide, for a command that reads only beneath it.
///
/// `None` also runs the command unconfined when the call may do anything
/// (`full` with nothing to hide), or when this machine has no sandbox and
/// the call is below `full`. Without a sandbox `bash` counts as `full`, so it
/// asked first. Fails at `full` with paths to hide and no sandbox.
pub(crate) fn current(
    shared: &LuaShared,
    scan_root: Option<&std::path::Path>,
) -> LuaResult<Option<smelt_sandbox::Policy>> {
    let Some((mut rules, access)) = current_call(shared)? else {
        return Ok(None);
    };
    if let Some(root) = scan_root {
        rules = rules.scanning(root);
    }
    let Some(mut policy) = rules.sandbox(access) else {
        return Ok(None);
    };
    match smelt_sandbox::backend() {
        Ok(_) => {
            policy.hidden_env = credential_env();
            Ok(Some(policy))
        }
        Err(_) if access < Access::Full => Ok(None),
        Err(reason) => Err(LuaError::RuntimeError(format!(
            "the sandbox is unavailable ({reason}), so this command cannot run while a \"deny\" path rule exists"
        ))),
    }
}

/// The path rules and the sandbox access of the tool call running now, or
/// `None` outside a tool call.
pub(crate) fn current_call(shared: &LuaShared) -> LuaResult<Option<(PathRules, Access)>> {
    let Some(invocation) = crate::lua::current_tool_invocation() else {
        return Ok(None);
    };
    let call = shared
        .call_sandboxes
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&invocation.invocation_id)
        .cloned();
    call.map(|call| {
        super::permissions::path_rules(shared, call.mode.as_str()).map(|rules| (rules, call.access))
    })
    .transpose()
}

/// The environment variables that hold smelt's provider credentials: each
/// provider's `api_key_env`, the web search key, and the OAuth token
/// variables.
fn credential_env() -> Vec<String> {
    let configured = crate::host::try_with_core(|core| {
        core.config
            .providers
            .iter()
            .filter_map(|provider| provider.api_key_env.clone())
            .chain(std::iter::once(
                core.config.settings.brave_search_api_key_env.clone(),
            ))
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let mut names: Vec<String> = configured
        .into_iter()
        .chain(engine::provider::CREDENTIAL_ENVS.map(String::from))
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}
