use std::path::{Path, PathBuf};

/// Extract local path effects from a shell command, returning the raw path
/// strings for legacy callers and approval matching.
pub fn extract_paths_from_command(cmd: &str) -> Vec<String> {
    crate::permissions::bash::analyze_shell_command(cmd, Path::new(""))
        .paths
        .into_iter()
        .map(|p| p.raw_path)
        .collect()
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

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn is_in_workspace(path_str: &str, workspace: &Path) -> bool {
    let resolved = resolve_path(path_str, workspace);
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    resolved.starts_with(&ws)
}
