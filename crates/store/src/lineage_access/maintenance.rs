use super::*;

pub(super) fn lineage_turns(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<Vec<crate::StoredTurn>> {
    let mut statement = conn.prepare(
        "SELECT turn_id, submitted_history_idx, submitted_history_hash,
                submitted_sequence, turn_kind, turn_state, continuation_of,
                created_at_ms, started_at_ms, finished_at_ms, terminal_reason
         FROM lineage_turns
         WHERE lineage_id = ?1 AND session_id = ?2
         ORDER BY turn_id",
    )?;
    let rows = statement.query_map((lineage.as_str(), branch.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<String>>(10)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (
            turn_id,
            history_idx,
            history_hash,
            revision,
            kind,
            state,
            continuation_of,
            created_at_ms,
            started_at_ms,
            finished_at_ms,
            terminal_reason,
        ) = row?;
        let turn_id = positive_u64(turn_id, "turn ID")?;
        turns.push(crate::StoredTurn {
            turn_id: crate::TurnId::new(turn_id),
            submitted_history_idx: crate::HistoryIndex::new(nonnegative_u64(
                history_idx,
                "submitted history index",
            )?),
            submitted_history_hash: history_hash,
            submitted_revision: crate::Revision::new(positive_u64(
                revision,
                "submitted branch sequence",
            )?),
            kind: crate::TurnKind::from_db(&kind).ok_or_else(|| {
                StoreError::Integrity(format!("invalid lineage turn kind {kind:?}"))
            })?,
            state: crate::TurnState::from_db(&state).ok_or_else(|| {
                StoreError::Integrity(format!("invalid lineage turn state {state:?}"))
            })?,
            continuation_of: continuation_of
                .map(|value| positive_u64(value, "continuation turn ID").map(crate::TurnId::new))
                .transpose()?,
            created_at_ms: nonnegative_u64(created_at_ms, "turn created_at_ms")?,
            started_at_ms: started_at_ms
                .map(|value| nonnegative_u64(value, "turn started_at_ms"))
                .transpose()?,
            finished_at_ms: finished_at_ms
                .map(|value| nonnegative_u64(value, "turn finished_at_ms"))
                .transpose()?,
            terminal_reason,
        });
    }
    Ok(turns)
}

pub(super) fn lineage_storage_stats(
    conn: &Connection,
    path: &Path,
    branch: Option<&BranchId>,
) -> Result<crate::StorageStats> {
    let history_rows = count_query(
        conn,
        "SELECT COUNT(*) FROM lineage_payload_object_refs WHERE payload_kind = 'history'",
        [],
        "history payload rows",
    )?;
    let transcript_record_rows = count_query(
        conn,
        "SELECT COUNT(*) FROM lineage_payload_object_refs WHERE payload_kind = 'transcript'",
        [],
        "transcript payload rows",
    )?;
    let (object_rows, object_raw_bytes, object_stored_bytes): (i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(raw_size), 0), COALESCE(SUM(stored_size), 0)
             FROM objects",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let request_rows = match branch {
        Some(branch) => count_query(
            conn,
            "SELECT COUNT(*) FROM lineage_request_attempts WHERE session_id = ?1",
            [branch.as_str()],
            "branch request rows",
        )?,
        None => count_query(
            conn,
            "SELECT COUNT(*) FROM lineage_request_attempts",
            [],
            "lineage request rows",
        )?,
    };
    Ok(crate::StorageStats {
        database_bytes: crate::diagnostics::file_size(path)?,
        wal_bytes: crate::diagnostics::file_size(&crate::diagnostics::sqlite_companion_path(
            path, "-wal",
        ))?,
        shm_bytes: crate::diagnostics::file_size(&crate::diagnostics::sqlite_companion_path(
            path, "-shm",
        ))?,
        history_rows,
        transcript_record_rows,
        object_rows: nonnegative_u64(object_rows, "object rows")?,
        object_raw_bytes: nonnegative_u64(object_raw_bytes, "object raw bytes")?,
        object_stored_bytes: nonnegative_u64(object_stored_bytes, "object stored bytes")?,
        request_rows,
    })
}

pub(super) fn lineage_doctor_report(
    conn: &Connection,
    path: &Path,
    lineage: &LineageId,
    branch: Option<(&BranchId, &Path)>,
) -> Result<crate::DoctorReport> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    let schema_version = crate::schema::user_version(conn)?;
    let mut issues = Vec::new();
    if let Err(error) = crate::schema::validate_lineage_schema(conn) {
        issues.push(format!("schema: {error}"));
    }
    let mut quick_check = conn.prepare("PRAGMA quick_check")?;
    for result in quick_check
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        if result != "ok" {
            issues.push(format!("quick_check: {result}"));
        }
    }
    let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check")?;
    for (table, rowid, parent, constraint) in foreign_key_check
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        issues.push(format!(
            "foreign_key_check: table={table} rowid={rowid:?} parent={parent} constraint={constraint}"
        ));
    }
    if let Err(error) = lineage::verify_archive_coordinates(conn, lineage) {
        issues.push(format!("archive coordinates: {error}"));
    }
    if let Err(error) = lineage::verify_session_receipt_results(conn, lineage) {
        issues.push(format!("session receipt result: {error}"));
    }
    let mut branches = conn.prepare(
        "SELECT session_id FROM lineage_branches
         WHERE lineage_id = ?1 AND deleted_at IS NULL
         ORDER BY session_id",
    )?;
    let branches = branches
        .query_map([lineage.as_str()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if branches.is_empty() {
        issues.push("lineage has no live branches".into());
    }
    for branch_id in branches {
        let result = (|| {
            let branch_id = BranchId::new(branch_id)?;
            let snapshot = lineage::lineage_session_snapshot(conn, lineage, &branch_id)?;
            lineage::validate_sequence(conn, lineage, &snapshot.history_root)?;
            lineage::validate_sequence(conn, lineage, &snapshot.transcript_root)?;
            lineage::validate_transcript_indexes(conn, lineage, &snapshot.transcript_root)?;
            Ok::<(), StoreError>(())
        })();
        if let Err(error) = result {
            issues.push(format!("canonical branch: {error}"));
        }
    }
    let stats = lineage_storage_stats(conn, path, branch.map(|(branch, _)| branch))?;
    let search = branch
        .map(|(branch, search_path)| {
            crate::lineage_search::search_projection_status(conn, search_path, lineage, branch)
        })
        .transpose()?;
    Ok(crate::DoctorReport {
        schema_version,
        healthy: issues.is_empty(),
        issues,
        stats,
        search,
    })
}

fn count_query<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    field: &str,
) -> Result<u64> {
    let value = conn.query_row(sql, params, |row| row.get::<_, i64>(0))?;
    nonnegative_u64(value, field)
}

