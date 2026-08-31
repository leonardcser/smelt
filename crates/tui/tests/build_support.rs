#[path = "../build_support.rs"]
mod build_support;

#[test]
fn branch_checkout_watches_head_resolved_branch_and_dirty_inputs() {
    assert_eq!(
        build_support::git_pathspecs(Some("refs/heads/main")),
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
        build_support::git_pathspecs(Some("HEAD")),
        vec!["HEAD", "index", "refs/tags", "packed-refs"]
    );
}

#[test]
fn missing_head_ref_still_watches_release_tag_paths() {
    assert_eq!(
        build_support::git_pathspecs(None),
        vec!["HEAD", "index", "refs/tags", "packed-refs"]
    );
}

#[test]
fn tracked_file_paths_resolve_from_repo_root() {
    let repo_root = std::path::Path::new("/repo");

    assert_eq!(
        build_support::tracked_file_paths(repo_root, "Cargo.toml\nsrc/main.rs\n\n"),
        vec![repo_root.join("Cargo.toml"), repo_root.join("src/main.rs")]
    );
}

#[test]
fn matching_release_tag_overrides_runner_local_dirty_state() {
    let identity = build_support::resolve_identity(
        Some("v0.5.0-alpha.12-12-g3311c2d9-dirty"),
        "0.6.0",
        Some("v0.6.0"),
    )
    .unwrap();

    assert_eq!(identity.tag, "v0.6.0");
    assert_eq!(identity.commits, "0");
    assert_eq!(identity.dirty, "0");
    assert_eq!(identity.display, "v0.6.0");
}

#[test]
fn release_tag_must_match_materialized_package_version() {
    let error = build_support::resolve_identity(None, "0.6.0", Some("v0.7.0")).unwrap_err();
    assert!(error.contains("does not match package version `0.6.0`"));
}

#[test]
fn development_identity_preserves_git_distance_and_dirty_state() {
    let identity = build_support::resolve_identity(
        Some("v0.5.0-alpha.12-12-g3311c2d9-dirty"),
        "0.5.0-alpha.12",
        None,
    )
    .unwrap();

    assert_eq!(identity.tag, "v0.5.0-alpha.12");
    assert_eq!(identity.commits, "12");
    assert_eq!(identity.dirty, "1");
    assert_eq!(identity.display, "v0.5.0-alpha.12-12-3311c2d9-dirty");
}
