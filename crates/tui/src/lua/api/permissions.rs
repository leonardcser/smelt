//! `smelt.permissions` bindings — list current session + workspace
//! rules, sync a Lua-built ruleset back through the App. Sits over
//! `RuntimeApprovals` + [`crate::permissions::store`].

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// A single session permission entry (one approved tool/pattern pair).
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SessionEntry")]
pub struct LuaPermissionSessionEntry {
    /// Tool name the rule applies to (e.g. `"shell"`).
    pub tool: String,
    /// Pattern matched against the tool's argument bucket.
    pub pattern: String,
}

/// A workspace permission rule (one tool with N patterns, persisted to disk).
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.WorkspaceRule")]
pub struct LuaPermissionWorkspaceRule {
    /// Tool name the rule applies to.
    pub tool: String,
    /// Patterns granted for this tool.
    pub patterns: Vec<String>,
}

/// Spec for `smelt.permissions.sync`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SyncSpec")]
pub struct LuaPermissionSyncSpec {
    /// Session entries; applied for this run only.
    #[lua(default)]
    pub session: Vec<LuaPermissionSessionEntry>,
    /// Workspace rules; persisted to disk under the current cwd.
    #[lua(default)]
    pub workspace: Vec<LuaPermissionWorkspaceRule>,
}

/// `allow`/`ask`/`deny` pattern arrays accepted by every permission slot.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.RuleSet")]
pub struct LuaRuleSet {
    /// Patterns that auto-allow without prompting.
    #[lua(default)]
    pub allow: Vec<String>,
    /// Patterns that always prompt.
    #[lua(default)]
    pub ask: Vec<String>,
    /// Patterns that auto-deny.
    #[lua(default)]
    pub deny: Vec<String>,
}

impl From<LuaRuleSet> for crate::permissions::rules::RawRuleSet {
    fn from(r: LuaRuleSet) -> Self {
        Self {
            allow: r.allow,
            ask: r.ask,
            deny: r.deny,
        }
    }
}

/// Permission slots that apply within a single agent mode. The fixed
/// `tools` key controls the tool itself; any additional key is treated
/// as a subcommand bucket and routed through that tool's subpattern
/// parser (e.g. `bash = { allow = { "git status" } }`).
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.ModePerms")]
pub struct LuaModePerms {
    /// Per-tool `allow`/`ask`/`deny` patterns.
    pub tools: Option<LuaRuleSet>,
    /// Subcommand patterns keyed by tool name (`"bash"`, `"edit"`, …).
    #[lua(rest)]
    pub subcommands: std::collections::HashMap<String, LuaRuleSet>,
}

