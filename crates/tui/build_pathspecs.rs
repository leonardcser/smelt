use std::path::{Path, PathBuf};

pub(crate) fn git_pathspecs(head_ref: Option<&str>) -> Vec<&str> {
    let mut pathspecs = vec!["HEAD"];
    if let Some(head_ref) = head_ref {
        if !head_ref.is_empty() && head_ref != "HEAD" {
            pathspecs.push(head_ref);
        }
    }
    pathspecs.extend(["index", "refs/tags", "packed-refs"]);
    pathspecs
}

pub(crate) fn tracked_file_paths(repo_root: &Path, git_ls_files: &str) -> Vec<PathBuf> {
    git_ls_files
        .lines()
        .filter(|path| !path.is_empty())
        .map(|path| repo_root.join(path))
        .collect()
}
