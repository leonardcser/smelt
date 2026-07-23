//! `smelt.permissions` bindings - list, sync, and extend permission policy state.
//! Session/workspace entries sit over `RuntimeApprovals` + [`crate::permissions::store`];
//! policy extensions layer on top of generated defaults.

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_class, Tier};
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// A single session permission entry (one approved tool/pattern pair).
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SessionEntry")]
pub struct LuaPermissionSessionEntry {
    /// Tool name the rule applies to (e.g. `"bash"`). Special value `"directory"` grants generic path access.
    pub tool: String,
    /// Pattern matched against the tool's argument bucket.
    pub pattern: String,
}

/// A tool-specific session path grant. Grants are in-memory only and can
/// satisfy workspace path checks for the matching tool. When `mode` is set,
/// the grant applies only in that mode.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SessionPathGrant")]
pub struct LuaPermissionSessionPathGrant {
    /// Grant kind. Currently only `"path"` is supported.
    pub kind: String,
    /// Optional mode, e.g. `"plan"`. Omit for mode-independent path trust.
    pub mode: Option<String>,
    /// Tool name the grant applies to, e.g. `"read_file"` or `"edit_file"`.
    pub tool: String,
    /// Path access granted: `"read"` or `"write"`.
    pub access: String,
    /// Directory prefix covered by the grant.
    pub path_prefix: String,
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

/// Result table returned by `smelt.permissions.list`.
pub struct LuaPermissionList(mlua::Table);

impl LuaType for LuaPermissionList {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.permissions.ListResult",
            doc: "Current permission state returned by `smelt.permissions.list()`.",
            fields: vec![
                LuaClassField {
                    name: "session",
                    ty: "smelt.permissions.SessionEntry[]".into(),
                    optional: false,
                    doc: "Session-scoped tool/pattern approvals for this run.",
                },
                LuaClassField {
                    name: "path_grants",
                    ty: "smelt.permissions.SessionPathGrant[]".into(),
                    optional: false,
                    doc: "Session-scoped path grants for this run.",
                },
                LuaClassField {
                    name: "workspace",
                    ty: "smelt.permissions.WorkspaceRule[]".into(),
                    optional: false,
                    doc: "Workspace rules loaded from the on-disk store rooted at the current cwd.",
                },
            ],
        });
        "smelt.permissions.ListResult".into()
    }
}

impl IntoLua for LuaPermissionList {
    fn into_lua(self, _: &Lua) -> LuaResult<mlua::Value> {
        Ok(mlua::Value::Table(self.0))
    }
}

/// Spec for `smelt.permissions.sync`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SyncSpec")]
pub struct LuaPermissionSyncSpec {
    /// Session entries; applied for this run only.
    #[lua(default)]
    pub session: Vec<LuaPermissionSessionEntry>,
    /// Tool-specific session path grants; applied for this run only.
    #[lua(default)]
    pub path_grants: Vec<LuaPermissionSessionPathGrant>,
    /// Workspace rules; persisted to disk under the current cwd.
    #[lua(default)]
    pub workspace: Vec<LuaPermissionWorkspaceRule>,
}

/// `allow`/`ask`/`deny` arrays accepted by permission policy sections.
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

/// Effect-level decisions that apply to tools without a more specific rule.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.EffectRules")]
pub struct LuaEffectRules {
    pub read: Option<String>,
    pub write: Option<String>,
    pub network: Option<String>,
    pub process: Option<String>,
    pub config: Option<String>,
    pub user: Option<String>,
    pub other: Option<String>,
}

impl From<LuaEffectRules> for crate::permissions::rules::RawEffectRules {
    fn from(e: LuaEffectRules) -> Self {
        Self {
            read: e.read.as_deref().map(parse_decision),
            write: e.write.as_deref().map(parse_decision),
            network: e.network.as_deref().map(parse_decision),
            process: e.process.as_deref().map(parse_decision),
            config: e.config.as_deref().map(parse_decision),
            user: e.user.as_deref().map(parse_decision),
            other: e.other.as_deref().map(parse_decision),
        }
    }
}

/// Permission slots that apply within a single agent mode.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.ModePerms")]
pub struct LuaModePerms {
    /// Exact tool-name `allow`/`ask`/`deny` entries.
    pub tools: Option<LuaRuleSet>,
    /// Effect-level decisions keyed by effect name.
    pub effects: Option<LuaEffectRules>,
    /// Tool-specific argument patterns keyed by tool name (`"bash"`, `"web_fetch"`, …).
    pub patterns: Option<std::collections::HashMap<String, LuaRuleSet>>,
}

