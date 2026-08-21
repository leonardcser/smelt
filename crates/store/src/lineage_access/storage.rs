use super::*;

pub(super) fn validate_storage_root(root: &Path) -> Result<()> {
    if root.exists() {
        ensure_private_directory(root)?;
    }
    Ok(())
}

pub(super) fn create_lineage_database(root: &Path) -> Result<LineageId> {
    ensure_private_directory_all(root)?;
    let layout = crate::SessionStoreLayout::from_sessions_root(root);
    loop {
        let lineage = LineageId::random()?;
        let directory = layout.lineage_dir(lineage.as_str());
        let staging = layout.staging_lineage_dir(lineage.as_str());
        match fs::create_dir(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(StoreError::Io(error)),
        }
        let prepare = (|| {
            ensure_private_directory(&staging)?;
            let path = layout.staging_lineage_database_path(lineage.as_str());
            let mut conn = open_write_connection(&path, &lineage)?;
            crate::schema::initialize_lineage_schema(&mut conn)?;
            lineage::create_lineage(&conn, &lineage, unix_timestamp_seconds()?)?;
            conn.close().map_err(|(_, error)| StoreError::from(error))?;
            sync_directory(&staging)
        })();
        if let Err(error) = prepare {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        match rename_without_replacement(&staging, &directory) {
            Ok(()) => {
                sync_directory(root)?;
                return Ok(lineage);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_dir_all(&staging);
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(StoreError::Io(error));
            }
        }
    }
}

fn catalog_lineage_hint(root: &Path, branch: &BranchId) -> Option<LineageId> {
    if root.file_name()? != std::ffi::OsStr::new("sessions") {
        return None;
    }
    let catalog_path = crate::SessionStoreLayout::from_sessions_root(root).catalog_path();
    let catalog = crate::catalog::CatalogReader::open_existing(catalog_path).ok()??;
    if !catalog.metadata().ok()?.is_reconciled() {
        return None;
    }
    let session = catalog.session(branch.as_str()).ok()??;
    if session.availability != crate::catalog::CatalogAvailability::Available {
        return None;
    }
    LineageId::from_hex(session.lineage_id?).ok()
}

fn branch_exists_in_lineage(root: &Path, lineage: &LineageId, branch: &BranchId) -> Result<bool> {
    let path = lineage_database_path(root, lineage);
    reject_symlink(&path)?;
    if !path.is_file() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    branch_exists(&conn, lineage, branch)
}

pub(super) fn locate_lineage(root: &Path, branch: &BranchId) -> Result<Option<LineageId>> {
    if let Some(lineage) = catalog_lineage_hint(root, branch) {
        if branch_exists_in_lineage(root, &lineage, branch)? {
            return Ok(Some(lineage));
        }
    }

    if root.exists() {
        ensure_private_directory(root)?;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error)),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(lineage) = LineageId::from_hex(name) else {
            continue;
        };
        let path = crate::SessionStoreLayout::from_sessions_root(root)
            .lineage_database_path(lineage.as_str());
        reject_symlink(&path)?;
        if !path.is_file() {
            continue;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let present = conn
            .query_row(
                "SELECT 1 FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL",
                (lineage.as_str(), branch.as_str()),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if present {
            if found.is_some() {
                return Err(StoreError::Integrity(format!(
                    "session {} belongs to multiple lineages",
                    branch.as_str()
                )));
            }
            found = Some(lineage);
        }
    }
    Ok(found)
}

pub(super) fn lineage_database_path(root: &Path, lineage: &LineageId) -> PathBuf {
    crate::SessionStoreLayout::from_sessions_root(root).lineage_database_path(lineage.as_str())
}

pub(super) fn open_write_connection(path: &Path, lineage: &LineageId) -> Result<Connection> {
    let _perf = smelt_perf::perf::begin("store:lineage:open_read_write");
    reject_symlink(path)?;
    let new_database = !path.exists();
    if let Some(parent) = path.parent() {
        ensure_private_directory_all(parent)?;
    }
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    conn.busy_timeout(LINEAGE_BUSY_TIMEOUT)?;
    if new_database {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let actual: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !actual.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Integrity(format!(
            "lineage {} did not enter WAL mode",
            lineage.as_str()
        )));
    }
    Ok(conn)
}

pub(super) fn branch_exists(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NULL",
            (lineage.as_str(), branch.as_str()),
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn lineage_exists(conn: &Connection, lineage: &LineageId) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM lineage_identity WHERE lineage_id = ?1",
            [lineage.as_str()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn unix_timestamp_millis() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| {
            StoreError::Integrity(format!("system clock precedes Unix epoch: {error}"))
        })
}

pub(super) fn unix_timestamp_seconds() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| {
            StoreError::Integrity(format!("system clock precedes Unix epoch: {error}"))
        })
}
