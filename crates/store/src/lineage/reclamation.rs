use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReclamationStep {
    /// Queued rows examined in this step.
    pub(crate) rows_examined: usize,
    pub(crate) complete: bool,
}

/// Queued row kinds in reclamation order. Owners come before what they
/// reference, so a step usually settles a row together with everything only it
/// kept alive.
#[derive(Clone, Copy)]
enum Kind {
    Branch,
    Revision,
    Root,
    HistoryIndex,
    Node,
    Payload,
    Object,
}

impl Kind {
    const ALL: [Self; 7] = [
        Self::Branch,
        Self::Revision,
        Self::Root,
        Self::HistoryIndex,
        Self::Node,
        Self::Payload,
        Self::Object,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Revision => "revision",
            Self::Root => "root",
            Self::HistoryIndex => "history_index",
            Self::Node => "node",
            Self::Payload => "payload",
            Self::Object => "object",
        }
    }
}

/// Whether anything still references a content row, and the statements that
/// delete it. Deleting fires the triggers that queue whatever the row referenced.
struct Content {
    referenced: &'static str,
    deletes: &'static [&'static str],
}

const ROOT: Content = Content {
    referenced: "SELECT EXISTS (SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND history_root_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND transcript_root_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_revision_state_roots WHERE lineage_id = ?1 AND root_id = ?2)",
    deletes: &[
        "DELETE FROM lineage_history_semantic_roots WHERE lineage_id = ?1 AND history_root_id = ?2",
        "DELETE FROM lineage_sequence_roots WHERE lineage_id = ?1 AND root_id = ?2",
    ],
};

const HISTORY_INDEX: Content = Content {
    referenced: "SELECT EXISTS (SELECT 1 FROM lineage_history_semantic_roots WHERE lineage_id = ?1 AND index_node_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_history_index_nodes WHERE lineage_id = ?1 AND left_node_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_history_index_nodes WHERE lineage_id = ?1 AND right_node_id = ?2)",
    deletes: &["DELETE FROM lineage_history_index_nodes WHERE lineage_id = ?1 AND node_id = ?2"],
};

const NODE: Content = Content {
    referenced: "SELECT EXISTS (SELECT 1 FROM lineage_sequence_entries WHERE lineage_id = ?1 AND child_node_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_sequence_roots WHERE lineage_id = ?1 AND root_node_id = ?2)",
    deletes: &["DELETE FROM lineage_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2"],
};

const PAYLOAD: Content = Content {
    referenced: "SELECT EXISTS (SELECT 1 FROM lineage_sequence_entries WHERE lineage_id = ?1 AND payload_id = ?2)
        OR EXISTS (SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND state_payload_id = ?2)",
    deletes: &["DELETE FROM lineage_payload_object_refs WHERE lineage_id = ?1 AND payload_id = ?2"],
};

const OBJECT: Content = Content {
    referenced: "SELECT EXISTS (SELECT 1 FROM lineage_payload_object_refs WHERE object_hash = ?2)
        OR EXISTS (SELECT 1 FROM lineage_payload_nested_object_refs WHERE object_hash = ?2)
        OR EXISTS (SELECT 1 FROM request_object_refs WHERE object_hash = ?2)",
    deletes: &["DELETE FROM objects WHERE hash = ?2"],
};

/// Revision references that keep it, and therefore its ancestors, alive. Turns,
/// branch associations and receipts belong to the revision instead.
const REVISION_REFERENCED: &str =
    "SELECT EXISTS (SELECT 1 FROM lineage_branches WHERE lineage_id = ?1 AND head_revision_id = ?2)
         OR EXISTS (SELECT 1 FROM lineage_branches WHERE lineage_id = ?1 AND initial_revision_id = ?2)
         OR EXISTS (SELECT 1 FROM lineage_retained_revisions WHERE lineage_id = ?1 AND revision_id = ?2)
         OR EXISTS (SELECT 1 FROM lineage_session_receipt_results WHERE lineage_id = ?1 AND result_revision_id = ?2)
         OR EXISTS (SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND parent_revision_id = ?2)";

/// Records that only reclamation may delete. Their guards are suspended for the
/// rest of a step that deletes a revision or a deleted branch's receipts.
const IMMUTABLE_RECORDS: [&str; 3] = [
    "lineage_commit_receipt_delete",
    "lineage_session_receipt_delete",
    "lineage_turn_transition_delete",
];

/// Delete guards dropped in this step, restored before it commits.
#[derive(Default)]
struct SuspendedGuards(Vec<String>);

