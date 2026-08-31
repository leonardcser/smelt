//! Embed build identity into the smelt binary.
//!
//! Emits `cargo:rustc-env=KEY=VAL` lines so the binary can report:
//!   - SMELT_BUILD_SHA       short git commit, or "unknown"
//!   - SMELT_BUILD_DATE      committer ISO date, or "unknown"
//!   - SMELT_TARGET          target triple (e.g. aarch64-apple-darwin)
//!   - SMELT_BUILD_TAG       most recent reachable app version tag, or "unknown"
//!   - SMELT_BUILD_COMMITS   commits since that tag, or "0"
//!   - SMELT_BUILD_DIRTY     "1" when the working tree has uncommitted changes, else "0"
//!   - SMELT_DISPLAY         canonical user-facing identity string. Single
//!     source of truth for the banner, `/version`, `/upgrade`, and `--version`.
//!     Shape: `{tag}` for a clean tagged build, `{tag}-{commits}-{sha}[-dirty]`
//!     for a dev build, `v{CARGO_PKG_VERSION}` when git is unavailable.
//!
//! The git lookups go through `git rev-parse` / `git show` / `git describe`,
//! so they work for ordinary checkouts and worktrees alike. Release runners
//! set `SMELT_RELEASE_TAG` after materializing the matching package version;
//! this preserves the exact tag identity despite runner-local manifest edits.
//! When Git is unavailable, the display falls back to `CARGO_PKG_VERSION`.

use std::path::PathBuf;
use std::process::Command;

mod build_support;

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = git(&["show", "-s", "--format=%cI", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());

    let pkg_version = env!("CARGO_PKG_VERSION");
    let described = git(&[
        "describe", "--tags", "--long", "--dirty", "--match", "v[0-9]*",
    ]);
    let release_tag = std::env::var("SMELT_RELEASE_TAG").ok();
    let identity =
        build_support::resolve_identity(described.as_deref(), pkg_version, release_tag.as_deref())
            .unwrap_or_else(|error| panic!("{error}"));

    println!("cargo:rustc-env=SMELT_BUILD_SHA={sha}");
    println!("cargo:rustc-env=SMELT_BUILD_DATE={date}");
    println!("cargo:rustc-env=SMELT_TARGET={target}");
    println!("cargo:rustc-env=SMELT_BUILD_TAG={}", identity.tag);
    println!("cargo:rustc-env=SMELT_BUILD_COMMITS={}", identity.commits);
    println!("cargo:rustc-env=SMELT_BUILD_DIRTY={}", identity.dirty);
    println!("cargo:rustc-env=SMELT_DISPLAY={}", identity.display);

    // Rebuild when HEAD moves. `.git/HEAD` only contains the current ref for
    // normal branch checkouts, so also watch the resolved branch ref where the
    // commit actually changes when new commits are added.
    for pathspec in
        build_support::git_pathspecs(git(&["rev-parse", "--symbolic-full-name", "HEAD"]).as_deref())
    {
        rerun_if_git_path(pathspec);
    }
    // The dirty flag depends on tracked working-tree contents, not just git
    // refs. Watch every tracked file so a cached build reruns when a source tree
    // becomes dirty or clean without changing HEAD.
    if let (Some(repo_root), Some(tracked_files)) =
        (git(&["rev-parse", "--show-toplevel"]), git(&["ls-files"]))
    {
        let repo_root = PathBuf::from(repo_root);
        for path in build_support::tracked_file_paths(&repo_root, &tracked_files) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support.rs");
    println!("cargo:rerun-if-env-changed=SMELT_RELEASE_TAG");
    println!("cargo:rerun-if-env-changed=TARGET");
}

fn rerun_if_git_path(pathspec: &str) {
    if let Some(path) = git(&["rev-parse", "--git-path", pathspec]) {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
