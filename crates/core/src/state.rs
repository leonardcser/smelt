//! Recent-pick memory for model / mode / reasoning effort.
//!
//! Not config - config lives in `init.lua`. This is the "what was I
//! using last" memory, analogous to Vim's shada / Neovim's session
//! info: a fresh launch lands where you left off, while `init.lua`
//! still owns the actual configuration.

use crate::config;
use protocol::{AgentMode, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Recent {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
}

fn recent_path() -> PathBuf {
    config::state_dir().join("recent.json")
}

fn recent_lock_path() -> PathBuf {
    config::state_dir().join("recent.lock")
}

impl Recent {
    pub fn load() -> Self {
        let path = recent_path();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&contents).unwrap_or_default()
    }

    fn save_unlocked(&self) {
        let path = recent_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&path, &json);
        }
    }

    pub fn mode(&self) -> Option<AgentMode> {
        AgentMode::parse(&self.mode)
    }
}

struct RecentLock(Option<std::fs::File>);

impl RecentLock {
    fn acquire() -> Self {
        let path = recent_lock_path();
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

impl Drop for RecentLock {
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
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("recent"),
        std::process::id()
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn update_recent(f: impl FnOnce(&mut Recent)) {
    let _lock = RecentLock::acquire();
    let mut s = Recent::load();
    f(&mut s);
    s.save_unlocked();
}

pub fn set_mode(mode: AgentMode) {
    update_recent(|s| {
        s.mode = mode.as_str().to_string();
    });
}

pub fn set_selected_model(key: String) {
    update_recent(|s| {
        s.selected_model = Some(key);
    });
}

pub fn set_reasoning_effort(effort: ReasoningEffort) {
    update_recent(|s| {
        s.reasoning_effort = effort;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn with_test_state_dir<T>(f: impl FnOnce() -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_util::isolate_xdg_state(dir.path());
        f()
    }

    #[test]
    fn concurrent_updates_preserve_unrelated_fields() {
        with_test_state_dir(|| {
            let barrier = Arc::new(Barrier::new(2));

            let b1 = barrier.clone();
            let mode_thread = std::thread::spawn(move || {
                b1.wait();
                update_recent(|s| {
                    s.mode = AgentMode::parse("apply").unwrap().as_str().to_string();
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

            let recent = Recent::load();
            assert_eq!(recent.mode(), Some(AgentMode::parse("apply").unwrap()));
            assert_eq!(recent.selected_model.as_deref(), Some("anthropic/claude"));
        });
    }
}
