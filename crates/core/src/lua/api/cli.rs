//! `smelt.cli` - declare and read CLI flags from Lua.
//!
//! `register_flag{}` is intended for `early.lua`: it runs *before* the
//! main binary finalizes clap parsing, so a Lua-declared flag becomes a
//! real CLI argument. Reading a flag with `get` works at any point after
//! parse - values are populated by the main binary once argv is parsed.
//! When called outside the two-pass main binary (e.g. headless test
//! harness), `get` falls back to the spec's default value.

use crate::lua::command_line::{CommandArgs, CommandSpec, FlagKind, FlagSpec, FlagValue, Operand};
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
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

impl From<LuaCliFlagKind> for FlagKind {
    fn from(k: LuaCliFlagKind) -> Self {
        match k {
            LuaCliFlagKind::Boolean => FlagKind::Boolean,
            LuaCliFlagKind::String => FlagKind::String,
            LuaCliFlagKind::Integer => FlagKind::Integer,
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

/// The arguments of a command, accepted by `smelt.cli.register_command` for
/// `smelt <name>` and by `smelt.cmd.register` as `opts.spec` for `/name`. Both
/// read them with one parser: flags go anywhere, `--name=value` and
/// `-s value` work, and an argument that starts with `-` goes after `--`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.cli.CommandSpec")]
pub struct LuaCommandSpec {
    /// Subcommand name, as typed after `smelt`. `register_command` needs it;
    /// `smelt.cmd.register` names the command itself, so one spec serves its
    /// aliases.
    pub name: Option<String>,
    /// One-line description for `--help`.
    pub description: Option<String>,
    /// Name of the free arguments in `--help` and hints, such as `"QUERY"`.
    /// Without it the command takes none.
    pub args: Option<String>,
    /// Help for the free arguments in `--help`.
    pub args_description: Option<String>,
    /// If true, the free arguments are one text, the first of `args`. A slash
    /// command keeps it as typed, quotes and spacing included, so its flags
    /// go before it. Defaults to `false`.
    pub raw: Option<bool>,
    /// Name of the arguments after `--`, such as `"PATH"`, which the handler
    /// gets apart from the free ones as `rest`. Without it, arguments after
    /// `--` are free ones too.
    pub rest: Option<String>,
    /// Help for the arguments after `--` in `--help`.
    pub rest_description: Option<String>,
    /// Flags of this command only, with the fields of `register_flag`, read
    /// by name from the handler's `flags`. `-C`/`--cwd` and `-h`/`--help` are
    /// taken.
    pub flags: Option<Vec<LuaRegisterFlagOpts>>,
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

fn value_from_lua(kind: FlagKind, v: &mlua::Value, function: &str) -> LuaResult<FlagValue> {
    Ok(match (kind, v) {
        (FlagKind::Boolean, mlua::Value::Boolean(b)) => FlagValue::Boolean(*b),
        (FlagKind::String, mlua::Value::String(s)) => FlagValue::String(s.to_string_lossy()),
        (FlagKind::Integer, mlua::Value::Integer(i)) => FlagValue::Integer(*i),
        (FlagKind::Boolean, mlua::Value::Nil) => FlagValue::Boolean(false),
        (FlagKind::String, mlua::Value::Nil) => FlagValue::None,
        (FlagKind::Integer, mlua::Value::Nil) => FlagValue::None,
        _ => {
            return Err(LuaError::RuntimeError(format!(
                "cli.{function}: default value type does not match kind {kind:?}"
            )))
        }
    })
}

fn value_to_lua(lua: &Lua, v: &FlagValue) -> LuaResult<mlua::Value> {
    Ok(match v {
        FlagValue::Boolean(b) => mlua::Value::Boolean(*b),
        FlagValue::String(s) => mlua::Value::String(lua.create_string(s.as_str())?),
        FlagValue::Integer(i) => mlua::Value::Integer(*i),
        FlagValue::None => mlua::Value::Nil,
    })
}

fn flag_spec(opts: LuaRegisterFlagOpts, function: &str) -> LuaResult<FlagSpec> {
    let kind: FlagKind = opts.kind.into();
    let default = match opts.default.as_ref() {
        Some(v) => value_from_lua(kind, v, function)?,
        None => match kind {
            FlagKind::Boolean => FlagValue::Boolean(false),
            _ => FlagValue::None,
        },
    };
    let short = match opts.short.as_deref() {
        None => None,
        Some(s) if s.chars().count() == 1 => s.chars().next(),
        Some(_) => {
            return Err(LuaError::RuntimeError(format!(
                "cli.{function}: short must be a single character"
            )));
        }
    };
    Ok(FlagSpec {
        name: opts.name,
        kind,
        default,
        description: opts.description,
        short,
        long: opts.long,
        value_optional: opts.value_optional.unwrap_or(false),
    })
}

/// Validate the spec of the command `name` for `function`: flags that are
/// unique and clear of the `-C` and `-h` flags every subcommand has, and free
/// arguments that are raw or leave a `--` section, not both.
pub(crate) fn command_spec(
    opts: LuaCommandSpec,
    name: String,
    function: &str,
) -> LuaResult<CommandSpec> {
    let error = |message: String| Err(LuaError::RuntimeError(format!("{function}: {message}")));
    let raw = opts.raw.unwrap_or(false);
    if raw && (opts.args.is_none() || opts.rest.is_some()) {
        return error("a raw spec needs `args` and no `rest`".into());
    }
    let mut flags: Vec<FlagSpec> = Vec::new();
    for opts in opts.flags.unwrap_or_default() {
        let spec = flag_spec(opts, function)?;
        let clash = |other: &FlagSpec| {
            other.name == spec.name
                || other.long() == spec.long()
                || (spec.short.is_some() && other.short == spec.short)
        };
        if matches!(spec.long(), "cwd" | "help")
            || matches!(spec.short, Some('C' | 'h'))
            || flags.iter().any(clash)
        {
            return error(format!("flag `{}` is already taken", spec.name));
        }
        flags.push(spec);
    }
    let operand = |name: Option<String>, description: Option<String>| {
        name.map(|name| Operand { name, description })
    };
    Ok(CommandSpec {
        name,
        description: opts.description,
        args: operand(opts.args, opts.args_description),
        raw,
        rest: operand(opts.rest, opts.rest_description),
        flags,
    })
}

/// The handler arguments of `command`: its free arguments, its flag values
/// by name and its arguments after `--`.
pub(crate) fn command_to_lua(
    lua: &Lua,
    command: &CommandArgs,
) -> LuaResult<(mlua::Table, mlua::Table, mlua::Table)> {
    let flags = lua.create_table()?;
    for (name, value) in &command.flags {
        flags.set(name.as_str(), value_to_lua(lua, value)?)?;
    }
    Ok((
        lua.create_sequence_from(command.args.iter().map(String::as_str))?,
        flags,
        lua.create_sequence_from(command.rest.iter().map(String::as_str))?,
    ))
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
                let spec = flag_spec(opts, "register_flag")?;
                if let Ok(mut specs) = s.cli_flag_specs.lock() {
                    if specs.iter().any(|sp| sp.name == spec.name) {
                        return Err(LuaError::RuntimeError(format!(
                            "cli.register_flag: `{}` already registered",
                            spec.name
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
            "Register a subcommand such as `smelt diff`. MUST be called from `early.lua`, like `register_flag`. Launching it starts the TUI without an agent session: nothing is persisted, no provider setup, engine, language servers or MCP servers start, and `smelt status` does not list the process. A plugin opens its UI as a modal overlay with `smelt.cli.on_command`. That overlay is the whole screen: smelt draws no prompt, transcript or statusline under it, and quits once no modal overlay shows. `spec` is a `smelt.cli.CommandSpec`; pass the same table to `smelt.cmd.register` as `opts.spec` so `/name` reads the same arguments. Every subcommand also accepts `-C <path>`. Errors if an earlier registration took the name. A name that a built-in subcommand uses is ignored with a warning at launch.",
            &["spec"],
            move |_, opts: LuaCommandSpec| -> LuaResult<()> {
                require_early(&s, "register_command")?;
                let name = opts.name.clone().unwrap_or_default();
                let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                if !valid {
                    return Err(LuaError::RuntimeError(format!(
                        "cli.register_command: name `{name}` must be lowercase letters, digits and dashes"
                    )));
                }
                let spec = command_spec(opts, name, "cli.register_command")?;
                let mut specs = s.cli_command_specs.lock().unwrap_or_else(|e| e.into_inner());
                if specs.iter().any(|other| other.name == spec.name) {
                    return Err(LuaError::RuntimeError(format!(
                        "cli.register_command: `{}` already registered",
                        spec.name
                    )));
                }
                specs.push(spec);
                Ok(())
            },
        )?;
    }

    {
        let s = shared.clone();
        m.fn_(
            "command",
            "Return the Lua subcommand this process was launched for as `{ name, args, flags, rest }`, where `args` lists its free arguments, `flags` maps each of its flag names to the parsed value or default, and `rest` lists its arguments after `--`; or `nil` for an agent session.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Value> {
                let Some(command) = s.cli_command.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
                    return Ok(mlua::Value::Nil);
                };
                let (args, flags, rest) = command_to_lua(lua, &command)?;
                let out = lua.create_table()?;
                out.set("name", command.name)?;
                out.set("args", args)?;
                out.set("flags", flags)?;
                out.set("rest", rest)?;
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
