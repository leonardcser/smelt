//! Public runtime status. Each process rewrites `<pid>.json` when its status
//! changes and holds an exclusive lock on `<pid>.lock` for its whole life. The
//! OS releases the lock however the process exits, so the lock, not a
//! timestamp or PID, proves liveness and an idle process never has to wake up.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicState {
    Idle,
    Busy,
    Waiting,
    Completed,
    NeedsAttention,
}

impl PublicState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicReason {
    Permission,
    Question,
    TurnComplete,
    Error,
    Auth,
    Setup,
    Interrupted,
}

impl PublicReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::TurnComplete => "turn_complete",
            Self::Error => "error",
            Self::Auth => "auth",
            Self::Setup => "setup",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusState {
    Focused,
    Unfocused,
    Unknown,
}

impl FocusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::Unfocused => "unfocused",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicStatus {
    pub schema: u32,
    pub app: String,
    pub pid: u32,
    pub state: PublicState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<PublicReason>,
    pub focus: FocusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default)]
    pub headless: bool,
    pub updated_at_ms: u64,
}

pub struct StatusPublisher {
    pid: u32,
    path: PathBuf,
    lock_path: PathBuf,
    _lock: fs::File,
    last: Option<StatusUpdate>,
}

impl StatusPublisher {
    pub fn new() -> io::Result<Self> {
        Self::new_in(&runtime_root(), std::process::id())
    }

    pub fn new_in(runtime_root: &Path, pid: u32) -> io::Result<Self> {
        let dir = status_dir_in(runtime_root);
        fs::create_dir_all(&dir)?;
        let lock_path = dir.join(format!("{pid}.lock"));
        Ok(Self {
            pid,
            path: dir.join(format!("{pid}.json")),
            _lock: acquire_lifetime_lock(&lock_path)?,
            lock_path,
            last: None,
        })
    }

    pub fn publish(&mut self, update: StatusUpdate) -> io::Result<()> {
        if self.last.as_ref() == Some(&update) {
            return Ok(());
        }
        let status = PublicStatus {
            schema: SCHEMA_VERSION,
            app: "smelt".to_string(),
            pid: self.pid,
            state: update.state,
            reason: update.reason,
            focus: update.focus,
            cwd: update.cwd.clone(),
            session_id: update.session_id.clone(),
            mode: update.mode.clone(),
            headless: update.headless,
            updated_at_ms: unix_ms(),
        };
        write_status_atomic(&self.path, &status)?;
        self.last = Some(update);
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StatusPublisher {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusUpdate {
    pub state: PublicState,
    pub reason: Option<PublicReason>,
    pub focus: FocusState,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub mode: Option<String>,
    pub headless: bool,
}

fn acquire_lifetime_lock(path: &Path) -> io::Result<fs::File> {
    loop {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        // Never wait: a held lock means another publisher owns this PID, or a
        // reader is removing a dead record of it.
        file.try_lock().map_err(|err| match err {
            fs::TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("status lock {} is held", path.display()),
            ),
            fs::TryLockError::Error(err) => err,
        })?;
        // A reader cleaning up a dead process with this PID removes the file
        // before releasing its probe; once locked, the file can no longer go.
        if path.exists() {
            return Ok(file);
        }
    }
}

enum Liveness {
    Running,
    /// The process is gone. The shared lock is held until this is dropped, so
    /// a new process reusing the PID cannot claim it during cleanup.
    Exited {
        _shared_lock: fs::File,
    },
}

fn probe(pid: u32) -> io::Result<Liveness> {
    let lock = fs::File::open(lock_path_for_pid(pid)).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            not_running(pid)
        } else {
            err
        }
    })?;
    match lock.try_lock_shared() {
        Ok(()) => Ok(Liveness::Exited { _shared_lock: lock }),
        Err(fs::TryLockError::WouldBlock) => Ok(Liveness::Running),
        Err(fs::TryLockError::Error(err)) => Err(err),
    }
}

fn not_running(pid: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("process {pid} is not running"),
    )
}

pub fn read_status_for_pid(pid: u32) -> io::Result<PublicStatus> {
    match probe(pid)? {
        Liveness::Running => read_status_file(pid),
        Liveness::Exited { .. } => Err(not_running(pid)),
    }
}

fn read_status_file(pid: u32) -> io::Result<PublicStatus> {
    let data = fs::read_to_string(status_path_for_pid(pid))?;
    let status: PublicStatus = serde_json::from_str(&data).map_err(io::Error::other)?;
    if status.schema != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported smelt status schema {}", status.schema),
        ));
    }
    if status.app != "smelt" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "status file is not for smelt",
        ));
    }
    if status.pid != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "status pid {} does not match requested pid {pid}",
                status.pid
            ),
        ));
    }
    Ok(status)
}

