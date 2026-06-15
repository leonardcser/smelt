//! Session- and workspace-scoped runtime auto-approvals, augmenting static config rules.

use crate::permissions::bash::split_shell_commands;
use crate::permissions::rules::{check_ruleset, matches_rule, RuleSet};
use crate::permissions::{workspace, PermissionGrant, PermissionRequirement, Permissions};
use protocol::AgentMode;
use protocol::Decision;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct RuntimeApprovals {
    session_tools: HashMap<String, Vec<glob::Pattern>>,
    session_dirs: Vec<PathBuf>,
    workspace_tools: HashMap<String, Vec<glob::Pattern>>,
    workspace_dirs: Vec<PathBuf>,
}

impl RuntimeApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add session-scoped tool approval patterns. Empty `patterns` = blanket approval.
    /// An existing blanket entry beats incoming patterns (blanket is broader).
    pub fn add_session_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        add_tool_patterns(&mut self.session_tools, tool, patterns);
    }

    /// Add workspace-scoped tool approval patterns. Empty `patterns` = blanket approval.
    /// An existing blanket entry beats incoming patterns (blanket is broader).
    pub fn add_workspace_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        add_tool_patterns(&mut self.workspace_tools, tool, patterns);
    }

    pub fn add_session_dir(&mut self, dir: PathBuf) {
        let dir = normalize_approval_dir(&dir);
        if !self
            .session_dirs
            .iter()
            .any(|existing| workspace::paths_equivalent(existing, &dir))
        {
            self.session_dirs.push(dir);
        }
    }

    pub fn add_workspace_dir(&mut self, dir: PathBuf) {
        let dir = normalize_approval_dir(&dir);
        if !self
            .workspace_dirs
            .iter()
            .any(|existing| workspace::paths_equivalent(existing, &dir))
        {
            self.workspace_dirs.push(dir);
        }
    }

    pub fn clear_session(&mut self) {
        self.session_tools.clear();
        self.session_dirs.clear();
    }

    /// Load workspace approvals from pre-compiled patterns (called at startup
    /// and after persisting new workspace rules).
    pub fn load_workspace(
        &mut self,
        tools: HashMap<String, Vec<glob::Pattern>>,
        dirs: Vec<PathBuf>,
    ) {
        self.workspace_tools = tools;
        self.workspace_dirs = dirs
            .into_iter()
            .map(|d| normalize_approval_dir(&d))
            .collect();
    }

    /// Returns `true` when a tool call should be auto-approved based on runtime patterns.
    /// Splits compound shell commands and checks each subcommand.
    pub(crate) fn is_approved(
        &self,
        tool_name: &str,
        desc: &str,
        config_subpatterns: Option<&RuleSet>,
    ) -> bool {
        let session = self.session_tools.get(tool_name);
        let workspace = self.workspace_tools.get(tool_name);

        if session.is_none() && workspace.is_none() {
            return false;
        }

        // Blanket approval (empty pattern list).
        let blanket =
            session.is_some_and(|p| p.is_empty()) || workspace.is_some_and(|p| p.is_empty());
        if blanket {
            return true;
        }

        let subcmds = split_shell_commands(desc);
        if subcmds.is_empty() {
            return false;
        }

        let all_pats: Vec<&glob::Pattern> =
            session.into_iter().chain(workspace).flatten().collect();

        subcmds.iter().all(|sc| {
            all_pats.iter().any(|p| matches_rule(p, sc))
                // Also check config allow patterns (e.g. bash's default_allow list).
                || config_subpatterns
                    .is_some_and(|rs| check_ruleset(rs, sc) == Decision::Allow)
        })
    }

    /// Full auto-approval check. Outside the workspace, directory approvals must also match.
    pub fn is_auto_approved(
        &self,
        permissions: &Permissions,
        mode: AgentMode,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> bool {
        let outcome = permissions.evaluate_tool(
            mode.clone(),
            crate::permissions::ToolOrigin::Lua,
            tool_name,
            args,
        );
        let config_subpatterns = permissions.subcommand_ruleset(mode, tool_name);
        if outcome.decision != Decision::Ask {
            return self.is_approved(
                tool_name,
                tool_command_text(tool_name, args).unwrap_or_default(),
                config_subpatterns,
            );
        }
        outcome
            .missing_requirements
            .iter()
            .all(|requirement| match requirement {
                PermissionRequirement::Command { tool, command } => {
                    self.is_approved(tool, command, config_subpatterns)
                }
                other => self.requirement_satisfied(other),
            })
    }

    /// `true` when `pattern` is already approved for `tool_name`.
    pub fn has_pattern(&self, tool_name: &str, pattern: &str) -> bool {
        let check = |pats: Option<&Vec<glob::Pattern>>| -> bool {
            pats.is_some_and(|ps| ps.is_empty() || ps.iter().any(|p| p.as_str() == pattern))
        };
        check(self.session_tools.get(tool_name)) || check(self.workspace_tools.get(tool_name))
    }

    pub fn add_session_grant(&mut self, grant: PermissionGrant) {
        match grant {
            PermissionGrant::Tool { tool } => self.add_session_tool(&tool, Vec::new()),
            PermissionGrant::Command { tool, pattern } => {
                if let Ok(pattern) = glob::Pattern::new(&pattern) {
                    self.add_session_tool(&tool, vec![pattern]);
                }
            }
            PermissionGrant::PathPrefix { dir } => self.add_session_dir(dir),
        }
    }

    pub fn requirement_satisfied(&self, requirement: &PermissionRequirement) -> bool {
        match requirement {
            PermissionRequirement::Tool { tool } => {
                self.session_tools.get(tool).is_some_and(Vec::is_empty)
                    || self.workspace_tools.get(tool).is_some_and(Vec::is_empty)
            }
            PermissionRequirement::Command { tool, command } => {
                self.is_approved(tool, command, None)
            }
            PermissionRequirement::PathPrefix { dir } => self.dir_approved_for_path(dir),
        }
    }

    /// Iterate session tool approvals (for display in status UI).
    pub fn session_tool_entries(&self) -> Vec<(String, Vec<String>)> {
        let mut tools: Vec<_> = self.session_tools.keys().cloned().collect();
        tools.sort();
        tools
            .into_iter()
            .map(|t| {
                let pats: Vec<String> = self.session_tools[&t]
                    .iter()
                    .map(|p| p.as_str().to_string())
                    .collect();
                (t, pats)
            })
            .collect()
    }

    /// Session directory approvals (for display in status UI).
    pub fn session_dirs(&self) -> &[PathBuf] {
        &self.session_dirs
    }

    /// Rebuild session state from flattened entries (used by permissions sync UI).
    pub fn set_session(&mut self, tools: HashMap<String, Vec<glob::Pattern>>, dirs: Vec<PathBuf>) {
        self.session_tools = tools;
        self.session_dirs = dirs
            .into_iter()
            .map(|d| normalize_approval_dir(&d))
            .collect();
    }

    fn dir_approved_for_path(&self, path: &Path) -> bool {
        self.session_dirs
            .iter()
            .chain(self.workspace_dirs.iter())
            .any(|approved| workspace::path_prefix_matches(approved, path))
    }
}

fn tool_command_text<'a>(tool_name: &str, args: &'a HashMap<String, Value>) -> Option<&'a str> {
    match tool_name {
        "bash" => args.get("command").and_then(Value::as_str),
        _ => None,
    }
}

fn normalize_approval_dir(dir: &Path) -> PathBuf {
    workspace::canonicalize_path_or_parent(&workspace::normalize_path(
        &engine::paths::expand_tilde(dir),
    ))
}

fn add_tool_patterns(
    tools: &mut HashMap<String, Vec<glob::Pattern>>,
    tool: &str,
    patterns: Vec<glob::Pattern>,
) {
    if patterns.is_empty() {
        tools.insert(tool.to_string(), Vec::new());
        return;
    }
    if let Some(existing) = tools.get(tool) {
        if existing.is_empty() {
            return;
        }
    }
    tools.entry(tool.to_string()).or_default().extend(patterns);
}
