//! Session- and workspace-scoped runtime auto-approvals, augmenting static config rules.

use crate::permissions::bash::split_shell_commands;
use crate::permissions::rules::{check_ruleset, RuleSet};
use crate::permissions::Permissions;
use protocol::AgentMode;
use protocol::Decision;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
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
    pub fn add_session_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        let entry = self.session_tools.entry(tool.to_string()).or_default();
        if patterns.is_empty() || entry.is_empty() {
            // Blanket approval: clear existing patterns.
            entry.clear();
        } else {
            entry.extend(patterns);
        }
    }

    /// Add workspace-scoped tool approval patterns. Empty `patterns` = blanket approval.
    pub fn add_workspace_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        let entry = self.workspace_tools.entry(tool.to_string()).or_default();
        if patterns.is_empty() || entry.is_empty() {
            entry.clear();
        } else {
            entry.extend(patterns);
        }
    }

    pub fn add_session_dir(&mut self, dir: PathBuf) {
        let dir = engine::paths::expand_tilde(&dir);
        if !self.session_dirs.contains(&dir) {
            self.session_dirs.push(dir);
        }
    }

    pub fn add_workspace_dir(&mut self, dir: PathBuf) {
        let dir = engine::paths::expand_tilde(&dir);
        if !self.workspace_dirs.contains(&dir) {
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
            .map(|d| engine::paths::expand_tilde(&d))
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
            all_pats.iter().any(|p| p.matches(sc))
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
        desc: &str,
    ) -> bool {
        let config_subpatterns = permissions.subcommand_ruleset(mode, tool_name);
        let tool_approved = self.is_approved(tool_name, desc, config_subpatterns);
        let outside = permissions.outside_workspace_paths(tool_name, args);
        if outside.is_empty() {
            return tool_approved;
        }
        let dirs_ok = self.dirs_approved(&outside);
        if dirs_ok && tool_approved {
            return true;
        }
        // Directory approved + base Allow (downgraded only by workspace restriction) → auto-approve.
        if dirs_ok && permissions.was_downgraded(mode, tool_name, args) {
            return true;
        }
        false
    }

    /// `true` when `pattern` is already approved for `tool_name`.
    pub fn has_pattern(&self, tool_name: &str, pattern: &str) -> bool {
        let check = |pats: Option<&Vec<glob::Pattern>>| -> bool {
            pats.is_some_and(|ps| ps.iter().any(|p| p.as_str() == pattern))
        };
        check(self.session_tools.get(tool_name)) || check(self.workspace_tools.get(tool_name))
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
            .map(|d| engine::paths::expand_tilde(&d))
            .collect();
    }

    /// Check whether all given outside-workspace paths are covered by
    /// approved directories.  Stored dirs are always in expanded (absolute)
    /// form — only the incoming paths need tilde expansion.
    pub(crate) fn dirs_approved(&self, paths: &[String]) -> bool {
        if paths.is_empty() {
            return true;
        }
        let all_dirs: Vec<&PathBuf> = self
            .session_dirs
            .iter()
            .chain(self.workspace_dirs.iter())
            .collect();
        if all_dirs.is_empty() {
            return false;
        }
        paths.iter().all(|p| {
            let path = engine::paths::expand_tilde(Path::new(p));
            let dir = path.parent().unwrap_or(&path);
            all_dirs
                .iter()
                .any(|ad| dir.starts_with(ad.as_path()) || path.starts_with(ad.as_path()))
        })
    }
}
