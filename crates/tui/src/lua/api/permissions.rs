//! `smelt.permissions` bindings - list, sync, and extend permission policy state.
//! Session/workspace/repository entries sit over `RuntimeApprovals` + [`crate::permissions::store`];
//! policy extensions layer on top of generated defaults.

use lua_doc_derive::{LuaAlias, LuaOpts};
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
            classification: smelt_core::lua::doc::classification_for_type(
                "smelt.permissions.ListResult",
            ),
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
                LuaClassField {
                    name: "workspace_revision",
                    ty: "integer".into(),
                    optional: false,
                    doc: "Revision required to replace the workspace rules safely.",
                },
                LuaClassField {
                    name: "repository",
                    ty: "smelt.permissions.WorkspaceRule[]".into(),
                    optional: false,
                    doc:
                        "Repository rules shared by all worktrees. Empty outside a Git repository.",
                },
                LuaClassField {
                    name: "repository_revision",
                    ty: "integer".into(),
                    optional: false,
                    doc: "Revision required to replace the repository rules safely.",
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

fn lua_workspace_rule_to_runtime(
    rule: LuaPermissionWorkspaceRule,
) -> crate::permissions::store::Rule {
    crate::permissions::store::Rule {
        tool: rule.tool,
        patterns: rule.patterns,
    }
}

fn lua_scope_replacement_to_runtime(
    replacement: LuaPermissionScopeReplacement,
) -> crate::permissions::store::Replacement {
    crate::permissions::store::Replacement {
        expected_revision: replacement.revision,
        rules: replacement
            .rules
            .into_iter()
            .map(lua_workspace_rule_to_runtime)
            .collect(),
    }
}

fn permission_rules_to_lua(
    lua: &Lua,
    rules: Vec<crate::permissions::store::Rule>,
) -> LuaResult<mlua::Table> {
    let array = lua.create_table()?;
    for (i, rule) in rules.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("tool", rule.tool)?;
        let patterns = lua.create_table()?;
        for (j, pattern) in rule.patterns.into_iter().enumerate() {
            patterns.set(j + 1, pattern)?;
        }
        row.set("patterns", patterns)?;
        array.set(i + 1, row)?;
    }
    Ok(array)
}

/// Revision-checked replacement for one persisted permission scope.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.ScopeReplacement")]
pub struct LuaPermissionScopeReplacement {
    /// Revision returned by the `smelt.permissions.list()` snapshot being edited.
    pub revision: u64,
    /// Complete replacement rule set for this scope.
    pub rules: Vec<LuaPermissionWorkspaceRule>,
}

/// Spec for `smelt.permissions.sync`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.SyncSpec")]
pub struct LuaPermissionSyncSpec {
    /// Session entries to replace for this run. Omit to leave them unchanged.
    pub session: Option<Vec<LuaPermissionSessionEntry>>,
    /// Tool-specific session path grants to replace. Omit to leave them unchanged.
    pub path_grants: Option<Vec<LuaPermissionSessionPathGrant>>,
    /// Revision-checked workspace replacement. Cannot be combined with `repository`.
    pub workspace: Option<LuaPermissionScopeReplacement>,
    /// Revision-checked repository replacement. Cannot be combined with `workspace`.
    pub repository: Option<LuaPermissionScopeReplacement>,
}