pub(super) fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

fn positive_u64(value: i64, field: &str) -> Result<u64> {
    let value = nonnegative_u64(value, field)?;
    if value == 0 {
        return Err(StoreError::Integrity(format!("{field} must be positive")));
    }
    Ok(value)
}

/// Queued rows examined per background reclamation transaction.
pub const RECLAMATION_STEP_ROWS: usize = 256;
/// Free space kept for reuse instead of being returned to the filesystem.
const COMPACTION_RESERVE_BYTES: u64 = 4 * 1024 * 1024;
/// Pages returned to the filesystem per compaction transaction.
const COMPACTION_STEP_PAGES: u64 = 256;

/// Returns free pages beyond [`COMPACTION_RESERVE_BYTES`] to the filesystem in
/// one bounded step of an incremental auto-vacuum database. Returns the pages
/// released; 0 means nothing is due.
pub(super) fn release_free_pages(conn: &Connection) -> Result<u64> {
    let excess = excess_free_pages(conn)?;
    if excess == 0 {
        return Ok(0);
    }
    let mut statement = conn.prepare(&format!(
        "PRAGMA incremental_vacuum({})",
        excess.min(COMPACTION_STEP_PAGES)
    ))?;
    let mut rows = statement.query([])?;
    let mut released = 0;
    while rows.next()?.is_some() {
        released += 1;
    }
    Ok(released)
}

