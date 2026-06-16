//! Permission policy for tool calls.

pub(crate) mod approvals;
pub(crate) mod bash;
pub mod rules;
pub mod store;
pub(crate) mod workspace;

#[cfg(test)]
mod tests;

pub use approvals::{RuntimeApprovals, SessionPathGrant};
pub use bash::{split_shell_commands, split_shell_commands_with_ops};
pub use protocol::Decision;
pub use rules::{SubpatternParserFn, ToolDefaults, ToolEffectKind};

use bash::{has_output_redirection, is_cd_command};

use protocol::AgentMode;
use rules::{
    build_mode, check_ruleset, compile_patterns, merge_mode, ModePerms, RawConfig, RawPerms,
    RuleSet,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    Lua,
    Core,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathTargetKind {
    File,
    Directory,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPath {
    pub path: String,
    pub target_kind: PathTargetKind,
}

impl ToolPath {
    pub fn unknown(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            target_kind: PathTargetKind::Unknown,
        }
    }

    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            target_kind: PathTargetKind::File,
        }
    }

    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            target_kind: PathTargetKind::Directory,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEffect {
    pub raw_path: String,
    pub base_dir: PathBuf,
    pub path: PathBuf,
    pub access: PathAccess,
    pub target_kind: PathTargetKind,
}

impl PathEffect {
    pub(super) fn from_raw(
        raw_path: String,
        base_dir: &std::path::Path,
        access: PathAccess,
    ) -> Self {
        Self::from_raw_with_kind(raw_path, base_dir, access, PathTargetKind::Unknown)
    }

    pub(super) fn from_tool_path(
        tool_path: ToolPath,
        base_dir: &std::path::Path,
        access: PathAccess,
    ) -> Self {
        Self::from_raw_with_kind(tool_path.path, base_dir, access, tool_path.target_kind)
    }

    pub(super) fn from_directory(
        raw_path: String,
        base_dir: &std::path::Path,
        access: PathAccess,
    ) -> Self {
        Self::from_raw_with_kind(raw_path, base_dir, access, PathTargetKind::Directory)
    }

