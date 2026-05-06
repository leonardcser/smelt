//! Cross-session cache for last-used picks (model, mode, reasoning effort,
//! accent). Genuinely cache-shaped: nothing here is config — config lives
//! in `init.lua`. The cache only remembers what the user picked last so a
//! fresh launch lands where they left off.

use crate::config;
use protocol::{AgentMode, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionCache {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

fn cache_path() -> PathBuf {
    config::state_dir().join("state.json")
}

fn cache_lock_path() -> PathBuf {
    config::state_dir().join("state.lock")
}

impl SessionCache {
    pub fn load() -> Self {
        let path = cache_path();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }

    fn save_unlocked(&self) {
        let path = cache_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&path, &json);
        }
    }

    pub fn mode(&self) -> AgentMode {
        AgentMode::parse(&self.mode).unwrap_or(AgentMode::Normal)
    }
}

struct CacheLock(Option<std::fs::File>);

impl CacheLock {
    fn acquire() -> Self {
        let path = cache_lock_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .ok();

        #[cfg(unix)]
        if let Some(ref f) = file {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(f.as_raw_fd(), libc::LOCK_EX);
            }
        }

        Self(file)
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(ref f) = self.0 {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(f.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return std::fs::write(path, contents);
    };
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("state"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn update_cache(f: impl FnOnce(&mut SessionCache)) {
    let _lock = CacheLock::acquire();
    let mut s = SessionCache::load();
    f(&mut s);
    s.save_unlocked();
}

pub fn set_mode(mode: AgentMode) {
    update_cache(|s| {
        s.mode = mode.as_str().to_string();
    });
}

pub fn set_selected_model(key: String) {
    update_cache(|s| {
        s.selected_model = Some(key);
    });
}

pub fn set_reasoning_effort(effort: ReasoningEffort) {
    update_cache(|s| {
        s.reasoning_effort = effort;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex, OnceLock};
    use std::time::Duration;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_test_state_dir<T>(f: impl FnOnce() -> T) -> T {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", dir.path());
        let result = f();
        if let Some(old) = old {
            std::env::set_var("XDG_STATE_HOME", old);
        } else {
            std::env::remove_var("XDG_STATE_HOME");
        }
        result
    }

    #[test]
    fn concurrent_updates_preserve_unrelated_fields() {
        with_test_state_dir(|| {
            let barrier = Arc::new(Barrier::new(2));

            let b1 = barrier.clone();
            let mode_thread = std::thread::spawn(move || {
                b1.wait();
                update_cache(|s| {
                    s.mode = AgentMode::Apply.as_str().to_string();
                    std::thread::sleep(Duration::from_millis(50));
                });
            });

            let b2 = barrier.clone();
            let model_thread = std::thread::spawn(move || {
                b2.wait();
                set_selected_model("anthropic/claude".to_string());
            });

            mode_thread.join().unwrap();
            model_thread.join().unwrap();

            let cache = SessionCache::load();
            assert_eq!(cache.mode(), AgentMode::Apply);
            assert_eq!(cache.selected_model.as_deref(), Some("anthropic/claude"));
        });
    }
}
