#[path = "../build_pathspecs.rs"]
mod build_pathspecs;

#[test]
fn branch_checkout_watches_head_and_resolved_branch_ref() {
    assert_eq!(
        build_pathspecs::git_pathspecs(Some("refs/heads/main")),
        vec!["HEAD", "refs/heads/main", "refs/tags", "packed-refs"]
    );
}

#[test]
fn detached_head_does_not_duplicate_head_watch() {
    assert_eq!(
        build_pathspecs::git_pathspecs(Some("HEAD")),
        vec!["HEAD", "refs/tags", "packed-refs"]
    );
}

#[test]
fn missing_head_ref_still_watches_release_tag_paths() {
    assert_eq!(
        build_pathspecs::git_pathspecs(None),
        vec!["HEAD", "refs/tags", "packed-refs"]
    );
}
