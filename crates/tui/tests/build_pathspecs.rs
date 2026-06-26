#[path = "../build_pathspecs.rs"]
mod build_pathspecs;

#[test]
fn branch_checkout_watches_head_resolved_branch_and_dirty_inputs() {
    assert_eq!(
        build_pathspecs::git_pathspecs(Some("refs/heads/main")),
        vec![
            "HEAD",
            "refs/heads/main",
            "index",
            "refs/tags",
            "packed-refs"
        ]
    );
}

#[test]
fn detached_head_does_not_duplicate_head_watch() {
    assert_eq!(
        build_pathspecs::git_pathspecs(Some("HEAD")),
        vec!["HEAD", "index", "refs/tags", "packed-refs"]
    );
}

#[test]
fn missing_head_ref_still_watches_release_tag_paths() {
    assert_eq!(
        build_pathspecs::git_pathspecs(None),
        vec!["HEAD", "index", "refs/tags", "packed-refs"]
    );
}

#[test]
fn tracked_file_paths_resolve_from_repo_root() {
    let repo_root = std::path::Path::new("/repo");

    assert_eq!(
        build_pathspecs::tracked_file_paths(repo_root, "Cargo.toml\nsrc/main.rs\n\n"),
        vec![repo_root.join("Cargo.toml"), repo_root.join("src/main.rs")]
    );
}
