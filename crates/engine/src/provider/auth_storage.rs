//! Shared credential storage for OAuth providers.
//!
//! Persists a JSON blob through three layers, in priority order on load:
//! 1. Environment variable (for child processes receiving creds from a parent)
//! 2. OS keyring (secure, preferred)
//! 3. On-disk JSON file with `0600` perms (fallback when keyring is unavailable)
//!
//! On save, writes to both keyring and disk (keyring failures are ignored so
//! the user isn't locked out when the OS service is flaky).

use std::path::PathBuf;

/// Credential persistence backend parameterized by provider-specific addresses.
pub(crate) struct CredStore {
    pub keyring_service: &'static str,
    pub keyring_user: &'static str,
    pub file_path: PathBuf,
    /// Environment variable checked first on load. Lets a parent process pass
    /// credentials to a child without touching disk.
    pub env_var: &'static str,
}

impl CredStore {
    pub(crate) fn save(&self, json: &str) -> Result<(), String> {
        self.file_save(json)?;
        let _ = self.keyring_save(json);
        Ok(())
    }

    /// Try env var → keyring → file, returning the first JSON blob found.
    pub(crate) fn load(&self) -> Option<String> {
        if let Ok(json) = std::env::var(self.env_var) {
            return Some(json);
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

    fn file_save(&self, json: &str) -> Result<(), String> {
        if let Some(parent) = self.file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.file_path, json).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.file_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn keyring_save(&self, json: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(self.keyring_service, self.keyring_user)
            .map_err(|e| e.to_string())?;
        entry.set_password(json).map_err(|e| e.to_string())
    }

    fn keyring_load(&self) -> Option<String> {
        let entry = keyring::Entry::new(self.keyring_service, self.keyring_user).ok()?;
        entry.get_password().ok()
    }

    fn keyring_delete(&self) -> Result<(), String> {
        let entry = keyring::Entry::new(self.keyring_service, self.keyring_user)
            .map_err(|e| e.to_string())?;
        entry.delete_credential().map_err(|e| e.to_string())
    }
}
