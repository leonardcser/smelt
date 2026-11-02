//! Installation-scoped snapshots. The OS lock, not a timestamp or PID, proves liveness.

use super::Channel;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATUS_FILE: &str = ".smelt-upgrade.status.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    Starting,
    Checking,
    Available,
    Downloading,
    Verifying,
    Extracting,
    Validating,
    Installing,
    Building,
    Installed,
    UpToDate,
    Failed,
    Interrupted,
}

impl Stage {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Available | Self::Installed | Self::UpToDate | Self::Failed | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Status {
    pub operation_id: String,
    pub worker_pid: Option<u32>,
    pub channel: Option<Channel>,
    pub target: Option<String>,
    pub executable: PathBuf,
    pub stage: Stage,
    pub running: bool,
    pub started_at: u64,
    pub updated_at: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub activity: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub failed_target: Option<String>,
    #[serde(skip)]
    write_warning: bool,
}

/// Shared by background startup and status reads. An unknown target stays unknown.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub current: String,
    pub status: Option<Status>,
}

impl Snapshot {
    pub fn read(exe: &Path, current: &str) -> Result<Self, String> {
        let mut status = read(exe)?;
        if let Some(status) = &mut status {
            // A crash or failed telemetry write after the swap must not hide a
            // successfully installed binary. This CLI executes the image on disk.
            if status.stage == Stage::Interrupted && status.target.as_deref() == Some(current) {
                status.stage = Stage::Installed;
            }
        }
        Ok(Self {
            current: current.to_string(),
            status,
        })
    }

    pub fn text(&self) -> String {
        self.status.as_ref().map_or_else(
            || "no upgrade has been recorded for this installation".to_string(),
            Status::text,
        )
    }
}

impl Status {
    pub fn new(exe: &Path, channel: Option<Channel>, stage: Stage) -> Self {
        let now = now();
        Self {
            operation_id: format!(
                "{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                std::process::id()
            ),
            worker_pid: channel.map(|_| std::process::id()),
            channel,
            target: None,
            executable: exe.to_path_buf(),
            stage,
            running: true,
            started_at: now,
            updated_at: now,
            downloaded_bytes: 0,
            total_bytes: None,
            activity: None,
            error: None,
            failed_target: None,
            write_warning: false,
        }
    }

    pub fn save(&mut self) -> Result<(), String> {
        self.updated_at = now();
        self.running = !self.stage.terminal();
        let path = path(&self.executable)?;
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        let mut file = atomic_write_file::AtomicWriteFile::open(&path)
            .map_err(|e| format!("open upgrade status {}: {e}", path.display()))?;
        file.write_all(&data)
            .and_then(|()| file.commit())
            .map_err(|e| format!("write upgrade status {}: {e}", path.display()))
    }

    pub fn publish(&mut self) {
        if let Err(error) = self.save() {
            if !self.write_warning {
                super::say_err(format_args!(
                    "warning: upgrade progress unavailable: {error}"
                ));
                self.write_warning = true;
            }
        }
    }

    pub fn stage(&mut self, stage: Stage) {
        self.stage = stage;
        self.activity = None;
        self.publish();
    }

    pub fn text(&self) -> String {
        let target = self.target.as_deref().unwrap_or("smelt");
        let channel = match self.channel {
            Some(Channel::Stable) => "stable",
            Some(Channel::Unstable) => "unstable",
            None => "unknown channel",
        };
        let mut text = format!(
            "{target} ({channel}): {}",
            match self.stage {
                Stage::Starting => "starting",
                Stage::Checking => "checking for updates",
                Stage::Available => "update available; run /upgrade to install",
                Stage::Downloading => "downloading",
                Stage::Verifying => "verifying checksum",
                Stage::Extracting => "extracting",
                Stage::Validating => "validating executable",
                Stage::Installing => "installing",
                Stage::Building => "building",
                Stage::Installed => "installed, restart smelt to use it",
                Stage::UpToDate => "up to date",
                Stage::Failed => "failed",
                Stage::Interrupted => "interrupted; run /upgrade to retry",
            }
        );
        if self.stage == Stage::Downloading {
            match self.total_bytes.filter(|total| *total > 0) {
                Some(total) => text.push_str(&format!(
                    " ({}/{} bytes, {}%)",
                    self.downloaded_bytes,
                    total,
                    self.downloaded_bytes.saturating_mul(100) / total
                )),
                None => text.push_str(&format!(" ({} bytes)", self.downloaded_bytes)),
            }
        }
        if let Some(activity) = &self.activity {
            text.push_str(&format!(": {activity}"));
        }
        if let Some(error) = &self.error {
            text.push_str(&format!(": {error}"));
        }
        text
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn path(exe: &Path) -> Result<PathBuf, String> {
    exe.parent()
        .map(|dir| dir.join(STATUS_FILE))
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))
}

