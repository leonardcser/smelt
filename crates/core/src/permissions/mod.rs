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
#[cfg(test)]
use rules::compile_patterns;
use rules::{build_mode, check_ruleset, merge_mode, ModePerms, RawConfig, RawPerms, RuleSet};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use workspace::{any_outside_workspace, is_in_workspace};

/// Maps `(tool_name, args)` to filesystem paths the call would touch.
/// Tools that don't touch paths don't register one; the workspace check short-circuits.
pub type PathsFn = dyn Fn(&str, &HashMap<String, Value>) -> Vec<String> + Send + Sync;

/// Per-tool decision override. `Some(decision)` skips the generic `check_tool` path.
pub type DecideFn =
    dyn Fn(&str, &HashMap<String, Value>, AgentMode) -> Option<Decision> + Send + Sync;

pub use rules::ModeBehavior;

#[derive(Clone)]
pub struct Permissions {
    modes: HashMap<String, ModePerms>,
    mode_behaviors: HashMap<String, ModeBehavior>,
    restrict_to_workspace: bool,
    workspace: PathBuf,
    paths_fn: Option<Arc<PathsFn>>,
    decide_hook_fn: Option<Arc<DecideFn>>,
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
                "decide_hook_fn",
                &self.decide_hook_fn.as_ref().map(|_| "<fn>"),
            )
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
            decide_hook_fn: None,
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
    #[cfg(test)]
    pub(crate) fn with_overrides(&self, overrides: &protocol::PermissionOverrides) -> Self {
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

    pub fn set_decide_hook_fn(&mut self, f: Arc<DecideFn>) {
        self.decide_hook_fn = Some(f);
    }

    fn decide_hook(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
        mode: AgentMode,
    ) -> Option<Decision> {
        self.decide_hook_fn
            .as_ref()
            .and_then(|f| f(tool_name, args, mode))
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

    /// Full decision including workspace restriction. MCP calls route through the `mcp` bucket.
    pub fn decide(
        &self,
        mode: AgentMode,
        tool_name: &str,
        args: &HashMap<String, Value>,
        is_mcp: bool,
    ) -> Decision {
        let base = if is_mcp {
            self.check_subcommand(mode.clone(), "mcp", tool_name)
        } else {
            decide_base(self, mode.clone(), tool_name, args)
        };
        if base == Decision::Allow
            && self.restrict_to_workspace
            && !self.workspace.as_os_str().is_empty()
            && any_outside_workspace(&self.paths_for_tool(tool_name, args), &self.workspace)
        {
            return Decision::Ask;
        }
        base
    }

    /// `true` when the base decision is Allow but was downgraded to Ask solely by workspace paths.
    pub fn was_downgraded(
        &self,
        mode: AgentMode,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> bool {
        let base = decide_base(self, mode, tool_name, args);
        base == Decision::Allow
            && self.restrict_to_workspace
            && !self.workspace.as_os_str().is_empty()
            && any_outside_workspace(&self.paths_for_tool(tool_name, args), &self.workspace)
    }

    pub fn outside_workspace_paths(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> Vec<String> {
        if !self.restrict_to_workspace || self.workspace.as_os_str().is_empty() {
            return vec![];
        }
        self.paths_for_tool(tool_name, args)
            .into_iter()
            .filter(|p| !is_in_workspace(p, &self.workspace))
            .collect()
    }
}

fn decide_base(
    permissions: &Permissions,
    mode: AgentMode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    if let Some(d) = permissions.decide_hook(tool_name, args, mode.clone()) {
        return d;
    }
    permissions.check_tool(mode, tool_name)
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