/// One exact permission entry to revoke transactionally.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.permissions.RevokeSpec")]
pub struct LuaPermissionRevokeSpec {
    /// Permission scope: `"session"`, `"workspace"`, or `"repository"`.
    pub scope: String,
    /// Tool name the entry applies to. Use `"directory"` for path-prefix entries.
    pub tool: String,
    /// Exact pattern to remove. Use `"*"` for a blanket tool approval.
    pub pattern: String,
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

/// Decision accepted by effect-level permission rules.
#[derive(Clone, Copy, Debug, LuaAlias)]
#[lua(name = "smelt.permissions.Decision")]
pub enum LuaPermissionDecision {
    Allow,
    Ask,
    Deny,
}

impl From<LuaPermissionDecision> for protocol::Decision {
    fn from(value: LuaPermissionDecision) -> Self {
        match value {
            LuaPermissionDecision::Allow => Self::Allow,
            LuaPermissionDecision::Ask => Self::Ask,
            LuaPermissionDecision::Deny => Self::Deny,
        }
    }
}

/// Effect-level decisions that apply to tools without a more specific rule.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.permissions.EffectRules")]
pub struct LuaEffectRules {
    /// Decision for tools that only read data.
    pub read: Option<LuaPermissionDecision>,
    /// Decision for tools that write or mutate data.
    pub write: Option<LuaPermissionDecision>,
    /// Decision for tools that access the network.
    pub network: Option<LuaPermissionDecision>,
    /// Decision for tools that start or control processes.
    pub process: Option<LuaPermissionDecision>,
    /// Decision for tools that modify configuration.
    pub config: Option<LuaPermissionDecision>,
    /// Decision for tools that require direct user interaction.
    pub user: Option<LuaPermissionDecision>,
    /// Decision for tools whose effect has no more specific category.
    pub other: Option<LuaPermissionDecision>,
}

impl From<LuaEffectRules> for crate::permissions::rules::RawEffectRules {
    fn from(e: LuaEffectRules) -> Self {
        Self {
            read: e.read.map(Into::into),
            write: e.write.map(Into::into),
            network: e.network.map(Into::into),
            process: e.process.map(Into::into),
            config: e.config.map(Into::into),
            user: e.user.map(Into::into),
            other: e.other.map(Into::into),
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
    let m = LuaMod::supported(
        lua,
        smelt,
        "permissions",
        "Inspect, revoke, and extend permission policy state, or synchronize live session and persisted grants.",
        Tier::Host,
    )?;
    let ui = LuaMod::extend_supported(lua, m.tbl.clone(), "smelt.permissions", Tier::UiHost);
    m.fn_(
        "list",
        "Return current permission rules and persisted scope revisions. Pass `workspace_revision` or `repository_revision` back as the matching scope replacement revision in `smelt.permissions.sync()`.",
        &[],
        |lua, ()| -> LuaResult<LuaPermissionList> {
            let snapshot = match crate::lua::try_with_platform_host(|host| {
                host.permission_snapshot()
            }) {
                Some(snapshot) => snapshot.map_err(LuaError::external)?,
                None => Default::default(),
            };
            let (session_entries, path_grants, workspace, repository) = (
                snapshot.session_entries,
                snapshot.path_grants,
                snapshot.workspace,
                snapshot.repository,
            );
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
            out.set("workspace_revision", workspace.revision)?;
            out.set("workspace", permission_rules_to_lua(lua, workspace.rules)?)?;
            out.set("repository_revision", repository.revision)?;
            out.set(
                "repository",
                permission_rules_to_lua(lua, repository.rules)?,
            )?;
            Ok(LuaPermissionList(out))
        },
    )?;
    let sync_context = Arc::clone(&shared.core);
    ui.live_only_fn(
        "sync",
        "Replace selected permission entries. Omitted fields are unchanged. Persisted replacements require the revision from `smelt.permissions.list()` and a call may replace only one of `workspace` or `repository`, so a stale snapshot cannot discard concurrent grants.",
        &["spec"],
        move |_, spec: LuaPermissionSyncSpec| -> LuaResult<()> {
            let session_entries = spec.session.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| smelt_core::PermissionEntry {
                        tool: entry.tool,
                        pattern: entry.pattern,
                    })
                    .collect()
            });
            let session_path_grants = spec
                .path_grants
                .map(|grants| {
                    grants
                        .into_iter()
                        .map(|grant| lua_path_grant_to_runtime(grant, &sync_context))
                        .collect::<LuaResult<Vec<_>>>()
                })
                .transpose()?;
            let workspace = spec.workspace.map(lua_scope_replacement_to_runtime);
            let repository = spec.repository.map(lua_scope_replacement_to_runtime);
            crate::lua::with_platform_host(|host| {
                host.sync_permissions(session_entries, session_path_grants, workspace, repository)
            })
            .map_err(LuaError::external)
        },
    )?;
    ui.live_only_fn(
        "revoke",
        "Remove one exact session, workspace, or repository permission entry transactionally. Returns false when the entry no longer exists.",
        &["spec"],
        |_, spec: LuaPermissionRevokeSpec| -> LuaResult<bool> {
            crate::lua::with_platform_host(|host| {
                host.revoke_permission(&spec.scope, &spec.tool, &spec.pattern)
            })
            .map_err(LuaError::external)
        },
    )?;
    let grant_context = Arc::clone(&shared.core);
    ui.live_only_fn(
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
