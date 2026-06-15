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

pub(super) fn normalize_path(path: &Path) -> PathBuf {
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

pub(super) fn path_candidates(path: &Path) -> Vec<PathBuf> {
    let expanded = engine::paths::expand_tilde(path);
    let normalized = normalize_path(&expanded);
    let mut out = vec![normalized.clone()];
    let canonical = canonicalize_path_or_ancestor(&normalized);
    if !out.iter().any(|p| p == &canonical) {
        out.push(canonical);
    }
    out
}

pub(super) fn paths_equivalent(a: &Path, b: &Path) -> bool {
    let a_candidates = path_candidates(a);
    let b_candidates = path_candidates(b);
    a_candidates
        .iter()
        .any(|a| b_candidates.iter().any(|b| a == b))
}

pub(super) fn path_prefix_matches(prefix: &Path, path: &Path) -> bool {
    let prefix_candidates = path_candidates(prefix);
    let path_candidates = path_candidates(path);
    path_candidates.iter().any(|path| {
        prefix_candidates
            .iter()
            .any(|prefix| path.starts_with(prefix.as_path()))
    })
}

pub(super) fn resolve_path(path_str: &str, workspace: &Path) -> PathBuf {
    let resolved = if let Some(rest) = path_str.strip_prefix("~/") {
        engine::paths::home_dir().join(rest)
    } else if path_str.starts_with('/') {
        PathBuf::from(path_str)
    } else {
        workspace.join(path_str)
    };
    canonicalize_path_or_ancestor(&normalize_path(&resolved))
}

/// Canonicalize `path` when it exists; otherwise canonicalize its immediate
/// parent and append the final component. This preserves the path the user is
/// approving for newly-created files or directories.
pub(super) fn canonicalize_path_or_parent(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Ok(mut canonical) = parent.canonicalize() else {
        return path.to_path_buf();
    };
    if let Some(name) = path.file_name() {
        canonical.push(name);
    }
    normalize_path(&canonical)
}

/// Canonicalize `path` when it exists; otherwise canonicalize the nearest
/// existing ancestor and append all missing components. This is for equivalence
/// checks where symlink aliases should still match missing descendants.
fn canonicalize_path_or_ancestor(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    let mut missing = Vec::new();
    let mut prefix = path;
    while let Some(parent) = prefix.parent() {
        if let Some(name) = prefix.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canonical) = parent.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return normalize_path(&canonical);
        }
        prefix = parent;
    }

    path.to_path_buf()
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn is_in_workspace(path_str: &str, workspace: &Path) -> bool {
    let resolved = resolve_path(path_str, workspace);
    path_prefix_matches(workspace, &resolved)
}
