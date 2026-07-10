use super::PathResolution;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const MAX_SYMLINKS: usize = 40;

/// Extract local path effects from a shell command, returning the path strings
/// used for approval matching.
pub fn extract_paths_from_command(cmd: &str) -> Vec<String> {
    crate::permissions::bash::analyze_shell_command(cmd, Path::new(""))
        .paths
        .into_iter()
        .map(|p| p.raw_path)
        .collect()
}

pub(super) fn resolve_tool_path(path: &str, base_dir: &Path) -> PathResolution {
    resolve_expanded_path(Path::new(path), base_dir)
}

pub(super) fn resolve_expanded_path(path: &Path, base_dir: &Path) -> PathResolution {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    resolve_filesystem_path(&path)
}

pub(super) fn resolve_shell_path(
    raw_path: &str,
    expanded_path: Option<&str>,
    cwd: &PathResolution,
) -> PathResolution {
    let Some(expanded_path) = expanded_path else {
        return unresolved_path(raw_path, cwd.path());
    };
    let path = Path::new(expanded_path);
    if path.is_absolute() {
        return resolve_filesystem_path(path);
    }
    match cwd {
        PathResolution::Resolved(cwd) => resolve_expanded_path(path, cwd),
        PathResolution::Unresolved(cwd) => unresolved_path(expanded_path, cwd),
    }
}

fn unresolved_path(path: &str, base_dir: &Path) -> PathResolution {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    PathResolution::Unresolved(crate::path::normalize(path))
}

pub(super) fn normalize_approval_path(path: &Path) -> PathBuf {
    resolve_approval_path(path).path().to_path_buf()
}

fn resolve_approval_path(path: &Path) -> PathResolution {
    resolve_filesystem_path(&engine::paths::expand_tilde(path))
}

fn comparable_path(path: &Path) -> Option<PathBuf> {
    resolve_approval_path(path)
        .resolved()
        .map(Path::to_path_buf)
}

pub(super) fn paths_equivalent(a: &Path, b: &Path) -> bool {
    comparable_path(a)
        .zip(comparable_path(b))
        .is_some_and(|(a, b)| a == b)
}

pub(super) fn path_prefix_matches(prefix: &Path, path: &Path) -> bool {
    comparable_path(path)
        .zip(comparable_path(prefix))
        .is_some_and(|(path, prefix)| path.starts_with(prefix))
}

/// Resolve symlinks through the longest existing prefix and normalize a
/// missing suffix. Only `NotFound` permits ancestor walking; every other I/O
/// failure remains unresolved so permission checks fail closed.
pub(super) fn resolve_filesystem_path(path: &Path) -> PathResolution {
    resolve_filesystem_path_inner(path, 0)
}

fn resolve_filesystem_path_inner(path: &Path, followed_symlinks: usize) -> PathResolution {
    let fallback = || PathResolution::Unresolved(crate::path::normalize(path));
    if !path.is_absolute() {
        return fallback();
    }
    if followed_symlinks >= MAX_SYMLINKS {
        return fallback();
    }

    let mut prefix = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&prefix) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return PathResolution::Resolved(crate::path::normalize(canonical));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(_) => return fallback(),
        }

        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let Ok(target) = std::fs::read_link(&prefix) else {
                    return fallback();
                };
                let mut target = if target.is_absolute() {
                    target
                } else {
                    prefix
                        .parent()
                        .unwrap_or_else(|| Path::new("/"))
                        .join(target)
                };
                for component in missing.iter().rev() {
                    target.push(component);
                }
                return resolve_filesystem_path_inner(&target, followed_symlinks + 1);
            }
            Ok(_) => return fallback(),
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(_) => return fallback(),
        }

        let Some(component) = prefix.components().next_back() else {
            return fallback();
        };
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            return fallback();
        }
        missing.push(component.as_os_str().to_os_string());
        if !prefix.pop() {
            return fallback();
        }
    }
}

#[cfg(test)]
pub(super) fn is_in_workspace(path: &str, workspace: &Path) -> bool {
    let resolution = resolve_tool_path(path, workspace);
    resolution.is_resolved() && path_prefix_matches(workspace, resolution.path())
}
