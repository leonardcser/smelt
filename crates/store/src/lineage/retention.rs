use super::*;

/// Newest submitted turns per branch whose revisions all keep full content.
pub(crate) const RECENT_TURNS: i64 = 16;

/// Releases the content of revisions in a rowid window that the retention
/// policy no longer needs. Every revision row stays as a chain link; content is
/// kept for branch endpoints, retained revisions, turn boundaries (each
/// submitted revision and its parent) and everything from the
/// [`RECENT_TURNS`]th newest submitted turn of a branch onwards. Reclamation then
/// sweeps whatever only released revisions referenced.
pub(crate) fn release_revision_content(
    tx: &Transaction<'_>,
    lineage: &LineageId,
    lower: i64,
    upper: i64,
) -> Result<usize> {
    let released = tx
        .prepare_cached(
            "UPDATE lineage_revisions AS revision
             SET history_root_id = NULL, transcript_root_id = NULL, state_payload_id = NULL
             WHERE revision.rowid BETWEEN ?2 AND ?3 AND revision.lineage_id = ?1
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
                         ORDER BY turn.turn_id DESC LIMIT 1 OFFSET ?4), 0))
             RETURNING revision_id",
        )?
        .query_map((lineage.as_str(), lower, upper, RECENT_TURNS - 1), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Released content can no longer replay a session result.
    let mut results = tx.prepare_cached(
        "DELETE FROM lineage_session_receipt_results
         WHERE lineage_id = ?1 AND result_revision_id = ?2",
    )?;
    for id in &released {
        results.execute((lineage.as_str(), id))?;
    }
    Ok(released.len())
}
