use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::error::{Result, StoreError};

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct StorageStats {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub history_rows: u64,
    pub transcript_record_rows: u64,
    pub object_rows: u64,
    pub object_raw_bytes: u64,
    pub object_stored_bytes: u64,
    pub request_rows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct DoctorReport {
    pub schema_version: i32,
    pub healthy: bool,
    pub issues: Vec<String>,
    pub stats: StorageStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<crate::SearchProjectionStatus>,
}

pub(crate) fn backup_connection_to(source: &Connection, destination: &Path) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(destination)?;
    secure_file(destination)?;
    drop(file);

    let result = (|| {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut destination_db = Connection::open_with_flags(destination, flags)?;
        let backup = rusqlite::backup::Backup::new(source, &mut destination_db)?;
        backup.run_to_completion(128, std::time::Duration::from_millis(10), None)?;
        drop(backup);
        let check: String = destination_db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            return Err(StoreError::Integrity(format!(
                "backup quick_check failed: {check}"
            )));
        }
        drop(destination_db);
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination)?
            .sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn file_size(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn sqlite_companion_path(path: &Path, suffix: &str) -> PathBuf {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.with_extension(suffix.trim_start_matches('-'));
    };
    path.with_file_name(format!("{name}{suffix}"))
}
