//! Permission policy for tool calls.

pub(crate) mod approvals;
pub(crate) mod bash;
pub mod rules;
pub mod store;
pub(crate) mod workspace;

#[cfg(test)]
mod tests;

pub use approvals::RuntimeApprovals;
pub use bash::{split_shell_commands, split_shell_commands_with_ops};
pub use protocol::Decision;
pub use rules::{SubpatternParserFn, ToolDefaults};

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
pub struct PathEffect {
    pub raw_path: String,
    pub base_dir: PathBuf,
    pub path: PathBuf,
    pub access: PathAccess,
}

impl PathEffect {
    pub(super) fn from_raw(
        raw_path: String,
        base_dir: &std::path::Path,
        access: PathAccess,
    ) -> Self {
        let path = workspace::resolve_path(&raw_path, base_dir);
        Self {
            raw_path,
            base_dir: base_dir.to_path_buf(),
            path,
            access,
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
pub struct PermissionOutcome {
    pub decision: Decision,
    pub outside_workspace_paths: Vec<String>,
    pub downgraded_by_workspace: bool,
}

/// Maps `(tool_name, args)` to filesystem paths the call would touch.
/// Tools that don't touch paths don't register one; the workspace check short-circuits.
pub type PathsFn = dyn Fn(&str, &HashMap<String, Value>) -> Vec<String> + Send + Sync;

pub use rules::ModeBehavior;

#[derive(Clone)]
pub struct Permissions {
    modes: HashMap<String, ModePerms>,
    mode_behaviors: HashMap<String, ModeBehavior>,
    restrict_to_workspace: bool,
    workspace: PathBuf,
    paths_fn: Option<Arc<PathsFn>>,
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

    fn paths_for_tool(&self, tool_name: &str, args: &HashMap<String, Value>) -> Vec<String> {
        match self.paths_fn.as_ref() {
            Some(f) => f(tool_name, args),
            None => Vec::new(),
        }
    }

    fn path_access_for_tool(tool_name: &str) -> PathAccess {
        match tool_name {
            "edit_file" | "write_file" | "edit_notebook" => PathAccess::Write,
            "bash" => PathAccess::Unknown,
            _ => PathAccess::Read,
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
                vec![ToolEffect::Shell {
                    command,
                    risk: analysis.risk,
                    paths: analysis.paths,
                }]
            }
            "web_fetch" | "web_search" => vec![ToolEffect::Network],
            "ask_user_question" => vec![ToolEffect::UserInteraction],
            "read_process_output" | "stop_process" => vec![ToolEffect::ProcessControl],
            "smelt_reload" => vec![ToolEffect::ConfigReload],
            _ => {
                let access = Self::path_access_for_tool(tool_name);
                let effects: Vec<_> = self
                    .paths_for_tool(tool_name, args)
                    .into_iter()
                    .map(|p| {
                        ToolEffect::Fs(PathEffect::from_raw(p, &self.workspace, access.clone()))
                    })
                    .collect();
                if effects.is_empty() {
                    vec![ToolEffect::Unknown]
                } else {
                    effects
                }
            }
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

    fn outside_workspace_effect_paths(&self, effects: &[ToolEffect]) -> Vec<String> {
        if !self.restrict_to_workspace || self.workspace.as_os_str().is_empty() {
            return vec![];
        }
        let workspace = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());
        let mut paths = Vec::new();
        Self::effect_paths(effects, &mut paths);
        paths
            .into_iter()
            .filter(|path| !path.path.starts_with(&workspace))
            .map(|path| path.raw_path.clone())
            .collect()
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

    pub fn evaluate_request(&self, request: PermissionRequest<'_>) -> PermissionOutcome {
        let base = match request.origin {
            ToolOrigin::Mcp => {
                self.check_subcommand(request.mode.clone(), "mcp", request.tool_name)
            }
            ToolOrigin::Lua | ToolOrigin::Core => {
                decide_base(self, request.mode.clone(), request.tool_name, request.args)
            }
        };
        let outside_workspace_paths = self.outside_workspace_effect_paths(&request.effects);
        let downgraded_by_workspace =
            base == Decision::Allow && !outside_workspace_paths.is_empty();
        let decision = if downgraded_by_workspace {
            Decision::Ask
        } else {
            base
        };
        PermissionOutcome {
            decision,
            outside_workspace_paths,
            downgraded_by_workspace,
        }
    }
}

fn decide_base(
    permissions: &Permissions,
    mode: AgentMode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    match tool_name {
        "bash" => decide_bash(permissions, mode, args),
        "web_fetch" => decide_web_fetch(permissions, mode, args),
        _ => permissions.check_tool(mode, tool_name),
    }
}

fn decide_bash(
    permissions: &Permissions,
    mode: AgentMode,
    args: &HashMap<String, Value>,
) -> Decision {
    let tool = permissions.check_tool(mode.clone(), "bash");
    if tool == Decision::Deny {
        return Decision::Deny;
    }

    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
    let sub = permissions.check_subcommand(mode.clone(), "bash", command);
    if sub == Decision::Deny {
        return Decision::Deny;
    }
    if tool == Decision::Allow && sub == Decision::Ask {
        return Decision::Ask;
    }
    if sub == Decision::Allow
        && permissions.mode_behavior(&mode).ask_on_output_redirection
        && shell_has_output_redirection(command)
    {
        return Decision::Ask;
    }
    sub
}

fn decide_web_fetch(
    permissions: &Permissions,
    mode: AgentMode,
    args: &HashMap<String, Value>,
) -> Decision {
    let tool = permissions.check_tool(mode.clone(), "web_fetch");
    if tool == Decision::Deny {
        return Decision::Deny;
    }

    let url = args.get("url").and_then(Value::as_str).unwrap_or("");
    let pattern = permissions.check_subcommand(mode, "web_fetch", url);
    if pattern == Decision::Deny || pattern == Decision::Allow {
        return pattern;
    }
    if tool == Decision::Allow && pattern == Decision::Ask {
        return Decision::Ask;
    }
    pattern
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
