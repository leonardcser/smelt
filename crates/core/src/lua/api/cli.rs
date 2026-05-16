//! `smelt.cli` — declare and read CLI flags from Lua.
//!
//! `register_flag{}` is intended for `early.lua`: it runs *before* the
//! main binary finalizes clap parsing, so a Lua-declared flag becomes a
//! real CLI argument. Reading a flag with `get` works at any point after
//! parse — values are populated by the main binary once argv is parsed.
//! When called outside the two-pass main binary (e.g. headless test
//! harness), `get` falls back to the spec's default value.

use crate::lua::doc::register_fn;
use crate::lua::shared::{CliFlagKind, CliFlagSpec, CliFlagValue};
use crate::lua::LuaShared;
use lua_doc_derive::{lua_module, LuaAlias, LuaOpts};
use mlua::prelude::*;
use std::sync::Arc;

/// Type of CLI flag declared via `smelt.cli.register_flag`. Matches the
/// subset of clap that we expose to Lua.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.cli.FlagKind")]
pub enum LuaCliFlagKind {
    Boolean,
    String,
    Integer,
}

impl From<LuaCliFlagKind> for CliFlagKind {
    fn from(k: LuaCliFlagKind) -> Self {
        match k {
            LuaCliFlagKind::Boolean => CliFlagKind::Boolean,
            LuaCliFlagKind::String => CliFlagKind::String,
            LuaCliFlagKind::Integer => CliFlagKind::Integer,
        }
    }
}

/// Flag specification accepted by `smelt.cli.register_flag`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.cli.RegisterFlagOpts")]
pub struct LuaRegisterFlagOpts {
    /// Flag name without `--`. Used as the key for `smelt.cli.get`.
    pub name: String,
    /// `"boolean"` (default `false`), `"string"`, or `"integer"`.
    pub kind: LuaCliFlagKind,
    /// Default value when the flag is absent from argv. Type must match `kind`.
    pub default: Option<mlua::Value>,
    /// Short flag character (e.g. `"u"` for `-u`). Optional.
    pub short: Option<String>,
    /// Long flag name override. Defaults to `name` when absent.
    pub long: Option<String>,
    /// Human-readable description for `--help`.
    pub description: Option<String>,
}

fn value_from_lua(kind: CliFlagKind, v: &mlua::Value) -> LuaResult<CliFlagValue> {
    Ok(match (kind, v) {
        (CliFlagKind::Boolean, mlua::Value::Boolean(b)) => CliFlagValue::Boolean(*b),
        (CliFlagKind::String, mlua::Value::String(s)) => {
            CliFlagValue::String(s.to_string_lossy())
        }
        (CliFlagKind::Integer, mlua::Value::Integer(i)) => CliFlagValue::Integer(*i),
        (CliFlagKind::Boolean, mlua::Value::Nil) => CliFlagValue::Boolean(false),
        (CliFlagKind::String, mlua::Value::Nil) => CliFlagValue::None,
        (CliFlagKind::Integer, mlua::Value::Nil) => CliFlagValue::None,
        _ => {
            return Err(LuaError::RuntimeError(format!(
                "cli.register_flag: default value type does not match kind {kind:?}"
            )))
        }
    })
}

fn value_to_lua(lua: &Lua, v: &CliFlagValue) -> LuaResult<mlua::Value> {
    Ok(match v {
        CliFlagValue::Boolean(b) => mlua::Value::Boolean(*b),
        CliFlagValue::String(s) => mlua::Value::String(lua.create_string(s.as_str())?),
        CliFlagValue::Integer(i) => mlua::Value::Integer(*i),
        CliFlagValue::None => mlua::Value::Nil,
    })
}

#[lua_module(
    name = "smelt.cli",
    doc = "Declare and read CLI flags from Lua. `register_flag` is intended \
to be called from `early.lua` so the flag is folded into the main binary's \
argument parser. `get(name)` returns the parsed value (or the declared \
default) after the binary has parsed argv."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let tbl = lua.create_table()?;

    {
        let s = shared.clone();
        register_fn(
            &tbl,
            "smelt.cli",
            "register_flag",
            "Register a CLI flag. MUST be called from `early.lua` — the runtime errors loudly if invoked in any later phase, because clap has already parsed argv by then. `opts.kind` is `\"boolean\"`, `\"string\"`, or `\"integer\"`; `opts.default` (optional) sets the value when the flag is absent. Booleans always default to `false` when not provided. Errors if a flag with the same name was already registered.",
            &["opts"],
            lua,
            move |_, opts: LuaRegisterFlagOpts| -> LuaResult<()> {
                if s.phase() != crate::lua::Phase::Early {
                    return Err(LuaError::RuntimeError(format!(
                        "cli.register_flag: only valid in `early.lua` (current phase: {})",
                        s.phase().as_str()
                    )));
                }
                let kind: CliFlagKind = opts.kind.into();
                let default = match opts.default.as_ref() {
                    Some(v) => value_from_lua(kind, v)?,
                    None => match kind {
                        CliFlagKind::Boolean => CliFlagValue::Boolean(false),
                        _ => CliFlagValue::None,
                    },
                };
                let short = match opts.short.as_deref() {
                    Some(s) if s.chars().count() == 1 => Some(s.chars().next().unwrap()),
                    Some(_) => {
                        return Err(LuaError::RuntimeError(
                            "cli.register_flag: short must be a single character".to_string(),
                        ));
                    }
                    None => None,
                };
                let spec = CliFlagSpec {
                    name: opts.name.clone(),
                    kind,
                    default,
                    description: opts.description,
                    short,
                    long: opts.long,
                };
                if let Ok(mut specs) = s.cli_flag_specs.lock() {
                    if specs.iter().any(|sp| sp.name == opts.name) {
                        return Err(LuaError::RuntimeError(format!(
                            "cli.register_flag: `{}` already registered",
                            opts.name
                        )));
                    }
                    specs.push(spec);
                }
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        register_fn(
            &tbl,
            "smelt.cli",
            "get",
            "Return the parsed value of the Lua-declared CLI flag `name`. Returns the declared default if the binary has not parsed argv yet (e.g. headless tests).",
            &["name"],
            lua,
            move |lua, name: String| -> LuaResult<mlua::Value> {
                let parsed = s
                    .cli_flag_values
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&name).cloned());
                if let Some(v) = parsed {
                    return value_to_lua(lua, &v);
                }
                let default = s
                    .cli_flag_specs
                    .lock()
                    .ok()
                    .and_then(|specs| specs.iter().find(|sp| sp.name == name).cloned())
                    .map(|sp| sp.default);
                match default {
                    Some(v) => value_to_lua(lua, &v),
                    None => Ok(mlua::Value::Nil),
                }
            },
        )?;
    }

    {
        let s = shared.clone();
        register_fn(
            &tbl,
            "smelt.cli",
            "list",
            "Return the names of every Lua-declared CLI flag, in registration order.",
            &[],
            lua,
            move |lua, ()| -> LuaResult<mlua::Table> {
                let names: Vec<String> = s
                    .cli_flag_specs
                    .lock()
                    .map(|specs| specs.iter().map(|sp| sp.name.clone()).collect())
                    .unwrap_or_default();
                let out = lua.create_table()?;
                for (i, name) in names.iter().enumerate() {
                    out.set(i + 1, name.as_str())?;
                }
                Ok(out)
            },
        )?;
    }

    smelt.set("cli", tbl)?;
    Ok(())
}
