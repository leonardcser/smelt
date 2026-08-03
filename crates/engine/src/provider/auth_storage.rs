//! Shared credential storage for OAuth providers.
//! Load priority: env var → on-disk JSON (0600) → OS keyring. Passive loads skip the keyring.
//! Save writes to both disk and keyring.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

#[derive(Clone)]
struct KeyringTarget {
    service: &'static str,
    user: &'static str,
}

#[derive(Clone, Default)]
enum KeyringRead {
    #[default]
    Unread,
    Complete(Option<String>),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
pub(crate) struct CredStore {
    keyring: Option<KeyringTarget>,
    keyring_read: Arc<Mutex<KeyringRead>>,
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

fn native_credential_store() -> Result<&'static Arc<keyring_core::CredentialStore>, String> {
    static STORE: OnceLock<Result<Arc<keyring_core::CredentialStore>, String>> = OnceLock::new();

    STORE
        .get_or_init(initialize_native_credential_store)
        .as_ref()
        .map_err(Clone::clone)
}

fn initialize_native_credential_store() -> Result<Arc<keyring_core::CredentialStore>, String> {
    #[cfg(target_os = "macos")]
    {
        let store: Arc<keyring_core::CredentialStore> =
            apple_native_keyring_store::keychain::Store::new().map_err(|err| err.to_string())?;
        Ok(store)
    }
    #[cfg(target_os = "windows")]
    {
        let store: Arc<keyring_core::CredentialStore> =
            windows_native_keyring_store::Store::new().map_err(|err| err.to_string())?;
        Ok(store)
    }
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    ))]
    {
        let store: Arc<keyring_core::CredentialStore> =
            zbus_secret_service_keyring_store::Store::new().map_err(|err| err.to_string())?;
        Ok(store)
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(
            unix,
            not(any(target_os = "macos", target_os = "ios", target_os = "android"))
        )
    )))]
    {
        Err("no native credential store is available on this platform".to_string())
    }
}

fn keyring_entry(target: &KeyringTarget) -> Result<keyring_core::Entry, String> {
    native_credential_store()?
        .build(target.service, target.user, None)
        .map_err(|err| err.to_string())
}

impl CredStore {
    pub(crate) fn production(
        service: &'static str,
        user: &'static str,
        file_path: PathBuf,
        env_var: &'static str,
    ) -> Self {
        Self {
            keyring: Some(KeyringTarget { service, user }),
            keyring_read: Arc::default(),
            file_path,
            env_var: Some(env_var),
        }
    }

    #[cfg(test)]
    pub(crate) fn file_only(file_path: PathBuf) -> Self {
        Self {
            keyring: None,
            keyring_read: Arc::default(),
            file_path,
            env_var: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn env_only(file_path: PathBuf, env_var: &'static str) -> Self {
        Self {
            keyring: None,
            keyring_read: Arc::default(),
            file_path,
            env_var: Some(env_var),
        }
    }

    pub(crate) fn save(&self, json: &str) -> Result<(), String> {
        write_secure(&self.file_path, json)?;
        let _ = self.keyring_save(json);
        Ok(())
    }

    pub(crate) fn load_passive(&self) -> Option<String> {
        if let Some(env_var) = self.env_var {
            if let Ok(json) = std::env::var(env_var) {
                return Some(json);
            }
        }
        std::fs::read_to_string(&self.file_path).ok()
    }

    pub(crate) fn load(&self) -> Option<String> {
        self.load_with_keyring(|| self.keyring_load())
    }

    fn load_with_keyring(&self, keyring_load: impl FnOnce() -> Option<String>) -> Option<String> {
        if let Some(json) = self.load_passive() {
            return Some(json);
        }

        let json = keyring_load()?;
        let _ = write_secure(&self.file_path, &json);
        Some(json)
    }

    pub(crate) fn delete(&self) {
        let _ = self.keyring_delete();
        let _ = std::fs::remove_file(&self.file_path);
    }

    fn keyring_save(&self, json: &str) -> Result<(), String> {
        let Some(keyring) = &self.keyring else {
            return Ok(());
        };
        let mut read = lock(&self.keyring_read);
        let result = keyring_entry(keyring)
            .and_then(|entry| entry.set_password(json).map_err(|error| error.to_string()));
        *read = KeyringRead::Complete(result.as_ref().ok().map(|()| json.to_string()));
        result
    }

    fn keyring_load(&self) -> Option<String> {
        self.keyring_load_with(|keyring| {
            let entry = keyring_entry(keyring).ok()?;
            entry.get_password().ok()
        })
    }

    fn keyring_load_with(
        &self,
        load: impl FnOnce(&KeyringTarget) -> Option<String>,
    ) -> Option<String> {
        let keyring = self.keyring.as_ref()?;
        let mut read = lock(&self.keyring_read);
        if let KeyringRead::Complete(password) = &*read {
            return password.clone();
        }

        let password = load(keyring);
        *read = KeyringRead::Complete(password.clone());
        password
    }

    fn keyring_delete(&self) -> Result<(), String> {
        let Some(keyring) = &self.keyring else {
            return Ok(());
        };
        let mut read = lock(&self.keyring_read);
        let result = keyring_entry(keyring)
            .and_then(|entry| entry.delete_credential().map_err(|error| error.to_string()));
        *read = KeyringRead::Complete(None);
        result
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
        CredStore::env_only(file_path, env_var)
    }

    fn keyring_store(file_path: PathBuf) -> CredStore {
        CredStore {
            keyring: Some(KeyringTarget {
                service: "smelt-test-keyring",
                user: "default",
            }),
            keyring_read: Arc::default(),
            file_path,
            env_var: None,
        }
    }

    #[test]
    fn load_returns_env_var_when_set() {
        let dir = tempdir().unwrap();
        let store = unique_store(
            dir.path().join("creds.json"),
            "SMELT_TEST_AUTH_ENV_PRIORITY_A",
        );
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.set_var(store.env_var.unwrap(), "from-env");
        let loaded = store.load();
        assert_eq!(loaded.as_deref(), Some("from-env"));
    }

    #[test]
    fn load_env_var_wins_over_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "from-file").unwrap();
        let store = unique_store(path, "SMELT_TEST_AUTH_ENV_PRIORITY_B");
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.set_var(store.env_var.unwrap(), "from-env");
        let loaded = store.load();
        assert_eq!(loaded.as_deref(), Some("from-env"));
    }

