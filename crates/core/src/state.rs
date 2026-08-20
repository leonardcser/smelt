//! Recent-pick memory for model / mode / reasoning effort.
//!
//! Not config - config lives in `init.lua`. This is the "what was I
//! using last" memory, analogous to Vim's shada / Neovim's session
//! info: a fresh launch lands where you left off, while `init.lua`
//! still owns the actual configuration.

use protocol::{AgentMode, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Recent {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone)]
pub struct RecentStore {
    state_root: PathBuf,
}

impl RecentStore {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
        }
    }

    pub fn from_env(env: &engine::env::RuntimeEnv) -> Self {
        Self::new(env.state_dir().clone())
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn load(&self) -> Recent {
        read_recent(&self.state_root).unwrap_or_default()
    }

    pub fn set_mode(&self, mode: AgentMode) -> io::Result<()> {
        self.update(|recent| {
            recent.mode = mode.as_str().to_string();
        })
    }

    pub fn set_selected_model(&self, key: String) -> io::Result<()> {
        self.update(|recent| {
            recent.selected_model = Some(key);
        })
    }

    pub fn set_reasoning_effort(&self, effort: ReasoningEffort) -> io::Result<()> {
        self.update(|recent| {
            recent.reasoning_effort = Some(effort);
        })
    }

    fn update(&self, f: impl FnOnce(&mut Recent)) -> io::Result<()> {
        let _lock = RecentLock::acquire(&self.state_root)?;
        let mut recent = read_recent(&self.state_root)?;
        f(&mut recent);
        recent.save_unlocked(&self.state_root)
    }
}

fn recent_path(state_root: &Path) -> PathBuf {
    state_root.join("recent.json")
}

fn recent_lock_path(state_root: &Path) -> PathBuf {
    state_root.join("recent.lock")
}

fn path_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

fn read_recent(state_root: &Path) -> io::Result<Recent> {
    let path = recent_path(state_root);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Recent::default()),
        Err(error) => return Err(path_error("read", &path, error)),
    };
    Ok(serde_json::from_str(&contents).unwrap_or_default())
}

impl Recent {
    fn save_unlocked(&self, state_root: &Path) -> io::Result<()> {
        let path = recent_path(state_root);
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::other(format!("serialize {}: {error}", path.display())))?;
        write_atomic(&path, &json).map_err(|error| path_error("write", &path, error))
    }

    pub fn mode(&self) -> Option<AgentMode> {
        AgentMode::parse(&self.mode)
    }
}

struct RecentLock {
    _file: std::fs::File,
}

impl RecentLock {
    fn acquire(state_root: &Path) -> io::Result<Self> {
        let path = recent_lock_path(state_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| path_error("create directory", parent, error))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| path_error("open lock", &path, error))?;

        file.lock()
            .map_err(|error| path_error("lock", &path, error))?;

        Ok(Self { _file: file })
    }
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    crate::fs::write_atomic(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn with_test_state_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        f(dir.path())
    }

    #[test]
    fn reasoning_memory_distinguishes_absent_from_explicit_off() {
        let absent: Recent = serde_json::from_str("{}").unwrap();
        let explicit: Recent = serde_json::from_str(r#"{"reasoning_effort":"off"}"#).unwrap();

        assert_eq!(absent.reasoning_effort, None);
        assert_eq!(explicit.reasoning_effort, Some(ReasoningEffort::Off));
    }

    #[test]
    fn concurrent_updates_preserve_unrelated_fields() {
        with_test_state_dir(|state_root| {
            let barrier = Arc::new(Barrier::new(2));
            let store = RecentStore::new(state_root);

            let b1 = barrier.clone();
            let mode_store = store.clone();
            let mode_thread = std::thread::spawn(move || {
                b1.wait();
                mode_store.update(|recent| {
                    recent.mode = AgentMode::parse("apply").unwrap().as_str().to_string();
                    std::thread::sleep(Duration::from_millis(50));
                })
            });

            let b2 = barrier.clone();
            let model_store = store.clone();
            let model_thread = std::thread::spawn(move || {
                b2.wait();
                model_store.set_selected_model("anthropic/claude".to_string())
            });

            mode_thread.join().unwrap().unwrap();
            model_thread.join().unwrap().unwrap();

            let recent = store.load();
            assert_eq!(recent.mode(), Some(AgentMode::parse("apply").unwrap()));
            assert_eq!(recent.selected_model.as_deref(), Some("anthropic/claude"));
        });
    }

    #[test]
    fn persistence_failures_are_returned() {
        with_test_state_dir(|state_root| {
            std::fs::create_dir(state_root.join("recent.lock")).unwrap();
            let store = RecentStore::new(state_root);

            let error = store
                .set_selected_model("anthropic/claude".to_string())
                .unwrap_err();

            assert!(error.to_string().contains("open lock"));
            assert!(error.to_string().contains("recent.lock"));
        });
    }
}
