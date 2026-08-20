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
    collapse_tilde_from(p, &home_dir())
}

pub fn collapse_tilde_from(p: &Path, home: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix(home) {
        PathBuf::from("~").join(rest)
    } else {
        p.to_path_buf()
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg_app_dir(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|root| root.join(APP_NAME))
}

pub fn config_dir() -> PathBuf {
    xdg_app_dir("XDG_CONFIG_HOME").unwrap_or_else(default_config_dir)
}

pub fn state_dir() -> PathBuf {
    xdg_app_dir("XDG_STATE_HOME").unwrap_or_else(default_state_dir)
}

pub fn cache_dir() -> PathBuf {
    xdg_app_dir("XDG_CACHE_HOME").unwrap_or_else(default_cache_dir)
}

pub fn data_dir() -> PathBuf {
    xdg_app_dir("XDG_DATA_HOME").unwrap_or_else(default_data_dir)
}

#[cfg(windows)]
fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
        .join(APP_NAME)
}

#[cfg(not(windows))]
fn default_config_dir() -> PathBuf {
    home_dir().join(".config").join(APP_NAME)
}

#[cfg(windows)]
fn default_state_dir() -> PathBuf {
    windows_local_app_dir().join("state")
}

#[cfg(not(windows))]
fn default_state_dir() -> PathBuf {
    home_dir().join(".local").join("state").join(APP_NAME)
}

#[cfg(windows)]
fn default_cache_dir() -> PathBuf {
    windows_local_app_dir().join("cache")
}

#[cfg(not(windows))]
fn default_cache_dir() -> PathBuf {
    home_dir().join(".cache").join(APP_NAME)
}

#[cfg(windows)]
fn default_data_dir() -> PathBuf {
    windows_local_app_dir().join("data")
}

#[cfg(not(windows))]
fn default_data_dir() -> PathBuf {
    home_dir().join(".local").join("share").join(APP_NAME)
}

#[cfg(windows)]
fn windows_local_app_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
        .join(APP_NAME)
}

