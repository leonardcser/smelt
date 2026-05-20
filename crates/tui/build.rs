//! Embed build identity into the smelt binary.
//!
//! Emits three `cargo:rustc-env=KEY=VAL` lines so the binary can report:
//!   - SMELT_BUILD_SHA   short git commit, or "unknown"
//!   - SMELT_BUILD_DATE  committer ISO date, or "unknown"
//!   - SMELT_TARGET      target triple (e.g. aarch64-apple-darwin)
//!
//! The git lookups go through `git rev-parse` / `git show`, so they work
//! for ordinary checkouts and worktrees alike. When the source tree
//! isn't a git repo (e.g. someone tar-extracted the crate) we fall back
//! to "unknown" without failing the build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = git(&["show", "-s", "--format=%cI", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());

    println!("cargo:rustc-env=SMELT_BUILD_SHA={sha}");
    println!("cargo:rustc-env=SMELT_BUILD_DATE={date}");
    println!("cargo:rustc-env=SMELT_TARGET={target}");

    // Rebuild when HEAD moves. `git rev-parse --git-path HEAD` resolves
    // to the right file for both regular checkouts and worktrees.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        let path = PathBuf::from(&head);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=TARGET");
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
