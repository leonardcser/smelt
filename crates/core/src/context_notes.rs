use std::path::Path;

pub fn cwd_note(cwd: &Path, worktree_root: &Path) -> String {
    let project = crate::worktree::project_context(cwd, Some(worktree_root));
    cwd_note_for_project(cwd, &project)
}

/// Format the workspace note from already discovered Git data.
pub fn cwd_note_for_project(cwd: &Path, project: &crate::worktree::ProjectContext) -> String {
    if project.managed_worktree {
        let base_path = project
            .default_base_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not currently checked out".to_string());
        return format!(
            "Current working directory: {cwd}. This is a Smelt-managed worktree: branch {branch}, worktree path {path}, default base {base}, base checkout {base_path}.",
            cwd = cwd.display(),
            branch = if project.branch.is_empty() { "HEAD" } else { &project.branch },
            path = project.active_root.display(),
            base = project.default_base,
        );
    }
    format!("Current working directory: {}.", cwd.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_note_for_regular_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            cwd_note(dir.path(), dir.path()),
            format!("Current working directory: {}.", dir.path().display())
        );
    }
}
