//! Process-level environment captured behind an indirection so callers
//! that make state-machine decisions don't touch `std::env` /
//! `std::process` directly. Pairs with [`crate::clock`]: clock answers
//! "what time is it"; this answers "who am I, where am I, how many
//! cores have I".
//!
//! Two impls are expected: [`RuntimeEnv::snapshot`] for production
//! (reads the real process env once at startup) and
//! [`RuntimeEnv::scripted`] for tests/headless drivers that need a
//! fully controlled environment.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::RwLock;

/// Snapshot of process-level environment captured at startup. Cheap to
/// share via `Arc`; mutations go through [`Self::set_cwd`].
#[derive(Debug)]
pub struct RuntimeEnv {
    pid: u32,
    home: PathBuf,
    config_dir: PathBuf,
    state_dir: PathBuf,
    cache_dir: PathBuf,
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    cwd: RwLock<PathBuf>,
    available_parallelism: NonZeroUsize,
}

impl RuntimeEnv {
    /// Capture the real process environment. Call once at startup.
    pub fn snapshot() -> Self {
        let home = crate::paths::home_dir();
        let config_dir = crate::paths::config_dir();
        let state_dir = crate::paths::state_dir();
        let cache_dir = crate::paths::cache_dir();
        let data_dir = crate::paths::data_dir();
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(std::env::temp_dir);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let available_parallelism =
            std::thread::available_parallelism().unwrap_or(NonZeroUsize::new(1).unwrap());
        Self {
            pid: std::process::id(),
            home,
            config_dir,
            state_dir,
            cache_dir,
            data_dir,
            runtime_dir,
            cwd: RwLock::new(cwd),
            available_parallelism,
        }
    }

    /// Construct a fully scripted env. Caller supplies every field so the
    /// resulting `RuntimeEnv` is reproducible across runs.
    #[allow(clippy::too_many_arguments)]
    pub fn scripted(
        pid: u32,
        home: PathBuf,
        config_dir: PathBuf,
        state_dir: PathBuf,
        cache_dir: PathBuf,
        data_dir: PathBuf,
        runtime_dir: PathBuf,
        cwd: PathBuf,
        available_parallelism: NonZeroUsize,
    ) -> Self {
        Self {
            pid,
            home,
            config_dir,
            state_dir,
            cache_dir,
            data_dir,
            runtime_dir,
            cwd: RwLock::new(cwd),
            available_parallelism,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    pub fn data_dir(&self) -> &PathBuf {
        &self.data_dir
    }

    pub fn runtime_dir(&self) -> &PathBuf {
        &self.runtime_dir
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.read().unwrap().clone()
    }

    /// Mutate the snapshot's working directory. Does not call
    /// `std::env::set_current_dir`; callers that want the real process
    /// cwd to follow must do it themselves.
    pub fn set_cwd(&self, new_cwd: PathBuf) {
        *self.cwd.write().unwrap() = new_cwd;
    }

    pub fn available_parallelism(&self) -> NonZeroUsize {
        self.available_parallelism
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_captures_real_pid_and_nonzero_parallelism() {
        let env = RuntimeEnv::snapshot();
        assert_eq!(env.pid(), std::process::id());
        assert!(env.available_parallelism().get() >= 1);
        assert!(!env.cwd().as_os_str().is_empty());
    }

    #[test]
    fn scripted_round_trips_fields() {
        let env = RuntimeEnv::scripted(
            42,
            PathBuf::from("/home/sim"),
            PathBuf::from("/home/sim/.config"),
            PathBuf::from("/home/sim/.state"),
            PathBuf::from("/home/sim/.cache"),
            PathBuf::from("/home/sim/.data"),
            PathBuf::from("/run/user/42"),
            PathBuf::from("/scenario/cwd"),
            NonZeroUsize::new(1).unwrap(),
        );
        assert_eq!(env.pid(), 42);
        assert_eq!(env.home(), &PathBuf::from("/home/sim"));
        assert_eq!(env.config_dir(), &PathBuf::from("/home/sim/.config"));
        assert_eq!(env.state_dir(), &PathBuf::from("/home/sim/.state"));
        assert_eq!(env.cache_dir(), &PathBuf::from("/home/sim/.cache"));
        assert_eq!(env.data_dir(), &PathBuf::from("/home/sim/.data"));
        assert_eq!(env.runtime_dir(), &PathBuf::from("/run/user/42"));
        assert_eq!(env.cwd(), PathBuf::from("/scenario/cwd"));
        assert_eq!(env.available_parallelism().get(), 1);
    }

    #[test]
    fn set_cwd_mutates_snapshot_only() {
        let env = RuntimeEnv::scripted(
            1,
            PathBuf::from("/h"),
            PathBuf::from("/h/.c"),
            PathBuf::from("/h/.s"),
            PathBuf::from("/h/.ca"),
            PathBuf::from("/h/.d"),
            PathBuf::from("/run/h"),
            PathBuf::from("/orig"),
            NonZeroUsize::new(1).unwrap(),
        );
        env.set_cwd(PathBuf::from("/new"));
        assert_eq!(env.cwd(), PathBuf::from("/new"));
    }
}
