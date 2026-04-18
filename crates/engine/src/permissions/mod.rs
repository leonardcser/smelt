//! Permission policy for tool calls.
//!
//! Layout:
//! - [`rules`]: rule-set types and pattern matching
//! - [`bash`]: shell command splitting / heredoc parsing / redirection detection
//! - [`workspace`]: path extraction + workspace boundary enforcement
//! - [`approvals`]: runtime auto-approval tracking
//!
//! The public surface is this module: `Permissions`, `Decision`,
//! `RuntimeApprovals`, and two helpers consumed by tool implementations
//! (`split_shell_commands`, `DEFAULT_BASH_ALLOW`).

pub mod approvals;
pub mod bash;
pub mod rules;
pub mod workspace;

#[cfg(test)]
mod tests;

pub use approvals::RuntimeApprovals;
pub use bash::{split_shell_commands, split_shell_commands_with_ops};
pub use rules::{Decision, RuleSet, DEFAULT_BASH_ALLOW};

use crate::tools::str_arg;
use bash::{has_output_redirection, is_cd_command};
use protocol::Mode;
use rules::{build_mode, check_ruleset, compile_patterns, merge_mode, ModePerms, RawConfig};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use workspace::{extract_tool_paths, has_paths_outside_workspace, is_in_workspace};

#[derive(Debug, Clone)]
pub struct Permissions {
    normal: ModePerms,
    plan: ModePerms,
    apply: ModePerms,
    yolo: ModePerms,
    restrict_to_workspace: bool,
    workspace: PathBuf,
}

impl Permissions {
    pub fn load() -> Self {
        let path = crate::paths::config_dir().join("config.yaml");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let raw: RawConfig = serde_yml::from_str(&contents).unwrap_or_default();
        let def = &raw.permissions.default;
        Self {
            normal: build_mode(&merge_mode(def, &raw.permissions.normal), Mode::Normal),
            plan: build_mode(&merge_mode(def, &raw.permissions.plan), Mode::Plan),
            apply: build_mode(&merge_mode(def, &raw.permissions.apply), Mode::Apply),
            yolo: build_mode(&merge_mode(def, &raw.permissions.yolo), Mode::Yolo),
            restrict_to_workspace: true,
            workspace: PathBuf::new(),
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

    pub fn restrict_to_workspace(&self) -> bool {
        self.restrict_to_workspace
    }

    pub fn set_restrict_to_workspace(&mut self, val: bool) {
        self.restrict_to_workspace = val;
    }

    fn mode_perms(&self, mode: Mode) -> &ModePerms {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
            Mode::Yolo => &self.yolo,
        }
    }

    pub fn check_tool(&self, mode: Mode, tool_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let default = if mode == Mode::Yolo {
            Decision::Allow
        } else {
            Decision::Ask
        };
        perms.tools.get(tool_name).cloned().unwrap_or(default)
    }

    pub fn check_tool_pattern(&self, mode: Mode, tool_name: &str, pattern: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let ruleset = match tool_name {
            "web_fetch" => &perms.web_fetch,
            _ => return Decision::Ask,
        };
        check_ruleset(ruleset, pattern)
    }

    /// Check permission for an MCP tool call. Matches the qualified tool name
    /// (e.g. `filesystem_read_file`) against glob patterns in the `mcp` ruleset.
    /// Defaults to Allow in yolo mode, Ask otherwise, if no pattern matches.
    pub fn check_mcp(&self, mode: Mode, qualified_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let decision = check_ruleset(&perms.mcp, qualified_name);
        if decision == Decision::Ask && mode == Mode::Yolo {
            Decision::Allow
        } else {
            decision
        }
    }

    pub fn check_bash(&self, mode: Mode, command: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let command = command.trim();
        // Escalate output redirection only in Normal/Plan modes.
        let escalate_redirect = matches!(mode, Mode::Normal | Mode::Plan);
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
        mode: Mode,
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
            && has_paths_outside_workspace(tool_name, args, &self.workspace)
        {
            return Decision::Ask;
        }
        base
    }

    /// Whether this tool call's base permission is Allow but was downgraded
    /// to Ask solely because of paths outside the workspace.
    pub fn was_downgraded(
        &self,
        mode: Mode,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> bool {
        let base = decide_base(self, mode, tool_name, args);
        base == Decision::Allow
            && self.restrict_to_workspace
            && !self.workspace.as_os_str().is_empty()
            && has_paths_outside_workspace(tool_name, args, &self.workspace)
    }

    /// Return paths from a tool call that fall outside the workspace.
    /// Empty if `restrict_to_workspace` is off or no paths escape.
    /// Get the bash ruleset for the given mode (used by RuntimeApprovals
    /// to check per-subcommand config decisions).
    pub fn bash_ruleset(&self, mode: Mode) -> &RuleSet {
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
        extract_tool_paths(tool_name, args)
            .into_iter()
            .filter(|p| !is_in_workspace(p, &self.workspace))
            .collect()
    }
}

// ── Base decision (without workspace restriction) ────────────────────────────

fn decide_base(
    permissions: &Permissions,
    mode: Mode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    if tool_name == "bash" {
        let cmd = str_arg(args, "command");
        let tool_decision = permissions.check_tool(mode, "bash");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let bash_decision = permissions.check_bash(mode, &cmd);
        match (&tool_decision, &bash_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (Decision::Allow, Decision::Ask) => Decision::Ask,
            _ => bash_decision,
        }
    } else if tool_name == "web_fetch" {
        let url = str_arg(args, "url");
        let tool_decision = permissions.check_tool(mode, "web_fetch");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let pattern_decision = permissions.check_tool_pattern(mode, "web_fetch", &url);
        match (&tool_decision, &pattern_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (_, Decision::Allow) => Decision::Allow,
            (Decision::Allow, Decision::Ask) => Decision::Ask,
            _ => pattern_decision,
        }
    } else {
        permissions.check_tool(mode, tool_name)
    }
}
