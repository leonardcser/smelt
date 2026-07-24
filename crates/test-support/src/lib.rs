use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static PROCESS_ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

/// Exclusive access to process-wide environment variables and the current directory.
///
/// Every mutation is restored when the guard is dropped. Tests should keep one guard
/// for the complete lifetime of code that may read the modified process state.
pub struct ProcessEnvironmentGuard {
    original_cwd: PathBuf,
    original_pwd: Option<OsString>,
    original_variables: RefCell<Vec<(OsString, Option<OsString>)>>,
    _lock: MutexGuard<'static, ()>,
}

impl ProcessEnvironmentGuard {
    pub fn capture() -> Self {
        let lock = PROCESS_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self {
            original_cwd: std::env::current_dir().expect("capture process cwd"),
            original_pwd: std::env::var_os("PWD"),
            original_variables: RefCell::new(Vec::new()),
            _lock: lock,
        }
    }

    pub fn set_var(&self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        let name = name.as_ref();
        self.remember_var(name);
        std::env::set_var(name, value);
    }

    pub fn remove_var(&self, name: impl AsRef<OsStr>) {
        let name = name.as_ref();
        self.remember_var(name);
        std::env::remove_var(name);
    }

    pub fn set_current_dir(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::env::set_current_dir(path)
    }

    fn remember_var(&self, name: &OsStr) {
        let mut originals = self.original_variables.borrow_mut();
        if originals.iter().any(|(recorded, _)| recorded == name) {
            return;
        }
        originals.push((name.to_os_string(), std::env::var_os(name)));
    }
}

impl Drop for ProcessEnvironmentGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        for (name, value) in self.original_variables.get_mut().drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        match &self.original_pwd {
            Some(pwd) => std::env::set_var("PWD", pwd),
            None => std::env::remove_var("PWD"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessEnvironmentGuard;

    #[test]
    fn restores_variables_and_current_directory() {
        const NAME: &str = "SMELT_TEST_SUPPORT_RESTORE";
        let original_value = std::env::var_os(NAME);
        let original_cwd = std::env::current_dir().unwrap();
        let original_pwd = std::env::var_os("PWD");
        let dir = std::env::temp_dir();
        {
            let guard = ProcessEnvironmentGuard::capture();
            guard.set_var(NAME, "changed");
            guard.set_current_dir(&dir).unwrap();
            std::env::set_var("PWD", &dir);
            assert_eq!(std::env::var(NAME).as_deref(), Ok("changed"));
            assert_eq!(std::env::current_dir().unwrap(), dir);
        }
        assert_eq!(std::env::var_os(NAME), original_value);
        assert_eq!(std::env::current_dir().unwrap(), original_cwd);
        assert_eq!(std::env::var_os("PWD"), original_pwd);
    }
}
