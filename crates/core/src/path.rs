//! Path manipulation primitives. These avoid touching the filesystem except for
//! `canonical`; `expand` reads process environment variables and the home dir.

use std::path::{Component, Path, PathBuf};

/// Collapse `.` and `..` components without touching the filesystem.
/// Leading `..` against a relative root are preserved (matches
/// `cargo`-style normalization, not `std::fs::canonicalize`). For
/// absolute paths, `..` past the root is dropped.
pub(crate) fn normalize(input: impl AsRef<Path>) -> PathBuf {
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

pub(crate) fn canonical(input: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(input)
}

/// Compute `target` relative to `base` (pure arithmetic, no symlink resolution).
/// Uses `..` when `target` is outside `base`. Both inputs are normalized first.
pub(crate) fn relative(base: impl AsRef<Path>, target: impl AsRef<Path>) -> PathBuf {
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

/// Expand config-path syntax without touching the filesystem: leading `~`,
/// `$VAR`, and `${VAR}`. This does not invoke a shell, expand globs, or
/// canonicalize symlinks.
pub fn expand(input: impl AsRef<Path>) -> Result<PathBuf, String> {
    let raw = input.as_ref().to_string_lossy();
    let expanded = shellexpand::full(raw.as_ref()).map_err(|err| {
        let name = err.var_name.to_string();
        format!("environment variable {name} is not set")
    })?;
    Ok(normalize(Path::new(expanded.as_ref())))
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
    fn expand_config_path_expands_home() {
        let home = dirs::home_dir().expect("test env has HOME");
        assert_eq!(expand("~").unwrap(), home);
        assert_eq!(expand("~/projects").unwrap(), home.join("projects"));
    }

    #[test]
    fn expand_config_path_expands_home_and_env() {
        std::env::set_var("SMELT_PATH_TEST_ROOT", "/tmp/smelt-path-test");
        assert_eq!(
            expand("$SMELT_PATH_TEST_ROOT/foo/../bar").unwrap(),
            PathBuf::from("/tmp/smelt-path-test/bar")
        );
        assert_eq!(
            expand("${SMELT_PATH_TEST_ROOT}/nested").unwrap(),
            PathBuf::from("/tmp/smelt-path-test/nested")
        );
    }

    #[test]
    fn expand_config_path_errors_for_missing_env() {
        std::env::remove_var("SMELT_PATH_TEST_MISSING");
        let err = expand("$SMELT_PATH_TEST_MISSING/worktrees").unwrap_err();
        assert!(err.contains("SMELT_PATH_TEST_MISSING"));
    }

    #[test]
    fn expand_config_path_normalizes_relative_paths() {
        assert_eq!(expand("foo/../bar").unwrap(), PathBuf::from("bar"));
    }

    #[test]
    fn expand_config_path_is_passthrough_for_non_expanding_paths() {
        assert_eq!(expand("/etc").unwrap(), PathBuf::from("/etc"));
        assert_eq!(
            expand("relative/path").unwrap(),
            PathBuf::from("relative/path")
        );
    }
}
