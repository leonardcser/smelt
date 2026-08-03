use super::*;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct ReachabilityReport {
    pub(crate) reachable_revisions: BTreeSet<String>,
    pub(crate) unreachable_revisions: BTreeSet<String>,
    pub(crate) reachable_roots: BTreeSet<String>,
    pub(crate) unreachable_roots: BTreeSet<String>,
    pub(crate) reachable_nodes: BTreeSet<String>,
    pub(crate) unreachable_nodes: BTreeSet<String>,
    pub(crate) reachable_payloads: BTreeSet<String>,
    pub(crate) unreachable_payloads: BTreeSet<String>,
    pub(crate) reachable_objects: BTreeSet<String>,
    pub(crate) unreachable_objects: BTreeSet<String>,
}

#[cfg(test)]
pub(crate) fn query_strings(
    conn: &Connection,
    sql: &str,
    lineage: &LineageId,
) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([lineage.as_str()], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<BTreeSet<_>, _>>()
        .map_err(StoreError::from)
}

#[cfg(test)]
pub(crate) fn inspect_reachability(
    conn: &Connection,
    lineage: &LineageId,
) -> Result<ReachabilityReport> {
    let mut revision_queue = VecDeque::new();
    for revision in query_strings(
        conn,
        "SELECT head_revision_id FROM lineage_branches
         WHERE lineage_id = ?1 AND deleted_at IS NULL
         UNION
         SELECT initial_revision_id FROM lineage_branches WHERE lineage_id = ?1
         UNION
         SELECT revision_id FROM lineage_retained_revisions WHERE lineage_id = ?1",
        lineage,
    )? {
        revision_queue.push_back(RevisionId::from_db(revision)?);
    }
    let mut reachable_revisions = BTreeSet::new();
    let mut reachable_roots = BTreeSet::new();
    let mut reachable_payloads = BTreeSet::new();
    while let Some(id) = revision_queue.pop_front() {
        if !reachable_revisions.insert(id.as_str().to_owned()) {
            continue;
        }
        let revision = load_revision(conn, lineage, &id)?;
        if let Some(parent) = revision.parent_id {
            revision_queue.push_back(parent);
        }
        reachable_roots.insert(revision.history_root.id.as_str().to_owned());
        reachable_roots.insert(revision.transcript_root.id.as_str().to_owned());
        reachable_payloads.insert(revision.state_payload_id.as_str().to_owned());
    }

    let mut node_queue = VecDeque::new();
    for root_id in &reachable_roots {
        let root = load_root(conn, lineage, &RootId::from_db(root_id.clone())?)?;
        if let Some(node_id) = root.node_id {
            node_queue.push_back(node_id);
        }
    }
    let mut reachable_nodes = BTreeSet::new();
    while let Some(id) = node_queue.pop_front() {
        if !reachable_nodes.insert(id.as_str().to_owned()) {
            continue;
        }
        let node = load_node_shallow(conn, lineage, &id, None)?;
        for entry in node.entries {
            match entry.target {
                EntryTarget::Item(id) => {
                    reachable_payloads.insert(id.as_str().to_owned());
                }
                EntryTarget::Child(id) => node_queue.push_back(id),
            }
        }
    }
    let mut reachable_objects = BTreeSet::new();
    for payload_id in &reachable_payloads {
        let payload = load_payload_ref(conn, lineage, &PayloadId::from_db(payload_id.clone())?)?;
        reachable_objects.insert(payload.object_hash);
        let mut statement = conn.prepare(
            "SELECT object_hash FROM lineage_payload_nested_object_refs
             WHERE lineage_id = ?1 AND payload_id = ?2",
        )?;
        let rows = statement.query_map((lineage.as_str(), payload_id), |row| {
            row.get::<_, String>(0)
        })?;
        reachable_objects.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
    }

    let all_revisions = query_strings(
        conn,
        "SELECT revision_id FROM lineage_revisions WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_roots = query_strings(
        conn,
        "SELECT root_id FROM lineage_sequence_roots WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_nodes = query_strings(
        conn,
        "SELECT node_id FROM lineage_sequence_nodes WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_payloads = query_strings(
        conn,
        "SELECT payload_id FROM lineage_payload_object_refs WHERE lineage_id = ?1",
        lineage,
    )?;
    let all_objects = query_strings(
        conn,
        "SELECT object_hash FROM lineage_payload_object_refs WHERE lineage_id = ?1
         UNION
         SELECT object_hash FROM lineage_payload_nested_object_refs WHERE lineage_id = ?1",
        lineage,
    )?;
    Ok(ReachabilityReport {
        unreachable_revisions: &all_revisions - &reachable_revisions,
        unreachable_roots: &all_roots - &reachable_roots,
        unreachable_nodes: &all_nodes - &reachable_nodes,
        unreachable_payloads: &all_payloads - &reachable_payloads,
        unreachable_objects: &all_objects - &reachable_objects,
        reachable_revisions,
        reachable_roots,
        reachable_nodes,
        reachable_payloads,
        reachable_objects,
    })
}
