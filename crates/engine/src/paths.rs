use std::path::{Path, PathBuf};

const APP_NAME: &str = "smelt";

/// Expand a leading `~` or `~/` to the user's home directory.
/// Non-tilde paths are returned as-is.
pub fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        home_dir().join(rest)
    } else {
        p.to_path_buf()
    }
}

/// Replace a leading home directory prefix with `~`.
/// Paths outside the home directory are returned as-is.
pub fn collapse_tilde(p: &Path) -> PathBuf {
    let home = home_dir();
    if let Ok(rest) = p.strip_prefix(&home) {
        PathBuf::from("~").join(rest)
    } else {
        p.to_path_buf()
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join(APP_NAME)
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("state"))
        .join(APP_NAME)
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join(APP_NAME)
}

/// Detect the git repository root for the given directory.
/// Returns `None` if not in a git repo, or if the root is the home directory
/// or filesystem root (too broad to be useful as a workspace boundary).
pub fn git_root(cwd: &std::path::Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let home = home_dir();
    if root == home || root.as_os_str() == "/" {
        return None;
    }
    Some(root)
}

/// Detect the current git branch, if any.
pub fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_home_prefix() {
        let home = home_dir();
        assert_eq!(expand_tilde(Path::new("~/foo/bar")), home.join("foo/bar"));
    }

    #[test]
    fn expand_tilde_bare() {
        let home = home_dir();
        assert_eq!(expand_tilde(Path::new("~")), home);
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        assert_eq!(
            expand_tilde(Path::new("/usr/local/bin")),
            PathBuf::from("/usr/local/bin")
        );
    }

    #[test]
    fn expand_tilde_relative_unchanged() {
        assert_eq!(expand_tilde(Path::new("foo/bar")), PathBuf::from("foo/bar"));
    }

    #[test]
    fn collapse_tilde_under_home() {
        let home = home_dir();
        let p = home.join("projects/rust");
        assert_eq!(collapse_tilde(&p), PathBuf::from("~/projects/rust"));
    }

    #[test]
    fn collapse_tilde_home_itself() {
        let home = home_dir();
        assert_eq!(collapse_tilde(&home), PathBuf::from("~"));
    }

    #[test]
    fn collapse_tilde_outside_home() {
        assert_eq!(
            collapse_tilde(Path::new("/tmp/foo")),
            PathBuf::from("/tmp/foo")
        );
    }

    #[test]
    fn roundtrip_expand_collapse() {
        let original = Path::new("~/syncthing/vault");
        let expanded = expand_tilde(original);
        let collapsed = collapse_tilde(&expanded);
        assert_eq!(collapsed, original);
    }
}