/// Detect the git repository root. Returns `None` outside a git repo, or when
/// the root is `~` or `/` (too broad to be a useful workspace boundary).
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

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn expand_tilde_home_prefix() {
        let home = home_dir();
        assert_eq!(expand_tilde(Path::new("~/foo/bar")), home.join("foo/bar"));
    }

    #[test]
    #[serial]
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
    #[serial]
    fn collapse_tilde_under_home() {
        let home = home_dir();
        let p = home.join("projects/rust");
        assert_eq!(collapse_tilde(&p), PathBuf::from("~/projects/rust"));
    }

    #[test]
    #[serial]
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
    #[serial]
    fn roundtrip_expand_collapse() {
        let original = Path::new("~/syncthing/vault");
        let expanded = expand_tilde(original);
        let collapsed = collapse_tilde(&expanded);
        assert_eq!(collapsed, original);
    }

    #[test]
    fn atomic_write_replaces_cache_without_leaving_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, b"old").unwrap();

        write_atomic(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    // ---- XDG-based dir helpers ----

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        for (name, value) in vars {
            match value {
                Some(value) => environment.set_var(name, value),
                None => environment.remove_var(name),
            }
        }
        f();
    }

    #[test]
    #[serial]
    fn home_dir_uses_home_env_when_set() {
        with_env(&[("HOME", Some("/tmp/test-home-abc"))], || {
            assert_eq!(home_dir(), PathBuf::from("/tmp/test-home-abc"));
        });
    }

    #[test]
    #[serial]
    fn home_dir_uses_platform_home_when_home_unset() {
        with_env(&[("HOME", None)], || {
            assert_eq!(
                home_dir(),
                dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
            );
        });
    }

    #[test]
    #[serial]
    fn empty_xdg_overrides_are_ignored() {
        with_env(
            &[
                ("XDG_CONFIG_HOME", Some("")),
                ("XDG_STATE_HOME", Some("")),
                ("XDG_CACHE_HOME", Some("")),
                ("XDG_DATA_HOME", Some("")),
                ("HOME", Some("/tmp/h")),
            ],
            || {
                assert_ne!(config_dir(), PathBuf::from("smelt"));
                assert_ne!(state_dir(), PathBuf::from("smelt"));
                assert_ne!(cache_dir(), PathBuf::from("smelt"));
                assert_ne!(data_dir(), PathBuf::from("smelt"));
            },
        );
    }

    #[test]
    #[serial]
    fn config_dir_uses_xdg_config_home_when_set() {
        with_env(&[("XDG_CONFIG_HOME", Some("/tmp/xdg-config"))], || {
            assert_eq!(config_dir(), PathBuf::from("/tmp/xdg-config/smelt"));
        });
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn config_dir_falls_back_to_home_dot_config_when_xdg_unset() {
        with_env(
            &[("XDG_CONFIG_HOME", None), ("HOME", Some("/tmp/h"))],
            || {
                assert_eq!(config_dir(), PathBuf::from("/tmp/h/.config/smelt"));
            },
        );
    }

    #[test]
    #[serial]
    fn state_dir_uses_xdg_state_home_when_set() {
        with_env(&[("XDG_STATE_HOME", Some("/tmp/state"))], || {
            assert_eq!(state_dir(), PathBuf::from("/tmp/state/smelt"));
        });
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn state_dir_falls_back_to_home_local_state_when_xdg_unset() {
        with_env(
            &[("XDG_STATE_HOME", None), ("HOME", Some("/tmp/h"))],
            || {
                assert_eq!(state_dir(), PathBuf::from("/tmp/h/.local/state/smelt"));
            },
        );
    }

    #[test]
    #[serial]
    fn cache_dir_uses_xdg_cache_home_when_set() {
        with_env(&[("XDG_CACHE_HOME", Some("/tmp/cache"))], || {
            assert_eq!(cache_dir(), PathBuf::from("/tmp/cache/smelt"));
        });
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn cache_dir_falls_back_to_home_cache_when_xdg_unset() {
        with_env(
            &[("XDG_CACHE_HOME", None), ("HOME", Some("/tmp/h"))],
            || {
                assert_eq!(cache_dir(), PathBuf::from("/tmp/h/.cache/smelt"));
            },
        );
    }

    #[test]
    #[serial]
    fn data_dir_uses_xdg_data_home_when_set() {
        with_env(&[("XDG_DATA_HOME", Some("/tmp/data"))], || {
            assert_eq!(data_dir(), PathBuf::from("/tmp/data/smelt"));
        });
    }

    #[cfg(not(windows))]
    #[test]
    #[serial]
    fn data_dir_falls_back_to_home_local_share_when_xdg_unset() {
        with_env(&[("XDG_DATA_HOME", None), ("HOME", Some("/tmp/h"))], || {
            assert_eq!(data_dir(), PathBuf::from("/tmp/h/.local/share/smelt"));
        });
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn directories_use_windows_known_folders_without_xdg_or_home() {
        with_env(
            &[
                ("HOME", None),
                ("XDG_CONFIG_HOME", None),
                ("XDG_STATE_HOME", None),
                ("XDG_CACHE_HOME", None),
                ("XDG_DATA_HOME", None),
            ],
            || {
                let home = home_dir();
                let roaming =
                    dirs::config_dir().unwrap_or_else(|| home.join("AppData").join("Roaming"));
                let local =
                    dirs::data_local_dir().unwrap_or_else(|| home.join("AppData").join("Local"));

                let local_app = local.join("smelt");
                assert_eq!(config_dir(), roaming.join("smelt"));
                assert_eq!(state_dir(), local_app.join("state"));
                assert_eq!(cache_dir(), local_app.join("cache"));
                assert_eq!(data_dir(), local_app.join("data"));
            },
        );
    }

    // ---- git_root / git_branch (best-effort, depends on the test workspace being a git repo) ----

    #[test]
    #[serial]
    fn git_root_returns_some_inside_repo_and_none_outside() {
        let cwd = std::env::current_dir().unwrap();
        // The crate's own repo; should return some path under the workspace.
        let root = git_root(&cwd);
        assert!(root.is_some());

        // Outside any git repo: a tempdir.
        let dir = tempfile::tempdir().unwrap();
        assert!(git_root(dir.path()).is_none());
    }

    #[test]
    #[serial]
    fn git_root_returns_none_when_root_equals_home() {
        // Spoof HOME to equal git's --show-toplevel by setting HOME to the actual git root.
        let cwd = std::env::current_dir().unwrap();
        let Some(actual_root) = git_root(&cwd) else {
            return; // Not a git repo in test env; skip.
        };
        with_env(&[("HOME", Some(actual_root.to_str().unwrap()))], || {
            assert!(git_root(&cwd).is_none());
        });
    }

    #[test]
    fn git_branch_returns_none_outside_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_branch(dir.path()).is_none());
    }
}
