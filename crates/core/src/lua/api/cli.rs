//! `smelt.cli` - declare and read CLI flags from Lua.
//!
//! `register_flag{}` is intended for `early.lua`: it runs *before* the
//! main binary finalizes clap parsing, so a Lua-declared flag becomes a
//! real CLI argument. Reading a flag with `get` works at any point after
//! parse - values are populated by the main binary once argv is parsed.
//! When called outside the two-pass main binary (e.g. headless test
//! harness), `get` falls back to the spec's default value.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::shared::{CliCommandSpec, CliFlagKind, CliFlagSpec, CliFlagValue};
use crate::lua::LuaShared;
use lua_doc_derive::{LuaAlias, LuaOpts};
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
    /// String/Integer flags only. When true, the flag may be passed without
    /// a value (`smelt -r` valid alongside `smelt -r abc`); the value-less
    /// form yields `""` for String and `0` for Integer. Defaults to `false`.
    /// Ignored for Boolean flags.
    pub value_optional: Option<bool>,
}

/// Subcommand specification accepted by `smelt.cli.register_command`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.cli.RegisterCommandOpts")]
pub struct LuaRegisterCommandOpts {
    /// Subcommand name, as typed after `smelt`.
    pub name: String,
    /// One-line description for `--help`.
    pub description: Option<String>,
    /// Name of the free arguments in `--help`, such as `"VIEW"`. Without it
    /// the subcommand takes none.
    pub args: Option<String>,
}

/// Argv is parsed after `early.lua`, so CLI declarations are only valid there.
fn require_early(shared: &LuaShared, function: &str) -> LuaResult<()> {
    if shared.phase() == crate::lua::Phase::Early {
        return Ok(());
    }
    Err(LuaError::RuntimeError(format!(
        "cli.{function}: only valid in `early.lua` (current phase: {})",
        shared.phase().as_str()
    )))
}

fn value_from_lua(kind: CliFlagKind, v: &mlua::Value) -> LuaResult<CliFlagValue> {
    Ok(match (kind, v) {
        (CliFlagKind::Boolean, mlua::Value::Boolean(b)) => CliFlagValue::Boolean(*b),
        (CliFlagKind::String, mlua::Value::String(s)) => CliFlagValue::String(s.to_string_lossy()),
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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "cli",
        "Declare and read CLI flags and subcommands from Lua. `register_flag` \
and `register_command` are intended to be called from `early.lua` so they \
are folded into the main binary's argument parser. `get(name)` returns a \
parsed flag value (or the declared default) and `command()` the launched \
subcommand after the binary has parsed argv.",
        Tier::Host,
    )?;

    {
        let s = shared.clone();
        m.fn_(
            "register_flag",
            "Register a CLI flag. MUST be called from `early.lua`. The runtime errors loudly if invoked in any later phase, because clap has already parsed argv by then. `opts.kind` is `\"boolean\"`, `\"string\"`, or `\"integer\"`; `opts.default` (optional) sets the value when the flag is absent. Booleans always default to `false` when not provided. Errors if a flag with the same name was already registered.",
            &["opts"],
            move |_, opts: LuaRegisterFlagOpts| -> LuaResult<()> {
                require_early(&s, "register_flag")?;
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
                    value_optional: opts.value_optional.unwrap_or(false),
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
        m.fn_(
            "get",
            "Return the parsed value of the Lua-declared CLI flag `name`. Returns the declared default if the binary has not parsed argv yet (e.g. headless tests).",
            &["name"],
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
        m.fn_(
            "register_command",
            "Register a subcommand such as `smelt diff`. MUST be called from `early.lua`, like `register_flag`. Launching it starts the TUI without an agent session: nothing is persisted, no provider setup or MCP servers start, and `smelt status` does not list the process. A plugin opens its UI as a modal overlay with `smelt.cli.on_command`; smelt quits once no modal overlay shows. Every subcommand also accepts `-C <path>`. Errors if an earlier registration took the name. A name that a built-in subcommand uses is ignored with a warning at launch.",
            &["opts"],
            move |_, opts: LuaRegisterCommandOpts| -> LuaResult<()> {
                require_early(&s, "register_command")?;
                let valid = opts.name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && opts.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                if !valid {
                    return Err(LuaError::RuntimeError(format!(
                        "cli.register_command: `{}` must be lowercase letters, digits and dashes",
                        opts.name
                    )));
                }
                let mut specs = s.cli_command_specs.lock().unwrap_or_else(|e| e.into_inner());
                if specs.iter().any(|spec| spec.name == opts.name) {
                    return Err(LuaError::RuntimeError(format!(
                        "cli.register_command: `{}` already registered",
                        opts.name
                    )));
                }
                specs.push(CliCommandSpec {
                    name: opts.name,
                    description: opts.description,
                    args: opts.args,
                });
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "command",
            "Return the Lua subcommand this process was launched for as `{ name, args }`, where `args` lists its free arguments, or `nil` for an agent session.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Value> {
                let Some(command) = s.cli_command.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
                    return Ok(mlua::Value::Nil);
                };
                let out = lua.create_table()?;
                out.set("name", command.name)?;
                out.set("args", command.args)?;
                Ok(mlua::Value::Table(out))
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return the names of every Lua-declared CLI flag, in registration order.",
            &[],
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

    Ok(())
}
