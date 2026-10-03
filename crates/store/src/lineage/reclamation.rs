use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReclamationStep {
    pub(crate) branch_heads_cleared: usize,
    pub(crate) canonical_rows_deleted: usize,
    pub(crate) objects_deleted: usize,
    pub(crate) rows_examined: usize,
    pub(crate) phase_advanced: bool,
    pub(crate) complete: bool,
}

impl ReclamationStep {
    pub(crate) fn work_rows(self) -> usize {
        self.rows_examined.max(
            self.branch_heads_cleared
                .saturating_add(self.canonical_rows_deleted)
                .saturating_add(self.objects_deleted),
        )
    }
}

fn suspend_reclamation_delete_guard(tx: &Transaction<'_>, table: &str) -> Result<Option<String>> {
    let name = match table {
        "lineage_session_receipts" => "lineage_session_receipt_delete",
        "lineage_turn_transitions" => "lineage_turn_transition_delete",
        "lineage_commit_receipts" => "lineage_commit_receipt_delete",
        "lineage_completed_sequence_nodes" => "lineage_completed_sequence_node_delete",
        "lineage_history_indexes" => "lineage_history_index_delete",
        "lineage_history_index_nodes" => "lineage_history_index_node_delete",
        _ => return Ok(None),
    };
    let definition = tx.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
        [name],
        |row| row.get::<_, String>(0),
    )?;
    tx.execute_batch(&format!("DROP TRIGGER {name}"))?;
    Ok(Some(definition))
}

const MARK_REVISION: i64 = 0;
const MARK_ROOT: i64 = 1;
const MARK_NODE: i64 = 2;
const MARK_PAYLOAD: i64 = 3;
const MARK_HISTORY_INDEX: i64 = 4;
const MARK_FRONTIER: i64 = 8;
const SWEEP: i64 = 9;
const COMPLETE: i64 = SWEEP + SWEEP_SPECS.len() as i64;

struct ReclamationPass {
    epoch: i64,
    phase: i64,
    cursor: i64,
    dirty: bool,
}

impl ReclamationPass {
    fn restart(&mut self) -> Result<()> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| StoreError::Integrity("reclamation epoch exhausted".into()))?;
        self.phase = 0;
        self.cursor = i64::MIN;
        self.dirty = false;
        Ok(())
    }

    fn advance(&mut self) {
        self.phase += 1;
        self.cursor = if self.phase >= SWEEP {
            i64::MAX
        } else {
            i64::MIN
        };
    }
}

