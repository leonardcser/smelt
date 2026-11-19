//! `smelt.permissions` - path rules and session approvals. The policy
//! function `smelt.permissions.check` is defined in `smelt/permissions.lua`.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaType;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use crate::permissions::{Access, Intent, Layer, PathRule, PathRules, Rule};
use lua_doc_derive::{LuaAlias, LuaOpts};
use mlua::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a tool call may change: `"read"` reads files only, `"write"` writes
/// inside the workspace or runs in the write sandbox, `"full"` writes
/// outside the workspace or runs without a sandbox.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.permissions.Access", mirror = "protocol::Access")]
pub enum LuaAccess {
    Read,
    Write,
    Full,
}

/// Whether a call reads or writes a path.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.permissions.Intent")]
pub enum LuaIntent {
    Read,
    Write,
}

/// What a path rule allows: `"allow"` (like the workspace: reads run, writes
/// need `write`), `"ask"` (reads and writes need `full`) or `"deny"` (never).
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.permissions.Rule")]
pub enum LuaRule {
    Allow,
    Ask,
    Deny,
}

impl From<LuaRule> for Rule {
    fn from(rule: LuaRule) -> Self {
        match rule {
            LuaRule::Allow => Rule::Allow,
            LuaRule::Ask => Rule::Ask,
            LuaRule::Deny => Rule::Deny,
        }
    }
}

impl From<Rule> for LuaRule {
    fn from(rule: Rule) -> Self {
        match rule {
            Rule::Allow => LuaRule::Allow,
            Rule::Ask => LuaRule::Ask,
            Rule::Deny => LuaRule::Deny,
        }
    }
}

/// A rule to set, or `false` to remove one.
struct LuaRuleEdit(Option<Rule>);

impl LuaType for LuaRuleEdit {
    fn lua_type() -> String {
        format!("{}|false", LuaRule::lua_type())
    }
}

impl FromLua for LuaRuleEdit {
    fn from_lua(value: LuaValue, lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::Boolean(false) => Ok(Self(None)),
            value => LuaRule::from_lua(value, lua)
                .map(|rule| Self(Some(rule.into())))
                .map_err(|_| {
                    LuaError::RuntimeError(
                        "smelt.permissions.paths: each rule must be \"allow\", \"ask\", \"deny\" or false".into(),
                    )
                }),
        }
    }
}

/// Options for `smelt.permissions.paths`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.PathsOpts")]
pub struct LuaPathsOpts {
    /// Set the rules for these modes only. Default: every mode.
    pub modes: Option<Vec<String>>,
}