pub fn read_all_statuses() -> io::Result<Vec<PublicStatus>> {
    let mut statuses = Vec::new();
    let dir = status_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(statuses),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(OsStr::to_str)
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        match probe(pid) {
            Ok(Liveness::Running) => {
                if let Ok(status) = read_status_file(pid) {
                    statuses.push(status);
                }
            }
            Ok(Liveness::Exited { .. }) => {
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(lock_path_for_pid(pid));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    statuses.sort_by_key(|status| status.pid);
    Ok(statuses)
}

pub fn status_path_for_pid(pid: u32) -> PathBuf {
    status_dir().join(format!("{pid}.json"))
}

fn lock_path_for_pid(pid: u32) -> PathBuf {
    status_dir().join(format!("{pid}.lock"))
}

pub fn status_dir() -> PathBuf {
    status_dir_in(&runtime_root())
}

fn status_dir_in(runtime_root: &Path) -> PathBuf {
    runtime_root.join("smelt").join("status")
}

fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
}

fn write_status_atomic(path: &Path, status: &PublicStatus) -> io::Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "status path has no parent"))?;
    fs::create_dir_all(parent)?;
    let data = serde_json::to_vec(status).map_err(io::Error::other)?;
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&data)?;
    file.commit()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> StatusUpdate {
        StatusUpdate {
            state: PublicState::Idle,
            reason: None,
            focus: FocusState::Unknown,
            cwd: None,
            session_id: None,
            mode: None,
            headless: false,
        }
    }

    fn write_record(pid: u32) {
        write_status_atomic(
            &status_path_for_pid(pid),
            &PublicStatus {
                schema: SCHEMA_VERSION,
                app: "smelt".to_string(),
                pid,
                state: PublicState::Busy,
                reason: None,
                focus: FocusState::Unknown,
                cwd: None,
                session_id: None,
                mode: None,
                headless: false,
                updated_at_ms: 0,
            },
        )
        .unwrap();
    }

    #[test]
    fn status_path_uses_xdg_runtime_dir() {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        environment.set_var("XDG_RUNTIME_DIR", dir.path());
        assert_eq!(
            status_path_for_pid(7),
            dir.path().join("smelt/status/7.json")
        );
    }

    #[test]
    fn publisher_writes_and_reads_own_status() {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        environment.set_var("XDG_RUNTIME_DIR", dir.path());
        let mut publisher = StatusPublisher::new().unwrap();
        publisher
            .publish(StatusUpdate {
                state: PublicState::NeedsAttention,
                reason: Some(PublicReason::TurnComplete),
                focus: FocusState::Unfocused,
                cwd: Some("/repo".to_string()),
                session_id: Some("session".to_string()),
                mode: Some("apply".to_string()),
                headless: false,
            })
            .unwrap();
        let status = read_status_for_pid(std::process::id()).unwrap();
        assert_eq!(status.state, PublicState::NeedsAttention);
        assert_eq!(status.reason, Some(PublicReason::TurnComplete));
        assert_eq!(status.focus, FocusState::Unfocused);
        assert_eq!(status.cwd.as_deref(), Some("/repo"));

        publisher
            .publish(StatusUpdate {
                focus: FocusState::Focused,
                cwd: Some("/other".to_string()),
                ..idle()
            })
            .unwrap();
        let replaced = read_status_for_pid(std::process::id()).unwrap();
        assert_eq!(replaced.state, PublicState::Idle);
        assert_eq!(replaced.focus, FocusState::Focused);
        assert_eq!(replaced.cwd.as_deref(), Some("/other"));
        assert_eq!(read_all_statuses().unwrap(), vec![replaced]);

        drop(publisher);
        assert!(!status_path_for_pid(std::process::id()).exists());
        assert!(!lock_path_for_pid(std::process::id()).exists());
    }

    #[test]
    fn publisher_writes_only_when_status_changes() {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        environment.set_var("XDG_RUNTIME_DIR", dir.path());
        let mut publisher = StatusPublisher::new().unwrap();
        let path = publisher.path().to_path_buf();
        publisher.publish(idle()).unwrap();
        fs::remove_file(&path).unwrap();

        publisher.publish(idle()).unwrap();
        assert!(!path.exists());

        publisher
            .publish(StatusUpdate {
                state: PublicState::Busy,
                ..idle()
            })
            .unwrap();
        assert_eq!(
            read_status_for_pid(std::process::id()).unwrap().state,
            PublicState::Busy
        );
    }

    #[test]
    fn status_is_live_exactly_while_its_lock_is_held() {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        environment.set_var("XDG_RUNTIME_DIR", dir.path());
        let pid = 424_242;
        fs::create_dir_all(status_dir()).unwrap();
        write_record(pid);
        let lock = acquire_lifetime_lock(&lock_path_for_pid(pid)).unwrap();
        assert_eq!(read_status_for_pid(pid).unwrap().state, PublicState::Busy);

        // The OS releases the lock however the process ends, leaving its files.
        drop(lock);
        let err = read_status_for_pid(pid).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(read_all_statuses().unwrap().is_empty());
        assert!(!status_path_for_pid(pid).exists());
        assert!(!lock_path_for_pid(pid).exists());
    }

    #[test]
    fn second_publisher_for_a_pid_fails_instead_of_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let _first = StatusPublisher::new_in(dir.path(), 7).unwrap();
        let err = StatusPublisher::new_in(dir.path(), 7).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn read_all_removes_records_without_lock() {
        let environment = smelt_test_support::ProcessEnvironmentGuard::capture();
        let dir = tempfile::tempdir().unwrap();
        environment.set_var("XDG_RUNTIME_DIR", dir.path());
        let pid = 424_243;
        write_record(pid);

        assert!(read_all_statuses().unwrap().is_empty());
        assert!(!status_path_for_pid(pid).exists());
    }
}
