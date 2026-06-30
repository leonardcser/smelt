use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
const STATUS_TTL_MS: u64 = 15_000;
const STATUS_HEARTBEAT_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicState {
    Idle,
    Busy,
    NeedsAttention,
}

impl PublicState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
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
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusKey {
    state: PublicState,
    reason: Option<PublicReason>,
    focus: FocusState,
    cwd: Option<String>,
    session_id: Option<String>,
    mode: Option<String>,
    headless: bool,
}

struct LastPublished {
    key: StatusKey,
    updated_at_ms: u64,
}

pub struct StatusPublisher {
    pid: u32,
    path: PathBuf,
    last: Option<LastPublished>,
}

impl StatusPublisher {
    pub fn new() -> io::Result<Self> {
        let pid = std::process::id();
        let dir = status_dir();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            pid,
            path: status_path_for_pid(pid),
            last: None,
        })
    }

    pub fn publish(&mut self, update: StatusUpdate) -> io::Result<()> {
        let key = StatusKey {
            state: update.state,
            reason: update.reason,
            focus: update.focus,
            cwd: update.cwd,
            session_id: update.session_id,
            mode: update.mode,
            headless: update.headless,
        };
        if self.last.as_ref().is_some_and(|last| {
            last.key == key && unix_ms().saturating_sub(last.updated_at_ms) < STATUS_HEARTBEAT_MS
        }) {
            return Ok(());
        }

        let updated_at_ms = unix_ms();
        let status = PublicStatus {
            schema: SCHEMA_VERSION,
            app: "smelt".to_string(),
            pid: self.pid,
            state: key.state,
            reason: key.reason,
            focus: key.focus,
            cwd: key.cwd.clone(),
            session_id: key.session_id.clone(),
            mode: key.mode.clone(),
            headless: key.headless,
            updated_at_ms,
            expires_at_ms: updated_at_ms.saturating_add(STATUS_TTL_MS),
        };
        write_status_atomic(&self.path, &status)?;
        self.last = Some(LastPublished { key, updated_at_ms });
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StatusPublisher {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug)]
pub struct StatusUpdate {
    pub state: PublicState,
    pub reason: Option<PublicReason>,
    pub focus: FocusState,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub mode: Option<String>,
    pub headless: bool,
}

pub fn read_status_for_pid(pid: u32) -> io::Result<PublicStatus> {
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
    if status.expires_at_ms < unix_ms() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("status for process {pid} is stale"),
        ));
    }
    if !process_exists(pid) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} is not running"),
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
        if let Ok(status) = read_status_for_pid(pid) {
            statuses.push(status);
        }
    }
    statuses.sort_by_key(|status| status.pid);
    Ok(statuses)
}

pub fn status_path_for_pid(pid: u32) -> PathBuf {
    status_dir().join(format!("{pid}.json"))
}

pub fn status_dir() -> PathBuf {
    runtime_root().join("smelt").join("status")
}

fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
}

fn write_status_atomic(path: &Path, status: &PublicStatus) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "status path has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let data = serde_json::to_vec(status).map_err(io::Error::other)?;
    write_private(&tmp, &data)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    fs::write(path, data)
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let pid = match libc::pid_t::try_from(pid) {
        Ok(pid) => pid,
        Err(_) => return false,
    };
    unsafe {
        libc::kill(pid, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
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
    use std::sync::Mutex;

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn status_path_uses_xdg_runtime_dir() {
        let _guard = ENV_GUARD.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
        assert_eq!(
            status_path_for_pid(7),
            dir.path().join("smelt/status/7.json")
        );
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn publisher_writes_and_reads_own_status() {
        let _guard = ENV_GUARD.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", dir.path());
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
        drop(publisher);
        assert!(!status_path_for_pid(std::process::id()).exists());
        std::env::remove_var("XDG_RUNTIME_DIR");
    }
}
