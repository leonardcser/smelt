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
//! so they work for ordinary checkouts and worktrees alike. When the source
//! tree isn't a git repo (e.g. someone tar-extracted the crate) we fall
//! back to `CARGO_PKG_VERSION` (and "unknown" for the git-only fields)
//! without failing the build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let date = git(&["show", "-s", "--format=%cI", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());

    let pkg_version = env!("CARGO_PKG_VERSION");
    let described = git(&[
        "describe", "--tags", "--long", "--dirty", "--match", "v[0-9]*",
    ]);
    let (tag, commits, dirty, display) = match described.as_deref() {
        Some(d) => parse_describe(d, pkg_version),
        None => (
            "unknown".into(),
            "0".into(),
            "0".into(),
            format!("v{pkg_version}"),
        ),
    };

    println!("cargo:rustc-env=SMELT_BUILD_SHA={sha}");
    println!("cargo:rustc-env=SMELT_BUILD_DATE={date}");
    println!("cargo:rustc-env=SMELT_TARGET={target}");
    println!("cargo:rustc-env=SMELT_BUILD_TAG={tag}");
    println!("cargo:rustc-env=SMELT_BUILD_COMMITS={commits}");
    println!("cargo:rustc-env=SMELT_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=SMELT_DISPLAY={display}");

    // Rebuild when HEAD moves. `git rev-parse --git-path HEAD` resolves
    // to the right file for both regular checkouts and worktrees.
    rerun_if_git_path("HEAD");
    // Release builds often add a tag without moving HEAD. Watch both loose and
    // packed tags so the embedded display updates after tagging.
    rerun_if_git_path("refs/tags");
    rerun_if_git_path("packed-refs");
    // The dirty flag flips when tracked files change without a commit;
    // there's no single sentinel file to watch, so re-run every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TARGET");
}

/// Split a `git describe --tags --long --dirty --match 'v[0-9]*'` result like
/// `v0.5.0-alpha.2-80-g827e6646-dirty` into `(tag, commits, dirty, display)`.
/// `display` is the canonical user-facing identity: `{tag}` on a clean
/// tagged build (commits == 0, not dirty), otherwise
/// `{tag}-{commits}-{sha}[-dirty]`. A missing or unparseable result
/// falls back to `v{pkg_version}`.
fn parse_describe(described: &str, pkg_version: &str) -> (String, String, String, String) {
    let (core, dirty_flag) = match described.strip_suffix("-dirty") {
        Some(rest) => (rest, "1"),
        None => (described, "0"),
    };
    // `core` looks like `<tag>-<commits>-g<sha>`. Tags may themselves
    // contain `-` (e.g. `v0.5.0-alpha.2`), so split from the right.
    let parts: Vec<&str> = core.rsplitn(3, '-').collect();
    let (tag, commits, sha) = if parts.len() == 3 && parts[0].starts_with('g') {
        (
            parts[2].to_string(),
            parts[1].to_string(),
            parts[0].trim_start_matches('g').to_string(),
        )
    } else {
        return (
            "unknown".into(),
            "0".into(),
            dirty_flag.into(),
            format!("v{pkg_version}"),
        );
    };
    let tag_with_v = if tag.starts_with('v') {
        tag.clone()
    } else {
        format!("v{tag}")
    };
    let display = if commits == "0" && dirty_flag == "0" {
        tag_with_v
    } else {
        let mut display = format!("{tag_with_v}-{commits}-{sha}");
        if dirty_flag == "1" {
            display.push_str("-dirty");
        }
        display
    };
    (tag, commits, dirty_flag.into(), display)
}

fn rerun_if_git_path(pathspec: &str) {
    if let Some(path) = git(&["rev-parse", "--git-path", pathspec]) {
        let path = PathBuf::from(&path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
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
