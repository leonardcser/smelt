//! Path manipulation primitives. Pure logic — does not touch the
//! filesystem except for `canonical`. Exposed to Lua via
//! `crates/tui/src/lua/api/path.rs` and composed by tools that need to
//! reason about workspace boundaries, display paths, or anchor a
//! relative reference.

use std::path::{Component, Path, PathBuf};

/// Collapse `.` and `..` components without touching the filesystem.
/// Leading `..` against a relative root are preserved (matches
/// `cargo`-style normalization, not `std::fs::canonicalize`). For
/// absolute paths, `..` past the root is dropped.
pub fn normalize(input: impl AsRef<Path>) -> PathBuf {
    let path = input.as_ref();
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => {
                out.push(p.as_os_str());
            }
            Component::RootDir => {
                out.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let pop = out
                    .components()
                    .next_back()
                    .is_some_and(|c| matches!(c, Component::Normal(_)));
                if pop {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(seg) => {
                out.push(seg);
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Resolve a path against the filesystem. Errors surface as
/// `std::io::Error` so callers (Lua bindings, tools) can decide how to
/// present them.
pub fn canonical(input: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(input)
}

/// Compute `target` relative to `base`. Pure path arithmetic — does not
/// resolve symlinks. If `target` lives outside `base`, the returned
/// path uses `..` to walk up. Both inputs are normalized first.
pub fn relative(base: impl AsRef<Path>, target: impl AsRef<Path>) -> PathBuf {
    let base = normalize(base.as_ref());
    let target = normalize(target.as_ref());

    let mut base_iter = base.components().peekable();
    let mut target_iter = target.components().peekable();

    while base_iter.peek().is_some() && base_iter.peek() == target_iter.peek() {
        base_iter.next();
        target_iter.next();
    }

    let mut out = PathBuf::new();
    for component in base_iter {
        if matches!(component, Component::Normal(_) | Component::ParentDir) {
            out.push("..");
        }
    }
    for component in target_iter {
        out.push(component.as_os_str());
    }

    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Expand a leading `~` to the user's home directory. Returns the
/// input unchanged when no home is available or the path does not
/// start with `~`.
pub fn expand_home(input: impl AsRef<Path>) -> PathBuf {
    let path = input.as_ref();
    let Some(rest) = path.strip_prefix("~").ok().or_else(|| {
        // Some callers pass `~/foo` as a single segment when constructing
        // via `PathBuf::from`. `strip_prefix` handles that for us above;
        // also handle the literal `~` case.
        if path == Path::new("~") {
            Some(Path::new(""))
        } else {
            None
        }
    }) else {
        return path.to_path_buf();
    };
    match dirs::home_dir() {
        Some(home) if rest.as_os_str().is_empty() => home,
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_curdir_and_parent() {
        assert_eq!(normalize("a/./b"), PathBuf::from("a/b"));
        assert_eq!(normalize("a/b/../c"), PathBuf::from("a/c"));
        assert_eq!(normalize("./a"), PathBuf::from("a"));
        assert_eq!(normalize(""), PathBuf::from("."));
    }

    #[test]
    fn normalize_keeps_leading_parent_for_relative() {
        assert_eq!(normalize("../a"), PathBuf::from("../a"));
        assert_eq!(normalize("../../a"), PathBuf::from("../../a"));
    }

    #[test]
    fn normalize_drops_parent_past_root_on_absolute() {
        assert_eq!(normalize("/a/../../b"), PathBuf::from("/b"));
    }

    #[test]
    fn relative_walks_up_when_target_outside_base() {
        assert_eq!(relative("/a/b/c", "/a/d/e"), PathBuf::from("../../d/e"));
    }

    #[test]
    fn relative_descends_when_target_inside_base() {
        assert_eq!(relative("/a/b", "/a/b/c/d"), PathBuf::from("c/d"));
    }

    #[test]
    fn relative_same_path_is_dot() {
        assert_eq!(relative("/a/b", "/a/b"), PathBuf::from("."));
    }

    #[test]
    fn expand_home_replaces_leading_tilde() {
        let home = dirs::home_dir().expect("test env has HOME");
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/projects"), home.join("projects"));
    }

    #[test]
    fn expand_home_is_passthrough_for_non_tilde() {
        assert_eq!(expand_home("/etc"), PathBuf::from("/etc"));
        assert_eq!(expand_home("relative/path"), PathBuf::from("relative/path"));
    }
}
