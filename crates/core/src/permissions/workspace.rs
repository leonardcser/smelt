use crate::permissions::bash::strip_heredoc_bodies;
use std::path::{Path, PathBuf};

/// Extract absolute and tilde-prefixed path tokens from a shell command.
pub fn extract_paths_from_command(cmd: &str) -> Vec<String> {
    let cmd = strip_heredoc_bodies(cmd); // heredoc bodies are data, not paths
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

pub(super) fn resolve_path(path_str: &str, workspace: &Path) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        let resolved = engine::paths::home_dir().join(rest);
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

pub(super) fn any_outside_workspace(paths: &[String], workspace: &Path) -> bool {
    paths.iter().any(|p| !is_in_workspace(p, workspace))
}
