//! `smelt.builtins` - opt out of bundled `smelt.<dotted>` modules before
//! they are auto-loaded. Designed to be called from `early.lua` so the
//! `require()` for a disabled module is skipped entirely. Calls made
//! later still mark the module as disabled, but the module body has
//! already executed by then, so prefer `smelt.tools.unregister` /
//! holding the `Reg` returned by `smelt.cmd.register` and calling
//! `:remove()` for post-hoc removal.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use std::sync::Arc;

/// Selector accepted by `smelt.builtins.disable` / `enable`. Each list
/// is a set of bundled module short-names - see the table below for the
/// `smelt.<dotted>` form each one expands to.
///
/// | field | expansion |
/// |---|---|
/// | `tools = { "web_search" }` | `smelt.tools.web_search` |
/// | `commands = { "compact" }` | `smelt.commands.compact` |
/// | `plugins = { "predict" }` | `smelt.plugins.predict` |
/// | `dialogs = { "resume" }` | `smelt.dialogs.resume` |
/// | `modules = { "smelt.foo.bar" }` | passed through verbatim |
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.builtins.Selector")]
pub struct LuaBuiltinsSelector {
    /// Short tool names under `smelt.tools.*` (e.g. `"bash"`, `"web_search"`).
    #[lua(default)]
    pub tools: Vec<String>,
    /// Short command names under `smelt.commands.*` (e.g. `"compact"`).
    #[lua(default)]
    pub commands: Vec<String>,
    /// Short plugin names under `smelt.plugins.*` (e.g. `"predict"`).
    #[lua(default)]
    pub plugins: Vec<String>,
    /// Short dialog names under `smelt.dialogs.*` (e.g. `"resume"`).
    #[lua(default)]
    pub dialogs: Vec<String>,
    /// Fully-qualified `smelt.<dotted>` module names, passed through verbatim.
    #[lua(default)]
    pub modules: Vec<String>,
}

fn expand(sel: &LuaBuiltinsSelector) -> Vec<String> {
    let mut out = Vec::new();
    for name in &sel.tools {
        out.push(format!("smelt.tools.{name}"));
    }
    for name in &sel.commands {
        out.push(format!("smelt.commands.{name}"));
    }
    for name in &sel.plugins {
        out.push(format!("smelt.plugins.{name}"));
    }
    for name in &sel.dialogs {
        out.push(format!("smelt.dialogs.{name}"));
    }
    for module in &sel.modules {
        out.push(module.clone());
    }
    out
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "builtins",
        "Opt out of bundled `smelt.<dotted>` modules. Call from \
`early.lua` to prevent the module from auto-loading; calls made later \
mark the module as disabled but its body has already run. For runtime \
removal of an already-loaded tool, call `smelt.tools.unregister` \
directly; for commands, hold the `Reg` returned by `smelt.cmd.register` \
and call `:remove()`.",
        Tier::Host,
    )?;

    {
        let s = shared.clone();
        m.fn_(
            "disable",
            "Mark the bundled modules in `selector` as disabled. The auto-loader skips matching `require()` calls. Returns the count of names marked. Call from `early.lua` for full effect.",
            &["selector"],
            move |_, sel: LuaBuiltinsSelector| -> LuaResult<usize> {
                let names = expand(&sel);
                let mut count = 0usize;
                if let Ok(mut set) = s.disabled_modules.lock() {
                    for n in names {
                        if set.insert(n) {
                            count += 1;
                        }
                    }
                }
                Ok(count)
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "enable",
            "Undo a prior `disable` for the bundled modules in `selector`. Returns the count of names un-marked. Has no effect if the modules were never disabled.",
            &["selector"],
            move |_, sel: LuaBuiltinsSelector| -> LuaResult<usize> {
                let names = expand(&sel);
                let mut count = 0usize;
                if let Ok(mut set) = s.disabled_modules.lock() {
                    for n in names {
                        if set.remove(&n) {
                            count += 1;
                        }
                    }
                }
                Ok(count)
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "is_disabled",
            "Return `true` if the dotted module name (e.g. `\"smelt.tools.web_search\"`) is currently in the disabled set.",
            &["module"],
            move |_, module: String| -> LuaResult<bool> {
                Ok(s.disabled_modules
                    .lock()
                    .map(|set| set.contains(&module))
                    .unwrap_or(false))
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return the sorted dotted module names that are currently disabled.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                let mut names: Vec<String> = s
                    .disabled_modules
                    .lock()
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default();
                names.sort();
                let out = lua.create_table()?;
                for (i, name) in names.iter().enumerate() {
                    out.set(i + 1, name.as_str())?;
                }
                Ok(out)
            },
        )?;
    }

    Ok(())
}