impl From<LuaModePerms> for crate::permissions::rules::RawModePerms {
    fn from(m: LuaModePerms) -> Self {
        Self {
            tools: m.tools.map(Into::into).unwrap_or_default(),
            subcommands: m
                .subcommands
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

/// Spec for `smelt.permissions.set_rules`. Each mode falls back to
/// `default` (and then to host-level rules) when its slot is `nil`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.RulesSpec")]
pub struct LuaPermissionRulesSpec {
    /// Baseline rules applied unless a mode-specific slot overrides.
    pub default: Option<LuaModePerms>,
    /// Rules active while the agent is in normal mode.
    pub normal: Option<LuaModePerms>,
    /// Rules active while the agent is in plan mode.
    pub plan: Option<LuaModePerms>,
    /// Rules active while the agent is in apply mode.
    pub apply: Option<LuaModePerms>,
    /// Rules active while the agent is in yolo mode.
    pub yolo: Option<LuaModePerms>,
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "permissions",
        "List session/workspace rules and sync a Lua-built ruleset back through the App. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "list",
        "Return current permission rules as `{ session = { { tool, pattern } }, workspace = { { tool, patterns } } }`. Session entries come from runtime approvals; workspace entries come from the on-disk store rooted at the current cwd.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let (session_entries, cwd) = crate::lua::try_with_app(|app| {
                let entries = app
                    .session_permission_entries()
                    .into_iter()
                    .map(|e| (e.tool, e.pattern))
                    .collect::<Vec<_>>();
                (entries, app.cwd.clone())
            })
            .unwrap_or_default();
            let out = lua.create_table()?;
            let session_arr = lua.create_table()?;
            for (i, (tool, pattern)) in session_entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("tool", tool)?;
                row.set("pattern", pattern)?;
                session_arr.set(i + 1, row)?;
            }
            out.set("session", session_arr)?;
            let workspace_arr = lua.create_table()?;
            for (i, rule) in crate::permissions::store::load(&cwd)
                .into_iter()
                .enumerate()
            {
                let row = lua.create_table()?;
                row.set("tool", rule.tool)?;
                let pats = lua.create_table()?;
                for (j, p) in rule.patterns.into_iter().enumerate() {
                    pats.set(j + 1, p)?;
                }
                row.set("patterns", pats)?;
                workspace_arr.set(i + 1, row)?;
            }
            out.set("workspace", workspace_arr)?;
            Ok(out)
        },
    )?;
    m.fn_(
        "sync",
        "Replace runtime + workspace permission entries with `spec.session` and `spec.workspace`. Persists workspace rules to disk; session rules apply for this run only.",
        &["spec"],
        |_, spec: LuaPermissionSyncSpec| -> LuaResult<()> {
            let session_entries: Vec<smelt_core::PermissionEntry> = spec
                .session
                .into_iter()
                .map(|e| smelt_core::PermissionEntry {
                    tool: e.tool,
                    pattern: e.pattern,
                })
                .collect();
            let workspace_rules: Vec<crate::permissions::store::Rule> = spec
                .workspace
                .into_iter()
                .map(|r| crate::permissions::store::Rule {
                    tool: r.tool,
                    patterns: r.patterns,
                })
                .collect();
            crate::lua::with_app(|app| app.sync_permissions(session_entries, workspace_rules));
            Ok(())
        },
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "set_rules",
            "Install the per-mode permission ruleset. See [`smelt.permissions.RulesSpec`](types.md#smeltpermissionsrulesspec).",
            &["spec"],
            move |_, spec: LuaPermissionRulesSpec| -> LuaResult<()> {
                let rules = crate::permissions::rules::RawPerms {
                    default: spec.default.map(Into::into).unwrap_or_default(),
                    normal: spec.normal.map(Into::into).unwrap_or_default(),
                    plan: spec.plan.map(Into::into).unwrap_or_default(),
                    apply: spec.apply.map(Into::into).unwrap_or_default(),
                    yolo: spec.yolo.map(Into::into).unwrap_or_default(),
                };
                let mut guard = shared
                    .permission_rules
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *guard = Some(rules);
                Ok(())
            },
        )?;
    }
    // Decision primitives for tool `decide` callbacks. Returns "allow"/"ask"/"deny".
    m.fn_(
        "check_tool",
        "Decision primitives for tool `decide` callbacks. Returns \"allow\"/\"ask\"/\"deny\".",
        &["mode_str", "name"],
        |_, (mode_str, name): (String, String)| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                let mode = parse_mode(&mode_str);
                decision_label(app.core.permissions.check_tool(mode, &name)).to_string()
            })
            .unwrap_or_else(|| "ask".to_string()))
        },
    )?;
    m.fn_(
        "check",
        "Decide a subcommand bucket (e.g. `(\"normal\", \"shell\", \"git status\")`) against the current ruleset. Returns `\"allow\"`, `\"ask\"`, or `\"deny\"`; defaults to `\"ask\"` when no app context is available.",
        &["mode_str", "bucket", "value"],
        |_, (mode_str, bucket, value): (String, String, String)| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                let mode = parse_mode(&mode_str);
                decision_label(app.core.permissions.check_subcommand(mode, &bucket, &value))
                    .to_string()
            })
            .unwrap_or_else(|| "ask".to_string()))
        },
    )?;

    Ok(())
}

fn parse_mode(s: &str) -> protocol::AgentMode {
    protocol::AgentMode::parse(s).unwrap_or(protocol::AgentMode::Normal)
}

fn decision_label(d: protocol::Decision) -> &'static str {
    match d {
        protocol::Decision::Allow => "allow",
        protocol::Decision::Ask => "ask",
        protocol::Decision::Deny => "deny",
        protocol::Decision::Error(_) => "ask",
    }
}
