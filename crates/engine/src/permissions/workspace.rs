//! Path extraction + workspace boundary enforcement.
//!
//! Given a tool call (name + args), pull out filesystem paths it touches
//! and decide whether any of them escape the configured workspace root.

use crate::permissions::bash::strip_heredoc_bodies;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn str_arg(args: &HashMap<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

// ── Workspace path restriction ───────────────────────────────────────────────

pub(super) fn extract_tool_paths(tool_name: &str, args: &HashMap<String, Value>) -> Vec<String> {
    match tool_name {
        "read_file" | "write_file" | "edit_file" => {
            let p = str_arg(args, "file_path");
            if p.is_empty() {
                vec![]
            } else {
                vec![p]
            }
        }
        "glob" | "grep" => {
            let p = str_arg(args, "path");
            if p.is_empty() {
                vec![]
            } else {
                vec![p]
            }
        }
        "bash" => extract_paths_from_command(&str_arg(args, "command")),
        _ => vec![],
    }
}

/// Extract tokens that look like absolute paths from a shell command.
/// Relative paths are fine (they resolve within the workspace).
pub(super) fn extract_paths_from_command(cmd: &str) -> Vec<String> {
    // Strip heredoc bodies — they are data, not shell commands.
    let cmd = strip_heredoc_bodies(cmd);
    let mut paths = Vec::new();
    for token in cmd.split_whitespace() {
        let clean = token.trim_matches(|c: char| c == '\'' || c == '"' || c == ';');
        if (clean.starts_with('/') && clean.len() > 1) || clean.starts_with("~/") {
            paths.push(clean.to_string());
        }
    }
    paths
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            c => components.push(c),
        }
    }
    components.iter().collect()
}

fn resolve_path(path_str: &str, workspace: &Path) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        let resolved = crate::paths::home_dir().join(rest);
        resolved
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(&resolved))
    } else if path_str.starts_with('/') {
        let p = PathBuf::from(path_str);
        p.canonicalize().unwrap_or_else(|_| normalize_path(&p))
    } else {
        let resolved = workspace.join(path_str);
        resolved
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(&resolved))
    }
}

pub(super) fn is_in_workspace(path_str: &str, workspace: &Path) -> bool {
    let resolved = resolve_path(path_str, workspace);
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    resolved.starts_with(&ws)
}

pub(super) fn has_paths_outside_workspace(
    tool_name: &str,
    args: &HashMap<String, Value>,
    workspace: &Path,
) -> bool {
    let paths = extract_tool_paths(tool_name, args);
    paths.iter().any(|p| !is_in_workspace(p, workspace))
}
