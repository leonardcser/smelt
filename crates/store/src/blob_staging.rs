use std::fs;
#[cfg(test)]
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::path::Component;

use crate::{Result, StoreError};

pub(crate) const BLOB_STAGING_DIR: &str = ".blob-staging";

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionBlob {
    pub filename: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct StagedBlobSet {
    session_dir: PathBuf,
    dir: PathBuf,
}

impl StagedBlobSet {
    pub(crate) fn publish(self) -> Result<()> {
        let blob_dir = self.session_dir.join("blobs");
        ensure_private_dir(&blob_dir)?;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(StoreError::Integrity(format!(
                    "invalid staged blob path {}",
                    entry.path().display()
                )));
            }
            publish_staged_blob(&entry.path(), &blob_dir.join(entry.file_name()))?;
        }
        fs::remove_dir(&self.dir)?;
        Ok(())
    }

    pub(crate) fn abandon(self) {
        let _ = fs::remove_dir_all(self.dir);
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
pub(crate) fn stage_session_blobs(
    session_dir: &Path,
    fingerprint: &str,
    staging_token: &str,
    blobs: &[SessionBlob],
) -> Result<Option<StagedBlobSet>> {
    if blobs.is_empty() {
        return Ok(None);
    }
    for blob in blobs {
        validate_blob_filename(&blob.filename)?;
    }
    let staging_root = session_dir.join(BLOB_STAGING_DIR);
    ensure_private_dir(&staging_root)?;
    let staging_dir = staging_root.join(format!("{fingerprint}-{staging_token}"));
    ensure_private_dir(&staging_dir)?;
    let staged = StagedBlobSet {
        session_dir: session_dir.to_path_buf(),
        dir: staging_dir,
    };
    for blob in blobs {
        if let Err(err) = write_private_new_file(&staged.dir.join(&blob.filename), &blob.bytes) {
            staged.abandon();
            return Err(err);
        }
    }
    Ok(Some(staged))
}

pub(crate) fn recover_blob_staging(
    session_dir: &Path,
    committed_fingerprint: Option<&str>,
) -> Result<()> {
    let staging_root = session_dir.join(BLOB_STAGING_DIR);
    let metadata = match fs::symlink_metadata(&staging_root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::Integrity(format!(
            "invalid blob staging path {}",
            staging_root.display()
        )));
    }
    for entry in fs::read_dir(&staging_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Err(StoreError::Integrity(format!(
                "invalid blob staging entry {}",
                entry.path().display()
            )));
        }
        let staged = StagedBlobSet {
            session_dir: session_dir.to_path_buf(),
            dir: entry.path(),
        };
        let fingerprint = staging_fingerprint(&entry.file_name());
        if fingerprint.as_deref() == committed_fingerprint {
            staged.publish()?;
        } else {
            staged.abandon();
        }
    }
    Ok(())
}

fn staging_fingerprint(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    let (fingerprint, token) = name.split_once('-')?;
    let valid_hex = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    };
    (valid_hex(fingerprint) && valid_hex(token)).then(|| fingerprint.to_string())
}

#[cfg(test)]
fn validate_blob_filename(filename: &str) -> Result<()> {
    let path = Path::new(filename);
    let mut components = path.components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        return Ok(());
    }
    Err(StoreError::Integrity(format!(
        "invalid session blob filename {filename:?}"
    )))
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StoreError::Integrity(format!(
                "refusing non-directory storage path {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(err) => return Err(err.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
fn write_private_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn publish_staged_blob(source: &Path, destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(StoreError::Integrity(format!(
                "refusing non-file blob destination {}",
                destination.display()
            )));
        }
        Ok(_) => {
            if fs::read(source)? != fs::read(destination)? {
                return Err(StoreError::Integrity(format!(
                    "blob destination collision at {}",
                    destination.display()
                )));
            }
            fs::remove_file(source)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(source, destination)?;
        }
        Err(err) => return Err(err.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
