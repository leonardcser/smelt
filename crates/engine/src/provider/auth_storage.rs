//! Shared credential storage for OAuth providers.
//! Load priority: env var → OS keyring → on-disk JSON (0600). Save writes to both keyring and disk.

use std::path::PathBuf;

pub(crate) struct CredStore {
    pub(crate) keyring_service: &'static str,
    pub(crate) keyring_user: &'static str,
    pub(crate) file_path: PathBuf,
    pub(crate) env_var: &'static str,
}

impl CredStore {
    pub(crate) fn save(&self, json: &str) -> Result<(), String> {
        self.file_save(json)?;
        let _ = self.keyring_save(json);
        Ok(())
    }

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