impl From<LuaModePerms> for crate::permissions::rules::RawModePerms {
    fn from(m: LuaModePerms) -> Self {
        Self {
            tools: m.tools.map(Into::into).unwrap_or_default(),
            effects: m.effects.map(Into::into).unwrap_or_default(),
            patterns: m
                .patterns
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

/// Spec for `smelt.permissions.extend`. Each mode falls back to `default`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.PolicySpec")]
pub struct LuaPermissionPolicySpec {
    /// Baseline rules applied unless a mode-specific slot overrides.
    pub default: Option<LuaModePerms>,
    /// Mode-specific rules keyed by registered mode name.
    #[lua(rest)]
    pub modes: std::collections::HashMap<String, LuaModePerms>,
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
        "List, sync, and extend permission policy state. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "list",
        "Return current permission rules as `{ session = { { tool, pattern } }, path_grants = { { kind = \"path\", mode?, tool, access, path_prefix } }, workspace = { { tool, patterns } } }`. Session entries and path grants come from runtime approvals; workspace entries come from the on-disk store rooted at the current cwd.",
        &[],
        |lua, ()| -> LuaResult<LuaPermissionList> {
            let snapshot = crate::lua::try_with_platform_host(|host| host.permission_snapshot());
            let (session_entries, path_grants, workspace_rules) = snapshot
                .map(|snapshot| {
                    (
                        snapshot.session_entries,
                        snapshot.path_grants,
                        snapshot.workspace_rules,
                    )
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
            let path_grants_arr = lua.create_table()?;
            for (i, grant) in path_grants.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("kind", "path")?;
                if let Some(mode) = grant.mode.as_ref() {
                    row.set("mode", mode.as_str())?;
                }
                row.set("tool", grant.tool)?;
                row.set("access", path_access_label(&grant.access))?;
                row.set("path_prefix", grant.dir.display().to_string())?;
                path_grants_arr.set(i + 1, row)?;
            }
            out.set("path_grants", path_grants_arr)?;
            let workspace_arr = lua.create_table()?;
            for (i, rule) in workspace_rules.into_iter().enumerate() {
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
            Ok(LuaPermissionList(out))
        },
    )?;
    let sync_context = Arc::clone(&shared.core);
    m.fn_(
        "sync",
        "Replace runtime + workspace permission entries with `spec.session`, `spec.path_grants`, and `spec.workspace`. Persists workspace rules to disk; session rules apply for this run only.",
        &["spec"],
        move |_, spec: LuaPermissionSyncSpec| -> LuaResult<()> {
            let session_entries: Vec<smelt_core::PermissionEntry> = spec
                .session
                .into_iter()
                .map(|e| smelt_core::PermissionEntry {
                    tool: e.tool,
                    pattern: e.pattern,
                })
                .collect();
            let session_path_grants = spec
                .path_grants
                .into_iter()
                .map(|grant| lua_path_grant_to_runtime(grant, &sync_context))
                .collect::<LuaResult<Vec<_>>>()?;
            let workspace_rules: Vec<crate::permissions::store::Rule> = spec
                .workspace
                .into_iter()
                .map(|r| crate::permissions::store::Rule {
                    tool: r.tool,
                    patterns: r.patterns,
                })
                .collect();
            crate::lua::with_platform_host(|host| {
                host.sync_permissions(session_entries, session_path_grants, workspace_rules)
            });
            Ok(())
        },
    )?;
    let grant_context = Arc::clone(&shared.core);
    m.fn_(
        "grant_session",
        "Add one session-scoped grant. Currently supports `{ kind = \"path\", mode?, tool, access = \"read\"|\"write\", path_prefix }` for tool-specific path access. Omit `mode` for mode-independent path trust; set `mode` to scope the grant to one mode.",
        &["grant"],
        move |_, grant: LuaPermissionSessionPathGrant| -> LuaResult<()> {
            let grant = lua_path_grant_to_runtime(grant, &grant_context)?;
            crate::lua::with_platform_host(|host| host.grant_session_path(grant));
            Ok(())
        },
    )?;
    {
        let shared = Arc::clone(shared);
        m.fn_(
            "extend",
            "Extend the generated permission policy with user rules. Supports `tools`, `effects`, and `patterns` sections under `default` or any mode name.",
            &["spec"],
            move |_, spec: LuaPermissionPolicySpec| -> LuaResult<()> {
                let incoming = crate::permissions::rules::RawPerms {
                    default: spec.default.map(Into::into).unwrap_or_default(),
                    modes: spec
                        .modes
                        .into_iter()
                        .map(|(k, v)| (k, v.into()))
                        .collect(),
                };
                let mut guard = shared
                    .permission_rules
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                merge_policy(guard.get_or_insert_with(Default::default), incoming);
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
            Ok(crate::lua::try_with_platform_host(|host| {
                host.check_tool_permission(&mode_str, &name).to_string()
            })
            .unwrap_or_else(|| "ask".to_string()))
        },
    )?;
    m.fn_(
        "check",
        "Decide a tool-specific pattern bucket (e.g. `(\"normal\", \"bash\", \"git status\")`) against the current policy. Returns `\"allow\"`, `\"ask\"`, or `\"deny\"`; defaults to `\"ask\"` when no app context is available.",
        &["mode_str", "bucket", "value"],
        |_, (mode_str, bucket, value): (String, String, String)| -> LuaResult<String> {
            Ok(crate::lua::try_with_platform_host(|host| {
                host.check_subcommand_permission(&mode_str, &bucket, &value)
                    .to_string()
            })
            .unwrap_or_else(|| "ask".to_string()))
        },
    )?;

    Ok(())
}

fn lua_path_grant_to_runtime(
    grant: LuaPermissionSessionPathGrant,
    context: &smelt_core::lua::LuaShared,
) -> LuaResult<smelt_core::permissions::SessionPathGrant> {
    if grant.kind != "path" {
        return Err(LuaError::RuntimeError(format!(
            "unsupported session grant kind `{}`",
            grant.kind
        )));
    }
    if grant.tool.is_empty() {
        return Err(LuaError::RuntimeError(
            "session path grant requires tool".to_string(),
        ));
    }
    if grant.path_prefix.is_empty() {
        return Err(LuaError::RuntimeError(
            "session path grant requires path_prefix".to_string(),
        ));
    }
    Ok(smelt_core::permissions::SessionPathGrant {
        mode: grant.mode.map(|mode| parse_grant_mode(&mode)).transpose()?,
        tool: grant.tool,
        access: parse_path_access(&grant.access)?,
        dir: context.resolve_project_path(grant.path_prefix),
    })
}

fn parse_grant_mode(mode: &str) -> LuaResult<protocol::AgentMode> {
    protocol::AgentMode::parse(mode)
        .ok_or_else(|| LuaError::RuntimeError(format!("invalid session grant mode `{mode}`")))
}

fn parse_path_access(access: &str) -> LuaResult<smelt_core::permissions::PathAccess> {
    match access {
        "read" => Ok(smelt_core::permissions::PathAccess::Read),
        "write" => Ok(smelt_core::permissions::PathAccess::Write),
        _ => Err(LuaError::RuntimeError(format!(
            "unsupported session path grant access `{access}`"
        ))),
    }
}

fn path_access_label(access: &smelt_core::permissions::PathAccess) -> &'static str {
    match access {
        smelt_core::permissions::PathAccess::Read => "read",
        smelt_core::permissions::PathAccess::Write => "write",
        smelt_core::permissions::PathAccess::Unknown => "unknown",
    }
}

fn parse_decision(value: &str) -> protocol::Decision {
    match value {
        "allow" => protocol::Decision::Allow,
        "deny" => protocol::Decision::Deny,
        _ => protocol::Decision::Ask,
    }
}

fn merge_ruleset(
    base: &mut crate::permissions::rules::RawRuleSet,
    incoming: crate::permissions::rules::RawRuleSet,
) {
    base.allow.extend(incoming.allow);
    base.ask.extend(incoming.ask);
    base.deny.extend(incoming.deny);
}

fn merge_effects(
    base: &mut crate::permissions::rules::RawEffectRules,
    incoming: crate::permissions::rules::RawEffectRules,
) {
    if incoming.read.is_some() {
        base.read = incoming.read;
    }
    if incoming.write.is_some() {
        base.write = incoming.write;
    }
    if incoming.network.is_some() {
        base.network = incoming.network;
    }
    if incoming.process.is_some() {
        base.process = incoming.process;
    }
    if incoming.config.is_some() {
        base.config = incoming.config;
    }
    if incoming.user.is_some() {
        base.user = incoming.user;
    }
    if incoming.other.is_some() {
        base.other = incoming.other;
    }
}

fn merge_mode(
    base: &mut crate::permissions::rules::RawModePerms,
    incoming: crate::permissions::rules::RawModePerms,
) {
    merge_ruleset(&mut base.tools, incoming.tools);
    merge_effects(&mut base.effects, incoming.effects);
    for (name, rules) in incoming.patterns {
        merge_ruleset(base.patterns.entry(name).or_default(), rules);
    }
}

fn merge_policy(
    base: &mut crate::permissions::rules::RawPerms,
    incoming: crate::permissions::rules::RawPerms,
) {
    merge_mode(&mut base.default, incoming.default);
    for (name, mode) in incoming.modes {
        merge_mode(base.modes.entry(name).or_default(), mode);
    }
}