impl From<LuaIntent> for Intent {
    fn from(intent: LuaIntent) -> Self {
        match intent {
            LuaIntent::Read => Intent::Read,
            LuaIntent::Write => Intent::Write,
        }
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "permissions",
        "Decide which agent tool calls run, ask, or are denied. Modes set how much a call may do; path rules (`smelt.permissions.paths`) set where. Wrap `smelt.permissions.check(call)` to change the policy. Approvals last until the session ends.",
        Tier::Host,
    )?;
    m.live_only_fn(
        "allow",
        "Let `tool` run at up to `access` without asking until the session ends.",
        &["tool", "access"],
        |_, (tool, access): (String, LuaAccess)| {
            crate::host::try_with_core(|core| core.permissions.allow(&tool, access.into()));
            Ok(())
        },
    )?;
    m.fn_(
        "allowed",
        "Return true when the user approved `tool` at `access` or more this session.",
        &["tool", "access"],
        |_, (tool, access): (String, LuaAccess)| {
            let access: Access = access.into();
            Ok(
                crate::host::try_with_core(|core| core.permissions.allowed(&tool, access))
                    .unwrap_or(false),
            )
        },
    )?;
    m.live_only_fn(
        "allow_path",
        "Let calls that touch only paths beneath `path` run at up to `access` without asking until the session ends. A `deny` rule still wins.",
        &["path", "access"],
        {
            let shared = Arc::clone(shared);
            move |_, (path, access): (String, LuaAccess)| {
                let path = resolve(&shared, &path);
                crate::host::try_with_core(|core| core.permissions.allow_path(&path, access.into()));
                Ok(())
            }
        },
    )?;
    m.fn_(
        "allowed_path",
        "Return true when the user approved a directory holding `path` at `access` or more this session.",
        &["path", "access"],
        {
            let shared = Arc::clone(shared);
            move |_, (path, access): (String, LuaAccess)| {
                let path = resolve(&shared, &path);
                Ok(crate::host::try_with_core(|core| {
                    core.permissions.allowed_path(&path, access.into())
                })
                .unwrap_or(false))
            }
        },
    )?;
    m.live_only_fn(
        "clear",
        "Forget every session approval, for tools and for paths.",
        &[],
        |_, ()| {
            crate::host::try_with_core(|core| core.permissions.clear_approvals());
            Ok(())
        },
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "paths",
            "Set path rules and return the rules of the layer. `rules` maps a path or glob to a rule, or to `false` to remove one. Each call merges into the rules already set, for every mode or only for `opts.modes`. `~` is the home directory, relative paths start at the project, and `*`, `?` and `**` make a glob (`**/x` matches anywhere). The rule that names the closest ancestor of a path wins; at the same depth a mode rule wins, then the longer pattern.",
            &["rules?", "opts?"],
            move |_,
                  (rules, opts): (
                Option<BTreeMap<String, LuaRuleEdit>>,
                Option<LuaPathsOpts>,
            )|
                  -> LuaResult<BTreeMap<String, LuaRule>> {
                let rules = rules.unwrap_or_default();
                for (pattern, edit) in &rules {
                    if pattern.is_empty() {
                        return Err(LuaError::RuntimeError(
                            "smelt.permissions.paths: each key must be a path or glob".into(),
                        ));
                    }
                    if let Some(rule) = edit.0 {
                        PathRule::parse(pattern, rule, Layer::Global, Path::new("/"), Path::new("/"))
                            .map_err(LuaError::RuntimeError)?;
                    }
                }
                let modes = opts.and_then(|opts| opts.modes);
                let layers: Vec<Option<&str>> = match &modes {
                    Some(modes) => modes.iter().map(|mode| Some(mode.as_str())).collect(),
                    None => vec![None],
                };
                let mut set = shared
                    .path_rules
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                for &mode in &layers {
                    let layer = set.layer_mut(mode);
                    for (pattern, edit) in &rules {
                        match edit.0 {
                            Some(rule) => layer.insert(pattern.clone(), rule),
                            None => layer.remove(pattern),
                        };
                    }
                }
                Ok(match layers.first() {
                    Some(&mode) => set
                        .layer_mut(mode)
                        .iter()
                        .map(|(pattern, rule)| (pattern.clone(), (*rule).into()))
                        .collect(),
                    None => BTreeMap::new(),
                })
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "rule",
            "Return the rule for `path` in `mode` (default: the current mode), or nil when no rule names it. The workspace, the temp directories and the session artifacts are `\"allow\"`. Relative paths resolve against the project cwd.",
            &["path", "mode?"],
            move |_, (path, mode): (String, Option<String>)| -> LuaResult<Option<LuaRule>> {
                let rules = path_rules(&shared, &mode.unwrap_or_else(current_mode))?;
                Ok(rules.rule(&resolve(&shared, &path)).map(Into::into))
            },
        )?;
    }
    let shared = Arc::clone(shared);
    m.fn_(
        "path_access",
        "Return the access a call needs to read or write `path` in `mode` (default: the current mode), or nil when a `deny` rule forbids it. `allow` paths need `\"read\"` to read and `\"write\"` to write; paths with no rule need `\"full\"` to write; `ask` paths need `\"full\"` for both. Writing a session artifact, such as a plan, needs only `\"read\"`.",
        &["path", "intent", "mode?"],
        move |_, (path, intent, mode): (String, LuaIntent, Option<String>)| -> LuaResult<Option<LuaAccess>> {
            let rules = path_rules(&shared, &mode.unwrap_or_else(current_mode))?;
            Ok(rules.need(&resolve(&shared, &path), intent.into()).map(Into::into))
        },
    )?;
    Ok(())
}

/// `path` made absolute against the project cwd, with `~` expanded.
pub(crate) fn resolve(shared: &LuaShared, path: &str) -> PathBuf {
    smelt_paths::path::resolve_from(path, shared.evaluation_cwd(), home())
}

fn home() -> PathBuf {
    crate::host::try_with_core(|core| core.env.home().clone()).unwrap_or_else(smelt_paths::home_dir)
}

/// The name of the active mode.
pub(crate) fn current_mode() -> String {
    crate::host::try_with_core(|core| core.config.mode.as_str().to_string())
        .unwrap_or_else(|| protocol::AgentMode::default().as_str().to_string())
}

/// The path rules of `mode`: the rules set with `smelt.permissions.paths`,
/// plus the workspace, the temp directories and the session artifacts.
pub(crate) fn path_rules(shared: &LuaShared, mode: &str) -> LuaResult<PathRules> {
    let explicit = shared
        .path_rules
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .resolve(mode, &shared.evaluation_cwd(), &home())
        .map_err(LuaError::RuntimeError)?;
    Ok(
        crate::host::try_with_core(|core| core.permissions.rules(explicit.clone()))
            .unwrap_or_else(|| crate::permissions::Permissions::default().rules(explicit)),
    )
}
