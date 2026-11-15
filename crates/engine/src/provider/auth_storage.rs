//! Shared credential storage for OAuth providers.
//! Credentials live in a private JSON file (0600 on unix). An environment
//! variable, when set, overrides the file.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct CredStore {
    pub(crate) file_path: PathBuf,
    env_var: Option<&'static str>,
}

/// Write `json` to `path`, creating parent dirs and applying 0600 perms on unix. Pure I/O.
pub(super) fn write_secure(path: &Path, json: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl CredStore {
    pub(crate) fn new(file_path: PathBuf, env_var: &'static str) -> Self {
        Self {
            file_path,
            env_var: Some(env_var),
        }
    }

    #[cfg(test)]
    pub(crate) fn file_only(file_path: PathBuf) -> Self {
        Self {
            file_path,
            env_var: None,
        }
    }

    pub(crate) fn save(&self, json: &str) -> Result<(), String> {
        write_secure(&self.file_path, json)
    }

    pub(crate) fn load(&self) -> Option<String> {
        if let Some(env_var) = self.env_var {
            if let Ok(json) = std::env::var(env_var) {
                return Some(json);
            }
        }
        std::fs::read_to_string(&self.file_path).ok()
    }

    pub(crate) fn delete(&self) {
        let _ = std::fs::remove_file(&self.file_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_secure_writes_content_to_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        write_secure(&path, "{\"token\":\"abc\"}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"token\":\"abc\"}"
        );
    }

    #[test]
    fn write_secure_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c").join("creds.json");
        write_secure(&nested, "payload").unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn write_secure_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        write_secure(&path, "first").unwrap();
        write_secure(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[cfg(unix)]
    #[test]
    fn write_secure_sets_0600_permissions_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        write_secure(&path, "secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn write_secure_errors_when_parent_cannot_be_created() {
        // Use an existing file as a "parent directory" to force create_dir_all to fail.
        let dir = tempdir().unwrap();
        let blocker = dir.path().join("file_not_dir");
        std::fs::write(&blocker, "x").unwrap();
        let path = blocker.join("creds.json");
        assert!(write_secure(&path, "y").is_err());
    }

    #[test]
    fn load_returns_env_var_when_set() {
        let dir = tempdir().unwrap();
        let store = CredStore::new(
            dir.path().join("creds.json"),
            "SMELT_TEST_AUTH_ENV_PRIORITY_A",
        );
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.set_var(store.env_var.unwrap(), "from-env");
        assert_eq!(store.load().as_deref(), Some("from-env"));
    }

    #[test]
    fn load_env_var_wins_over_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "from-file").unwrap();
        let store = CredStore::new(path, "SMELT_TEST_AUTH_ENV_PRIORITY_B");
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.set_var(store.env_var.unwrap(), "from-env");
        assert_eq!(store.load().as_deref(), Some("from-env"));
    }

    #[test]
    fn load_observes_file_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CredStore::file_only(path.clone());

        assert!(store.load().is_none());
        std::fs::write(&path, "first").unwrap();
        assert_eq!(store.load().as_deref(), Some("first"));
        std::fs::write(path, "second").unwrap();
        assert_eq!(store.load().as_deref(), Some("second"));
    }

    #[test]
    fn load_returns_none_when_env_unset_and_no_file() {
        let dir = tempdir().unwrap();
        let store = CredStore::new(dir.path().join("creds.json"), "SMELT_TEST_AUTH_ENV_UNSET_C");
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.remove_var(store.env_var.unwrap());
        assert!(store.load().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let store = CredStore::file_only(dir.path().join("auth").join("creds.json"));
        store.save("payload").unwrap();
        assert_eq!(store.load().as_deref(), Some("payload"));
    }

    #[test]
    fn delete_removes_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "x").unwrap();
        let store = CredStore::file_only(path.clone());
        store.delete();
        assert!(!path.exists());
    }

    #[test]
    fn delete_is_noop_when_file_missing() {
        let dir = tempdir().unwrap();
        let store = CredStore::file_only(dir.path().join("does_not_exist.json"));
        store.delete(); // Must not panic.
    }
}