impl SuspendedGuards {
    fn suspend(&mut self, tx: &Transaction<'_>) -> Result<()> {
        if !self.0.is_empty() {
            return Ok(());
        }
        for name in IMMUTABLE_RECORDS {
            let definition = tx.query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
                [name],
                |row| row.get::<_, String>(0),
            )?;
            tx.execute_batch(&format!("DROP TRIGGER {name}"))?;
            self.0.push(definition);
        }
        Ok(())
    }

    fn restore(self, tx: &Transaction<'_>) -> Result<()> {
        for definition in self.0 {
            tx.execute_batch(&definition)?;
        }
        Ok(())
    }
}

/// Reclaims up to `max_rows` queued rows in one transaction. A queued row that
/// is still referenced leaves the queue; it is queued again when it loses a
/// reference, so the work follows the garbage rather than the store size.
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
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut step = ReclamationStep::default();
    let mut guards = SuspendedGuards::default();
    for kind in Kind::ALL {
        let budget = max_rows - step.rows_examined;
        if budget == 0 {
            break;
        }
        let ids = tx
            .prepare_cached(
                "SELECT id FROM lineage_reclamation_queue
                 WHERE lineage_id = ?1 AND kind = ?2 ORDER BY id LIMIT ?3",
            )?
            .query_map(
                (
                    lineage.as_str(),
                    kind.name(),
                    i64::try_from(budget).unwrap_or(i64::MAX),
                ),
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            tx.prepare_cached(
                "DELETE FROM lineage_reclamation_queue
                 WHERE lineage_id = ?1 AND kind = ?2 AND id = ?3",
            )?
            .execute((lineage.as_str(), kind.name(), &id))?;
            step.rows_examined += 1;
            match kind {
                Kind::Branch => reclaim_deleted_branch(&tx, lineage, &id, &mut guards)?,
                Kind::Revision => reclaim_revision(&tx, lineage, &id, &mut guards)?,
                Kind::Root => reclaim_content(&tx, lineage, &id, &ROOT)?,
                Kind::HistoryIndex => reclaim_content(&tx, lineage, &id, &HISTORY_INDEX)?,
                Kind::Node => reclaim_content(&tx, lineage, &id, &NODE)?,
                Kind::Payload => reclaim_content(&tx, lineage, &id, &PAYLOAD)?,
                Kind::Object => reclaim_content(&tx, lineage, &id, &OBJECT)?,
            }
        }
    }
    guards.restore(&tx)?;
    step.complete = !reclamation_pending(&tx, lineage)?;
    tx.commit()?;
    Ok(step)
}

pub(crate) fn reclamation_pending(conn: &Connection, lineage: &LineageId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM lineage_reclamation_queue WHERE lineage_id = ?1)",
        )?
        .query_row([lineage.as_str()], |row| row.get(0))?)
}

fn referenced(tx: &Transaction<'_>, sql: &str, lineage: &LineageId, id: &str) -> Result<bool> {
    Ok(tx
        .prepare_cached(sql)?
        .query_row((lineage.as_str(), id), |row| row.get(0))?)
}

fn reclaim_content(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    id: &str,
    content: &Content,
) -> Result<()> {
    if referenced(tx, content.referenced, lineage, id)? {
        return Ok(());
    }
    for sql in content.deletes {
        tx.prepare_cached(sql)?.execute((lineage.as_str(), id))?;
    }
    Ok(())
}

