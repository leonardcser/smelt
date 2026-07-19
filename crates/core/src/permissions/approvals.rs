//! Session- and workspace-scoped runtime auto-approvals, augmenting static config rules.

use crate::permissions::bash::split_shell_commands;
use crate::permissions::rules::{check_ruleset, matches_rule, RuleSet};
use crate::permissions::{
    workspace, PathAccess, PermissionGrant, PermissionRequirement, Permissions,
};
use protocol::AgentMode;
use protocol::Decision;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct RuntimeApprovals {
    session_tools: HashMap<String, ToolApprovals>,
    session_dirs: Vec<PathBuf>,
    session_path_trusts: Vec<SessionPathTrust>,
    session_path_write_exceptions: Vec<SessionPathWriteException>,
    workspace_tools: HashMap<String, ToolApprovals>,
    workspace_dirs: Vec<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct ToolApprovals {
    blanket: bool,
    patterns: Vec<glob::Pattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToolApproval {
    pub tool: String,
    /// `None` represents blanket approval for the tool.
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPathGrant {
    pub mode: Option<AgentMode>,
    pub tool: String,
    pub access: PathAccess,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPathTrust {
    mode: Option<AgentMode>,
    tool: String,
    access: PathAccess,
    dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionPathWriteException {
    mode: AgentMode,
    tool: String,
    dir: PathBuf,
}

impl RuntimeApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add session-scoped tool approval patterns. Empty `patterns` means blanket approval.
    /// Explicit patterns are retained alongside a blanket for opaque-command checks.
    pub fn add_session_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        add_tool_patterns(&mut self.session_tools, tool, patterns);
    }

    /// Add workspace-scoped tool approval patterns. Empty `patterns` means blanket approval.
    /// Explicit patterns are retained alongside a blanket for opaque-command checks.
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

    pub fn add_session_path_grant(
        &mut self,
        mode: AgentMode,
        tool: impl Into<String>,
        access: PathAccess,
        dir: PathBuf,
    ) {
        let tool = tool.into();
        self.add_session_path_trust_inner(
            Some(mode.clone()),
            tool.clone(),
            access.clone(),
            dir.clone(),
        );
        if access == PathAccess::Write {
            self.add_session_path_write_exception(mode, tool, dir);
        }
    }

    pub fn add_session_path_trust(
        &mut self,
        tool: impl Into<String>,
        access: PathAccess,
        dir: PathBuf,
    ) {
        self.add_session_path_trust_inner(None, tool.into(), access, dir);
    }

    fn add_session_path_trust_inner(
        &mut self,
        mode: Option<AgentMode>,
        tool: String,
        access: PathAccess,
        dir: PathBuf,
    ) {
        let dir = normalize_approval_dir(&dir);
        if !self.session_path_trusts.iter().any(|existing| {
            existing.mode == mode
                && existing.tool == tool
                && existing.access == access
                && workspace::paths_equivalent(&existing.dir, &dir)
        }) {
            self.session_path_trusts.push(SessionPathTrust {
                mode,
                tool,
                access,
                dir,
            });
        }
    }

    fn add_session_path_write_exception(&mut self, mode: AgentMode, tool: String, dir: PathBuf) {
        let dir = normalize_approval_dir(&dir);
        if !self.session_path_write_exceptions.iter().any(|existing| {
            existing.mode == mode
                && existing.tool == tool
                && workspace::paths_equivalent(&existing.dir, &dir)
        }) {
            self.session_path_write_exceptions
                .push(SessionPathWriteException { mode, tool, dir });
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
        self.session_path_trusts.clear();
        self.session_path_write_exceptions.clear();
    }

    /// Load workspace approvals from pre-compiled patterns (called at startup
    /// and after persisting new workspace rules).
    pub fn load_workspace(
        &mut self,
        tools: HashMap<String, Vec<glob::Pattern>>,
        dirs: Vec<PathBuf>,
    ) {
        self.workspace_tools = tools
            .into_iter()
            .map(|(tool, patterns)| (tool, ToolApprovals::from_patterns(patterns)))
            .collect();
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

        if session.is_some_and(|approval| approval.blanket)
            || workspace.is_some_and(|approval| approval.blanket)
        {
            return true;
        }

        let subcmds = split_shell_commands(desc);
        if subcmds.is_empty() {
            return false;
        }

        let all_pats: Vec<&glob::Pattern> = session
            .into_iter()
            .chain(workspace)
            .flat_map(|approval| &approval.patterns)
            .collect();

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
                PermissionRequirement::OpaqueCommand { tool, command, .. } => {
                    self.explicit_command_approved(tool, command, config_subpatterns)
                }
                other => self.requirement_satisfied(other),
            })
    }

    /// `true` when `pattern` is already approved for `tool_name`.
    pub fn has_pattern(&self, tool_name: &str, pattern: &str) -> bool {
        self.session_tools
            .get(tool_name)
            .into_iter()
            .chain(self.workspace_tools.get(tool_name))
            .any(|approval| {
                approval.blanket
                    || approval
                        .patterns
                        .iter()
                        .any(|existing| existing.as_str() == pattern)
            })
    }

    pub fn has_explicit_pattern(&self, tool_name: &str, pattern: &str) -> bool {
        self.session_tools
            .get(tool_name)
            .into_iter()
            .chain(self.workspace_tools.get(tool_name))
            .flat_map(|approval| &approval.patterns)
            .any(|existing| existing.as_str() == pattern)
    }

    fn explicit_command_approved(
        &self,
        tool_name: &str,
        command: &str,
        config_subpatterns: Option<&RuleSet>,
    ) -> bool {
        self.session_tools
            .get(tool_name)
            .into_iter()
            .chain(self.workspace_tools.get(tool_name))
            .flat_map(|approval| &approval.patterns)
            .any(|pattern| matches_rule(pattern, command))
            || config_subpatterns
                .is_some_and(|rules| check_ruleset(rules, command) == Decision::Allow)
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
                self.session_tools
                    .get(tool)
                    .is_some_and(|entry| entry.blanket)
                    || self
                        .workspace_tools
                        .get(tool)
                        .is_some_and(|entry| entry.blanket)
            }
            PermissionRequirement::Command { tool, command } => {
                self.is_approved(tool, command, None)
            }
            PermissionRequirement::OpaqueCommand { tool, command, .. } => {
                self.explicit_command_approved(tool, command, None)
            }
            PermissionRequirement::PathPrefix { dir } => self.dir_approved_for_path(dir),
        }
    }

    /// Return session tool approvals without collapsing blanket and explicit grants.
    pub fn session_tool_approvals(&self) -> Vec<SessionToolApproval> {
        let mut tools: Vec<_> = self.session_tools.keys().cloned().collect();
        tools.sort();
        let mut out = Vec::new();
        for tool in tools {
            let approval = &self.session_tools[&tool];
            if approval.blanket {
                out.push(SessionToolApproval {
                    tool: tool.clone(),
                    pattern: None,
                });
            }
            out.extend(approval.patterns.iter().map(|pattern| SessionToolApproval {
                tool: tool.clone(),
                pattern: Some(pattern.as_str().to_string()),
            }));
        }
        out
    }

    /// Session directory approvals (for display in status UI).
    pub fn session_dirs(&self) -> &[PathBuf] {
        &self.session_dirs
    }

    pub fn session_path_grants(&self) -> Vec<SessionPathGrant> {
        self.session_path_trusts
            .iter()
            .map(|grant| SessionPathGrant {
                mode: grant.mode.clone(),
                tool: grant.tool.clone(),
                access: grant.access.clone(),
                dir: grant.dir.clone(),
            })
            .collect()
    }

    /// Rebuild session state from flattened entries (used by permissions sync UI).
    pub fn set_session(
        &mut self,
        tools: Vec<SessionToolApproval>,
        dirs: Vec<PathBuf>,
        path_grants: Vec<SessionPathGrant>,
    ) {
        self.session_tools.clear();
        for approval in tools {
            let patterns = match approval.pattern {
                None => Vec::new(),
                Some(pattern) => {
                    let Ok(pattern) = glob::Pattern::new(&pattern) else {
                        continue;
                    };
                    vec![pattern]
                }
            };
            add_tool_patterns(&mut self.session_tools, &approval.tool, patterns);
        }
        self.session_dirs = dirs
            .into_iter()
            .map(|d| normalize_approval_dir(&d))
            .collect();
        self.session_path_trusts.clear();
        self.session_path_write_exceptions.clear();
        for grant in path_grants {
            if let Some(mode) = grant.mode {
                self.add_session_path_grant(mode, grant.tool, grant.access, grant.dir);
            } else {
                self.add_session_path_trust(grant.tool, grant.access, grant.dir);
            }
        }
    }

    pub fn session_path_grant_approved_for_path(
        &self,
        mode: &AgentMode,
        tool: &str,
        access: &PathAccess,
        path: &Path,
    ) -> bool {
        self.session_path_trusts.iter().any(|grant| {
            grant
                .mode
                .as_ref()
                .is_none_or(|grant_mode| grant_mode == mode)
                && grant.tool == tool
                && grant.access == *access
                && workspace::path_prefix_matches(&grant.dir, path)
        })
    }

    pub fn session_path_write_exception_approved_for_path(
        &self,
        mode: &AgentMode,
        tool: &str,
        path: &Path,
    ) -> bool {
        self.session_path_write_exceptions.iter().any(|grant| {
            grant.mode == *mode
                && grant.tool == tool
                && workspace::path_prefix_matches(&grant.dir, path)
        })
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
    workspace::normalize_approval_path(dir)
}

impl ToolApprovals {
    fn from_patterns(patterns: Vec<glob::Pattern>) -> Self {
        Self {
            blanket: patterns.is_empty(),
            patterns,
        }
    }

    fn add(&mut self, patterns: Vec<glob::Pattern>) {
        if patterns.is_empty() {
            self.blanket = true;
            return;
        }
        for pattern in patterns {
            if !self
                .patterns
                .iter()
                .any(|existing| existing.as_str() == pattern.as_str())
            {
                self.patterns.push(pattern);
            }
        }
    }
}

fn add_tool_patterns(
    tools: &mut HashMap<String, ToolApprovals>,
    tool: &str,
    patterns: Vec<glob::Pattern>,
) {
    tools.entry(tool.to_string()).or_default().add(patterns);
}
