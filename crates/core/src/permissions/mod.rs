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
use rules::RuleSet;
pub use rules::DEFAULT_BASH_ALLOW;

use bash::{has_output_redirection, is_cd_command};

use protocol::AgentMode;
#[cfg(test)]
use rules::compile_patterns;
use rules::{build_mode, check_ruleset, merge_mode, ModePerms, RawConfig, RawPerms};
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
            if let Some(ref bash) = overrides.bash {
                let mut allow = compile_patterns(&bash.allow);
                allow.append(&mut mode.bash.allow);
                mode.bash.allow = allow;
                let mut ask = compile_patterns(&bash.ask);
                ask.append(&mut mode.bash.ask);
                mode.bash.ask = ask;
                let mut deny = compile_patterns(&bash.deny);
                deny.append(&mut mode.bash.deny);
                mode.bash.deny = deny;
            }
            if let Some(ref wf) = overrides.web_fetch {
                let mut allow = compile_patterns(&wf.allow);
                allow.append(&mut mode.web_fetch.allow);
                mode.web_fetch.allow = allow;
                let mut ask = compile_patterns(&wf.ask);
                ask.append(&mut mode.web_fetch.ask);
                mode.web_fetch.ask = ask;
                let mut deny = compile_patterns(&wf.deny);
                deny.append(&mut mode.web_fetch.deny);
                mode.web_fetch.deny = deny;
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

    /// Check a URL against the `web_fetch` ruleset of `mode`. Owned by
    /// the tool's `decide` Lua callback (`tools/web_fetch.lua`); the
    /// engine never reaches this directly.
    pub fn check_web_fetch(&self, mode: AgentMode, url: &str) -> Decision {
        check_ruleset(&self.mode_perms(mode).web_fetch, url)
    }

    /// Check permission for an MCP tool call. Matches the qualified tool name
    /// (e.g. `filesystem_read_file`) against glob patterns in the `mcp` ruleset.
    /// Defaults to Allow in yolo mode, Ask otherwise, if no pattern matches.
    pub fn check_mcp(&self, mode: AgentMode, qualified_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let decision = check_ruleset(&perms.mcp, qualified_name);
        if decision == Decision::Ask && mode == AgentMode::Yolo {
            Decision::Allow
        } else {
            decision
        }
    }

    pub fn check_bash(&self, mode: AgentMode, command: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let command = command.trim();
        // Escalate output redirection only in Normal/Plan modes.
        let escalate_redirect = matches!(mode, AgentMode::Normal | AgentMode::Plan);
        let subcmds = split_shell_commands(command);
        if subcmds.len() <= 1 {
            if is_cd_command(command) {
                return Decision::Allow;
            }
            let d = check_ruleset(&perms.bash, command);
            if escalate_redirect && d == Decision::Allow && has_output_redirection(command) {
                return Decision::Ask;
            }
            return d;
        }
        let mut worst = Decision::Allow;
        for subcmd in subcmds {
            // `cd` is always allowed at the command level; the workspace
            // path restriction in `decide()` handles outside-workspace paths.
            if is_cd_command(&subcmd) {
                continue;
            }
            let d = check_ruleset(&perms.bash, &subcmd);
            let d = if escalate_redirect && d == Decision::Allow && has_output_redirection(&subcmd)
            {
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

    /// Full permission decision for a tool call, including workspace restriction.
    /// When `is_mcp` is true, routes through the MCP ruleset instead of the
    /// normal tool/bash/web_fetch rulesets.
    pub fn decide(
        &self,
        mode: AgentMode,
        tool_name: &str,
        args: &HashMap<String, Value>,
        is_mcp: bool,
    ) -> Decision {
        let base = if is_mcp {
            self.check_mcp(mode, tool_name)
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

    /// Return paths from a tool call that fall outside the workspace.
    /// Empty if `restrict_to_workspace` is off or no paths escape.
    /// Get the bash ruleset for the given mode (used by RuntimeApprovals
    /// to check per-subcommand config decisions).
    pub(crate) fn bash_ruleset(&self, mode: AgentMode) -> &RuleSet {
        &self.mode_perms(mode).bash
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