fn prepare_reclamation_pass(tx: &Transaction<'_>, lineage: &LineageId) -> Result<ReclamationPass> {
    // TEMP state belongs to the canonical connection. Reopening starts a fresh pass;
    // a transaction rollback cannot publish a frontier or a sweep cursor.
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS smelt_gc_pass (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             lineage_id TEXT NOT NULL, epoch INTEGER NOT NULL,
             phase INTEGER NOT NULL, cursor INTEGER NOT NULL, dirty INTEGER NOT NULL,
             expected_changes INTEGER NOT NULL, data_version INTEGER NOT NULL,
             schema_version INTEGER NOT NULL
         );
         CREATE TEMP TABLE IF NOT EXISTS smelt_gc_marks (
             epoch INTEGER NOT NULL, kind INTEGER NOT NULL, id TEXT NOT NULL,
             expanded INTEGER NOT NULL,
             PRIMARY KEY (epoch, kind, id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS temp.smelt_gc_frontier
             ON smelt_gc_marks(epoch, expanded, kind, id);
         CREATE TEMP TABLE IF NOT EXISTS smelt_gc_receipts (
             epoch INTEGER NOT NULL, session_id TEXT NOT NULL, fingerprint TEXT NOT NULL,
             PRIMARY KEY (epoch, session_id, fingerprint)
         ) WITHOUT ROWID;
         CREATE TEMP VIEW IF NOT EXISTS smelt_reachable_revisions AS
             SELECT id AS revision_id FROM smelt_gc_marks
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1) AND kind = 0;
         CREATE TEMP VIEW IF NOT EXISTS smelt_reachable_roots AS
             SELECT id AS root_id FROM smelt_gc_marks
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1) AND kind = 1;
         CREATE TEMP VIEW IF NOT EXISTS smelt_reachable_nodes AS
             SELECT id AS node_id FROM smelt_gc_marks
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1) AND kind = 2;
         CREATE TEMP VIEW IF NOT EXISTS smelt_reachable_payloads AS
             SELECT id AS payload_id FROM smelt_gc_marks
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1) AND kind = 3;
         CREATE TEMP VIEW IF NOT EXISTS smelt_reachable_history_indexes AS
             SELECT id AS node_id FROM smelt_gc_marks
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1) AND kind = 4;
         CREATE TEMP VIEW IF NOT EXISTS smelt_owned_session_receipts AS
             SELECT session_id, fingerprint FROM smelt_gc_receipts
             WHERE epoch = (SELECT epoch FROM smelt_gc_pass WHERE singleton = 1);",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO smelt_gc_pass VALUES (1, ?1, 0, 0, ?2, 0, -1, -1, -1)",
        (lineage.as_str(), i64::MIN),
    )?;
    let (owner, mut pass, expected_changes, data_version, schema_version) = tx.query_row(
        "SELECT lineage_id, epoch, phase, cursor, dirty, expected_changes, data_version,
                schema_version FROM smelt_gc_pass WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                ReclamationPass {
                    epoch: row.get(1)?,
                    phase: row.get(2)?,
                    cursor: row.get(3)?,
                    dirty: row.get(4)?,
                },
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        },
    )?;
    // BEGIN IMMEDIATE fences these checks and the entire step against other writers.
    // total_changes also detects same-connection writes and rolled-back attempts.
    if owner != lineage.as_str()
        || u64::try_from(expected_changes).ok() != Some(tx.total_changes())
        || data_version != main_pragma(tx, "data_version")?
        || schema_version != main_pragma(tx, "schema_version")?
    {
        pass.restart()?;
    }
    // Publish the epoch before querying the reachability views in this transaction.
    tx.execute(
        "UPDATE smelt_gc_pass SET lineage_id = ?1, epoch = ?2 WHERE singleton = 1",
        (lineage.as_str(), pass.epoch),
    )?;
    Ok(pass)
}

fn main_pragma(conn: &Connection, name: &str) -> Result<i64> {
    Ok(conn.pragma_query_value(Some("main"), name, |row| row.get(0))?)
}

fn finish_reclamation_step(
    tx: Transaction<'_>,
    pass: &ReclamationPass,
    step: ReclamationStep,
) -> Result<ReclamationStep> {
    let expected_changes = i64::try_from(tx.total_changes())
        .ok()
        .and_then(|changes| changes.checked_add(1))
        .ok_or_else(|| StoreError::Integrity("reclamation change counter exhausted".into()))?;
    tx.execute(
        "UPDATE smelt_gc_pass SET phase = ?1, cursor = ?2, dirty = ?3,
             expected_changes = ?4, data_version = ?5, schema_version = ?6 WHERE singleton = 1",
        rusqlite::params![
            pass.phase,
            pass.cursor,
            pass.dirty,
            expected_changes,
            main_pragma(&tx, "data_version")?,
            main_pragma(&tx, "schema_version")?
        ],
    )?;
    tx.commit()?;
    Ok(step)
}

fn mark(tx: &Transaction<'_>, epoch: i64, kind: i64, id: Option<&str>) -> Result<()> {
    if let Some(id) = id {
        tx.execute(
            "INSERT OR IGNORE INTO smelt_gc_marks VALUES (?1, ?2, ?3, ?4)",
            (epoch, kind, id, kind == MARK_PAYLOAD),
        )?;
    }
    Ok(())
}

