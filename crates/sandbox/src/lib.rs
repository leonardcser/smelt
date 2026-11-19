//! Run commands so they can write only inside chosen directories and cannot
//! see hidden paths.
//!
//! The network and other processes stay reachable. Writes are allowed only
//! beneath [`Policy::writable`]. Paths in [`Policy::hidden`] can be neither
//! read nor written, except the [`Policy::visible`] paths inside them. The
//! kernel enforces the policy, and every child process inherits it. The
//! [`Policy::hidden_env`] variables are removed from the environment.
//!
//! Engines:
//! - Linux: bubblewrap (`bwrap`), which mounts the file system read-only,
//!   mounts the writable directories read-write, and mounts an empty,
//!   unreadable entry over each hidden path.
//! - macOS: Seatbelt, by running the program under `/usr/bin/sandbox-exec`.
//!
//! Without an engine, [`backend`] reports why, and [`command`] fails.

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "macos")]
mod seatbelt;

#[cfg(target_os = "linux")]
use bwrap::Bwrap as Engine;
#[cfg(target_os = "macos")]
use seatbelt::Seatbelt as Engine;

/// The engine that enforces the sandbox on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Bubblewrap,
    Seatbelt,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Bubblewrap => "bwrap",
            Backend::Seatbelt => "seatbelt",
        }
    }
}

/// What a sandboxed command may touch. Paths that do not exist are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Directories (or files) the command may create, change and delete
    /// entries beneath. `None` leaves writes unrestricted.
    pub writable: Option<Vec<PathBuf>>,
    /// Paths the command can neither read nor write.
    pub hidden: Vec<PathBuf>,
    /// Paths beneath `hidden` ones that stay readable, and writable when
    /// they are also beneath `writable`.
    pub visible: Vec<PathBuf>,
    /// Environment variables the command does not get, such as API keys.
    pub hidden_env: Vec<String>,
}

/// The engine on this machine, or why there is none. The probe runs once
/// per process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn backend() -> Result<Backend, String> {
    #[cfg(target_os = "linux")]
    const BACKEND: Backend = Backend::Bubblewrap;
    #[cfg(target_os = "macos")]
    const BACKEND: Backend = Backend::Seatbelt;
    engine().map(|_| BACKEND)
}

/// The engine on this machine, or why there is none.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn backend() -> Result<Backend, String> {
    Err("no sandbox for this platform".into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn engine() -> Result<&'static Engine, String> {
    static ENGINE: std::sync::OnceLock<Result<Engine, String>> = std::sync::OnceLock::new();
    ENGINE
        .get_or_init(Engine::probe)
        .as_ref()
        .map_err(Clone::clone)
}

/// A command that runs `program` inside the sandbox described by `policy`.
/// Add arguments, environment and stdio to it as usual. Fails when no engine
/// is available, so a caller can never run unsandboxed by accident.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn command(program: impl AsRef<OsStr>, policy: &Policy) -> io::Result<Command> {
    let engine = engine().map_err(io::Error::other)?;
    let canonical = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths
            .iter()
            .filter_map(|path| path.canonicalize().ok())
            .collect()
    };
    let canonical_policy = Policy {
        writable: policy.writable.as_deref().map(canonical),
        hidden: canonical(&policy.hidden),
        visible: canonical(&policy.visible),
        hidden_env: Vec::new(),
    };
    let mut command = engine.command(program.as_ref(), &canonical_policy);
    for name in &policy.hidden_env {
        command.env_remove(name);
    }
    Ok(command)
}

/// A command that runs `program` inside the sandbox described by `policy`.
/// Always fails: this platform has no engine.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn command(_program: impl AsRef<OsStr>, _policy: &Policy) -> io::Result<Command> {
    Err(io::Error::other(backend().unwrap_err()))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
