use super::*;

/// Newest submitted turns per branch whose revisions all keep full content.
pub(crate) const RECENT_TURNS: i64 = 16;

/// Releases a revision's content once the retention policy no longer needs it.
/// The revision row stays as a chain link; content is kept for branch
/// endpoints, retained revisions, turn boundaries (each submitted revision and
/// its parent) and everything from the [`RECENT_TURNS`]th newest submitted turn
/// of a branch onwards. Releasing queues the content for reclamation.
pub(crate) fn release_revision_content(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    revision: &str,
) -> Result<bool> {
    let released = tx
        .prepare_cached(
            "UPDATE lineage_revisions AS revision
             SET history_root_id = NULL, transcript_root_id = NULL, state_payload_id = NULL
             WHERE revision.lineage_id = ?1 AND revision.revision_id = ?2
               AND revision.state_payload_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_branches branch
                   WHERE branch.lineage_id = revision.lineage_id
                     AND (branch.head_revision_id = revision.revision_id
                          OR branch.initial_revision_id = revision.revision_id))
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_retained_revisions retained
                   WHERE retained.lineage_id = revision.lineage_id
                     AND retained.revision_id = revision.revision_id)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_turns turn
                   WHERE turn.lineage_id = revision.lineage_id
                     AND turn.submitted_revision_id = revision.revision_id)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_revisions child
                   JOIN lineage_turns turn ON turn.lineage_id = child.lineage_id
                    AND turn.submitted_revision_id = child.revision_id
                   WHERE child.lineage_id = revision.lineage_id
                     AND child.parent_revision_id = revision.revision_id)
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_branch_revisions association
                   WHERE association.lineage_id = revision.lineage_id
                     AND association.revision_id = revision.revision_id
                     AND association.branch_sequence >= coalesce((
                         SELECT turn.submitted_sequence FROM lineage_turns turn
                         WHERE turn.lineage_id = association.lineage_id
                           AND turn.session_id = association.session_id
                         ORDER BY turn.turn_id DESC LIMIT 1 OFFSET ?3), 0))",
        )?
        .execute((lineage.as_str(), revision, RECENT_TURNS - 1))?
        == 1;
    if released {
        // Released content can no longer replay a session result.
        tx.prepare_cached(
            "DELETE FROM lineage_session_receipt_results
             WHERE lineage_id = ?1 AND result_revision_id = ?2",
        )?
        .execute((lineage.as_str(), revision))?;
    }
    Ok(released)
}

/// Queues the revisions a newly submitted turn moved out of its branch's
/// retention window, so reclamation can release their content.
pub(crate) fn queue_retention_window(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO lineage_reclamation_queue (lineage_id, kind, id)
         SELECT lineage_id, 'revision', revision_id FROM lineage_branch_revisions
         WHERE lineage_id = ?1 AND session_id = ?2
           AND branch_sequence >= coalesce((
               SELECT submitted_sequence FROM lineage_turns
               WHERE lineage_id = ?1 AND session_id = ?2
               ORDER BY turn_id DESC LIMIT 1 OFFSET ?3), 0)
           AND branch_sequence < coalesce((
               SELECT submitted_sequence FROM lineage_turns
               WHERE lineage_id = ?1 AND session_id = ?2
               ORDER BY turn_id DESC LIMIT 1 OFFSET ?3 - 1), 0)",
    )?
    .execute((lineage.as_str(), branch.as_str(), RECENT_TURNS))?;
    Ok(())
}
