//! Permission policy for tool calls.
//!
//! Layout:
//! - [`rules`]: rule-set types and pattern matching
//! - [`bash`]: shell command splitting / heredoc parsing / redirection detection
//! - [`workspace`]: path extraction + workspace boundary enforcement
//! - [`approvals`]: runtime auto-approval tracking
//! - [`store`]: workspace JSON store
//!
//! The public surface is this module: `Permissions`, `Decision`,
//! `RuntimeApprovals`, and two helpers consumed by tool implementations
//! (`split_shell_commands`, `DEFAULT_BASH_ALLOW`).

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
pub use rules::DEFAULT_BASH_ALLOW;

use bash::{has_output_redirection, is_cd_command};

use protocol::AgentMode;
#[cfg(test)]
use rules::compile_patterns;
use rules::{build_mode, check_ruleset, merge_mode, ModePerms, RawConfig, RawPerms, RuleSet};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use workspace::{any_outside_workspace, is_in_workspace};

/// Lookup "what filesystem paths does this tool call touch?" without
/// hardcoding tool names in Rust. Wired from each Lua tool's
/// `paths_for_workspace(args)` callback at startup; tools that don't
/// touch paths simply don't register one and the workspace-boundary
/// check short-circuits to "in".
pub type PathsFn = dyn Fn(&str, &HashMap<String, Value>) -> Vec<String> + Send + Sync;

/// Tool-decision override: `Some(decision)` skips Rust's generic
/// `check_tool` path. Wired from each Lua tool's `decide(args, mode)`
/// callback at startup; tools without one fall through to the generic
/// path. The bash + web_fetch tools register decide callbacks that
/// compose `check_tool` + `check_bash` / `check_tool_pattern` from Lua,
/// so the historical "if name == bash / web_fetch" branches in Rust
/// retire.
pub type DecideFn =
    dyn Fn(&str, &HashMap<String, Value>, AgentMode) -> Option<Decision> + Send + Sync;

#[derive(Clone)]
pub struct Permissions {
    normal: ModePerms,
    plan: ModePerms,
    apply: ModePerms,
    yolo: ModePerms,
    restrict_to_workspace: bool,
    workspace: PathBuf,
    paths_fn: Option<Arc<PathsFn>>,
    decide_hook_fn: Option<Arc<DecideFn>>,
}

impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permissions")
            .field("normal", &self.normal)
            .field("plan", &self.plan)
            .field("apply", &self.apply)
            .field("yolo", &self.yolo)
            .field("restrict_to_workspace", &self.restrict_to_workspace)
            .field("workspace", &self.workspace)
            .field("paths_fn", &self.paths_fn.as_ref().map(|_| "<fn>"))
            .field(
                "decide_hook_fn",
                &self.decide_hook_fn.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
}

impl Permissions {
    pub fn load() -> Self {
        let raw = RawConfig::default();
        let def = &raw.permissions.default;
        Self {
            normal: build_mode(&merge_mode(def, &raw.permissions.normal), AgentMode::Normal),
            plan: build_mode(&merge_mode(def, &raw.permissions.plan), AgentMode::Plan),
            apply: build_mode(&merge_mode(def, &raw.permissions.apply), AgentMode::Apply),
            yolo: build_mode(&merge_mode(def, &raw.permissions.yolo), AgentMode::Yolo),
            restrict_to_workspace: true,
            workspace: PathBuf::new(),
            paths_fn: None,
            decide_hook_fn: None,
        }
    }

    /// Build from a Lua-populated `RawPerms`. Called by startup after
    /// `init.lua` has run `smelt.permissions.set_rules`.
    pub fn from_raw(raw: &RawPerms) -> Self {
        let def = &raw.default;
        Self {
            normal: build_mode(&merge_mode(def, &raw.normal), AgentMode::Normal),
            plan: build_mode(&merge_mode(def, &raw.plan), AgentMode::Plan),
            apply: build_mode(&merge_mode(def, &raw.apply), AgentMode::Apply),
            yolo: build_mode(&merge_mode(def, &raw.yolo), AgentMode::Yolo),
            restrict_to_workspace: true,
            workspace: PathBuf::new(),
            paths_fn: None,
            decide_hook_fn: None,
        }
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
        apply_to_mode(&mut cloned.normal, overrides);
        apply_to_mode(&mut cloned.plan, overrides);
        apply_to_mode(&mut cloned.apply, overrides);
        apply_to_mode(&mut cloned.yolo, overrides);
        cloned
    }

    pub fn set_workspace(&mut self, path: PathBuf) {
        self.workspace = path;
    }

    pub fn set_restrict_to_workspace(&mut self, val: bool) {
        self.restrict_to_workspace = val;
    }

    /// Install the per-tool path-extraction callback. Called once at
    /// startup after Lua tool defs are registered. The callback's job
    /// is to map `(tool_name, args) -> [paths]` by invoking each
    /// tool's `paths_for_workspace(args)` Lua hook (and returning
    /// `[]` for tools that didn't register one).
    pub fn set_paths_fn(&mut self, f: Arc<PathsFn>) {
        self.paths_fn = Some(f);
    }

