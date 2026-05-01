use crate::config;
use protocol::{Mode, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Toggle settings persisted across sessions. Each field is `Option<bool>`:
/// `Some(v)` = user explicitly toggled it, `None` = use config/default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_tps: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_tokens: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cost: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_prediction: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_slug: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restrict_to_workspace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact_secrets: Option<bool>,
}

impl PersistedSettings {
    /// Resolve against config defaults: state wins, then config, then hardcoded default.
    pub fn resolve(&self, cfg: &crate::config::SettingsConfig) -> ResolvedSettings {
        ResolvedSettings {
            vim: self.vim_mode.or(cfg.vim_mode).unwrap_or(false),
            auto_compact: self.auto_compact.or(cfg.auto_compact).unwrap_or(false),
            show_tps: self.show_tps.or(cfg.show_tps).unwrap_or(true),
            show_tokens: self.show_tokens.or(cfg.show_tokens).unwrap_or(true),
            show_cost: self.show_cost.or(cfg.show_cost).unwrap_or(true),
            show_prediction: self
                .input_prediction
                .or(cfg.input_prediction)
                .unwrap_or(true),
            show_slug: self.task_slug.or(cfg.task_slug).unwrap_or(true),
            show_thinking: self.show_thinking.or(cfg.show_thinking).unwrap_or(true),
            restrict_to_workspace: self
                .restrict_to_workspace
                .or(cfg.restrict_to_workspace)
                .unwrap_or(true),
            redact_secrets: self.redact_secrets.or(cfg.redact_secrets).unwrap_or(true),
        }
    }
}

/// Fully resolved boolean settings (no more Options).
#[derive(Debug, Clone)]
pub struct ResolvedSettings {
    pub vim: bool,
    pub auto_compact: bool,
    pub show_tps: bool,
    pub show_tokens: bool,
    pub show_cost: bool,
    pub show_prediction: bool,
    pub show_slug: bool,
    pub show_thinking: bool,
    pub restrict_to_workspace: bool,
    pub redact_secrets: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub(crate) mode: String,
    // Legacy field — migrated into `settings.vim_mode` on load.
    #[serde(default)]
    pub(crate) vim_enabled: bool,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub(crate) accent_color: Option<u8>,
    // Legacy field — migrated into `settings.show_thinking` on load.
    #[serde(default)]
    pub(crate) show_thinking: Option<bool>,
    #[serde(default)]
    pub settings: PersistedSettings,
}

fn state_path() -> PathBuf {
    config::state_dir().join("state.json")
}

fn state_lock_path() -> PathBuf {
    config::state_dir().join("state.lock")
}

impl State {
    pub fn load() -> Self {
        Self::load_from_disk()
    }

    fn load_from_disk() -> Self {
        let path = state_path();
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        Self::migrate_legacy_fields(serde_json::from_str(&contents).unwrap_or_default())
    }

    fn migrate_legacy_fields(mut s: Self) -> Self {
        if s.vim_enabled && s.settings.vim_mode.is_none() {
            s.settings.vim_mode = Some(true);
            s.vim_enabled = false;
        }
        if let Some(v) = s.show_thinking.take() {
            if s.settings.show_thinking.is_none() {
                s.settings.show_thinking = Some(v);
            }
        }
        s
    }

    fn save_unlocked(&self) {
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&path, &json);
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        Mode::parse(&self.mode).unwrap_or(Mode::Normal)
    }
}

struct StateLock(Option<std::fs::File>);

impl StateLock {
    fn acquire() -> Self {
        let path = state_lock_path();
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

impl Drop for StateLock {
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

fn update_state(f: impl FnOnce(&mut State)) {
    let _lock = StateLock::acquire();
    let mut s = State::load_from_disk();
    f(&mut s);
    s.save_unlocked();
}

// ── Read-modify-write helpers ─────────────────────────────────────────────

pub(crate) fn set_mode(mode: Mode) {
    update_state(|s| {
        s.mode = mode.as_str().to_string();
    });
}

pub(crate) fn set_selected_model(key: String) {
    update_state(|s| {
        s.selected_model = Some(key);
    });
}

pub(crate) fn set_reasoning_effort(effort: ReasoningEffort) {
    update_state(|s| {
        s.reasoning_effort = effort;
    });
}

/// Persist all toggle settings from the resolved values.
pub(crate) fn save_settings(resolved: &ResolvedSettings) {
    update_state(|s| {
        s.settings = PersistedSettings {
            vim_mode: Some(resolved.vim),
            auto_compact: Some(resolved.auto_compact),
            show_tps: Some(resolved.show_tps),
            show_tokens: Some(resolved.show_tokens),
            show_cost: Some(resolved.show_cost),
            input_prediction: Some(resolved.show_prediction),
            task_slug: Some(resolved.show_slug),
            show_thinking: Some(resolved.show_thinking),
            restrict_to_workspace: Some(resolved.restrict_to_workspace),
            redact_secrets: Some(resolved.redact_secrets),
        };
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

    fn sample_settings(vim: bool) -> ResolvedSettings {
        ResolvedSettings {
            vim,
            auto_compact: false,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: true,
            redact_secrets: true,
        }
    }

    #[test]
    fn load_migrates_legacy_fields() {
        with_test_state_dir(|| {
            let path = state_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                r#"{
  "vim_enabled": true,
  "show_thinking": false
}"#,
            )
            .unwrap();

            let state = State::load();
            assert_eq!(state.settings.vim_mode, Some(true));
            assert_eq!(state.settings.show_thinking, Some(false));
            assert!(!state.vim_enabled);
            assert_eq!(state.show_thinking, None);
        });
    }

    #[test]
    fn concurrent_updates_preserve_unrelated_fields() {
        with_test_state_dir(|| {
            let barrier = Arc::new(Barrier::new(2));

            let b1 = barrier.clone();
            let mode_thread = std::thread::spawn(move || {
                b1.wait();
                update_state(|state| {
                    state.mode = Mode::Apply.as_str().to_string();
                    std::thread::sleep(Duration::from_millis(50));
                });
            });

            let b2 = barrier.clone();
            let settings_thread = std::thread::spawn(move || {
                b2.wait();
                save_settings(&sample_settings(true));
            });

            mode_thread.join().unwrap();
            settings_thread.join().unwrap();

            let state = State::load();
            assert_eq!(state.mode(), Mode::Apply);
            assert_eq!(state.settings.vim_mode, Some(true));
        });
    }
}
