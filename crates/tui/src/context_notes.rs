use std::path::Path;

pub(crate) fn cwd_note(cwd: &Path, worktree_root: &Path) -> String {
    if let Some(ctx) = smelt_core::worktree::managed_context(cwd, Some(worktree_root)) {
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
