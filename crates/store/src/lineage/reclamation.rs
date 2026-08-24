use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReclamationStep {
    pub(crate) branch_heads_cleared: usize,
    pub(crate) canonical_rows_deleted: usize,
    pub(crate) objects_deleted: usize,
    pub(crate) complete: bool,
}

impl ReclamationStep {
    pub(crate) fn work_rows(self) -> usize {
        self.branch_heads_cleared
            .saturating_add(self.canonical_rows_deleted)
            .saturating_add(self.objects_deleted)
    }
}

fn suspend_receipt_delete_guards(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DROP TRIGGER lineage_session_receipt_delete;
         DROP TRIGGER lineage_turn_transition_delete;
         DROP TRIGGER lineage_commit_receipt_delete;",
    )?;
    Ok(())
}

fn restore_receipt_delete_guards(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS lineage_session_receipt_delete
         BEFORE DELETE ON lineage_session_receipts
         BEGIN
             SELECT RAISE(ABORT, 'lineage session receipts are immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS lineage_turn_transition_delete
         BEFORE DELETE ON lineage_turn_transitions
         BEGIN
             SELECT RAISE(ABORT, 'lineage turn transitions are immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS lineage_commit_receipt_delete
         BEFORE DELETE ON lineage_commit_receipts
         BEGIN
             SELECT RAISE(ABORT, 'lineage commit receipts are immutable');
         END;",
    )?;
    Ok(())
}

fn prepare_reclamation_marks(tx: &Transaction<'_>, lineage: &LineageId) -> Result<()> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_revisions (
             revision_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_roots (
             root_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_nodes (
             node_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS smelt_reachable_payloads (
             payload_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         DELETE FROM smelt_reachable_revisions;
         DELETE FROM smelt_reachable_roots;
         DELETE FROM smelt_reachable_nodes;
         DELETE FROM smelt_reachable_payloads;",
    )?;
    tx.execute(
        "WITH RECURSIVE reachable(revision_id) AS (
             SELECT head_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NULL
             UNION
             SELECT initial_revision_id
             FROM lineage_branches
             WHERE lineage_id = ?1
             UNION
             SELECT revision_id
             FROM lineage_retained_revisions
             WHERE lineage_id = ?1
             UNION
             SELECT revision.parent_revision_id
             FROM reachable
             JOIN lineage_revisions revision
               ON revision.lineage_id = ?1
              AND revision.revision_id = reachable.revision_id
             WHERE revision.parent_revision_id IS NOT NULL
         )
         INSERT OR IGNORE INTO smelt_reachable_revisions (revision_id)
         SELECT revision_id FROM reachable WHERE revision_id IS NOT NULL",
        [lineage.as_str()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO smelt_reachable_roots (root_id)
         SELECT history_root_id FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)
         UNION
         SELECT transcript_root_id FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)",
        [lineage.as_str()],
    )?;
    tx.execute(
        "WITH RECURSIVE reachable(node_id) AS (
             SELECT root_node_id
             FROM lineage_sequence_roots
             WHERE lineage_id = ?1
               AND root_id IN (SELECT root_id FROM smelt_reachable_roots)
               AND root_node_id IS NOT NULL
             UNION
             SELECT entry.child_node_id
             FROM reachable
             JOIN lineage_sequence_entries entry
               ON entry.lineage_id = ?1 AND entry.node_id = reachable.node_id
             WHERE entry.entry_kind = 'child'
         )
         INSERT OR IGNORE INTO smelt_reachable_nodes (node_id)
         SELECT node_id FROM reachable WHERE node_id IS NOT NULL",
        [lineage.as_str()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO smelt_reachable_payloads (payload_id)
         SELECT state_payload_id
         FROM lineage_revisions
         WHERE lineage_id = ?1
           AND revision_id IN (SELECT revision_id FROM smelt_reachable_revisions)
         UNION
         SELECT payload_id
         FROM lineage_sequence_entries
         WHERE lineage_id = ?1
           AND node_id IN (SELECT node_id FROM smelt_reachable_nodes)
           AND entry_kind = 'item'",
        [lineage.as_str()],
    )?;
    Ok(())
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
    prepare_reclamation_marks(&tx, lineage)?;

    let cleared = tx.execute(
        "UPDATE lineage_branches
         SET head_revision_id = NULL
         WHERE rowid IN (
             SELECT rowid FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NOT NULL
               AND head_revision_id IS NOT NULL
             LIMIT ?2
         )",
        rusqlite::params![lineage.as_str(), limit],
    )?;
    if cleared > 0 {
        tx.commit()?;
        return Ok(ReclamationStep {
            branch_heads_cleared: cleared,
            complete: false,
            ..ReclamationStep::default()
        });
    }

    suspend_receipt_delete_guards(&tx)?;
    let statements = [
        "DELETE FROM lineage_turn_transitions
         WHERE rowid IN (
             SELECT transition.rowid
             FROM lineage_turn_transitions transition
             JOIN lineage_turns turn
               ON turn.lineage_id = transition.lineage_id
              AND turn.session_id = transition.session_id
              AND turn.turn_id = transition.turn_id
             WHERE transition.lineage_id = ?1
               AND turn.submitted_revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_session_receipts
         WHERE rowid IN (
             SELECT receipt.rowid
             FROM lineage_session_receipts receipt
             WHERE receipt.lineage_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_turn_transitions transition
                   WHERE transition.lineage_id = receipt.lineage_id
                     AND transition.session_id = receipt.session_id
                     AND transition.fingerprint = receipt.fingerprint
               )
               AND (
                   receipt.turn_id IN (
                       SELECT turn.turn_id
                       FROM lineage_turns turn
                       WHERE turn.lineage_id = receipt.lineage_id
                         AND turn.session_id = receipt.session_id
                         AND turn.submitted_revision_id NOT IN (
                             SELECT revision_id FROM smelt_reachable_revisions
                         )
                   )
                   OR receipt.fingerprint IN (
                       SELECT stored_commit.fingerprint
                       FROM lineage_commit_receipts stored_commit
                       WHERE stored_commit.lineage_id = receipt.lineage_id
                         AND (
                             stored_commit.result_revision_id NOT IN (
                                 SELECT revision_id FROM smelt_reachable_revisions
                             )
                             OR stored_commit.prior_revision_id NOT IN (
                                 SELECT revision_id FROM smelt_reachable_revisions
                             )
                         )
                   )
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_commit_receipts
         WHERE rowid IN (
             SELECT rowid FROM lineage_commit_receipts
             WHERE lineage_id = ?1
               AND (
                   result_revision_id NOT IN (
                       SELECT revision_id FROM smelt_reachable_revisions
                   )
                   OR prior_revision_id NOT IN (
                       SELECT revision_id FROM smelt_reachable_revisions
                   )
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_branch_revisions
         WHERE rowid IN (
             SELECT rowid FROM lineage_branch_revisions
             WHERE lineage_id = ?1
               AND revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_turns
         WHERE rowid IN (
             SELECT turn.rowid
             FROM lineage_turns turn
             WHERE turn.lineage_id = ?1
               AND turn.submitted_revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_commit_receipts receipt
                   WHERE receipt.lineage_id = turn.lineage_id
                     AND receipt.session_id = turn.session_id
                     AND receipt.turn_id = turn.turn_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_turns continuation
                   WHERE continuation.lineage_id = turn.lineage_id
                     AND continuation.session_id = turn.session_id
                     AND continuation.continuation_of = turn.turn_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_revisions
         WHERE rowid IN (
             SELECT revision.rowid
             FROM lineage_revisions revision
             WHERE revision.lineage_id = ?1
               AND revision.revision_id NOT IN (
                   SELECT revision_id FROM smelt_reachable_revisions
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions child
                   WHERE child.lineage_id = revision.lineage_id
                     AND child.parent_revision_id = revision.revision_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_roots
         WHERE rowid IN (
             SELECT root.rowid
             FROM lineage_sequence_roots root
             WHERE root.lineage_id = ?1
               AND root.root_id NOT IN (SELECT root_id FROM smelt_reachable_roots)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions revision
                   WHERE revision.lineage_id = root.lineage_id
                     AND (revision.history_root_id = root.root_id
                          OR revision.transcript_root_id = root.root_id)
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_transcript_extent_nodes
         WHERE rowid IN (
             SELECT profile.rowid
             FROM lineage_transcript_extent_nodes profile
             WHERE profile.lineage_id = ?1
               AND profile.node_id NOT IN (SELECT node_id FROM smelt_reachable_nodes)
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_entries
         WHERE rowid IN (
             SELECT entry.rowid
             FROM lineage_sequence_entries entry
             WHERE entry.lineage_id = ?1
               AND entry.node_id NOT IN (SELECT node_id FROM smelt_reachable_nodes)
             LIMIT ?2
         )",
        "DELETE FROM lineage_sequence_nodes
         WHERE rowid IN (
             SELECT node.rowid
             FROM lineage_sequence_nodes node
             WHERE node.lineage_id = ?1
               AND node.node_id NOT IN (SELECT node_id FROM smelt_reachable_nodes)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = node.lineage_id
                     AND (entry.node_id = node.node_id OR entry.child_node_id = node.node_id)
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_roots root
                   WHERE root.lineage_id = node.lineage_id
                     AND root.root_node_id = node.node_id
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_transcript_record_profiles
         WHERE rowid IN (
             SELECT profile.rowid
             FROM lineage_transcript_record_profiles profile
             WHERE profile.lineage_id = ?1
               AND profile.payload_id NOT IN (
                   SELECT payload_id FROM smelt_reachable_payloads
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_payload_nested_object_refs
         WHERE rowid IN (
             SELECT nested.rowid
             FROM lineage_payload_nested_object_refs nested
             WHERE nested.lineage_id = ?1
               AND nested.payload_id NOT IN (
                   SELECT payload_id FROM smelt_reachable_payloads
               )
             LIMIT ?2
         )",
        "DELETE FROM lineage_payload_object_refs
         WHERE rowid IN (
             SELECT payload.rowid
             FROM lineage_payload_object_refs payload
             WHERE payload.lineage_id = ?1
               AND payload.payload_id NOT IN (
                   SELECT payload_id FROM smelt_reachable_payloads
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_payload_nested_object_refs nested
                   WHERE nested.lineage_id = payload.lineage_id
                     AND nested.payload_id = payload.payload_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = payload.lineage_id
                     AND entry.payload_id = payload.payload_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions revision
                   WHERE revision.lineage_id = payload.lineage_id
                     AND revision.state_payload_id = payload.payload_id
               )
             LIMIT ?2
         )",
    ];
    for statement in statements {
        let deleted = tx.execute(statement, rusqlite::params![lineage.as_str(), limit])?;
        if deleted > 0 {
            restore_receipt_delete_guards(&tx)?;
            tx.commit()?;
            return Ok(ReclamationStep {
                canonical_rows_deleted: deleted,
                complete: false,
                ..ReclamationStep::default()
            });
        }
    }
    restore_receipt_delete_guards(&tx)?;

    let objects_deleted = tx.execute(
        "DELETE FROM objects
         WHERE rowid IN (
             SELECT object.rowid
             FROM objects object
             WHERE NOT EXISTS (
                 SELECT 1 FROM request_object_refs request
                 WHERE request.object_hash = object.hash
             )
               AND NOT EXISTS (
                 SELECT 1 FROM lineage_payload_object_refs payload
                 WHERE payload.object_hash = object.hash
             )
               AND NOT EXISTS (
                 SELECT 1 FROM lineage_payload_nested_object_refs nested
                 WHERE nested.object_hash = object.hash
             )
             LIMIT ?1
         )",
        [limit],
    )?;
    tx.commit()?;
    Ok(ReclamationStep {
        objects_deleted,
        complete: objects_deleted == 0,
        ..ReclamationStep::default()
    })
}