fn excess_free_pages(conn: &Connection) -> Result<u64> {
    let pragma = |name: &str| -> Result<u64> {
        let value = conn.pragma_query_value(None, name, |row| row.get::<_, i64>(0))?;
        nonnegative_u64(value, name)
    };
    if pragma("auto_vacuum")? != 2 {
        return Ok(0);
    }
    let reserve = COMPACTION_RESERVE_BYTES / pragma("page_size")?.max(1);
    Ok(pragma("freelist_count")?.saturating_sub(reserve))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct IdleMaintenance {
    pub lineages_maintained: usize,
    pub lineages_failed: usize,
    pub pages_released: u64,
}

/// Brings every lineage no process is writing up to date: completes a
/// reclamation pass when content changed since the last completed
/// pass, and returns excess free pages to the filesystem. Work runs in bounded
/// transactions at half duty cycle under shared ownership, so opening a session
/// is never blocked; a lineage is left to its owner as soon as another
/// connection writes to it. Returns once every lineage was visited or `cancel`
/// is set.
pub fn maintain_idle_lineages(
    root: impl AsRef<Path>,
    cancel: &AtomicBool,
) -> Result<IdleMaintenance> {
    let root = root.as_ref();
    validate_storage_root(root)?;
    let mut report = IdleMaintenance::default();
    let entries = match fs::read_dir(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        entries => entries?,
    };
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(lineage) = entry
            .file_name()
            .to_str()
            .and_then(|name| LineageId::from_hex(name.to_owned()).ok())
        else {
            continue;
        };
        match maintain_idle_lineage(root, &lineage, cancel, &mut report) {
            Ok(true) => report.lineages_maintained += 1,
            Ok(false) => {}
            Err(_) => report.lineages_failed += 1,
        }
    }
    Ok(report)
}

fn maintain_idle_lineage(
    root: &Path,
    lineage: &LineageId,
    cancel: &AtomicBool,
    report: &mut IdleMaintenance,
) -> Result<bool> {
    let path = lineage_database_path(root, lineage);
    reject_symlink(&path)?;
    if !path.is_file() || !idle_maintenance_due(&path, lineage)? {
        return Ok(false);
    }
    let maintenance_name = format!("{}.maintenance", lineage.as_str());
    let (_maintenance, _lease) = match LineageLease::acquire_named(root, &maintenance_name, false)
        .and_then(|maintenance| Ok((maintenance, LineageLease::acquire_shared(root, lineage)?)))
    {
        Ok(leases) => leases,
        Err(StoreError::OwnershipConflict { .. }) => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut conn = open_write_connection(&path, lineage)?;
    crate::schema::initialize_lineage_schema(&mut conn)?;
    let data_version = |conn: &Connection| {
        conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
    };
    let mut seen = data_version(&conn)?;
    let mut reclaimed = false;
    loop {
        // data_version only moves for other connections' commits.
        if cancel.load(Ordering::Relaxed) || data_version(&conn)? != seen {
            return Ok(false);
        }
        let started = Instant::now();
        let released = if reclaimed {
            let released = release_free_pages(&conn)?;
            report.pages_released += released;
            released
        } else {
            reclaimed = lineage::reclaim_step(&mut conn, lineage, RECLAMATION_STEP_ROWS)?.complete;
            1
        };
        if released == 0 {
            break;
        }
        seen = data_version(&conn)?;
        std::thread::sleep(started.elapsed());
    }
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(true)
}

fn idle_maintenance_due(path: &Path, lineage: &LineageId) -> Result<bool> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    if crate::schema::user_version(&conn)? != crate::schema::LINEAGE_SCHEMA_VERSION {
        return Ok(false);
    }
    Ok(lineage::reclamation_pending(&conn, lineage)? || excess_free_pages(&conn)? > 0)
}