    #[test]
    fn load_returns_file_without_accessing_keyring() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, "from-file").unwrap();
        let store = CredStore::file_only(path);

        let loaded = store.load_with_keyring(|| panic!("keyring should not be accessed"));

        assert_eq!(loaded.as_deref(), Some("from-file"));
    }

    #[test]
    fn load_imports_keyring_credential_into_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CredStore::file_only(path.clone());

        let loaded = store.load_with_keyring(|| Some("from-keyring".to_string()));
        assert_eq!(loaded.as_deref(), Some("from-keyring"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "from-keyring");

        let loaded = store.load_with_keyring(|| panic!("keyring should not be accessed again"));
        assert_eq!(loaded.as_deref(), Some("from-keyring"));
    }

    #[test]
    fn passive_load_observes_file_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        let store = CredStore::file_only(path.clone());

        assert!(store.load_passive().is_none());
        std::fs::write(&path, "first").unwrap();
        assert_eq!(store.load_passive().as_deref(), Some("first"));
        std::fs::write(path, "second").unwrap();
        assert_eq!(store.load_passive().as_deref(), Some("second"));
    }

    #[test]
    fn passive_load_never_uses_cached_keyring_credential() {
        let dir = tempdir().unwrap();
        let store = keyring_store(dir.path().join("creds.json"));
        *lock(&store.keyring_read) = KeyringRead::Complete(Some("from-keyring".to_string()));

        assert!(store.load_passive().is_none());
    }

    #[test]
    fn failed_keyring_read_is_cached_across_store_clones() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempdir().unwrap();
        let store = keyring_store(dir.path().join("creds.json"));
        let clone = store.clone();
        let attempts = AtomicUsize::new(0);

        for store in [&store, &clone, &store] {
            assert!(store
                .keyring_load_with(|_| {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    None
                })
                .is_none());
        }

        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn successful_keyring_read_is_cached_when_file_migration_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempdir().unwrap();
        let blocker = dir.path().join("file_not_dir");
        std::fs::write(&blocker, "x").unwrap();
        let store = keyring_store(blocker.join("creds.json"));
        let attempts = AtomicUsize::new(0);

        for _ in 0..3 {
            let loaded = store.load_with_keyring(|| {
                store.keyring_load_with(|_| {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    Some("from-keyring".to_string())
                })
            });
            assert_eq!(loaded.as_deref(), Some("from-keyring"));
        }

        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn load_returns_none_when_env_unset_and_no_file() {
        let dir = tempdir().unwrap();
        let store = unique_store(dir.path().join("creds.json"), "SMELT_TEST_AUTH_ENV_UNSET_C");
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        environment.remove_var(store.env_var.unwrap());
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