fn expand_mark(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    epoch: i64,
    kind: i64,
    id: &str,
) -> Result<()> {
    match kind {
        MARK_REVISION => {
            let (parent, history, transcript, state) = tx.query_row(
                "SELECT parent_revision_id, history_root_id, transcript_root_id, state_payload_id
                 FROM lineage_revisions WHERE lineage_id = ?1 AND revision_id = ?2",
                (lineage.as_str(), id),
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?;
            mark(tx, epoch, MARK_REVISION, parent.as_deref())?;
            mark(tx, epoch, MARK_ROOT, Some(&history))?;
            mark(tx, epoch, MARK_ROOT, Some(&transcript))?;
            mark(tx, epoch, MARK_PAYLOAD, Some(&state))?;
        }
        MARK_ROOT => {
            let node: Option<String> = tx.query_row(
                "SELECT root_node_id FROM lineage_sequence_roots WHERE lineage_id = ?1 AND root_id = ?2",
                (lineage.as_str(), id), |row| row.get(0),
            )?;
            mark(tx, epoch, MARK_NODE, node.as_deref())?;
            let index: Option<Option<String>> = tx.query_row(
                "SELECT index_node_id FROM lineage_history_indexes WHERE lineage_id = ?1 AND history_root_id = ?2",
                (lineage.as_str(), id), |row| row.get(0),
            ).optional()?;
            mark(tx, epoch, MARK_HISTORY_INDEX, index.flatten().as_deref())?;
        }
        MARK_NODE => {
            let entries = tx
                .prepare(
                    "SELECT child_node_id, payload_id FROM lineage_sequence_entries
                 WHERE lineage_id = ?1 AND node_id = ?2 ORDER BY entry_index LIMIT ?3",
                )?
                .query_map(
                    (
                        lineage.as_str(),
                        id,
                        i64::try_from(super::sequence::SEQUENCE_FANOUT + 1).unwrap(),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if entries.len() > super::sequence::SEQUENCE_FANOUT {
                return Err(StoreError::Integrity(
                    "reclamation node exceeds sequence fanout".into(),
                ));
            }
            for (child, payload) in entries {
                mark(tx, epoch, MARK_NODE, child.as_deref())?;
                mark(tx, epoch, MARK_PAYLOAD, payload.as_deref())?;
            }
        }
        MARK_HISTORY_INDEX => {
            let (left, right) = tx.query_row(
                "SELECT left_node_id, right_node_id FROM lineage_history_index_nodes
                 WHERE lineage_id = ?1 AND node_id = ?2",
                (lineage.as_str(), id),
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )?;
            mark(tx, epoch, MARK_HISTORY_INDEX, left.as_deref())?;
            mark(tx, epoch, MARK_HISTORY_INDEX, right.as_deref())?;
        }
        _ => {
            return Err(StoreError::Integrity(
                "invalid reclamation frontier kind".into(),
            ))
        }
    }
    tx.execute(
        "UPDATE smelt_gc_marks SET expanded = 1 WHERE epoch = ?1 AND kind = ?2 AND id = ?3",
        (epoch, kind, id),
    )?;
    Ok(())
}

fn advance_reclamation_marks(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    pass: &mut ReclamationPass,
    limit: i64,
) -> Result<ReclamationStep> {
    let phase = pass.phase;
    let mut examined = 0;
    match phase {
        0 => {
            examined = tx.execute(
                "DELETE FROM smelt_gc_marks WHERE (epoch, kind, id) IN (
                     SELECT epoch, kind, id FROM smelt_gc_marks WHERE epoch < ?1 LIMIT ?2)",
                (pass.epoch, limit),
            )?;
            if examined == 0 {
                pass.advance();
            }
        }
        1 => {
            examined = tx.execute(
                "DELETE FROM smelt_gc_receipts WHERE (epoch, session_id, fingerprint) IN (
                     SELECT epoch, session_id, fingerprint FROM smelt_gc_receipts WHERE epoch < ?1 LIMIT ?2)",
                (pass.epoch, limit),
            )?;
            if examined == 0 {
                pass.advance();
            }
        }
        MARK_FRONTIER => {
            // Expand one bounded adjacency list at a time. Newly discovered nodes
            // enter the indexed frontier, never a recursive SQL traversal.
            while examined < usize::try_from(limit).unwrap() {
                let next = tx
                    .query_row(
                        "SELECT kind, id FROM smelt_gc_marks
                     WHERE epoch = ?1 AND expanded = 0 ORDER BY kind, id LIMIT 1",
                        [pass.epoch],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?;
                let Some((kind, id)) = next else {
                    pass.advance();
                    break;
                };
                expand_mark(tx, lineage, pass.epoch, kind, &id)?;
                examined += 1;
            }
        }
        2..=7 => {
            let (table, columns) = match phase {
                2 => (
                    "lineage_branches",
                    "initial_revision_id, CASE WHEN deleted_at IS NULL THEN head_revision_id END",
                ),
                3 => ("lineage_retained_revisions", "revision_id, NULL"),
                4 => (
                    "lineage_session_receipt_results",
                    "result_revision_id, session_id, fingerprint",
                ),
                5 => ("object_data_roots", "root_id, NULL"),
                6 => ("lineage_revision_state_roots", "root_id, NULL"),
                7 => (
                    "lineage_revision_state_projections",
                    "projected_payload_id, NULL",
                ),
                _ => unreachable!(),
            };
            // Seek candidates before filtering ownership, including foreign
            // lineage rows in the budget. No retained prefix can be rescanned.
            let rows = tx
                .prepare(&format!(
                    "SELECT rowid, lineage_id, {columns} FROM {table} NOT INDEXED
                     WHERE rowid >= ?1 ORDER BY rowid LIMIT ?2"
                ))?
                .query_map((pass.cursor, limit), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        if phase == 4 {
                            row.get::<_, Option<String>>(4)?
                        } else {
                            None
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            examined = rows.len();
            for (_, owner, first, second, fingerprint) in &rows {
                if owner != lineage.as_str() {
                    continue;
                }
                let kind = if phase <= 4 {
                    MARK_REVISION
                } else if phase <= 6 {
                    MARK_ROOT
                } else {
                    MARK_PAYLOAD
                };
                mark(tx, pass.epoch, kind, first.as_deref())?;
                if phase == 2 {
                    mark(tx, pass.epoch, MARK_REVISION, second.as_deref())?;
                }
                if phase == 4 {
                    tx.execute(
                        "INSERT OR IGNORE INTO smelt_gc_receipts VALUES (?1, ?2, ?3)",
                        (pass.epoch, second.as_deref(), fingerprint.as_deref()),
                    )?;
                }
            }
            if let Some(next) = rows.last().and_then(|row| row.0.checked_add(1)) {
                pass.cursor = next;
                if examined < usize::try_from(limit).unwrap() {
                    pass.advance();
                }
            } else {
                pass.advance();
            }
        }
        _ => {
            return Err(StoreError::Integrity(
                "invalid reclamation marking phase".into(),
            ))
        }
    }
    Ok(ReclamationStep {
        rows_examined: examined,
        phase_advanced: pass.phase != phase,
        ..ReclamationStep::default()
    })
}

pub(crate) fn reclaim_step(
    conn: &mut Connection,
    lineage: &LineageId,
    max_rows: usize,
) -> Result<ReclamationStep> {
    if max_rows == 0 {
        return Err(StoreError::Integrity(
            "lineage reclamation row budget must be positive".into(),
        ));
    }
    let limit = i64::try_from(max_rows).map_err(|_| {
        StoreError::Integrity("lineage reclamation row budget overflows i64".into())
    })?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut pass = prepare_reclamation_pass(&tx, lineage)?;
    if pass.phase < SWEEP {
        let step = advance_reclamation_marks(&tx, lineage, &mut pass, limit)?;
        return finish_reclamation_step(tx, &pass, step);
    }
    if pass.phase == COMPLETE {
        return finish_reclamation_step(
            tx,
            &pass,
            ReclamationStep {
                complete: true,
                ..ReclamationStep::default()
            },
        );
    }
    let step = advance_reclamation_sweep(&tx, lineage, &mut pass, limit)?;
    finish_reclamation_step(tx, &pass, step)
}

fn advance_reclamation_sweep(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    pass: &mut ReclamationPass,
    limit: i64,
) -> Result<ReclamationStep> {
    let index = usize::try_from(pass.phase - SWEEP)
        .map_err(|_| StoreError::Integrity("invalid reclamation sweep phase".into()))?;
    let &(table, predicate) = SWEEP_SPECS
        .get(index)
        .ok_or_else(|| StoreError::Integrity("invalid reclamation sweep phase".into()))?;
    // Fence and seek first, then evaluate ownership only inside this candidate
    // window. NOT INDEXED keeps the rowid bounds ahead of retained-row filtering.
    let rows = tx
        .prepare(&format!(
            "SELECT rowid FROM {table} NOT INDEXED WHERE rowid <= ?1 ORDER BY rowid DESC LIMIT ?2"
        ))?
        .query_map((pass.cursor, limit), |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut step = ReclamationStep {
        rows_examined: rows.len(),
        ..ReclamationStep::default()
    };
    if let (Some(&upper), Some(&lower)) = (rows.first(), rows.last()) {
        let owner = if table == "objects" {
            "?1 IS NOT NULL"
        } else {
            "candidate.lineage_id = ?1"
        };
        let eligible = tx
            .prepare(&format!(
                "SELECT candidate.rowid FROM {table} candidate NOT INDEXED
             WHERE candidate.rowid BETWEEN ?2 AND ?3 AND {owner} AND ({predicate})
             ORDER BY candidate.rowid DESC"
            ))?
            .query_map((lineage.as_str(), lower, upper), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !eligible.is_empty() {
            let guard = suspend_reclamation_delete_guard(tx, table)?;
            for id in eligible {
                if index == 0 {
                    step.branch_heads_cleared += tx.execute(
                        "UPDATE lineage_branches SET head_revision_id = NULL WHERE rowid = ?1",
                        [id],
                    )?;
                } else {
                    let deleted =
                        tx.execute(&format!("DELETE FROM {table} WHERE rowid = ?1"), [id])?;
                    if table == "objects" {
                        step.objects_deleted += deleted;
                    } else {
                        step.canonical_rows_deleted += deleted;
                    }
                }
            }
            if let Some(sql) = guard {
                tx.execute_batch(&sql)?;
            }
            pass.dirty = true;
        }
        if let Some(next) = lower.checked_sub(1) {
            pass.cursor = next;
            if rows.len() < usize::try_from(limit).unwrap() {
                pass.advance();
                step.phase_advanced = true;
            }
        } else {
            pass.advance();
            step.phase_advanced = true;
        }
    } else {
        pass.advance();
        step.phase_advanced = true;
    }
    if pass.phase == COMPLETE {
        if pass.dirty {
            // Removing layout and receipt owners can expose formerly pinned graphs.
            // Re-mark them in another bounded pass instead of keeping ghost owners.
            pass.restart()?;
            tx.execute(
                "UPDATE smelt_gc_pass SET epoch = ?1 WHERE singleton = 1",
                [pass.epoch],
            )?;
        } else {
            step.complete = true;
        }
    }
    Ok(step)
}

const SWEEP_SPECS: &[(&str, &str)] = &[
    ("lineage_branches", "candidate.deleted_at IS NOT NULL AND candidate.head_revision_id IS NOT NULL"),
    // Release deleted-session result owners before ordinary reachability predicates.
    ("lineage_turn_transitions", "EXISTS (
        SELECT 1 FROM lineage_session_receipt_results result
        JOIN lineage_branches branch ON branch.lineage_id = result.lineage_id AND branch.session_id = result.session_id
        WHERE result.lineage_id = candidate.lineage_id AND result.session_id = candidate.session_id
          AND result.fingerprint = candidate.fingerprint AND branch.deleted_at IS NOT NULL
    )"),
    ("lineage_session_receipts", "EXISTS (
        SELECT 1 FROM lineage_session_receipt_results result
        JOIN lineage_branches branch ON branch.lineage_id = result.lineage_id AND branch.session_id = result.session_id
        WHERE result.lineage_id = candidate.lineage_id AND result.session_id = candidate.session_id
          AND result.fingerprint = candidate.fingerprint AND branch.deleted_at IS NOT NULL
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_turn_transitions transition WHERE transition.lineage_id = candidate.lineage_id
          AND transition.session_id = candidate.session_id AND transition.fingerprint = candidate.fingerprint
    )"),
    ("lineage_turn_transitions", "EXISTS (
        SELECT 1 FROM lineage_turns turn WHERE turn.lineage_id = candidate.lineage_id
          AND turn.session_id = candidate.session_id AND turn.turn_id = candidate.turn_id
          AND NOT EXISTS (SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = turn.submitted_revision_id)
    )"),
    ("lineage_session_receipts", "NOT EXISTS (
        SELECT 1 FROM smelt_owned_session_receipts owned
        WHERE owned.session_id = candidate.session_id AND owned.fingerprint = candidate.fingerprint
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_turn_transitions transition WHERE transition.lineage_id = candidate.lineage_id
          AND transition.session_id = candidate.session_id AND transition.fingerprint = candidate.fingerprint
    ) AND (EXISTS (
        SELECT 1 FROM lineage_turns turn WHERE turn.lineage_id = candidate.lineage_id
          AND turn.session_id = candidate.session_id AND turn.turn_id = candidate.turn_id
          AND NOT EXISTS (SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = turn.submitted_revision_id)
    ) OR EXISTS (
        SELECT 1 FROM lineage_commit_receipts receipt WHERE receipt.lineage_id = candidate.lineage_id
          AND receipt.session_id = candidate.session_id AND receipt.fingerprint = candidate.fingerprint
          AND (NOT EXISTS (SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = receipt.result_revision_id)
            OR (receipt.prior_revision_id IS NOT NULL AND NOT EXISTS (
                SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = receipt.prior_revision_id)))
    ))"),
    ("lineage_commit_receipts", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = candidate.result_revision_id
    ) OR (candidate.prior_revision_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = candidate.prior_revision_id
    ))"),
    ("lineage_branch_revisions", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = candidate.revision_id
    )"),
    ("lineage_turns", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = candidate.submitted_revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_commit_receipts receipt WHERE receipt.lineage_id = candidate.lineage_id
          AND receipt.session_id = candidate.session_id AND receipt.turn_id = candidate.turn_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_turns continuation WHERE continuation.lineage_id = candidate.lineage_id
          AND continuation.session_id = candidate.session_id AND continuation.continuation_of = candidate.turn_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_turn_transitions transition WHERE transition.lineage_id = candidate.lineage_id
          AND transition.session_id = candidate.session_id AND transition.turn_id = candidate.turn_id
    )"),
    ("lineage_revisions", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_revisions mark WHERE mark.revision_id = candidate.revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revisions child WHERE child.lineage_id = candidate.lineage_id AND child.parent_revision_id = candidate.revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_turns turn WHERE turn.lineage_id = candidate.lineage_id AND turn.submitted_revision_id = candidate.revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_branch_revisions association WHERE association.lineage_id = candidate.lineage_id AND association.revision_id = candidate.revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_commit_receipts receipt WHERE receipt.lineage_id = candidate.lineage_id AND receipt.result_revision_id = candidate.revision_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_commit_receipts receipt WHERE receipt.lineage_id = candidate.lineage_id AND receipt.prior_revision_id = candidate.revision_id
    )"),
    ("lineage_history_indexes", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_roots mark WHERE mark.root_id = candidate.history_root_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revisions revision WHERE revision.lineage_id = candidate.lineage_id AND revision.history_root_id = candidate.history_root_id
    )"),
    ("lineage_history_index_nodes", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_history_indexes mark WHERE mark.node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_history_indexes semantic WHERE semantic.lineage_id = candidate.lineage_id AND semantic.index_node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_history_index_nodes parent WHERE parent.lineage_id = candidate.lineage_id AND parent.left_node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_history_index_nodes parent WHERE parent.lineage_id = candidate.lineage_id AND parent.right_node_id = candidate.node_id
    )"),
    ("lineage_sequence_roots", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_roots mark WHERE mark.root_id = candidate.root_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revisions revision WHERE revision.lineage_id = candidate.lineage_id AND revision.history_root_id = candidate.root_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revisions revision WHERE revision.lineage_id = candidate.lineage_id AND revision.transcript_root_id = candidate.root_id
    ) AND NOT EXISTS (
        SELECT 1 FROM object_data_roots object WHERE object.lineage_id = candidate.lineage_id AND object.root_id = candidate.root_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revision_state_roots archive WHERE archive.lineage_id = candidate.lineage_id AND archive.root_id = candidate.root_id
    )"),
    ("lineage_transcript_extent_nodes", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_nodes mark WHERE mark.node_id = candidate.node_id
    )"),
    // Incoming edges must disappear before a child is unsealed. This protects
    // surviving ancestor proofs with a bounded point check, even for shared DAGs.
    ("lineage_completed_sequence_nodes", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_nodes mark WHERE mark.node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_entries entry WHERE entry.lineage_id = candidate.lineage_id AND entry.child_node_id = candidate.node_id
    )"),
    ("lineage_sequence_entries", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_nodes mark WHERE mark.node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_completed_sequence_nodes complete WHERE complete.lineage_id = candidate.lineage_id AND complete.node_id = candidate.node_id
    )"),
    ("lineage_sequence_nodes", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_nodes mark WHERE mark.node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_entries entry WHERE entry.lineage_id = candidate.lineage_id AND entry.node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_entries entry WHERE entry.lineage_id = candidate.lineage_id AND entry.child_node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_roots root WHERE root.lineage_id = candidate.lineage_id AND root.root_node_id = candidate.node_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_completed_sequence_nodes complete WHERE complete.lineage_id = candidate.lineage_id AND complete.node_id = candidate.node_id
    )"),
    ("lineage_transcript_record_profiles", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_payloads mark WHERE mark.payload_id = candidate.payload_id
    )"),
    ("lineage_payload_nested_object_refs", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_payloads mark WHERE mark.payload_id = candidate.payload_id
    )"),
    ("lineage_payload_object_refs", "NOT EXISTS (
        SELECT 1 FROM smelt_reachable_payloads mark WHERE mark.payload_id = candidate.payload_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_payload_nested_object_refs nested WHERE nested.lineage_id = candidate.lineage_id AND nested.payload_id = candidate.payload_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_sequence_entries entry WHERE entry.lineage_id = candidate.lineage_id AND entry.payload_id = candidate.payload_id
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_revisions revision WHERE revision.lineage_id = candidate.lineage_id AND revision.state_payload_id = candidate.payload_id
    )"),
    ("objects", "NOT EXISTS (
        SELECT 1 FROM request_object_refs request WHERE request.object_hash = candidate.hash
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_payload_object_refs payload WHERE payload.object_hash = candidate.hash
    ) AND NOT EXISTS (
        SELECT 1 FROM lineage_payload_nested_object_refs nested WHERE nested.object_hash = candidate.hash
    )"),
];
