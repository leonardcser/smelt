use std::fs::{self, File};
use std::path::Path;

use crate::error::{Result, StoreError};

#[cfg(unix)]
fn path_cstring(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains a null byte: {}", path.display()),
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = path_cstring(source)?;
    let destination = path_cstring(destination)?;
    // SAFETY: both paths are valid null-terminated byte strings and remain alive
    // for the duration of the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = path_cstring(source)?;
    let destination = path_cstring(destination)?;
    // SAFETY: both paths are valid null-terminated byte strings and remain alive
    // for the duration of the call.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_without_replacement(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
pub(crate) fn rename_without_replacement(
    _source: &Path,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}

pub(crate) fn ensure_private_directory_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    ensure_private_directory(path)
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing non-directory storage path {}", path.display()),
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing symlinked storage path {}", path.display()),
            )))
        }
        Ok(metadata) if !metadata.is_file() => Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-file storage path {}", path.display()),
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
