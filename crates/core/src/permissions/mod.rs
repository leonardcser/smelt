//! Permission policy for tool calls.

pub(crate) mod approvals;
pub(crate) mod bash;
pub mod rules;
mod shell_parse;
pub mod store;
pub(crate) mod workspace;

#[cfg(test)]
mod tests;

pub use approvals::{RuntimeApprovals, SessionPathGrant, SessionToolApproval};
pub use bash::{split_shell_commands, split_shell_commands_with_ops};
pub use protocol::Decision;
pub use rules::{SubpatternParserFn, ToolDefaults, ToolEffectKind};

use bash::{has_output_redirection, is_cd_command};

use protocol::AgentMode;
use rules::{
    build_mode, check_ruleset, check_ruleset_match, compile_patterns, merge_mode, ModePerms,
    RawConfig, RawPerms, RuleSet, ToolPermDefaults,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
pub enum PathResolution {
    Resolved(PathBuf),
    Unresolved(PathBuf),
}

impl PathResolution {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Resolved(path) | Self::Unresolved(path) => path,
        }
    }

    pub fn resolved(&self) -> Option<&std::path::Path> {
        match self {
            Self::Resolved(path) => Some(path),
            Self::Unresolved(_) => None,
        }
    }

    pub fn is_resolved(&self) -> bool {
        matches!(self, Self::Resolved(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEffect {
    pub raw_path: String,
    pub base_dir: PathBuf,
    pub resolution: PathResolution,
    pub access: PathAccess,
    pub target_kind: PathTargetKind,
}

impl PathEffect {
    pub(super) fn from_tool_path(
        tool_path: ToolPath,
        base_dir: &std::path::Path,
        access: PathAccess,
    ) -> Self {
        let resolution = workspace::resolve_tool_path(&tool_path.path, base_dir);
        Self::new(
            tool_path.path,
            base_dir,
            resolution,
            access,
            tool_path.target_kind,
        )
    }

    pub(super) fn from_shell_path(
        raw_path: String,
        expanded_path: Option<&str>,
        cwd: &PathResolution,
        access: PathAccess,
        target_kind: PathTargetKind,
    ) -> Self {
        let resolution = workspace::resolve_shell_path(&raw_path, expanded_path, cwd);
        Self::new(raw_path, cwd.path(), resolution, access, target_kind)
    }

    fn new(
        raw_path: String,
        base_dir: &std::path::Path,
        resolution: PathResolution,
        access: PathAccess,
        target_kind: PathTargetKind,
    ) -> Self {
        Self {
            raw_path,
            base_dir: base_dir.to_path_buf(),
            resolution,
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
pub struct OpaqueShellCommand {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEffect {
    Fs(PathEffect),
    FsAccess(PathAccess),
    Shell {
        command: String,
        risk: ShellRisk,
        paths: Vec<PathEffect>,
        opaque_commands: Vec<OpaqueShellCommand>,
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
    OpaqueCommand { tool: String, command: String },
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
                PermissionRequirement::Command { tool, command }
                | PermissionRequirement::OpaqueCommand { tool, command, .. },
            ) => {
                if grant_tool != tool {
                    return false;
                }
                let Ok(pattern) = glob::Pattern::new(pattern) else {
                    return false;
                };
                command_patterns_satisfy(tool, &[&pattern], command, None)
            }
            (
                PermissionGrant::PathPrefix { dir },
                PermissionRequirement::PathPrefix { dir: path },
            ) => workspace::path_prefix_matches(dir, path),
            _ => false,
        }
    }

    pub fn display_subject(&self, home: &Path) -> String {
        match self {
            PermissionGrant::Tool { tool } => tool.clone(),
            PermissionGrant::Command { pattern, .. } => {
                let display = pattern.strip_suffix("/*").unwrap_or(pattern);
                display
                    .split_once("://")
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_else(|| display.to_string())
            }
            PermissionGrant::PathPrefix { dir } => engine::paths::collapse_tilde_from(dir, home)
                .to_string_lossy()
                .into_owned(),
        }
    }

    pub fn display_subjects(grants: &[PermissionGrant], home: &Path) -> String {
        let mut command_head = None;
        grants
            .iter()
            .map(|grant| {
                let subject = grant.display_subject(home);
                match grant {
                    PermissionGrant::Command { .. } => {
                        if let Some(head) = &command_head {
                            subject
                                .strip_prefix(head)
                                .and_then(|rest| {
                                    let separator =
                                        smelt_buffer::cell_width::graphemes(rest).next()?;
                                    (separator == " ").then(|| &rest[separator.len()..])
                                })
                                .unwrap_or(&subject)
                                .to_string()
                        } else {
                            let head_end = smelt_buffer::cell_width::grapheme_indices(&subject)
                                .find(|(_, grapheme)| grapheme.chars().all(char::is_whitespace))
                                .map(|(start, _)| start)
                                .unwrap_or(subject.len());
                            command_head = (head_end > 0).then(|| subject[..head_end].to_string());
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
    active_root: PathBuf,
    home: PathBuf,
    allowed_roots: Vec<PathBuf>,
    paths_fn: Option<Arc<PathsFn>>,
    tool_decisions: HashMap<String, ToolPermDefaults>,
    tool_effects: HashMap<String, ToolEffectKind>,
    subpattern_parsers: HashMap<String, Arc<SubpatternParserFn>>,
    /// Session approvals are shared across immutable policy snapshots. Static
    /// rules and workspace roots are snapshotted; approvals intentionally stay
    /// live for an active turn.
    pub approvals: Arc<RwLock<RuntimeApprovals>>,
}

/// Live owner for the current permission policy.
///
/// Long-lived consumers snapshot this handle when they evaluate a request.
/// Active turns keep the returned [`Arc<Permissions>`], so later policy reloads
/// affect future turns while grants added to the shared approval store remain
/// visible immediately.
#[derive(Clone)]
pub struct PermissionsHandle {
    current: Arc<RwLock<Arc<Permissions>>>,
    approvals: Arc<RwLock<RuntimeApprovals>>,
}

/// Complete read model for the current session and persisted permission scopes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSnapshot {
    pub session_tools: Vec<SessionToolApproval>,
    pub session_dirs: Vec<PathBuf>,
    pub session_path_grants: Vec<SessionPathGrant>,
    pub workspace: store::Snapshot,
    pub repository: store::Snapshot,
}

impl std::fmt::Debug for PermissionsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionsHandle")
            .field("current", &self.snapshot())
            .finish()
    }
}

impl PermissionsHandle {
    pub fn new(mut permissions: Permissions) -> Self {
        let approvals = Arc::clone(&permissions.approvals);
        permissions.approvals = Arc::clone(&approvals);
        Self {
            current: Arc::new(RwLock::new(Arc::new(permissions))),
            approvals,
        }
    }

    pub fn snapshot(&self) -> Arc<Permissions> {
        self.current
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn replace(&self, mut permissions: Permissions) -> Arc<Permissions> {
        permissions.approvals = Arc::clone(&self.approvals);
        let permissions = Arc::new(permissions);
        *self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Arc::clone(&permissions);
        permissions
    }

    pub fn approvals(&self) -> Arc<RwLock<RuntimeApprovals>> {
        Arc::clone(&self.approvals)
    }

    pub fn paths_fn(&self) -> Option<Arc<PathsFn>> {
        self.snapshot().paths_fn.clone()
    }

    pub(crate) fn install_home(&self, home: PathBuf) {
        let mut permissions = self.snapshot().as_ref().clone();
        permissions.set_home(home);
        self.replace(permissions);
    }

    pub fn check_tool(&self, mode: AgentMode, tool_name: &str) -> Decision {
        self.snapshot().check_tool(mode, tool_name)
    }

    pub fn check_subcommand(&self, mode: AgentMode, bucket: &str, value: &str) -> Decision {
        self.snapshot().check_subcommand(mode, bucket, value)
    }

    pub fn evaluate_tool(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> PermissionOutcome {
        self.snapshot().evaluate_tool(mode, origin, tool_name, args)
    }

    pub fn evaluate_tool_with_paths(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: &[ToolPath],
    ) -> PermissionOutcome {
        self.snapshot()
            .evaluate_tool_with_paths(mode, origin, tool_name, args, paths)
    }

    pub fn evaluate_tool_with_approvals(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> PermissionOutcome {
        self.snapshot()
            .evaluate_tool_with_approvals(mode, origin, tool_name, args)
    }

    pub fn evaluate_tool_with_paths_and_approvals(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: &[ToolPath],
    ) -> PermissionOutcome {
        self.snapshot()
            .evaluate_tool_with_paths_and_approvals(mode, origin, tool_name, args, paths)
    }

    pub fn from_resolution(resolution: PermissionResolution) -> Self {
        let PermissionResolution {
            policy,
            workspace_approvals,
            repository_approvals,
            permission_store,
            workspace,
            repository_key,
        } = resolution;
        let handle = Self::new(policy);
        handle.install_persisted_approvals(
            workspace_approvals,
            repository_approvals,
            permission_store,
            workspace,
            repository_key,
        );
        handle
    }

    pub fn apply_resolution(&self, resolution: PermissionResolution) -> Arc<Permissions> {
        let PermissionResolution {
            mut policy,
            workspace_approvals,
            repository_approvals,
            permission_store,
            workspace,
            repository_key,
        } = resolution;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(|error| error.into_inner());
        self.install_persisted_approvals(
            workspace_approvals,
            repository_approvals,
            permission_store,
            workspace,
            repository_key,
        );
        policy.approvals = Arc::clone(&self.approvals);
        let permissions = Arc::new(policy);
        *current = Arc::clone(&permissions);
        permissions
    }

    fn install_persisted_approvals(
        &self,
        workspace_approvals: store::CompiledApprovals,
        repository_approvals: store::CompiledApprovals,
        permission_store: store::PermissionStore,
        workspace: PathBuf,
        repository_key: Option<PathBuf>,
    ) {
        let mut approvals = self
            .approvals
            .write()
            .unwrap_or_else(|error| error.into_inner());
        approvals.load_workspace(workspace_approvals.tools, workspace_approvals.dirs);
        approvals.load_repository(repository_approvals.tools, repository_approvals.dirs);
        approvals.configure_persisted_source(permission_store, workspace, repository_key);
    }
}

pub(crate) fn read_snapshot(
    permissions: &PermissionsHandle,
    permission_store: &store::PermissionStore,
    cwd: &Path,
    worktree_root: Option<&Path>,
) -> Result<PermissionSnapshot, String> {
    let (session_tools, session_dirs, session_path_grants) = {
        let approvals = permissions.approvals();
        let approvals = approvals.read().unwrap_or_else(|error| error.into_inner());
        (
            approvals.session_tool_approvals(),
            approvals.session_dirs().to_vec(),
            approvals.session_path_grants(),
        )
    };

    let workspace_key = cwd.to_string_lossy();
    let workspace = permission_store
        .load_snapshot(workspace_key.as_ref(), store::PersistenceScope::Workspace)
        .map_err(|error| format!("load workspace permissions: {error}"))?;
    let project = crate::worktree::project_context(cwd, worktree_root);
    let repository = match project.repository_key.as_deref() {
        Some(key) => permission_store
            .load_snapshot(
                key.to_string_lossy().as_ref(),
                store::PersistenceScope::Repository,
            )
            .map_err(|error| format!("load repository permissions: {error}"))?,
        None => Default::default(),
    };

    Ok(PermissionSnapshot {
        session_tools,
        session_dirs,
        session_path_grants,
        workspace,
        repository,
    })
}

/// A fully resolved static policy plus the workspace grants that become live
/// with it. Building this value does not mutate the active permission handle.
pub struct PermissionResolution {
    policy: Permissions,
    workspace_approvals: store::CompiledApprovals,
    repository_approvals: store::CompiledApprovals,
    permission_store: store::PermissionStore,
    workspace: PathBuf,
    repository_key: Option<PathBuf>,
}

/// Runtime-owned paths used while resolving permission policy.
#[derive(Clone, Copy)]
pub struct PermissionRuntimePaths<'a> {
    pub cwd: &'a std::path::Path,
    pub home: &'a std::path::Path,
}

/// Resolve permission declarations, setting-controlled workspace policy, cwd
/// roots, and persisted workspace grants through one path.
pub fn resolve_permissions(
    raw: &RawPerms,
    tool_defaults: &ToolDefaults,
    mode_behaviors: HashMap<String, ModeBehavior>,
    settings: &crate::config::ResolvedSettings,
    runtime_paths: PermissionRuntimePaths<'_>,
    permission_store: &store::PermissionStore,
    paths_fn: Option<Arc<PathsFn>>,
) -> std::io::Result<PermissionResolution> {
    let PermissionRuntimePaths { cwd, home } = runtime_paths;
    let context =
        crate::worktree::project_context(cwd, Some(std::path::Path::new(&settings.worktree_root)));
    let repository_key = context.repository_key.clone();
    let mut policy = Permissions::from_raw_with_mode_behaviors(raw, tool_defaults, mode_behaviors);
    policy.set_allowed_roots(context.active_root, context.allowed_roots);
    policy.set_home(home.to_path_buf());
    policy.set_restrict_to_workspace(settings.restrict_to_workspace);
    if let Some(paths_fn) = paths_fn {
        policy.set_paths_fn(paths_fn);
    }
    let workspace_approvals = permission_store
        .load_approvals(&cwd.to_string_lossy(), store::PersistenceScope::Workspace)?;
    let repository_approvals = if let Some(repository_key) = repository_key.as_deref() {
        permission_store.load_approvals(
            &repository_key.to_string_lossy(),
            store::PersistenceScope::Repository,
        )?
    } else {
        store::CompiledApprovals::default()
    };
    Ok(PermissionResolution {
        policy,
        workspace_approvals,
        repository_approvals,
        permission_store: permission_store.clone(),
        workspace: cwd.to_path_buf(),
        repository_key,
    })
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
            .field("active_root", &self.active_root)
            .field("home", &self.home)
            .field("allowed_roots", &self.allowed_roots)
            .field("paths_fn", &self.paths_fn.as_ref().map(|_| "<fn>"))
            .field("tool_decisions", &self.tool_decisions)
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
            active_root: PathBuf::new(),
            home: engine::paths::home_dir(),
            allowed_roots: Vec::new(),
            paths_fn: None,
            tool_decisions: tool_defaults.tool_decisions.clone(),
            tool_effects: tool_defaults.tool_effects.clone(),
            subpattern_parsers: tool_defaults.subpattern_parsers.clone(),
            approvals: Arc::new(RwLock::new(RuntimeApprovals::new())),
        }
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
                let entry = mode.patterns.entry(bucket.clone()).or_insert(RuleSet {
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
        self.active_root = path.clone();
        self.allowed_roots = vec![path];
    }

    fn set_home(&mut self, home: PathBuf) {
        self.home = home;
    }

    pub fn set_allowed_roots(&mut self, active: PathBuf, roots: Vec<PathBuf>) {
        self.active_root = active.clone();
        self.allowed_roots.clear();
        for root in std::iter::once(active).chain(roots) {
            if !self
                .allowed_roots
                .iter()
                .any(|existing| workspace::paths_equivalent(existing, &root))
            {
                self.allowed_roots.push(root);
            }
        }
    }

    pub fn set_restrict_to_workspace(&mut self, val: bool) {
        self.restrict_to_workspace = val;
    }

    pub fn restrict_to_workspace(&self) -> bool {
        self.restrict_to_workspace
    }

    pub fn set_paths_fn(&mut self, f: Arc<PathsFn>) {
        self.paths_fn = Some(f);
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
            .unwrap_or(ToolEffectKind::Other)
    }

    fn declared_effects_for_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: Option<&[ToolPath]>,
    ) -> Vec<ToolEffect> {
        match self.tool_effect_kind(tool_name) {
            ToolEffectKind::Read => {
                self.path_effects_for_tool(tool_name, args, paths, PathAccess::Read)
            }
            ToolEffectKind::Write => {
                self.path_effects_for_tool(tool_name, args, paths, PathAccess::Write)
            }
            ToolEffectKind::Network => vec![ToolEffect::Network],
            ToolEffectKind::User => vec![ToolEffect::UserInteraction],
            ToolEffectKind::Process => vec![ToolEffect::ProcessControl],
            ToolEffectKind::Config => vec![ToolEffect::ConfigReload],
            ToolEffectKind::Other => vec![ToolEffect::Unknown],
        }
    }

    fn path_effects_for_tool(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: Option<&[ToolPath]>,
        access: PathAccess,
    ) -> Vec<ToolEffect> {
        let paths = paths
            .map(|paths| paths.to_vec())
            .unwrap_or_else(|| self.paths_for_tool(tool_name, args));
        let effects: Vec<_> = paths
            .into_iter()
            .map(|path| {
                ToolEffect::Fs(PathEffect::from_tool_path(
                    path,
                    &self.active_root,
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
        self.effects_for_tool_with_optional_paths(origin, tool_name, args, None)
    }

    pub fn effects_for_tool_with_paths(
        &self,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: &[ToolPath],
    ) -> Vec<ToolEffect> {
        self.effects_for_tool_with_optional_paths(origin, tool_name, args, Some(paths))
    }

    fn effects_for_tool_with_optional_paths(
        &self,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: Option<&[ToolPath]>,
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
                let analysis =
                    bash::analyze_shell_command_in(&command, &self.active_root, &self.home);
                let mut effects = vec![ToolEffect::Shell {
                    command,
                    risk: analysis.risk,
                    paths: analysis.paths,
                    opaque_commands: analysis.opaque_commands,
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
            _ => self.declared_effects_for_tool(tool_name, args, paths),
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

    fn outside_workspace_requirements(
        &self,
        mode: &AgentMode,
        effects: &[ToolEffect],
    ) -> Vec<PermissionRequirement> {
        if !self.restrict_to_workspace || self.active_root.as_os_str().is_empty() {
            return vec![];
        }
        let roots = if self.allowed_roots.is_empty() {
            vec![self.active_root.clone()]
        } else {
            self.allowed_roots.clone()
        };
        let allowed_roots: Vec<_> = roots
            .iter()
            .map(|root| workspace::resolve_filesystem_path(root))
            .collect();
        let mut paths = Vec::new();
        Self::effect_paths(effects, &mut paths);
        let mut out = Vec::new();
        for effect in paths {
            if effect.resolution.resolved().is_some_and(|path| {
                allowed_roots
                    .iter()
                    .filter_map(PathResolution::resolved)
                    .any(|root| path.starts_with(root))
            }) {
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

        let rules = self.subcommand_ruleset(mode.clone(), "bash");
        for effect in effects {
            let ToolEffect::Shell {
                opaque_commands, ..
            } = effect
            else {
                continue;
            };
            for opaque in opaque_commands {
                if rules
                    .is_some_and(|rules| check_ruleset(rules, &opaque.command) == Decision::Allow)
                {
                    continue;
                }
                let req = PermissionRequirement::OpaqueCommand {
                    tool: "bash".to_string(),
                    command: opaque.command.clone(),
                };
                if !out.contains(&req) {
                    out.push(req);
                }
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
        self.mode_perms(&mode)
            .and_then(|perms| perms.tools.get(tool_name).cloned())
            .unwrap_or_else(|| self.fallback_tool_decision(&mode, tool_name))
    }

    fn fallback_tool_decision(&self, mode: &AgentMode, tool_name: &str) -> Decision {
        self.effect_decision(mode, self.tool_effect_kind(tool_name))
            .or_else(|| {
                self.tool_decisions
                    .get(tool_name)
                    .and_then(|perms| perms.for_mode(mode).cloned())
            })
            .unwrap_or_else(|| self.mode_behavior(mode).default_decision)
    }

    fn effect_decision(&self, mode: &AgentMode, effect: ToolEffectKind) -> Option<Decision> {
        let effects = &self.mode_perms(mode)?.effects;
        match effect {
            ToolEffectKind::Read => effects.read.clone(),
            ToolEffectKind::Write => effects.write.clone(),
            ToolEffectKind::Network => effects.network.clone(),
            ToolEffectKind::Process => effects.process.clone(),
            ToolEffectKind::Config => effects.config.clone(),
            ToolEffectKind::User => effects.user.clone(),
            ToolEffectKind::Other => effects.other.clone(),
        }
    }

    fn shell_effect_decision(
        &self,
        mode: &AgentMode,
        effects: &[ToolEffect],
        background: bool,
    ) -> Option<Decision> {
        if background {
            return self.effect_decision(mode, ToolEffectKind::Process);
        }
        let (risk, paths) = effects.iter().find_map(|effect| match effect {
            ToolEffect::Shell { risk, paths, .. } => Some((risk, paths)),
            _ => None,
        })?;
        let writes_path = paths.iter().any(|path| path.access == PathAccess::Write);
        let effect = match risk {
            ShellRisk::ReadOnly if !writes_path => ToolEffectKind::Read,
            ShellRisk::ReadOnly | ShellRisk::Writes | ShellRisk::Destructive => {
                ToolEffectKind::Write
            }
            ShellRisk::Unknown if writes_path => ToolEffectKind::Write,
            ShellRisk::Unknown => ToolEffectKind::Other,
        };
        self.effect_decision(mode, effect)
    }

    pub fn subcommand_ruleset(&self, mode: AgentMode, bucket: &str) -> Option<&RuleSet> {
        self.mode_perms(&mode)?.patterns.get(bucket)
    }

    /// Check `value` against the bucket's ruleset. Custom parsers (e.g. `bash`'s shell parser)
    /// run when registered; otherwise plain glob-match. Falls back to the active mode's default
    /// decision when no bucket is registered.
    pub fn check_subcommand(&self, mode: AgentMode, bucket: &str, value: &str) -> Decision {
        let behavior = self.mode_behavior(&mode);
        self.check_pattern(mode, bucket, value)
            .unwrap_or(behavior.default_decision)
    }

    fn check_pattern(&self, mode: AgentMode, bucket: &str, value: &str) -> Option<Decision> {
        let behavior = self.mode_behavior(&mode);
        let rs = self.subcommand_ruleset(mode.clone(), bucket)?;
        if let Some(parser) = self.subpattern_parsers.get(bucket) {
            let decision = parser(rs, value, mode);
            if decision == Decision::Ask && behavior.allow_subcommands_by_default {
                Some(Decision::Allow)
            } else {
                Some(decision)
            }
        } else {
            let decision = check_ruleset_match(rs, value)?;
            if decision == Decision::Ask && behavior.allow_subcommands_by_default {
                Some(Decision::Allow)
            } else {
                Some(decision)
            }
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
        self.evaluate_tool_effects(mode, origin, tool_name, args, effects)
    }

    pub fn evaluate_tool_with_paths(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: &[ToolPath],
    ) -> PermissionOutcome {
        let effects = self.effects_for_tool_with_paths(origin.clone(), tool_name, args, paths);
        self.evaluate_tool_effects(mode, origin, tool_name, args, effects)
    }

    fn evaluate_tool_effects(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        effects: Vec<ToolEffect>,
    ) -> PermissionOutcome {
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
        self.evaluate_tool_effects_with_approvals(mode, origin, tool_name, args, effects)
    }

    pub fn evaluate_tool_with_paths_and_approvals(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        paths: &[ToolPath],
    ) -> PermissionOutcome {
        let effects = self.effects_for_tool_with_paths(origin.clone(), tool_name, args, paths);
        self.evaluate_tool_effects_with_approvals(mode, origin, tool_name, args, effects)
    }

    fn evaluate_tool_effects_with_approvals(
        &self,
        mode: AgentMode,
        origin: ToolOrigin,
        tool_name: &str,
        args: &HashMap<String, Value>,
        effects: Vec<ToolEffect>,
    ) -> PermissionOutcome {
        let mut outcome = self.evaluate_request(PermissionRequest {
            mode: mode.clone(),
            tool_name,
            args,
            origin,
            effects: effects.clone(),
        });
        let mut approvals = self
            .approvals
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let refresh_error = approvals.refresh_persisted().err();
        if outcome.decision == Decision::Ask {
            outcome.missing_requirements.retain(|req| {
                !request_requirement_satisfied(req, &mode, tool_name, &effects, &approvals)
            });
            if outcome.missing_requirements.is_empty() {
                outcome.decision = Decision::Allow;
            } else if let Some(error) = refresh_error {
                outcome.decision =
                    Decision::Error(format!("failed to refresh persisted permissions: {error}"));
            }
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
        let mut command_grants =
            self.approval_pattern_candidates(&approvals, tool_name, candidates);
        for requirement in &outcome.missing_requirements {
            let PermissionRequirement::OpaqueCommand { tool, command } = requirement else {
                continue;
            };
            let covered = command_grants.iter().any(|pattern| {
                glob::Pattern::new(pattern)
                    .is_ok_and(|pattern| rules::matches_rule(&pattern, command))
            });
            if tool == tool_name
                && !covered
                && !approvals.has_explicit_pattern(tool_name, command)
                && glob::Pattern::new(command).is_ok()
            {
                command_grants.push(command.clone());
            }
        }
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
                PermissionRequirement::Command { .. }
                | PermissionRequirement::OpaqueCommand { .. } => {}
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
        let base = base_evaluation(self, &request);
        if base.decision == Decision::Deny {
            return PermissionOutcome {
                decision: Decision::Deny,
                missing_requirements: Vec::new(),
            };
        }

        let mut missing = base.missing_requirements;
        if base.decision == Decision::Ask && missing.is_empty() {
            missing.push(PermissionRequirement::Tool {
                tool: request.tool_name.to_string(),
            });
        }
        missing.extend(self.outside_workspace_requirements(&request.mode, &request.effects));
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

pub(super) fn command_patterns_satisfy(
    tool_name: &str,
    patterns: &[&glob::Pattern],
    command: &str,
    config_subpatterns: Option<&RuleSet>,
) -> bool {
    if patterns.is_empty() && config_subpatterns.is_none() {
        return false;
    }

    if tool_name != "bash" {
        return command_text_satisfied(command, patterns, config_subpatterns);
    }

    let subcommands = split_shell_commands(command);
    if subcommands.is_empty() {
        return false;
    }

    let mut checked_command = false;
    for subcommand in subcommands {
        if is_cd_command(&subcommand) {
            continue;
        }
        checked_command = true;
        if !command_text_satisfied(&subcommand, patterns, config_subpatterns) {
            return false;
        }
    }
    checked_command
}

fn command_text_satisfied(
    command: &str,
    patterns: &[&glob::Pattern],
    config_subpatterns: Option<&RuleSet>,
) -> bool {
    patterns
        .iter()
        .any(|pattern| rules::matches_rule(pattern, command))
        || config_subpatterns.is_some_and(|rules| check_ruleset(rules, command) == Decision::Allow)
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
    let path = effect.resolution.path();
    if !effect.resolution.is_resolved() {
        let absolute = if path.is_absolute() {
            path
        } else {
            &effect.base_dir
        };
        return absolute
            .ancestors()
            .last()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
    }
    if effect.target_kind == PathTargetKind::Directory || path.is_dir() {
        return path.to_path_buf();
    }
    path.parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
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
            "bash" => bash_evaluation(
                permissions,
                request.mode.clone(),
                request.args,
                &request.effects,
            ),
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
    effects: &[ToolEffect],
) -> BaseEvaluation {
    let tool = permissions.check_tool(mode.clone(), "bash");
    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    let pattern = permissions
        .subcommand_ruleset(mode.clone(), "bash")
        .and_then(|rs| shell_parser_decide_match(rs, command));
    if pattern == Some(Decision::Deny) {
        return BaseEvaluation {
            decision: Decision::Deny,
            missing_requirements: Vec::new(),
        };
    }
    let background = args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let decision = pattern.unwrap_or_else(|| {
        if background {
            permissions
                .effect_decision(&mode, ToolEffectKind::Process)
                .unwrap_or(tool)
        } else {
            permissions
                .shell_effect_decision(&mode, effects, background)
                .unwrap_or(tool)
        }
    });
    let asks_for_command = decision == Decision::Ask
        || (decision == Decision::Allow
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
        decision,
        missing_requirements: Vec::new(),
    }
}

fn web_fetch_evaluation(
    permissions: &Permissions,
    mode: AgentMode,
    args: &HashMap<String, Value>,
) -> BaseEvaluation {
    let tool = permissions.check_tool(mode.clone(), "web_fetch");
    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    let decision = permissions
        .check_pattern(mode, "web_fetch", url)
        .unwrap_or(tool);
    if decision == Decision::Deny || decision == Decision::Allow {
        return BaseEvaluation {
            decision,
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
    shell_parser_decide_match(rs, command).unwrap_or(Decision::Ask)
}

fn shell_parser_decide_match(rs: &RuleSet, command: &str) -> Option<Decision> {
    let command = smelt_buffer::text::trim_whitespace(command);
    let subcmds = split_shell_commands(command);
    if subcmds.len() <= 1 {
        if is_cd_command(command) {
            return Some(Decision::Allow);
        }
        return check_ruleset_match(rs, command);
    }
    let mut worst = None;
    let mut unmatched = false;
    for subcmd in subcmds {
        if is_cd_command(&subcmd) {
            continue;
        }
        let Some(d) = check_ruleset_match(rs, &subcmd) else {
            unmatched = true;
            continue;
        };
        match d {
            Decision::Deny => return Some(Decision::Deny),
            Decision::Ask => worst = Some(Decision::Ask),
            Decision::Allow if worst.is_none() => worst = Some(Decision::Allow),
            _ => {}
        }
    }
    if unmatched && worst.is_some() && worst != Some(Decision::Ask) {
        Some(Decision::Ask)
    } else {
        worst
    }
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