    fn from_raw_with_kind(
        raw_path: String,
        base_dir: &std::path::Path,
        access: PathAccess,
        target_kind: PathTargetKind,
    ) -> Self {
        let path = workspace::resolve_path(&raw_path, base_dir);
        Self {
            raw_path,
            base_dir: base_dir.to_path_buf(),
            path,
            access,
            target_kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRisk {
    ReadOnly,
    Writes,
    Destructive,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEffect {
    Fs(PathEffect),
    FsAccess(PathAccess),
    Shell {
        command: String,
        risk: ShellRisk,
        paths: Vec<PathEffect>,
    },
    Network,
    Mcp {
        tool: String,
    },
    UserInteraction,
    ProcessRead,
    ProcessControl,
    ConfigReload,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReadOnlyDisposition {
    Read,
    Ask,
    Deny,
}

impl ReadOnlyDisposition {
    fn merge(self, other: Self) -> Self {
        self.max(other)
    }
}

pub struct PermissionRequest<'a> {
    pub mode: AgentMode,
    pub tool_name: &'a str,
    pub args: &'a HashMap<String, Value>,
    pub origin: ToolOrigin,
    pub effects: Vec<ToolEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequirement {
    Tool { tool: String },
    Command { tool: String, command: String },
    PathPrefix { dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionGrant {
    Tool { tool: String },
    Command { tool: String, pattern: String },
    PathPrefix { dir: PathBuf },
}

impl PermissionGrant {
    pub fn satisfies(&self, requirement: &PermissionRequirement) -> bool {
        match (self, requirement) {
            (PermissionGrant::Tool { tool: grant }, PermissionRequirement::Tool { tool }) => {
                grant == tool
            }
            (
                PermissionGrant::Tool { tool: grant },
                PermissionRequirement::Command { tool, .. },
            ) => grant == tool,
            (
                PermissionGrant::Command {
                    tool: grant_tool,
                    pattern,
                },
                PermissionRequirement::Command { tool, command },
            ) => {
                grant_tool == tool
                    && glob::Pattern::new(pattern).is_ok_and(|p| rules::matches_rule(&p, command))
            }
            (
                PermissionGrant::PathPrefix { dir },
                PermissionRequirement::PathPrefix { dir: path },
            ) => workspace::path_prefix_matches(dir, path),
            _ => false,
        }
    }

    pub fn display_subject(&self) -> String {
        match self {
            PermissionGrant::Tool { tool } => tool.clone(),
            PermissionGrant::Command { pattern, .. } => {
                let display = pattern.strip_suffix("/*").unwrap_or(pattern);
                display
                    .split_once("://")
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_else(|| display.to_string())
            }
            PermissionGrant::PathPrefix { dir } => engine::paths::collapse_tilde(dir)
                .to_string_lossy()
                .into_owned(),
        }
    }

    pub fn display_subjects(grants: &[PermissionGrant]) -> String {
        let mut command_head = None;
        grants
            .iter()
            .map(|grant| {
                let subject = grant.display_subject();
                match grant {
                    PermissionGrant::Command { .. } => {
                        if let Some(head) = &command_head {
                            subject
                                .strip_prefix(head)
                                .and_then(|rest| rest.strip_prefix(' '))
                                .unwrap_or(&subject)
                                .to_string()
                        } else {
                            command_head = subject.split_whitespace().next().map(str::to_string);
                            subject
                        }
                    }
                    _ => subject,
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOutcome {
    pub decision: Decision,
    pub missing_requirements: Vec<PermissionRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionApprovalOptions {
    pub grant_sets: Vec<Vec<PermissionGrant>>,
}

/// Maps `(tool_name, args)` to filesystem paths the call would touch.
/// Tools that don't touch paths don't register one; the workspace check short-circuits.
pub type PathsFn = dyn Fn(&str, &HashMap<String, Value>) -> Vec<ToolPath> + Send + Sync;

pub use rules::ModeBehavior;

#[derive(Clone)]
pub struct Permissions {
    modes: HashMap<String, ModePerms>,
    mode_behaviors: HashMap<String, ModeBehavior>,
    restrict_to_workspace: bool,
    workspace: PathBuf,
    paths_fn: Option<Arc<PathsFn>>,
    tool_effects: HashMap<String, ToolEffectKind>,
    subpattern_parsers: HashMap<String, Arc<SubpatternParserFn>>,
    /// Interior-mutable so `Arc<Permissions>` holders can grant approvals without a writable handle.
    pub approvals: Arc<RwLock<RuntimeApprovals>>,
}

impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permissions")
            .field("modes", &self.modes.keys().collect::<Vec<_>>())
            .field(
                "mode_behaviors",
                &self.mode_behaviors.keys().collect::<Vec<_>>(),
            )
            .field("restrict_to_workspace", &self.restrict_to_workspace)
            .field("workspace", &self.workspace)
            .field("paths_fn", &self.paths_fn.as_ref().map(|_| "<fn>"))
            .field("tool_effects", &self.tool_effects)
            .field(
                "subpattern_parsers",
                &self.subpattern_parsers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Permissions {
    pub fn load() -> Self {
        Self::from_raw(&RawConfig::default().permissions, &ToolDefaults::default())
    }

    pub fn from_raw(raw: &RawPerms, tool_defaults: &ToolDefaults) -> Self {
        Self::from_raw_with_mode_behaviors(raw, tool_defaults, HashMap::new())
    }

    pub fn from_raw_with_mode_behaviors(
        raw: &RawPerms,
        tool_defaults: &ToolDefaults,
        mode_behaviors: HashMap<String, ModeBehavior>,
    ) -> Self {
        let def = &raw.default;
        let mut modes = HashMap::new();
        let mut names: std::collections::HashSet<String> = raw.modes.keys().cloned().collect();
        names.extend(mode_behaviors.keys().cloned());
        names.insert(protocol::AgentMode::normal().to_string());
        for name in names {
            let mode = AgentMode::parse(&name).unwrap_or_else(protocol::AgentMode::normal);
            let raw_mode = raw.modes.get(&name).cloned().unwrap_or_default();
            let behavior = mode_behaviors.get(&name).cloned().unwrap_or_default();
            modes.insert(
                name,
                build_mode(&merge_mode(def, &raw_mode), &mode, behavior, tool_defaults),
            );
        }
        Self {
            modes,
            mode_behaviors,
            restrict_to_workspace: true,
            workspace: PathBuf::new(),
            paths_fn: None,
            tool_effects: tool_defaults.tool_effects.clone(),
            subpattern_parsers: tool_defaults.subpattern_parsers.clone(),
            approvals: Arc::new(RwLock::new(RuntimeApprovals::new())),
        }
    }

    /// Convenience: same as `from_raw`, plus loads workspace-scoped
    /// auto-approvals from `<cwd>/.smelt/permissions.json` (or wherever
    /// `permissions::store` reads from). Called once at startup.
    pub fn from_raw_with_workspace(
        raw: &RawPerms,
        tool_defaults: &ToolDefaults,
        cwd: &str,
    ) -> Self {
        let perms = Self::from_raw(raw, tool_defaults);
        let rules = store::load(cwd);
        let (ws_tools, ws_dirs) = store::into_approvals(&rules);
        perms
            .approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
        perms
    }

    /// Create a clone with per-turn permission overrides layered on top.
    /// Override rules are prepended (checked first) to the existing rules
    /// for every mode.
    pub fn with_overrides(&self, overrides: &protocol::PermissionOverrides) -> Self {
        let mut cloned = self.clone();
        fn apply_to_mode(mode: &mut ModePerms, overrides: &protocol::PermissionOverrides) {
            if let Some(ref tools) = overrides.tools {
                for name in &tools.allow {
                    mode.tools.insert(name.clone(), Decision::Allow);
                }
                for name in &tools.ask {
                    mode.tools.insert(name.clone(), Decision::Ask);
                }
                for name in &tools.deny {
                    mode.tools.insert(name.clone(), Decision::Deny);
                }
            }
            for (bucket, rs) in &overrides.subcommands {
                let entry = mode.subcommands.entry(bucket.clone()).or_insert(RuleSet {
                    allow: vec![],
                    ask: vec![],
                    deny: vec![],
                });
                let mut allow = compile_patterns(&rs.allow);
                allow.append(&mut entry.allow);
                entry.allow = allow;
                let mut ask = compile_patterns(&rs.ask);
                ask.append(&mut entry.ask);
                entry.ask = ask;
                let mut deny = compile_patterns(&rs.deny);
                deny.append(&mut entry.deny);
                entry.deny = deny;
            }
        }
        for mode in cloned.modes.values_mut() {
            apply_to_mode(mode, overrides);
        }
        cloned
    }

    pub fn set_workspace(&mut self, path: PathBuf) {
        self.workspace = path;
    }

    pub fn set_restrict_to_workspace(&mut self, val: bool) {
        self.restrict_to_workspace = val;
    }

    pub fn set_paths_fn(&mut self, f: Arc<PathsFn>) {
        self.paths_fn = Some(f);
    }

    /// Carry live session state onto a freshly-built policy snapshot.
    ///
    /// Reload rebuilds mode rules and tool defaults from Lua, but runtime
    /// approvals, workspace restriction, and path resolvers belong to the
    /// running app and must survive that rebuild.
    pub fn with_runtime_state_from(mut self, prev: &Self) -> Self {
        self.restrict_to_workspace = prev.restrict_to_workspace;
        self.workspace = prev.workspace.clone();
        self.paths_fn = prev.paths_fn.clone();
        self.approvals = prev.approvals.clone();
        self
    }

    fn paths_for_tool(&self, tool_name: &str, args: &HashMap<String, Value>) -> Vec<ToolPath> {
        match self.paths_fn.as_ref() {
            Some(f) => f(tool_name, args),
            None => Vec::new(),
        }
    }

    fn tool_effect_kind(&self, tool_name: &str) -> ToolEffectKind {
        self.tool_effects
            .get(tool_name)
            .copied()
            .unwrap_or(ToolEffectKind::Unknown)
    }

    fn declared_effects_for_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> Vec<ToolEffect> {
        match self.tool_effect_kind(tool_name) {
            ToolEffectKind::PathRead => {
                self.path_effects_for_tool(tool_name, args, PathAccess::Read)
            }
            ToolEffectKind::PathWrite => {
                self.path_effects_for_tool(tool_name, args, PathAccess::Write)
            }
            ToolEffectKind::Network => vec![ToolEffect::Network],
            ToolEffectKind::UserInteraction => vec![ToolEffect::UserInteraction],
            ToolEffectKind::ProcessRead => vec![ToolEffect::ProcessRead],
            ToolEffectKind::ProcessControl => vec![ToolEffect::ProcessControl],
            ToolEffectKind::ConfigReload => vec![ToolEffect::ConfigReload],
            ToolEffectKind::Unknown => vec![ToolEffect::Unknown],
        }
    }

    fn path_effects_for_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
        access: PathAccess,
    ) -> Vec<ToolEffect> {
        let effects: Vec<_> = self
            .paths_for_tool(tool_name, args)
            .into_iter()
            .map(|p| {
                ToolEffect::Fs(PathEffect::from_tool_path(
                    p,
                    &self.workspace,
                    access.clone(),
                ))
            })
            .collect();
        if effects.is_empty() {
            vec![ToolEffect::FsAccess(access)]
        } else {
            effects
        }
    }

    pub fn effects_for_tool(
        &self,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> Vec<ToolEffect> {
        if origin == ToolOrigin::Mcp {
            return vec![ToolEffect::Mcp {
                tool: tool_name.to_string(),
            }];
        }

        match tool_name {
            "bash" => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let analysis = bash::analyze_shell_command(&command, &self.workspace);
                let mut effects = vec![ToolEffect::Shell {
                    command,
                    risk: analysis.risk,
                    paths: analysis.paths,
                }];
                if args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    effects.push(ToolEffect::ProcessControl);
                }
                effects
            }
            _ => self.declared_effects_for_tool(tool_name, args),
        }
    }

    fn effect_paths<'a>(effects: &'a [ToolEffect], out: &mut Vec<&'a PathEffect>) {
        for effect in effects {
            match effect {
                ToolEffect::Fs(path) => out.push(path),
                ToolEffect::Shell { paths, .. } => out.extend(paths),
                _ => {}
            }
        }
    }

    fn outside_workspace_requirements(&self, effects: &[ToolEffect]) -> Vec<PermissionRequirement> {
        if !self.restrict_to_workspace || self.workspace.as_os_str().is_empty() {
            return vec![];
        }
        let workspace = workspace::canonicalize_path_or_parent(&self.workspace);
        let mut paths = Vec::new();
        Self::effect_paths(effects, &mut paths);
        let mut out = Vec::new();
        for effect in paths {
            if workspace::path_prefix_matches(&workspace, &effect.path) {
                continue;
            }
            let dir = display_dir_for_effect(effect);
            let req = PermissionRequirement::PathPrefix { dir };
            if !out
                .iter()
                .any(|existing| requirements_equivalent(existing, &req))
            {
                out.push(req);
            }
        }
        out
    }

    fn mode_behavior(&self, mode: &AgentMode) -> ModeBehavior {
        self.mode_behaviors
            .get(mode.as_str())
            .cloned()
            .unwrap_or_default()
    }

    fn mode_perms(&self, mode: &AgentMode) -> Option<&ModePerms> {
        self.modes.get(mode.as_str())
    }

    pub fn check_tool(&self, mode: AgentMode, tool_name: &str) -> Decision {
        let behavior = self.mode_behavior(&mode);
        self.mode_perms(&mode)
            .and_then(|perms| perms.tools.get(tool_name).cloned())
            .unwrap_or(behavior.default_decision)
    }

    pub fn subcommand_ruleset(&self, mode: AgentMode, bucket: &str) -> Option<&RuleSet> {
        self.mode_perms(&mode)?.subcommands.get(bucket)
    }

    /// Check `value` against the bucket's ruleset. Custom parsers (e.g. `bash`'s shell parser)
    /// run when registered; otherwise plain glob-match. Falls back to the active mode's default
    /// decision when no bucket is registered.
    pub fn check_subcommand(&self, mode: AgentMode, bucket: &str, value: &str) -> Decision {
        let behavior = self.mode_behavior(&mode);
        let Some(rs) = self.subcommand_ruleset(mode.clone(), bucket) else {
            return behavior.default_decision;
        };
        if let Some(parser) = self.subpattern_parsers.get(bucket) {
            return parser(rs, value, mode);
        }
        let decision = check_ruleset(rs, value);
        if decision == Decision::Ask && behavior.allow_subcommands_by_default {
            Decision::Allow
        } else {
            decision
        }
    }

    pub fn evaluate_tool(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> PermissionOutcome {
        let effects = self.effects_for_tool(origin.clone(), tool_name, args);
        self.evaluate_request(PermissionRequest {
            mode,
            tool_name,
            args,
            origin,
            effects,
        })
    }

    pub fn evaluate_tool_with_approvals(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> PermissionOutcome {
        let effects = self.effects_for_tool(origin.clone(), tool_name, args);
        let mut outcome = self.evaluate_request(PermissionRequest {
            mode: mode.clone(),
            tool_name,
            args,
            origin,
            effects: effects.clone(),
        });
        let approvals = self.approvals.read().unwrap();
        if outcome.decision == Decision::Ask {
            outcome.missing_requirements.retain(|req| {
                !request_requirement_satisfied(req, &mode, tool_name, &effects, &approvals)
            });
            if outcome.missing_requirements.is_empty() {
                outcome.decision = Decision::Allow;
            }
        } else if outcome.decision == Decision::Deny
            && self.mode_behavior(&mode).read_only
            && read_only_session_write_approved(&mode, tool_name, &effects, &approvals)
        {
            outcome.decision = Decision::Allow;
        }
        outcome
    }

    pub fn approval_options(
        &self,
        tool_name: &str,
        candidates: &[String],
        outcome: &PermissionOutcome,
    ) -> PermissionApprovalOptions {
        if outcome.decision != Decision::Ask || outcome.missing_requirements.is_empty() {
            return PermissionApprovalOptions { grant_sets: vec![] };
        }

        let approvals = self.approvals.read().unwrap();
        let command_grants = self.approval_pattern_candidates(&approvals, tool_name, candidates);
        let mut grants = Vec::new();

        if !command_grants.is_empty() {
            grants.push(
                command_grants
                    .iter()
                    .map(|pattern| PermissionGrant::Command {
                        tool: tool_name.to_string(),
                        pattern: pattern.clone(),
                    })
                    .collect::<Vec<_>>(),
            );
        }

        for requirement in &outcome.missing_requirements {
            match requirement {
                PermissionRequirement::Tool { tool } => {
                    grants.push(vec![PermissionGrant::Tool { tool: tool.clone() }]);
                }
                PermissionRequirement::Command { tool, .. } if command_grants.is_empty() => {
                    grants.push(vec![PermissionGrant::Tool { tool: tool.clone() }]);
                }
                PermissionRequirement::PathPrefix { dir } => {
                    grants.push(vec![PermissionGrant::PathPrefix { dir: dir.clone() }]);
                }
                PermissionRequirement::Command { .. } => {}
            }
        }

        let mut grant_sets = Vec::new();
        for grant in &grants {
            if grants_satisfy_requirements(grant, &outcome.missing_requirements) {
                push_unique_grant_set(&mut grant_sets, grant.clone());
            }
        }

        let combined: Vec<_> = grants.into_iter().flatten().collect();
        if grant_sets.is_empty()
            && combined.len() > 1
            && grants_satisfy_requirements(&combined, &outcome.missing_requirements)
        {
            push_unique_grant_set(&mut grant_sets, combined);
        }

        PermissionApprovalOptions { grant_sets }
    }

    fn approval_pattern_candidates(
        &self,
        approvals: &RuntimeApprovals,
        tool_name: &str,
        candidates: &[String],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for candidate in candidates {
            if approvals.has_pattern(tool_name, candidate) || out.iter().any(|p| p == candidate) {
                continue;
            }
            if glob::Pattern::new(candidate).is_ok() {
                out.push(candidate.clone());
            }
        }
        out
    }

    pub fn evaluate_request(&self, request: PermissionRequest<'_>) -> PermissionOutcome {
        let behavior = self.mode_behavior(&request.mode);
        let base = base_evaluation(self, &request);
        if base.decision == Decision::Deny {
            return PermissionOutcome {
                decision: Decision::Deny,
                missing_requirements: Vec::new(),
            };
        }

        let adjusted = read_only_adjusted_decision(&behavior, base.decision, &request.effects);
        if adjusted == Decision::Deny {
            return PermissionOutcome {
                decision: Decision::Deny,
                missing_requirements: Vec::new(),
            };
        }

        let mut missing = base.missing_requirements;
        if adjusted == Decision::Ask && missing.is_empty() {
            missing.push(PermissionRequirement::Tool {
                tool: request.tool_name.to_string(),
            });
        }
        missing.extend(self.outside_workspace_requirements(&request.effects));
        dedupe_requirements(&mut missing);
        PermissionOutcome {
            decision: if missing.is_empty() {
                Decision::Allow
            } else {
                Decision::Ask
            },
            missing_requirements: missing,
        }
    }
}

struct BaseEvaluation {
    decision: Decision,
    missing_requirements: Vec<PermissionRequirement>,
}

fn grants_satisfy_requirements(
    grants: &[PermissionGrant],
    requirements: &[PermissionRequirement],
) -> bool {
    requirements
        .iter()
        .all(|requirement| grants.iter().any(|grant| grant.satisfies(requirement)))
}

fn grant_equivalent(a: &PermissionGrant, b: &PermissionGrant) -> bool {
    match (a, b) {
        (PermissionGrant::PathPrefix { dir: a }, PermissionGrant::PathPrefix { dir: b }) => {
            workspace::paths_equivalent(a, b)
        }
        _ => a == b,
    }
}

fn grant_set_contains(set: &[PermissionGrant], grant: &PermissionGrant) -> bool {
    set.iter().any(|existing| grant_equivalent(existing, grant))
}

fn grant_set_is_subset(a: &[PermissionGrant], b: &[PermissionGrant]) -> bool {
    a.iter().all(|grant| grant_set_contains(b, grant))
}

fn push_unique_grant_set(sets: &mut Vec<Vec<PermissionGrant>>, set: Vec<PermissionGrant>) {
    if sets
        .iter()
        .any(|existing| grant_set_is_subset(existing, &set))
    {
        return;
    }
    sets.retain(|existing| !grant_set_is_subset(&set, existing));
    sets.push(set);
}

fn requirements_equivalent(a: &PermissionRequirement, b: &PermissionRequirement) -> bool {
    match (a, b) {
        (
            PermissionRequirement::PathPrefix { dir: a },
            PermissionRequirement::PathPrefix { dir: b },
        ) => workspace::paths_equivalent(a, b),
        _ => a == b,
    }
}

fn dedupe_requirements(requirements: &mut Vec<PermissionRequirement>) {
    let mut out = Vec::new();
    for requirement in requirements.drain(..) {
        if !out
            .iter()
            .any(|existing| requirements_equivalent(existing, &requirement))
        {
            out.push(requirement);
        }
    }
    *requirements = out;
}

fn display_dir_for_effect(effect: &PathEffect) -> PathBuf {
    let raw = std::path::Path::new(&effect.raw_path);
    if effect.target_kind == PathTargetKind::Directory {
        return effect.path.clone();
    }
    if !raw.is_absolute() && !effect.raw_path.starts_with("~/") {
        return effect.base_dir.clone();
    }
    effect
        .path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| effect.path.clone())
}

fn base_evaluation(permissions: &Permissions, request: &PermissionRequest<'_>) -> BaseEvaluation {
    match request.origin {
        ToolOrigin::Mcp => {
            let decision =
                permissions.check_subcommand(request.mode.clone(), "mcp", request.tool_name);
            BaseEvaluation {
                decision: decision.clone(),
                missing_requirements: requirements_for_decision(
                    decision,
                    PermissionRequirement::Command {
                        tool: "mcp".to_string(),
                        command: request.tool_name.to_string(),
                    },
                ),
            }
        }
        ToolOrigin::Lua | ToolOrigin::Core => match request.tool_name {
            "bash" => bash_evaluation(permissions, request.mode.clone(), request.args),
            "web_fetch" => web_fetch_evaluation(permissions, request.mode.clone(), request.args),
            tool => {
                let decision = permissions.check_tool(request.mode.clone(), tool);
                BaseEvaluation {
                    decision: decision.clone(),
                    missing_requirements: requirements_for_decision(
                        decision,
                        PermissionRequirement::Tool {
                            tool: tool.to_string(),
                        },
                    ),
                }
            }
        },
    }
}

fn requirements_for_decision(
    decision: Decision,
    requirement: PermissionRequirement,
) -> Vec<PermissionRequirement> {
    if decision == Decision::Ask {
        vec![requirement]
    } else {
        Vec::new()
    }
}

fn bash_evaluation(
    permissions: &Permissions,
    mode: AgentMode,
    args: &HashMap<String, Value>,
) -> BaseEvaluation {
    let tool = permissions.check_tool(mode.clone(), "bash");
    if tool == Decision::Deny {
        return BaseEvaluation {
            decision: Decision::Deny,
            missing_requirements: Vec::new(),
        };
    }

    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    let sub = permissions.check_subcommand(mode.clone(), "bash", command);
    if sub == Decision::Deny {
        return BaseEvaluation {
            decision: Decision::Deny,
            missing_requirements: Vec::new(),
        };
    }
    let asks_for_command = sub == Decision::Ask
        || (sub == Decision::Allow
            && permissions.mode_behavior(&mode).ask_on_output_redirection
            && shell_has_output_redirection(command));
    if asks_for_command {
        return BaseEvaluation {
            decision: Decision::Ask,
            missing_requirements: vec![PermissionRequirement::Command {
                tool: "bash".to_string(),
                command: command.to_string(),
            }],
        };
    }
    BaseEvaluation {
        decision: sub,
        missing_requirements: Vec::new(),
    }
}

fn web_fetch_evaluation(
    permissions: &Permissions,
    mode: AgentMode,
    args: &HashMap<String, Value>,
) -> BaseEvaluation {
    let tool = permissions.check_tool(mode.clone(), "web_fetch");
    if tool == Decision::Deny {
        return BaseEvaluation {
            decision: Decision::Deny,
            missing_requirements: Vec::new(),
        };
    }

    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    let pattern = permissions.check_subcommand(mode, "web_fetch", url);
    if pattern == Decision::Deny || pattern == Decision::Allow {
        return BaseEvaluation {
            decision: pattern,
            missing_requirements: Vec::new(),
        };
    }
    BaseEvaluation {
        decision: Decision::Ask,
        missing_requirements: vec![PermissionRequirement::Command {
            tool: "web_fetch".to_string(),
            command: url.to_string(),
        }],
    }
}

fn read_only_adjusted_decision(
    behavior: &ModeBehavior,
    base: Decision,
    effects: &[ToolEffect],
) -> Decision {
    if !behavior.read_only || base == Decision::Deny {
        return base;
    }

    match read_only_disposition(effects) {
        ReadOnlyDisposition::Deny => Decision::Deny,
        ReadOnlyDisposition::Ask if base == Decision::Allow => Decision::Ask,
        _ => base,
    }
}

fn request_requirement_satisfied(
    requirement: &PermissionRequirement,
    mode: &AgentMode,
    tool_name: &str,
    effects: &[ToolEffect],
    approvals: &RuntimeApprovals,
) -> bool {
    if approvals.requirement_satisfied(requirement) {
        return true;
    }

    let PermissionRequirement::PathPrefix { dir } = requirement else {
        return false;
    };

    effects
        .iter()
        .any(|effect| effect_path_grant_satisfied(effect, mode, tool_name, dir, approvals))
}

fn effect_path_grant_satisfied(
    effect: &ToolEffect,
    mode: &AgentMode,
    tool_name: &str,
    dir: &std::path::Path,
    approvals: &RuntimeApprovals,
) -> bool {
    match effect {
        ToolEffect::Fs(path) => path_grant_satisfied(path, mode, tool_name, dir, approvals),
        ToolEffect::Shell { paths, .. } => paths
            .iter()
            .any(|path| path_grant_satisfied(path, mode, tool_name, dir, approvals)),
        _ => false,
    }
}

fn path_grant_satisfied(
    path: &PathEffect,
    mode: &AgentMode,
    tool_name: &str,
    dir: &std::path::Path,
    approvals: &RuntimeApprovals,
) -> bool {
    if path.access == PathAccess::Unknown {
        return false;
    }
    let effect_dir = display_dir_for_effect(path);
    workspace::paths_equivalent(&effect_dir, dir)
        && approvals.session_path_grant_approved_for_path(
            mode,
            tool_name,
            &path.access,
            &effect_dir,
        )
}

fn read_only_session_write_approved(
    mode: &AgentMode,
    tool_name: &str,
    effects: &[ToolEffect],
    approvals: &RuntimeApprovals,
) -> bool {
    let mut saw_write = false;
    for effect in effects {
        match effect {
            ToolEffect::Fs(path) if path.access == PathAccess::Write => {
                saw_write = true;
                if !approvals.session_path_grant_approved_for_path(
                    mode,
                    tool_name,
                    &PathAccess::Write,
                    &display_dir_for_effect(path),
                ) {
                    return false;
                }
            }
            ToolEffect::FsAccess(PathAccess::Write)
            | ToolEffect::Shell {
                risk: ShellRisk::Writes | ShellRisk::Destructive,
                ..
            }
            | ToolEffect::ProcessControl
            | ToolEffect::ConfigReload => return false,
            _ => {}
        }
    }
    saw_write
}

fn read_only_disposition(effects: &[ToolEffect]) -> ReadOnlyDisposition {
    effects
        .iter()
        .map(effect_read_only_disposition)
        .fold(ReadOnlyDisposition::Read, ReadOnlyDisposition::merge)
}

fn effect_read_only_disposition(effect: &ToolEffect) -> ReadOnlyDisposition {
    match effect {
        ToolEffect::Fs(path) => path_access_disposition(&path.access),
        ToolEffect::FsAccess(access) => path_access_disposition(access),
        ToolEffect::Shell { risk, paths, .. } => {
            let path_disposition = paths
                .iter()
                .map(|path| path_access_disposition(&path.access))
                .fold(ReadOnlyDisposition::Read, ReadOnlyDisposition::merge);
            shell_risk_disposition(risk).merge(path_disposition)
        }
        ToolEffect::Network | ToolEffect::Unknown => ReadOnlyDisposition::Ask,
        ToolEffect::Mcp { tool } => mcp_tool_name_disposition(tool),
        ToolEffect::UserInteraction | ToolEffect::ProcessRead => ReadOnlyDisposition::Read,
        ToolEffect::ProcessControl | ToolEffect::ConfigReload => ReadOnlyDisposition::Deny,
    }
}

fn path_access_disposition(access: &PathAccess) -> ReadOnlyDisposition {
    match access {
        PathAccess::Read => ReadOnlyDisposition::Read,
        PathAccess::Write => ReadOnlyDisposition::Deny,
        PathAccess::Unknown => ReadOnlyDisposition::Ask,
    }
}

fn shell_risk_disposition(risk: &ShellRisk) -> ReadOnlyDisposition {
    match risk {
        ShellRisk::ReadOnly => ReadOnlyDisposition::Read,
        ShellRisk::Unknown => ReadOnlyDisposition::Ask,
        ShellRisk::Writes | ShellRisk::Destructive => ReadOnlyDisposition::Deny,
    }
}

fn mcp_tool_name_disposition(name: &str) -> ReadOnlyDisposition {
    let lower = name.to_ascii_lowercase();
    let write_words = [
        "write", "edit", "create", "delete", "remove", "update", "patch", "rename", "move",
        "mkdir", "rmdir",
    ];
    if write_words.iter().any(|word| lower.contains(word)) {
        ReadOnlyDisposition::Deny
    } else {
        ReadOnlyDisposition::Ask
    }
}

#[cfg(test)]
fn decide_base(
    permissions: &Permissions,
    mode: AgentMode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    base_evaluation(
        permissions,
        &PermissionRequest {
            mode,
            tool_name,
            args,
            origin: ToolOrigin::Lua,
            effects: permissions.effects_for_tool(ToolOrigin::Lua, tool_name, args),
        },
    )
    .decision
}

/// Shell-aware decision: splits on operators, folds subcommands to the worst decision,
/// trusts `cd` unconditionally.
pub fn shell_parser_decide(rs: &RuleSet, command: &str, _mode: AgentMode) -> Decision {
    let command = command.trim();
    let subcmds = split_shell_commands(command);
    if subcmds.len() <= 1 {
        if is_cd_command(command) {
            return Decision::Allow;
        }
        return check_ruleset(rs, command);
    }
    let mut worst = Decision::Allow;
    for subcmd in subcmds {
        if is_cd_command(&subcmd) {
            continue;
        }
        let d = check_ruleset(rs, &subcmd);
        match d {
            Decision::Deny => return Decision::Deny,
            Decision::Ask if worst == Decision::Allow => worst = Decision::Ask,
            _ => {}
        }
    }
    worst
}

pub fn shell_has_output_redirection(command: &str) -> bool {
    split_shell_commands(command)
        .into_iter()
        .any(|cmd| has_output_redirection(&cmd))
}

/// Look up a built-in subpattern parser by name. Currently only `"shell"`.
pub fn builtin_subpattern_parser(kind: &str) -> Option<Arc<SubpatternParserFn>> {
    match kind {
        "shell" => Some(Arc::new(shell_parser_decide)),
        _ => None,
    }
}