/// Read-only and offline, including for installations the reader cannot modify.
pub(super) fn read(exe: &Path) -> Result<Option<Status>, String> {
    let path = path(exe)?;
    let lock_path = path.with_file_name(super::LOCK_FILE);
    let lock = match fs::File::open(&lock_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read upgrade lock: {e}")),
    };
    // Hold a successful shared probe through the read, so a new writer cannot
    // publish between the liveness check and the snapshot and appear interrupted.
    let active = match lock.try_lock_shared() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(e)) => return Err(format!("inspect upgrade lock: {e}")),
    };
    let mut status: Option<Status> = match fs::read(&path) {
        Ok(data) => {
            Some(serde_json::from_slice(&data).map_err(|e| format!("read upgrade status: {e}"))?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read upgrade status: {e}")),
    };
    if active && status.is_none() {
        // An older installer may hold the lock without publishing snapshots.
        status = Some(Status::new(exe, None, Stage::Starting));
    }
    if let Some(status) = &mut status {
        status.running = active && !status.stage.terminal();
        if !active && !status.stage.terminal() {
            status.stage = Stage::Interrupted;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::InstallLock;

    #[test]
    fn lock_proves_liveness_even_when_snapshot_is_old() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("smelt");
        assert!(read(&exe).unwrap().is_none());
        let lock = InstallLock::try_acquire(&exe).unwrap().unwrap();
        let mut status = Status::new(&exe, Some(Channel::Stable), Stage::Downloading);
        status.started_at = 1;
        status.target = Some("v0.7.0".into());
        status.downloaded_bytes = 512;
        status.total_bytes = Some(1024);
        status.save().unwrap();
        let running = read(&exe).unwrap().unwrap();
        assert!(running.running);
        assert_eq!(running.operation_id, status.operation_id);
        assert!(running.text().contains("50%"));
        drop(lock);
        let interrupted = read(&exe).unwrap().unwrap();
        assert!(!interrupted.running);
        assert_eq!(interrupted.stage, Stage::Interrupted);
        assert!(InstallLock::try_acquire(&exe).unwrap().is_some());
    }

    #[test]
    fn disk_identity_recovers_completion_after_missing_final_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("smelt");
        let lock = InstallLock::try_acquire(&exe).unwrap().unwrap();
        let mut status = Status::new(&exe, Some(Channel::Stable), Stage::Installing);
        status.target = Some("v0.7.0".into());
        status.save().unwrap();
        drop(lock);
        let unchanged = Snapshot::read(&exe, "v0.6.0").unwrap().status.unwrap();
        assert_eq!(unchanged.stage, Stage::Interrupted);
        let installed = Snapshot::read(&exe, "v0.7.0").unwrap().status.unwrap();
        assert_eq!(installed.stage, Stage::Installed);
        assert!(!installed.running);
    }

    #[test]
    fn terminal_results_survive_unlock_without_becoming_interrupted() {
        for stage in [
            Stage::Available,
            Stage::Installed,
            Stage::UpToDate,
            Stage::Failed,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join("smelt");
            let lock = InstallLock::try_acquire(&exe).unwrap().unwrap();
            let mut status = Status::new(&exe, Some(Channel::Unstable), stage);
            status.save().unwrap();
            assert_eq!(read(&exe).unwrap().unwrap().stage, stage);
            drop(lock);
            let result = read(&exe).unwrap().unwrap();
            assert_eq!(result.stage, stage);
            assert!(!result.running);
        }
    }

    #[test]
    fn legacy_lock_without_snapshot_is_reported_as_starting() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("smelt");
        let lock = InstallLock::try_acquire(&exe).unwrap().unwrap();
        let status = read(&exe).unwrap().unwrap();
        assert!(status.running);
        assert_eq!(status.stage, Stage::Starting);
        assert!(status.channel.is_none());
        drop(lock);
        assert!(read(&exe).unwrap().is_none());
    }
}
