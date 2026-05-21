//! Embed build identity into the smelt binary.
//!
//! Emits `cargo:rustc-env=KEY=VAL` lines so the binary can report:
//!   - SMELT_BUILD_SHA       short git commit, or "unknown"
//!   - SMELT_BUILD_DATE      committer ISO date, or "unknown"
//!   - SMELT_TARGET          target triple (e.g. aarch64-apple-darwin)
//!   - SMELT_BUILD_TAG       most recent reachable tag, or "unknown"
//!   - SMELT_BUILD_COMMITS   commits since that tag, or "0"
//!   - SMELT_BUILD_DIRTY     "1" when the working tree has uncommitted changes, else "0"
//!   - SMELT_VERSION_STRING  composed display string consumed by `--version`
//!     and `smelt.build.version_string` (e.g. `0.5.1-60-g3349b5f-dirty`)
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
    let described = git(&["describe", "--tags", "--long", "--dirty"]);
    let (tag, commits, dirty, version_string) = match described.as_deref() {
        Some(d) => parse_describe(d, pkg_version),
        None => ("unknown".into(), "0".into(), "0".into(), pkg_version.into()),
    };

    println!("cargo:rustc-env=SMELT_BUILD_SHA={sha}");
    println!("cargo:rustc-env=SMELT_BUILD_DATE={date}");
    println!("cargo:rustc-env=SMELT_TARGET={target}");
    println!("cargo:rustc-env=SMELT_BUILD_TAG={tag}");
    println!("cargo:rustc-env=SMELT_BUILD_COMMITS={commits}");
    println!("cargo:rustc-env=SMELT_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=SMELT_VERSION_STRING={version_string}");

    // Rebuild when HEAD moves. `git rev-parse --git-path HEAD` resolves
    // to the right file for both regular checkouts and worktrees.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        let path = PathBuf::from(&head);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    // The dirty flag flips when tracked files change without a commit;
    // there's no single sentinel file to watch, so re-run every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TARGET");
}

/// Split a `git describe --tags --long --dirty` result like
/// `v0.5.0-alpha.2-80-g827e6646-dirty` into `(tag, commits, dirty, display)`.
/// `display` strips the leading `v` so it matches the format requested in
/// issue #9 (e.g. `0.5.1-60-g3349b5f-dirty`). A missing or unparseable
/// result falls back to `pkg_version`.
fn parse_describe(described: &str, pkg_version: &str) -> (String, String, String, String) {
    let (core, dirty_flag) = match described.strip_suffix("-dirty") {
        Some(rest) => (rest, "1"),
        None => (described, "0"),
    };
    // `core` looks like `<tag>-<commits>-g<sha>`. Tags may themselves
    // contain `-` (e.g. `v0.5.0-alpha.2`), so split from the right.
    let parts: Vec<&str> = core.rsplitn(3, '-').collect();
    let (tag, commits) = if parts.len() == 3 && parts[0].starts_with('g') {
        (parts[2].to_string(), parts[1].to_string())
    } else {
        return (
            "unknown".into(),
            "0".into(),
            dirty_flag.into(),
            pkg_version.into(),
        );
    };
    let display_core = core.strip_prefix('v').unwrap_or(core);
    let display = if dirty_flag == "1" {
        format!("{display_core}-dirty")
    } else {
        display_core.into()
    };
    (tag, commits, dirty_flag.into(), display)
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