    fn paths_for_tool(&self, tool_name: &str, args: &HashMap<String, Value>) -> Vec<String> {
        match self.paths_fn.as_ref() {
            Some(f) => f(tool_name, args),
            None => Vec::new(),
        }
    }

    /// Install the per-tool decide-hook callback. When set, a tool's
    /// `decide(args, mode)` Lua callback returning `Some(decision)`
    /// short-circuits the generic `check_tool` path. See [`DecideFn`].
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

    fn mode_perms(&self, mode: AgentMode) -> &ModePerms {
        match mode {
            AgentMode::Normal => &self.normal,
            AgentMode::Plan => &self.plan,
            AgentMode::Apply => &self.apply,
            AgentMode::Yolo => &self.yolo,
        }
    }

    pub fn check_tool(&self, mode: AgentMode, tool_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let default = if mode == AgentMode::Yolo {
            Decision::Allow
        } else {
            Decision::Ask
        };
        perms.tools.get(tool_name).cloned().unwrap_or(default)
    }

    /// Look up a per-tool subpattern ruleset for the given mode. Tools
    /// register subpattern buckets via `smelt.permissions.set_rules`
    /// (`bash`, `web_fetch`, `mcp`, plus any custom-named tool); each
    /// is consulted by the tool's own `decide` Lua callback through
    /// [`Permissions::check_subcommand`].
    pub fn subcommand_ruleset(&self, mode: AgentMode, bucket: &str) -> Option<&RuleSet> {
        self.mode_perms(mode).subcommands.get(bucket)
    }

    /// Check a value against a tool's subpattern ruleset for the given
    /// mode. `bucket` is the tool name. Special-cases:
    /// - `bash`: splits on shell operators and folds per-subcommand.
    /// - everything else: simple glob match against the value.
    ///
    /// Returns `Decision::Ask` (or Allow in Yolo) when no bucket is
    /// registered.
    pub fn check_subcommand(&self, mode: AgentMode, bucket: &str, value: &str) -> Decision {
        let Some(rs) = self.subcommand_ruleset(mode, bucket) else {
            return if mode == AgentMode::Yolo {
                Decision::Allow
            } else {
                Decision::Ask
            };
        };
        if bucket == "bash" {
            return check_bash_against(rs, value, mode);
        }
        let decision = check_ruleset(rs, value);
        if decision == Decision::Ask && mode == AgentMode::Yolo {
            Decision::Allow
        } else {
            decision
        }
    }

    /// Full permission decision for a tool call, including workspace restriction.
    /// When `is_mcp` is true, routes through the `mcp` subpattern bucket
    /// instead of the generic `check_tool` path.
    pub fn decide(
        &self,
        mode: AgentMode,
        tool_name: &str,
        args: &HashMap<String, Value>,
        is_mcp: bool,
    ) -> Decision {
        let base = if is_mcp {
            self.check_subcommand(mode, "mcp", tool_name)
        } else {
            decide_base(self, mode, tool_name, args)
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

    /// Whether this tool call's base permission is Allow but was downgraded
    /// to Ask solely because of paths outside the workspace.
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

// ── Base decision (without workspace restriction) ────────────────────────────

fn decide_base(
    permissions: &Permissions,
    mode: AgentMode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    if let Some(d) = permissions.decide_hook(tool_name, args, mode) {
        return d;
    }
    permissions.check_tool(mode, tool_name)
}

/// Bash-aware ruleset matching: splits on shell operators, folds per
/// subcommand to the worst decision, escalates output redirection in
/// Normal/Plan, and trusts `cd` unconditionally (workspace restriction
/// in [`Permissions::decide`] still rejects outside-workspace paths).
fn check_bash_against(rs: &RuleSet, command: &str, mode: AgentMode) -> Decision {
    let command = command.trim();
    let escalate_redirect = matches!(mode, AgentMode::Normal | AgentMode::Plan);
    let subcmds = split_shell_commands(command);
    if subcmds.len() <= 1 {
        if is_cd_command(command) {
            return Decision::Allow;
        }
        let d = check_ruleset(rs, command);
        if escalate_redirect && d == Decision::Allow && has_output_redirection(command) {
            return Decision::Ask;
        }
        return d;
    }
    let mut worst = Decision::Allow;
    for subcmd in subcmds {
        if is_cd_command(&subcmd) {
            continue;
        }
        let d = check_ruleset(rs, &subcmd);
        let d = if escalate_redirect && d == Decision::Allow && has_output_redirection(&subcmd) {
            Decision::Ask
        } else {
            d
        };
        match d {
            Decision::Deny => return Decision::Deny,
            Decision::Ask if worst == Decision::Allow => worst = Decision::Ask,
            _ => {}
        }
    }
    worst
}
