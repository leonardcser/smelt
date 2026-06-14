//! Shared credential storage for OAuth providers.
//! Load priority: env var → OS keyring → on-disk JSON (0600). Save writes to both keyring and disk.

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct CredStore {
    pub(crate) keyring_service: Option<&'static str>,
    pub(crate) keyring_user: Option<&'static str>,
    pub(crate) file_path: PathBuf,
    pub(crate) env_var: Option<&'static str>,
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
    pub(crate) fn save(&self, json: &str) -> Result<(), String> {
        write_secure(&self.file_path, json)?;
        let _ = self.keyring_save(json);
        Ok(())
    }

    pub(crate) fn load(&self) -> Option<String> {
        if let Some(env_var) = self.env_var {
            if let Ok(json) = std::env::var(env_var) {
                return Some(json);
            }
        }
        if let Some(json) = self.keyring_load() {
            return Some(json);
        }
        std::fs::read_to_string(&self.file_path).ok()
    }

    pub(crate) fn delete(&self) {
        let _ = self.keyring_delete();
        let _ = std::fs::remove_file(&self.file_path);
    }

    fn keyring_save(&self, json: &str) -> Result<(), String> {
        let (Some(service), Some(user)) = (self.keyring_service, self.keyring_user) else {
            return Ok(());
        };
        let entry = keyring::Entry::new(service, user).map_err(|e| e.to_string())?;
        entry.set_password(json).map_err(|e| e.to_string())
    }

    fn keyring_load(&self) -> Option<String> {
        let (Some(service), Some(user)) = (self.keyring_service, self.keyring_user) else {
            return None;
        };
        let entry = keyring::Entry::new(service, user).ok()?;
        entry.get_password().ok()
    }

    fn keyring_delete(&self) -> Result<(), String> {
        let (Some(service), Some(user)) = (self.keyring_service, self.keyring_user) else {
            return Ok(());
        };
        let entry = keyring::Entry::new(service, user).map_err(|e| e.to_string())?;
        entry.delete_credential().map_err(|e| e.to_string())
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

    fn unique_store(file_path: PathBuf, env_var: &'static str) -> CredStore {
        // Use a keyring service/user pair the OS keyring almost certainly
        // doesn't know about so keyring_load returns None.
        CredStore {
            keyring_service: Some("smelt-test-nonexistent-service-xyzzy-9f3c"),
            keyring_user: Some("smelt-test-nonexistent-user-xyzzy-9f3c"),
            file_path,
            env_var: Some(env_var),
        }
    }

    #[test]
    fn load_returns_env_var_when_set() {
        let dir = tempdir().unwrap();
        let store = unique_store(
            dir.path().join("creds.json"),
            "SMELT_TEST_AUTH_ENV_PRIORITY_A",
        );
        // SAFETY: env-var mutation in tests; unique name avoids cross-test races.
        unsafe { std::env::set_var(store.env_var.unwrap(), "from-env") };
        let loaded = store.load();
        unsafe { std::env::remove_var(store.env_var.unwrap()) };
        assert_eq!(loaded.as_deref(), Some("from-env"));
    }

    #[test]
    fn load_env_var_wins_over_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "from-file").unwrap();
        let store = unique_store(path, "SMELT_TEST_AUTH_ENV_PRIORITY_B");
        unsafe { std::env::set_var(store.env_var.unwrap(), "from-env") };
        let loaded = store.load();
        unsafe { std::env::remove_var(store.env_var.unwrap()) };
        assert_eq!(loaded.as_deref(), Some("from-env"));
    }

    #[test]
    fn load_returns_none_when_env_unset_and_no_file() {
        let dir = tempdir().unwrap();
        let store = unique_store(dir.path().join("creds.json"), "SMELT_TEST_AUTH_ENV_UNSET_C");
        unsafe { std::env::remove_var(store.env_var.unwrap()) };
        assert!(store.load().is_none());
    }

    #[test]
    fn delete_removes_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "x").unwrap();
        let store = unique_store(path.clone(), "SMELT_TEST_AUTH_DELETE_D");
        store.delete();
        assert!(!path.exists());
    }

    #[test]
    fn delete_is_noop_when_file_missing() {
        let dir = tempdir().unwrap();
        let store = unique_store(
            dir.path().join("does_not_exist.json"),
            "SMELT_TEST_AUTH_DELETE_E",
        );
        store.delete(); // Must not panic.
    }
}