/// Releases the content of a revision the branches still reach, or deletes an
/// unreachable revision with the turns, associations and receipts it owns.
fn reclaim_revision(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    id: &str,
    guards: &mut SuspendedGuards,
) -> Result<()> {
    if !revision_exists(tx, lineage, id)? {
        return Ok(());
    }
    if referenced(tx, REVISION_REFERENCED, lineage, id)? {
        release_revision_content(tx, lineage, id)?;
        return Ok(());
    }
    guards.suspend(tx)?;
    let params = (lineage.as_str(), id);
    tx.prepare_cached(
        "DELETE FROM lineage_turn_transitions
         WHERE (lineage_id, session_id, turn_id) IN (
             SELECT lineage_id, session_id, turn_id FROM lineage_turns
             WHERE lineage_id = ?1 AND submitted_revision_id = ?2)",
    )?
    .execute(params)?;
    let receipts = tx
        .prepare_cached(
            "DELETE FROM lineage_commit_receipts
             WHERE lineage_id = ?1 AND (result_revision_id = ?2 OR prior_revision_id = ?2)
             RETURNING session_id, fingerprint",
        )?
        .query_map(params, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Session receipts that a result still replays stay with that result.
    let unowned = "NOT EXISTS (
            SELECT 1 FROM lineage_session_receipt_results result
            WHERE result.lineage_id = receipt.lineage_id AND result.session_id = receipt.session_id
              AND result.fingerprint = receipt.fingerprint)
        AND NOT EXISTS (
            SELECT 1 FROM lineage_turn_transitions transition
            WHERE transition.lineage_id = receipt.lineage_id
              AND transition.session_id = receipt.session_id
              AND transition.fingerprint = receipt.fingerprint)";
    for (session, fingerprint) in &receipts {
        tx.prepare_cached(&format!(
            "DELETE FROM lineage_session_receipts AS receipt
             WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3 AND {unowned}"
        ))?
        .execute((lineage.as_str(), session, fingerprint))?;
    }
    tx.prepare_cached(&format!(
        "DELETE FROM lineage_session_receipts AS receipt
         WHERE (lineage_id, session_id, turn_id) IN (
             SELECT lineage_id, session_id, turn_id FROM lineage_turns
             WHERE lineage_id = ?1 AND submitted_revision_id = ?2)
           AND {unowned}"
    ))?
    .execute(params)?;
    tx.prepare_cached(
        "DELETE FROM lineage_branch_revisions WHERE lineage_id = ?1 AND revision_id = ?2",
    )?
    .execute(params)?;
    // A continuation submitted at the same revision goes before the turn it continues.
    loop {
        let deleted = tx
            .prepare_cached(
                "DELETE FROM lineage_turns AS turn
                 WHERE lineage_id = ?1 AND submitted_revision_id = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM lineage_commit_receipts receipt
                       WHERE receipt.lineage_id = turn.lineage_id
                         AND receipt.session_id = turn.session_id
                         AND receipt.turn_id = turn.turn_id)
                   AND NOT EXISTS (
                       SELECT 1 FROM lineage_turns continuation
                       WHERE continuation.lineage_id = turn.lineage_id
                         AND continuation.session_id = turn.session_id
                         AND continuation.continuation_of = turn.turn_id)
                   AND NOT EXISTS (
                       SELECT 1 FROM lineage_turn_transitions transition
                       WHERE transition.lineage_id = turn.lineage_id
                         AND transition.session_id = turn.session_id
                         AND transition.turn_id = turn.turn_id)",
            )?
            .execute(params)?;
        if deleted == 0 {
            break;
        }
    }
    tx.prepare_cached(
        "DELETE FROM lineage_revisions
         WHERE lineage_id = ?1 AND revision_id = ?2 AND NOT EXISTS (
             SELECT 1 FROM lineage_turns
             WHERE lineage_id = ?1 AND submitted_revision_id = ?2)",
    )?
    .execute(params)?;
    Ok(())
}

/// A deleted branch no longer replays its session results. Dropping the
/// receipts that own them releases the result revisions.
fn reclaim_deleted_branch(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    session: &str,
    guards: &mut SuspendedGuards,
) -> Result<()> {
    let deleted = tx
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM lineage_branches
             WHERE lineage_id = ?1 AND session_id = ?2 AND deleted_at IS NOT NULL)",
        )?
        .query_row((lineage.as_str(), session), |row| row.get::<_, bool>(0))?;
    if !deleted {
        return Ok(());
    }
    guards.suspend(tx)?;
    let owned = "EXISTS (
        SELECT 1 FROM lineage_session_receipt_results result
        WHERE result.lineage_id = owner.lineage_id AND result.session_id = owner.session_id
          AND result.fingerprint = owner.fingerprint)";
    tx.prepare_cached(&format!(
        "DELETE FROM lineage_turn_transitions AS owner
         WHERE lineage_id = ?1 AND session_id = ?2 AND {owned}"
    ))?
    .execute((lineage.as_str(), session))?;
    tx.prepare_cached(&format!(
        "DELETE FROM lineage_session_receipts AS owner
         WHERE lineage_id = ?1 AND session_id = ?2 AND {owned}"
    ))?
    .execute((lineage.as_str(), session))?;
    Ok(())
}

fn revision_exists(tx: &Transaction<'_>, lineage: &LineageId, id: &str) -> Result<bool> {
    Ok(tx
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM lineage_revisions WHERE lineage_id = ?1 AND revision_id = ?2)",
        )?
        .query_row((lineage.as_str(), id), |row| row.get(0))?)
}
