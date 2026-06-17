use std::path::Path;

pub fn cwd_note(cwd: &Path, worktree_root: &Path) -> String {
    if let Some(ctx) = crate::worktree::managed_context(cwd, Some(worktree_root)) {
        let base_path = ctx
            .base_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not currently checked out".to_string());
        return format!(
            "Current working directory: {cwd}. This is a Smelt-managed worktree: branch {branch}, worktree path {path}, default base {base}, base checkout {base_path}.",
            cwd = cwd.display(),
            branch = ctx.branch,
            path = ctx.path.display(),
            base = ctx.base,
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
