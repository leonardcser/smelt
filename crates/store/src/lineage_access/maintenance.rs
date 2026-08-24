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
    branch: Option<&BranchId>,
) -> Result<crate::DoctorReport> {
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
    let stats = lineage_storage_stats(conn, path, branch)?;
    let search = branch
        .map(|branch| crate::lineage_search::search_projection_status(conn, path, lineage, branch))
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
